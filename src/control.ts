import { Effect, Option, Schema } from "effect";
import { chmodSync, rmSync } from "node:fs";
import * as Path from "node:path";
import { stateDir } from "./config.js";
import { ListenError } from "./errors.js";

export const socketPath = Path.join(stateDir, "control.sock");

export const ServerView = Schema.Struct({
  key: Schema.String,
  slug: Schema.String,
  name: Schema.String,
  url: Schema.String,
  state: Schema.Literals(["starting", "waiting", "live", "refused"]),
  detail: Schema.String,
  tools: Schema.Number,
});

export type ServerView = typeof ServerView.Type;

export const DaemonState = Schema.Struct({
  pid: Schema.Number,
  startedAt: Schema.Number,
  tunnel: Schema.String,
  publicUrl: Schema.String,
  servers: Schema.Array(ServerView),
});

export type DaemonState = typeof DaemonState.Type;

export const AddResult = Schema.Union([
  Schema.Struct({ ok: Schema.Literal(true), server: ServerView }),
  Schema.Struct({ ok: Schema.Literal(false), error: Schema.String, nextAction: Schema.optional(Schema.String) }),
]);

export type AddResult = typeof AddResult.Type;

export const RemoveResult = Schema.Struct({ removed: Schema.Boolean });

export type RemoveResult = typeof RemoveResult.Type;

export interface ControlHandlers {
  state: () => Effect.Effect<DaemonState>;
  add: (body: unknown) => Effect.Effect<AddResult>;
  remove: (body: unknown) => Effect.Effect<RemoveResult>;
}

const readJson = (request: Request) =>
  Effect.tryPromise(() => request.json()).pipe(Effect.orElseSucceed((): unknown => undefined));

export const startControl = Effect.fn("Control.start")(function* (handlers: ControlHandlers) {
  const removeSocket = Effect.sync(() => rmSync(socketPath, { force: true }));
  yield* removeSocket;
  const run = <A>(effect: Effect.Effect<A>) =>
    Effect.runPromise(effect.pipe(Effect.map((value) => Response.json(value))));
  const server = yield* Effect.acquireRelease(
    Effect.try({
      try: () =>
        Bun.serve({
          unix: socketPath,
          routes: {
            "/state": { GET: () => run(handlers.state()) },
            "/servers": {
              POST: (request) => run(readJson(request).pipe(Effect.flatMap(handlers.add))),
              DELETE: (request) => run(readJson(request).pipe(Effect.flatMap(handlers.remove))),
            },
          },
          fetch: () => new Response("Not found", { status: 404 }),
        }),
      catch: (cause) => new ListenError({ message: `Couldn't open ${socketPath}`, cause }),
    }),
    (server) => Effect.promise(() => server.stop()).pipe(Effect.andThen(removeSocket)),
  );
  yield* Effect.try({
    try: () => chmodSync(socketPath, 0o600),
    catch: (cause) => new ListenError({ message: `Couldn't secure ${socketPath}`, cause }),
  });
  return server;
});

const request = (path: string, init: RequestInit = {}) =>
  Effect.tryPromise((signal) => fetch(`http://porchlight${path}`, { ...init, unix: socketPath, signal })).pipe(
    Effect.flatMap((response) => Effect.tryPromise(() => response.json())),
    Effect.timeout("20 seconds"),
    Effect.option,
  );

export const daemonState = Effect.fn("Control.state")(function* () {
  const body = yield* request("/state");
  return Option.flatMap(body, Schema.decodeUnknownOption(DaemonState));
});

export const addToDaemon = Effect.fn("Control.add")(function* (key: string, overrides: unknown) {
  const body = yield* request("/servers", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ key, overrides }),
  });
  return Option.flatMap(body, Schema.decodeUnknownOption(AddResult));
});

export const removeFromDaemon = Effect.fn("Control.remove")(function* (name: string) {
  const body = yield* request("/servers", {
    method: "DELETE",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name }),
  });
  return Option.flatMap(body, Schema.decodeUnknownOption(RemoveResult));
});
