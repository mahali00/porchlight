import { $ } from "bun";
import { Effect } from "effect";
import * as Fs from "node:fs/promises";
import * as Os from "node:os";
import * as Path from "node:path";
import { logDir } from "./config.js";
import { ShellError } from "./errors.js";

const isMac = process.platform === "darwin";
const isLinux = process.platform === "linux";

export const serviceSupported = isMac || isLinux;

export const isAlive = (pid: number): boolean => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

const failure = (message: string, cause?: unknown) =>
  new ShellError({ message: cause instanceof Error ? `${message}: ${cause.message}` : message, cause });

const shell = (message: string, command: () => $.ShellPromise) =>
  Effect.tryPromise({ try: () => command().quiet(), catch: (cause) => failure(message, cause) }).pipe(Effect.asVoid);

const output = (command: () => $.ShellPromise) => Effect.promise(() => command().quiet().nothrow());

const writeFile = (path: string, content: string) =>
  Effect.tryPromise({
    try: async () => {
      await Fs.mkdir(Path.dirname(path), { recursive: true });
      await Bun.write(path, content);
    },
    catch: (cause) => failure(`Couldn't write ${path}`, cause),
  });

const removeFile = (path: string) =>
  Effect.tryPromise({
    try: () => Fs.rm(path, { force: true }),
    catch: (cause) => failure(`Couldn't remove ${path}`, cause),
  });

interface ServiceManager {
  path: string;
  install: (command: ReadonlyArray<string>) => Effect.Effect<void, ShellError>;
  uninstall: Effect.Effect<void, ShellError>;
  pid: Effect.Effect<number | undefined>;
}

const positive = (value: number): number | undefined => (value > 0 ? value : undefined);

const outLog = Path.join(logDir, "service.out.log");
const errLog = Path.join(logDir, "service.err.log");

const selfCommand = (): Array<string> =>
  Path.basename(process.execPath) === "bun" ? [process.execPath, Bun.main] : [process.execPath];

const launchd = (() => {
  const label = "dev.porchlight.agent";
  const path = Path.join(Os.homedir(), "Library", "LaunchAgents", `${label}.plist`);
  const escape = (value: string): string => value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const plist = (command: ReadonlyArray<string>): string => `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${label}</string>
  <key>ProgramArguments</key>
  <array>
${command.map((arg) => `    <string>${escape(arg)}</string>`).join("\n")}
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>${escape(process.env["PATH"] ?? "/usr/bin:/bin")}</string>
  </dict>
  <key>StandardOutPath</key><string>${escape(outLog)}</string>
  <key>StandardErrorPath</key><string>${escape(errLog)}</string>
</dict>
</plist>
`;
  return {
    path,
    install: (command) =>
      writeFile(path, plist(command)).pipe(
        Effect.andThen(output(() => $`launchctl unload ${path}`)),
        Effect.andThen(shell("launchctl couldn't load the agent", () => $`launchctl load ${path}`)),
      ),
    uninstall: output(() => $`launchctl unload ${path}`).pipe(Effect.andThen(removeFile(path))),
    pid: output(() => $`launchctl list ${label}`).pipe(
      Effect.map((listing) => positive(Number(/"PID" = (\d+)/.exec(listing.stdout.toString())?.[1]))),
    ),
  } satisfies ServiceManager;
})();

const systemd = (() => {
  const unit = "porchlight.service";
  const configHome = process.env["XDG_CONFIG_HOME"] ?? Path.join(Os.homedir(), ".config");
  const path = Path.join(configHome, "systemd", "user", unit);
  const quote = (arg: string): string =>
    `"${arg.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/%/g, "%%").replace(/\$/g, "$$$$")}"`;
  const service = (command: ReadonlyArray<string>): string => `[Unit]
Description=porchlight
After=network-online.target

[Service]
ExecStart=${command.map(quote).join(" ")}
Restart=always
RestartSec=5
Environment=${quote(`PATH=${process.env["PATH"] ?? "/usr/bin:/bin"}`)}
StandardOutput=append:${outLog}
StandardError=append:${errLog}

[Install]
WantedBy=default.target
`;
  const install = Effect.fn("System.systemdInstall")(function* (command: ReadonlyArray<string>) {
    const environment = yield* output(() => $`systemctl --user show-environment`);
    if (environment.exitCode !== 0) {
      return yield* failure("systemd user services aren't available here. Run porchlight directly instead.");
    }
    yield* writeFile(path, service(command));
    yield* shell("systemctl couldn't reload units", () => $`systemctl --user daemon-reload`);
    yield* shell("systemctl couldn't enable the service", () => $`systemctl --user enable --now ${unit}`);
    yield* shell("systemctl couldn't restart the service", () => $`systemctl --user restart ${unit}`);
  });
  return {
    path,
    install,
    uninstall: output(() => $`systemctl --user disable --now ${unit}`).pipe(
      Effect.andThen(removeFile(path)),
      Effect.andThen(output(() => $`systemctl --user daemon-reload`)),
      Effect.asVoid,
    ),
    pid: output(() => $`systemctl --user show --property MainPID --value ${unit}`).pipe(
      Effect.map((shown) => positive(Number(shown.stdout.toString().trim()))),
    ),
  } satisfies ServiceManager;
})();

const manager: ServiceManager | undefined = isMac ? launchd : isLinux ? systemd : undefined;

const unsupported = () =>
  new ShellError({ message: `A background service isn't supported on ${process.platform}. Run porchlight directly.` });

export const installService = Effect.fn("System.installService")(function* (args: ReadonlyArray<string>) {
  if (!manager) return yield* unsupported();
  yield* Effect.promise(() => Fs.mkdir(logDir, { recursive: true }));
  yield* manager
    .install([...selfCommand(), ...args])
    .pipe(Effect.mapError((error) => failure("Failed to install the background service", error)));
});

export const uninstallService = Effect.fn("System.uninstallService")(function* () {
  if (!manager) return;
  yield* manager.uninstall.pipe(Effect.mapError((error) => failure("Failed to remove the background service", error)));
});

export const serviceStatus = Effect.fn("System.serviceStatus")(function* () {
  if (!manager) return { installed: false, running: false, pid: undefined };
  const installed = yield* Effect.promise(() => Bun.file(manager.path).exists());
  const pid = yield* manager.pid;
  return { installed, running: pid !== undefined, pid };
});

export const notify = (title: string, message: string) => {
  if (isMac) {
    const script = `display notification ${JSON.stringify(message)} with title ${JSON.stringify(title)}`;
    return shell("Failed to show notification", () => $`osascript -e ${script}`).pipe(Effect.ignore);
  }
  if (isLinux) {
    return shell("Failed to show notification", () => $`notify-send ${title} ${message}`).pipe(Effect.ignore);
  }
  return Effect.void;
};

export const copyToClipboard = (text: string) =>
  isMac
    ? shell("Failed to copy to clipboard", () => $`pbcopy < ${new Response(text)}`).pipe(
        Effect.as(true),
        Effect.orElseSucceed(() => false),
      )
    : Effect.succeed(false);

export const keepAwake = () =>
  isMac
    ? Effect.sync(() => {
        Bun.spawn(["caffeinate", "-s", "-w", String(process.pid)], { stdout: "ignore", stderr: "ignore" });
      })
    : Effect.void;
