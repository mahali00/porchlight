import { Effect, Option, Result, Schema, Stream } from "effect";
import type { CliInput, CliTool } from "../config.js";
import { UpstreamError } from "../errors.js";
import { placeholder, placeholdersOf, type Tool } from "../policy.js";
import { processEnvironment } from "./process.js";
import { rpcError, rpcResult } from "./rpc.js";
import type { Upstream } from "./types.js";

export interface CliSpec {
  name: string;
  tools: Record<string, CliTool>;
  env: Record<string, string>;
  cwd: string | undefined;
}

interface Outcome {
  text: string;
  isError: boolean;
}

const timeout = "60 seconds";
const maxOutput = 100_000;
const maxInput = 4_096;

const RpcRequest = Schema.Struct({
  id: Schema.optional(Schema.Union([Schema.String, Schema.Number, Schema.Null])),
  method: Schema.String,
  params: Schema.optional(Schema.Unknown),
});

type RpcRequest = typeof RpcRequest.Type;

const decodeBody = Schema.decodeUnknownOption(
  Schema.fromJsonString(Schema.Union([RpcRequest, Schema.Array(RpcRequest)])),
);

const CallParams = Schema.Struct({ name: Schema.String, arguments: Schema.optional(Schema.Unknown) });

const InitializeParams = Schema.Struct({ protocolVersion: Schema.optional(Schema.String) });

const inputValue = (settings: CliInput | undefined) => {
  const value = settings?.choices
    ? Schema.Literals(settings.choices)
    : Schema.String.check(
        Schema.isMaxLength(maxInput, { message: `must be at most ${maxInput} characters` }),
        Schema.isPattern(/^[^-]/, { message: 'can\'t be empty or start with "-"' }),
      );
  return settings?.description ? value.annotate({ description: settings.description }) : value;
};

const argumentsSchema = (tool: CliTool) =>
  Schema.Struct(Object.fromEntries(placeholdersOf(tool.run).map((name) => [name, inputValue(tool.inputs?.[name])])));

const noInputs = { type: "object", properties: {} };

const toolFor = (name: string, tool: CliTool): Tool => ({
  name,
  description: tool.description ?? `Runs ${tool.run.join(" ")}`,
  annotations: { readOnlyHint: tool.readOnly === true },
  inputSchema:
    placeholdersOf(tool.run).length === 0 ? noInputs : Schema.toJsonSchemaDocument(argumentsSchema(tool)).schema,
});

const fill = (run: ReadonlyArray<string>, values: Record<string, unknown>): ReadonlyArray<string> =>
  run.map((word) => {
    const name = placeholder.exec(word)?.[1];
    return name === undefined ? word : String(values[name]);
  });

const spawn = (argv: ReadonlyArray<string>, spec: CliSpec) =>
  Effect.try({
    try: () =>
      Bun.spawn([...argv], {
        cwd: spec.cwd,
        env: processEnvironment(spec.env),
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
      }),
    catch: (cause) => new UpstreamError({ message: `Couldn't run ${argv[0] ?? "the command"}`, cause }),
  });

type Child = Effect.Success<ReturnType<typeof spawn>>;

const kill = (child: Child) => Effect.sync(() => (child.exitCode === null ? child.kill("SIGKILL") : undefined));

const collect = (output: ReadableStream<Uint8Array>) =>
  Stream.fromReadableStream({
    evaluate: () => output,
    onError: (cause) => new UpstreamError({ message: "Couldn't read the command's output", cause }),
  }).pipe(
    Stream.decodeText,
    Stream.runFold(
      () => "",
      (text, chunk) => (text.length > maxOutput ? text : text + chunk),
    ),
    Effect.map((text) => (text.length > maxOutput ? `${text.slice(0, maxOutput)}\n… output cut off` : text.trim())),
  );

const execute = Effect.fn("CliUpstream.execute")(
  function* (argv: ReadonlyArray<string>, spec: CliSpec) {
    const child = yield* Effect.acquireRelease(spawn(argv, spec), kill);
    const { stdout, stderr, code } = yield* Effect.all(
      { stdout: collect(child.stdout), stderr: collect(child.stderr), code: Effect.promise(() => child.exited) },
      { concurrency: "unbounded" },
    );
    if (code === 0) return { text: stdout || "Done.", isError: false } satisfies Outcome;
    return { text: [stdout, stderr].filter(Boolean).join("\n") || `Exited with code ${code}`, isError: true };
  },
  Effect.scoped,
  Effect.timeoutOrElse({
    duration: timeout,
    orElse: () => Effect.succeed<Outcome>({ text: `Stopped after ${timeout}`, isError: true }),
  }),
  Effect.catch((error) => Effect.succeed<Outcome>({ text: error.message, isError: true })),
);

export const makeCliUpstream = (spec: CliSpec): Upstream => {
  const tools = Object.entries(spec.tools).map(([name, tool]) => toolFor(name, tool));

  const call = Effect.fn("CliUpstream.call")(function* (request: RpcRequest) {
    const params = Schema.decodeUnknownOption(CallParams)(request.params);
    const tool = Option.flatMap(params, ({ name }) => Option.fromUndefinedOr(spec.tools[name]));
    if (Option.isNone(params) || Option.isNone(tool)) return rpcError(request.id, -32602, "Unknown tool");
    const values = yield* Schema.decodeUnknownEffect(argumentsSchema(tool.value))(params.value.arguments ?? {}).pipe(
      Effect.result,
    );
    if (Result.isFailure(values)) return rpcError(request.id, -32602, `Invalid arguments: ${values.failure.message}`);
    const { text, isError } = yield* execute(fill(tool.value.run, values.success), spec);
    return rpcResult(request.id, { content: [{ type: "text", text }], isError });
  });

  const initialize = (request: RpcRequest) => {
    const params = Schema.decodeUnknownOption(InitializeParams)(request.params);
    return rpcResult(request.id, {
      protocolVersion: Option.getOrUndefined(params)?.protocolVersion ?? "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: spec.name, version: "1.0.0" },
    });
  };

  const answer = (request: RpcRequest) => {
    switch (request.method) {
      case "initialize":
        return Effect.succeed(initialize(request));
      case "ping":
        return Effect.succeed(rpcResult(request.id, {}));
      case "tools/list":
        return Effect.succeed(rpcResult(request.id, { tools }));
      case "tools/call":
        return call(request);
      default:
        return Effect.succeed(rpcError(request.id, -32601, `Method not found: ${request.method}`));
    }
  };

  return {
    describe: spec.name,
    listTools: Effect.succeed(tools),
    handle: Effect.fn("CliUpstream.handle")(function* (request, body) {
      if (request.method !== "POST") return new Response(null, { status: 405, headers: { allow: "POST" } });
      const decoded = decodeBody(new TextDecoder().decode(body));
      if (Option.isNone(decoded)) return Response.json(rpcError(null, -32700, "Parse error"), { status: 400 });
      const batch = Array.isArray(decoded.value);
      const requests = (batch ? decoded.value : [decoded.value]).filter((item) => item.id !== undefined);
      const replies = yield* Effect.forEach(requests, answer, { concurrency: "unbounded" });
      if (replies.length === 0) return new Response(null, { status: 202 });
      return Response.json(batch ? replies : replies[0]);
    }),
  };
};
