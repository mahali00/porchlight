import { Effect, Stream } from "effect";
import { closeSync, mkdirSync, openSync } from "node:fs";
import * as Path from "node:path";
import { UpstreamError } from "../errors.js";

export interface ProcessSpec {
  command: ReadonlyArray<string>;
  cwd: string | undefined;
  env: Record<string, string>;
  logFile: string;
}

export interface RunningProcess {
  pid: number;
  write: (line: string) => Effect.Effect<void, UpstreamError>;
  exited: Effect.Effect<void>;
}

const inheritedVariables = ["PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR"];

const maxLineBytes = 16 * 1024 * 1024;

export const processEnvironment = (extra: Record<string, string>): Record<string, string> => ({
  ...Object.fromEntries(
    inheritedVariables.flatMap((name) => {
      const value = process.env[name];
      return value === undefined ? [] : [[name, value]];
    }),
  ),
  ...extra,
});

const lineTooLong = new UpstreamError({ message: `Output line exceeds ${maxLineBytes} bytes` });

const splitLines = (buffer: string, chunk: string) => {
  const lines = (buffer + chunk).split("\n");
  const rest = lines.pop() ?? "";
  if (rest.length > maxLineBytes) return Effect.fail(lineTooLong);
  const complete = lines.map((line) => line.trim()).filter((line) => line.length > 0);
  return Effect.succeed([rest, complete] as const);
};

const lines = (stdout: ReadableStream<Uint8Array>) =>
  Stream.fromReadableStream({
    evaluate: () => stdout,
    onError: (cause) => new UpstreamError({ message: "Failed to read output", cause }),
  }).pipe(
    Stream.decodeText,
    Stream.mapAccumEffect(() => "", splitLines),
  );

const spawn = (spec: ProcessSpec) =>
  Effect.try({
    try: () => {
      mkdirSync(Path.dirname(spec.logFile), { recursive: true, mode: 0o700 });
      const log = openSync(spec.logFile, "a", 0o600);
      try {
        return Bun.spawn([...spec.command], {
          cwd: spec.cwd,
          env: processEnvironment(spec.env),
          stdin: "pipe",
          stdout: "pipe",
          stderr: log,
        });
      } finally {
        closeSync(log);
      }
    },
    catch: (cause) => new UpstreamError({ message: `Failed to start ${spec.command.join(" ")}`, cause }),
  });

type Child = Effect.Success<ReturnType<typeof spawn>>;

const terminate = (child: Child) =>
  Effect.gen(function* () {
    if (child.exitCode !== null) return;
    child.stdin.end();
    child.kill("SIGTERM");
    yield* Effect.promise(() => child.exited).pipe(
      Effect.timeoutOrElse({ duration: "2 seconds", orElse: () => Effect.sync(() => child.kill("SIGKILL")) }),
    );
  });

export const startProcess = Effect.fn("Process.start")(function* (
  spec: ProcessSpec,
  onLine: (line: string) => Effect.Effect<void>,
) {
  const child = yield* Effect.acquireRelease(spawn(spec), terminate);
  yield* lines(child.stdout).pipe(
    Stream.runForEach(onLine),
    Effect.catch(() => terminate(child)),
    Effect.forkScoped,
  );
  return {
    pid: child.pid,
    write: (line) =>
      Effect.try({
        try: () => {
          child.stdin.write(`${line}\n`);
          child.stdin.flush();
        },
        catch: (cause) => new UpstreamError({ message: `Failed to write to ${spec.command.join(" ")}`, cause }),
      }),
    exited: Effect.promise(() => child.exited).pipe(Effect.asVoid),
  } satisfies RunningProcess;
});
