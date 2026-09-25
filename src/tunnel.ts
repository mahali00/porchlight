import { $ } from "bun";
import { Console, Effect } from "effect";
import { closeSync, existsSync, mkdirSync, openSync } from "node:fs";
import * as Os from "node:os";
import * as Path from "node:path";
import { logDir } from "./config.js";
import { TunnelError } from "./errors.js";

const bunGlobalBin = Path.join(process.env["BUN_INSTALL"] ?? Path.join(Os.homedir(), ".bun"), "bin", "opentunnel");

export const findOpenTunnel = (): string | undefined =>
  Bun.which("opentunnel") ?? (existsSync(bunGlobalBin) ? bunGlobalBin : undefined);

const bunExecutable = (): string | undefined =>
  Path.basename(process.execPath) === "bun" ? process.execPath : (Bun.which("bun") ?? undefined);

export const installOpenTunnel = Effect.fn("Tunnel.install")(function* (quiet: boolean) {
  const bun = bunExecutable();
  if (!bun) return false;
  const output = quiet ? "ignore" : "inherit";
  const exitCode = yield* Effect.promise(
    () => Bun.spawn([bun, "add", "-g", "opentunnel"], { stdout: output, stderr: output }).exited,
  );
  return exitCode === 0 && findOpenTunnel() !== undefined;
});

const routeName = "porchlight";

export const parseCertificateExpiry = (output: string): Date | undefined => {
  const match = /Certificate expiry:\s*(\S+)/i.exec(output)?.[1];
  const date = match ? new Date(match) : undefined;
  return date && !Number.isNaN(date.getTime()) ? date : undefined;
};

export const daysUntil = (date: Date): number => Math.floor((date.getTime() - Date.now()) / 86_400_000);

export const parsePublicUrl = (output: string): string | undefined => {
  const host = /(?:https:\/\/)?([a-z0-9-]+\.opentunnel\.xyz)/i.exec(output)?.[1];
  return host ? `https://${host}` : undefined;
};

const opentunnel = Effect.fn("Tunnel.opentunnel")(function* (args: ReadonlyArray<string>) {
  const bin = findOpenTunnel();
  if (bin === undefined) {
    return yield* new TunnelError({ message: "OpenTunnel isn't installed. Run `bun add -g opentunnel`." });
  }
  const result = yield* Effect.tryPromise({
    try: () => $`${bin} ${args}`.quiet().nothrow(),
    catch: (cause) =>
      new TunnelError({
        message: "OpenTunnel isn't installed. Run `bun add -g opentunnel`.",
        cause,
      }),
  });
  return { ok: result.exitCode === 0, output: `${result.stdout}${result.stderr}` };
});

const run = Effect.fn("Tunnel.run")(function* (...args: ReadonlyArray<string>) {
  const result = yield* opentunnel(args);
  if (!result.ok) return yield* new TunnelError({ message: `opentunnel ${args.join(" ")} failed: ${result.output}` });
  return result.output;
});

const ensureTunnel = Effect.fn("Tunnel.ensure")(function* () {
  const result = yield* opentunnel(["create"]);
  if (!result.ok && !/already has a tunnel/i.test(result.output)) {
    return yield* new TunnelError({ message: `opentunnel create failed: ${result.output}` });
  }
});

const ensureRoute = Effect.fn("Tunnel.ensureRoute")(function* (localPort: number) {
  const target = `127.0.0.1:${localPort}`;
  const routes = yield* run("route", "list");
  if (routes.includes(`${routeName}.`) && routes.includes(target)) return;
  if (routes.includes(`${routeName}.`)) yield* run("route", "remove", routeName);
  yield* run("route", "add", routeName, target);
});

export const info = () => run("info");

const restartDelay = "2 seconds";
const serviceCheckInterval = "30 seconds";
const tunnelLog = Path.join(logDir, "opentunnel.log");

const spawnServe = Effect.try({
  try: () => {
    mkdirSync(logDir, { recursive: true, mode: 0o700 });
    const log = openSync(tunnelLog, "a", 0o600);
    try {
      return Bun.spawn([findOpenTunnel() ?? "opentunnel", "serve"], { stdout: log, stderr: log });
    } finally {
      closeSync(log);
    }
  },
  catch: (cause) => new TunnelError({ message: "Couldn't start opentunnel serve", cause }),
});

const serviceRunning = opentunnel(["service", "status"]).pipe(
  Effect.map((result) => result.ok && /\bis running\b/i.test(result.output)),
  Effect.orElseSucceed(() => false),
);

const serveTunnel = Effect.acquireUseRelease(
  spawnServe,
  (child) => Effect.promise(() => child.exited),
  (child) => Effect.sync(() => child.kill()),
);

const keepServing = Effect.gen(function* () {
  if (yield* serviceRunning) return yield* Effect.sleep(serviceCheckInterval);
  const code = yield* serveTunnel;
  yield* Effect.logWarning(`opentunnel serve exited with code ${code}, restarting`);
  yield* Effect.sleep(restartDelay);
}).pipe(Effect.catch((error) => Effect.logWarning(error.message).pipe(Effect.andThen(Effect.sleep(restartDelay)))));

export const openTunnel = Effect.fn("Tunnel.open")(function* (localPort: number) {
  yield* ensureTunnel();
  yield* ensureRoute(localPort);
  const details = yield* info();
  const tunnelUrl = parsePublicUrl(details);
  if (!tunnelUrl) return yield* new TunnelError({ message: "opentunnel info did not print a hostname" });
  const expiry = parseCertificateExpiry(details);
  if (expiry && daysUntil(expiry) < 14) {
    yield* Console.error(
      `! OpenTunnel's certificate expires in ${Math.max(daysUntil(expiry), 0)} days, ` +
        "and OpenTunnel doesn't renew it yet. " +
        "Use your own tunnel for an address that has to last: `porchlight tunnel <https-url>`.",
    );
  }
  yield* keepServing.pipe(Effect.forever, Effect.forkScoped);
  return tunnelUrl.replace("https://", `https://${routeName}.`);
});
