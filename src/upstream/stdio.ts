import { Deferred, Effect, Exit, Option, Predicate, PubSub, Schedule, Schema, Scope, Stream } from "effect";
import type { Duration } from "effect";
import { randomToken } from "../crypto.js";
import { UpstreamError } from "../errors.js";
import { Store } from "../store.js";
import { startProcess, type ProcessSpec, type RunningProcess } from "./process.js";
import { decodeToolsList, rpcError } from "./rpc.js";
import type { Upstream } from "./types.js";

export interface StdioSpec extends ProcessSpec {
  name: string;
  slug: string;
}

interface Session {
  id: string;
  scope: Scope.Closeable;
  process: RunningProcess;
  pending: Map<string, Deferred.Deferred<unknown>>;
  events: PubSub.PubSub<Uint8Array>;
  lastUsed: number;
}

const requestTimeout = "10 minutes";
const startTimeout = "1 minute";
const idleTimeoutMs = 30 * 60_000;

const RpcMessage = Schema.Record(Schema.String, Schema.Unknown);
const decodeMessages = Schema.decodeUnknownOption(
  Schema.fromJsonString(Schema.Union([RpcMessage, Schema.Array(RpcMessage)])),
);
const decodeLine = Schema.decodeUnknownOption(Schema.fromJsonString(RpcMessage));
const decodeParams = Schema.decodeUnknownOption(Schema.fromJsonString(Schema.Unknown));

const porchlightInitialize = {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "porchlight", version: "0.1.0" },
};

const encoder = new TextEncoder();
const decoder = new TextDecoder();

const idKey = (id: unknown): string => JSON.stringify(id);

const isRequest = (message: Record<string, unknown>): boolean =>
  typeof message["method"] === "string" && message["id"] !== undefined;

const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache" };

export const makeStdioUpstream = Effect.fn("StdioUpstream.make")(function* (spec: StdioSpec) {
  const parent = yield* Scope.Scope;
  const store = yield* Store;
  const sessions = new Map<string, Session>();
  const resuming = new Map<string, Deferred.Deferred<Session | undefined, UpstreamError>>();

  const close = (session: Session) => Scope.close(session.scope, Exit.void);

  const remember = (session: Session) =>
    store.stdioSessions.touch([{ id: session.id, lastUsed: session.lastUsed }]).pipe(Effect.ignore);

  const open = Effect.fn("StdioUpstream.open")(function* (id: string = randomToken(16)) {
    const scope = yield* Scope.fork(parent);
    const pending = new Map<string, Deferred.Deferred<unknown>>();
    const events = yield* PubSub.unbounded<Uint8Array>();
    const deliver = (line: string) =>
      Effect.suspend(() => {
        const message = decodeLine(line);
        if (Option.isNone(message)) return Effect.void;
        const key = idKey(message.value["id"]);
        const reply = typeof message.value["method"] === "string" ? undefined : pending.get(key);
        if (!reply) return PubSub.publish(events, encoder.encode(`event: message\ndata: ${line}\n\n`));
        pending.delete(key);
        return Deferred.succeed(reply, message.value);
      }).pipe(Effect.asVoid);
    const process = yield* startProcess(spec, deliver).pipe(
      Scope.provide(scope),
      Effect.onError(() => Scope.close(scope, Exit.void)),
    );
    const session: Session = { id, scope, process, pending, events, lastUsed: Date.now() };
    sessions.set(session.id, session);
    yield* Scope.addFinalizer(
      scope,
      Effect.gen(function* () {
        sessions.delete(session.id);
        yield* Effect.forEach(pending.values(), (reply) => Deferred.succeed(reply, undefined), { discard: true });
        pending.clear();
        yield* PubSub.shutdown(events);
        yield* remember(session);
      }),
    );
    yield* process.exited.pipe(Effect.andThen(close(session)), Effect.forkIn(parent));
    return session;
  });

  const send = (session: Session, message: Record<string, unknown>, timeout: Duration.Input) =>
    Effect.gen(function* () {
      session.lastUsed = Date.now();
      if (!isRequest(message)) {
        yield* session.process.write(JSON.stringify(message));
        return undefined;
      }
      const id = message["id"];
      const key = idKey(id);
      if (session.pending.has(key)) return rpcError(id, -32600, "A request with this id is already in progress");
      const reply = yield* Deferred.make<unknown>();
      session.pending.set(key, reply);
      yield* session.process.write(JSON.stringify(message));
      return yield* Deferred.await(reply).pipe(
        Effect.map((value) => value ?? rpcError(id, -32000, `${spec.name} stopped`)),
        Effect.timeoutOrElse({
          duration: timeout,
          orElse: () => Effect.succeed(rpcError(id, -32001, "Request timed out")),
        }),
        Effect.ensuring(Effect.sync(() => session.pending.delete(key))),
      );
    });

  const initialize = (session: Session, params: unknown) =>
    Effect.gen(function* () {
      const message = { jsonrpc: "2.0", id: "porchlight-initialize", method: "initialize", params };
      const reply = yield* send(session, message, startTimeout);
      if (!Predicate.isObject(reply) || reply["result"] === undefined) {
        return yield* new UpstreamError({ message: `${spec.name} didn't start. See ${spec.logFile}` });
      }
      yield* send(session, { jsonrpc: "2.0", method: "notifications/initialized" }, startTimeout);
    });

  const resume = Effect.fn("StdioUpstream.resume")(function* (id: string) {
    const saved = yield* store.stdioSessions.get(id, spec.slug).pipe(Effect.orElseSucceed(() => undefined));
    if (!saved || Date.now() - saved.lastUsed > idleTimeoutMs) return undefined;
    const session = yield* open(id);
    const params = Option.getOrElse(decodeParams(saved.initialize), () => porchlightInitialize);
    yield* initialize(session, params).pipe(Effect.onError(() => close(session)));
    yield* store.audit.write("session.resumed", `${spec.name} | ${id.slice(0, 8)}`).pipe(Effect.ignore);
    return session;
  });

  const find = (id: string): Effect.Effect<Session | undefined, UpstreamError> =>
    Effect.suspend(() => {
      const inFlight = resuming.get(id);
      if (inFlight) return Deferred.await(inFlight);
      const live = sessions.get(id);
      if (live) return Effect.succeed(live);
      const done = Deferred.makeUnsafe<Session | undefined, UpstreamError>();
      resuming.set(id, done);
      return resume(id).pipe(
        Effect.exit,
        Effect.tap((result) => Deferred.done(done, result)),
        Effect.ensuring(Effect.sync(() => resuming.delete(id))),
        Effect.flatten,
      );
    });

  const stream = (session: Session): Response =>
    new Response(Stream.fromPubSub(session.events).pipe(Stream.toReadableStream()), { headers: sseHeaders });

  const post = Effect.fn("StdioUpstream.post")(function* (request: Request, body: ArrayBuffer | undefined) {
    const decoded = decodeMessages(decoder.decode(body));
    if (Option.isNone(decoded)) return Response.json(rpcError(null, -32700, "Parse error"), { status: 400 });
    const batch = Array.isArray(decoded.value);
    const messages = Array.isArray(decoded.value) ? decoded.value : [decoded.value];
    const initializeMessage = messages.find((message) => message["method"] === "initialize");
    const sessionId = request.headers.get("mcp-session-id");
    const session = initializeMessage ? yield* open() : sessionId === null ? undefined : yield* find(sessionId);
    if (!session) {
      return sessionId === null
        ? Response.json(rpcError(null, -32000, "Missing Mcp-Session-Id header"), { status: 400 })
        : Response.json(rpcError(null, -32001, "Session not found"), { status: 404 });
    }
    if (initializeMessage) {
      const saved = { id: session.id, server: spec.slug, lastUsed: session.lastUsed };
      yield* store.stdioSessions
        .save({ ...saved, initialize: JSON.stringify(initializeMessage["params"] ?? porchlightInitialize) })
        .pipe(Effect.ignore);
    }
    const replies = yield* Effect.forEach(messages, (message) => send(session, message, requestTimeout), {
      concurrency: "unbounded",
    });
    const answered = replies.filter((reply) => reply !== undefined);
    const headers = { "mcp-session-id": session.id };
    if (answered.length === 0) return new Response(null, { status: 202, headers });
    return Response.json(batch ? answered : answered[0], { headers });
  });

  const handle = Effect.fn("StdioUpstream.handle")(function* (request: Request, body: ArrayBuffer | undefined) {
    const sessionId = request.headers.get("mcp-session-id");
    if (request.method === "POST") return yield* post(request, body);
    if (sessionId === null) return new Response(null, { status: 404 });
    if (request.method === "GET") {
      const session = yield* find(sessionId);
      return session ? stream(session) : new Response(null, { status: 404 });
    }
    const live = sessions.get(sessionId);
    const saved = yield* store.stdioSessions.get(sessionId, spec.slug).pipe(Effect.orElseSucceed(() => undefined));
    yield* store.stdioSessions.remove(sessionId).pipe(Effect.ignore);
    if (live) yield* close(live);
    return new Response(null, { status: live || saved ? 204 : 404 });
  });

  const handshake = (session: Session) =>
    initialize(session, porchlightInitialize).pipe(
      Effect.andThen(send(session, { jsonrpc: "2.0", id: 2, method: "tools/list" }, startTimeout)),
    );

  const listTools = Effect.acquireUseRelease(open(), handshake, close).pipe(
    Effect.flatMap((reply) => decodeToolsList(JSON.stringify(reply), spec.name)),
  );

  const sweep = Effect.gen(function* () {
    const now = Date.now();
    const live = [...sessions.values()];
    const idle = live.filter((session) => now - session.lastUsed > idleTimeoutMs);
    yield* Effect.forEach(idle, close, { concurrency: "unbounded", discard: true });
    yield* store.stdioSessions.touch(live.map(({ id, lastUsed }) => ({ id, lastUsed })));
    yield* store.stdioSessions.prune(spec.slug, now - idleTimeoutMs);
  }).pipe(Effect.ignore);

  yield* sweep.pipe(Effect.repeat(Schedule.spaced("1 minute")), Effect.forkScoped);

  return { describe: spec.command.join(" "), listTools, handle } satisfies Upstream;
});
