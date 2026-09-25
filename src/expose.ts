import { Console, Effect, Option } from "effect";
import { Prompt } from "effect/unstable/cli";
import { isHttpsUrl, loadSettings, saveSettings, type Settings } from "./config.js";
import { addToDaemon, daemonState, type ServerView } from "./control.js";
import { discover, type DiscoveredServer } from "./discovery.js";
import { ExitCode, ExitError } from "./errors.js";
import { addServer, serve } from "./serve.js";
import { resolveSpec, type Overrides } from "./servers.js";
import { Store } from "./store.js";
import { copyToClipboard, installService, keepAwake, serviceSupported } from "./system.js";
import { findOpenTunnel, installOpenTunnel } from "./tunnel.js";

export interface ExposeInput {
  targets: ReadonlyArray<string>;
  allow: Option.Option<string>;
  deny: Option.Option<string>;
  readOnly: boolean;
  json: boolean;
}

interface Requested {
  key: string;
  overrides: Overrides;
}

const noServerHelp = "Point porchlight at one: `porchlight http://127.0.0.1:8080/mcp`";

const dim = (text: string): string => (process.stdout.isTTY ? `\x1b[2m${text}\x1b[22m` : text);

const interactive = (input: ExposeInput): boolean => !input.json && process.stdin.isTTY;

export const splitList = (value: string): ReadonlyArray<string> =>
  value
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);

const overridesFrom = (input: ExposeInput): Overrides => ({
  ...Option.match(input.allow, { onNone: () => ({}), onSome: (value) => ({ allow: splitList(value) }) }),
  ...Option.match(input.deny, { onNone: () => ({}), onSome: (value) => ({ deny: splitList(value) }) }),
  ...(input.readOnly ? { readOnly: true } : {}),
});

const exposable = (server: DiscoveredServer): boolean => server.local && (server.kind !== "http" || server.running);

const kindNote = { http: "running", stdio: "starts when a client connects", cli: "runs commands when a client asks" };

const choiceTitle = (server: DiscoveredServer): string =>
  `${server.displayName}  ${dim(`${server.source} · ${kindNote[server.kind]}`)}`;

const pickDiscovered = Effect.fn("Expose.pickDiscovered")(function* (settings: Settings, input: ExposeInput) {
  const found = (yield* discover(settings.servers)).filter(exposable);
  const running = found.filter((server) => server.kind === "http");
  if (!interactive(input)) return running;
  if (found.length === 0) {
    return yield* new ExitError({
      code: ExitCode.noServer,
      message: "No local MCP server found",
      nextAction: noServerHelp,
    });
  }
  const [only] = found;
  if (found.length === 1 && only && only.kind === "http") return [only];
  const choice = yield* Prompt.run(
    Prompt.select({
      message: "Which app should porchlight share?",
      choices: found.map((server) => ({ title: choiceTitle(server), value: server })),
    }),
  ).pipe(Effect.mapError(() => new ExitError({ code: ExitCode.internal, message: "Cancelled" })));
  return [choice];
});

const requestedServers = Effect.fn("Expose.requested")(function* (input: ExposeInput, settings: Settings) {
  const overrides = overridesFrom(input);
  if (input.targets.length > 0) return input.targets.map((key): Requested => ({ key, overrides }));
  if (!interactive(input)) {
    const saved = yield* (yield* Store).exposed.get();
    if (saved.length > 0) return saved.map((item): Requested => ({ key: item.key, overrides: item.overrides }));
  }
  const picked = yield* pickDiscovered(settings, input);
  if (picked.length === 0) {
    return yield* new ExitError({
      code: ExitCode.noServer,
      message: "No running MCP server found",
      nextAction: noServerHelp,
    });
  }
  return picked.map((server): Requested => ({ key: server.name, overrides }));
});

const askTunnel = Effect.fn("Expose.askTunnel")(function* (gatewayPort: number) {
  const choice = yield* Prompt.run(
    Prompt.select({
      message: "How should MCP clients reach this computer?",
      choices: [
        {
          title:
            findOpenTunnel() === undefined
              ? "OpenTunnel (free relay, can't read your traffic · porchlight installs it)"
              : "OpenTunnel (installed · free relay, can't read your traffic)",
          value: "opentunnel",
        },
        { title: "My own tunnel (enter its HTTPS URL)", value: "custom" },
      ],
    }),
  );
  if (choice === "opentunnel") return choice;
  const url = yield* Prompt.run(
    Prompt.text({
      message: "Your tunnel's public HTTPS URL",
      validate: (value) => {
        const trimmed = value.trim().replace(/\/$/, "");
        return isHttpsUrl(trimmed) ? Effect.succeed(trimmed) : Effect.fail("Enter an https:// URL");
      },
    }),
  );
  yield* Console.log(dim(`  Point it at http://127.0.0.1:${gatewayPort}`));
  return url;
});

const resolveTunnel = Effect.fn("Expose.resolveTunnel")(function* (
  input: ExposeInput,
  settings: Settings,
  gatewayPort: number,
) {
  if (settings.tunnel) {
    return { publicUrl: settings.tunnel === "opentunnel" ? undefined : settings.tunnel, asked: false };
  }
  if (!interactive(input)) return { publicUrl: undefined, asked: false };
  const choice = yield* askTunnel(gatewayPort).pipe(
    Effect.mapError(() => new ExitError({ code: ExitCode.internal, message: "Cancelled" })),
  );
  yield* saveSettings({ ...(yield* loadSettings()), tunnel: choice });
  return { publicUrl: choice === "opentunnel" ? undefined : choice, asked: true };
});

const ensureOpenTunnel = Effect.fn("Expose.ensureOpenTunnel")(function* (
  input: ExposeInput,
  justChosen: boolean,
  savedChoice: boolean,
) {
  if (findOpenTunnel() !== undefined) return true;
  if (!interactive(input)) return savedChoice ? yield* installOpenTunnel(true) : false;
  const install =
    justChosen ||
    (yield* Prompt.run(
      Prompt.confirm({ message: "OpenTunnel isn't installed. Install it now? (bun add -g opentunnel)", initial: true }),
    ).pipe(Effect.orElseSucceed(() => false)));
  if (!install) return false;
  yield* Console.log(dim("Installing OpenTunnel (bun add -g opentunnel)…"));
  return yield* installOpenTunnel(false);
});

const listenBriefly = (port: number) =>
  Effect.try({
    try: () => Bun.serve({ hostname: "127.0.0.1", port, fetch: () => new Response() }),
    catch: () => "in use",
  }).pipe(
    Effect.flatMap((server) => {
      const bound = server.port ?? 0;
      return Effect.promise(() => server.stop(true)).pipe(Effect.as(bound));
    }),
  );

const portIsFree = (port: number) =>
  listenBriefly(port).pipe(
    Effect.as(true),
    Effect.orElseSucceed(() => false),
  );

const freePort = () => listenBriefly(0).pipe(Effect.orElseSucceed(() => 0));

const choosePorts = Effect.fn("Expose.choosePorts")(function* (
  input: ExposeInput,
  settings: Settings,
  ownTunnel: boolean,
) {
  const store = yield* Store;
  const stored = yield* store.ports();
  const fixed = settings.port;
  const ports = { gatewayPort: fixed ?? stored.gatewayPort, approvalPort: stored.approvalPort };
  if (!(yield* portIsFree(ports.gatewayPort))) {
    if (fixed !== undefined || ownTunnel) {
      return yield* new ExitError({
        code: ExitCode.internal,
        message: `Port ${ports.gatewayPort} is in use by another app`,
        nextAction: ownTunnel
          ? 'Free that port, or set another "port" in porchlight.json and point your tunnel at it.'
          : 'Free that port, or set another "port" in porchlight.json.',
      });
    }
    ports.gatewayPort = yield* freePort();
    yield* store.setPort("gatewayPort", ports.gatewayPort);
  }
  if (!(yield* portIsFree(ports.approvalPort))) {
    ports.approvalPort = yield* freePort();
    yield* store.setPort("approvalPort", ports.approvalPort);
  }
  return ports;
});

const describeView = (server: ServerView): Array<string> => {
  if (server.state === "live") {
    return [
      `✓ ${server.name} is live at ${server.url}`,
      dim(`  sharing ${server.tools} ${server.tools === 1 ? "tool" : "tools"} (--read-only, --allow, --deny to limit)`),
    ];
  }
  if (server.state === "refused") return [`✗ ${server.name} ${server.detail}`];
  return [`… ${server.name}: ${server.detail}. It goes live on its own at ${server.url}`];
};

const printJson = (servers: ReadonlyArray<ServerView>, running: boolean) =>
  Console.log(
    JSON.stringify({
      ok: servers.some((server) => server.state !== "refused"),
      servers: servers.map((server) => ({
        name: server.slug,
        url: server.url,
        state: server.state,
        detail: server.detail,
        tools: server.tools,
      })),
      status: running ? "already_running" : "awaiting_owner_approval",
      next_action: [
        "Give the URL to the human.",
        "They add it as a remote MCP server in their MCP client and approve the connection on this computer.",
        "You cannot approve on their behalf.",
      ].join(" "),
    }),
  );

const announce = Effect.fn("Expose.announce")(function* (servers: ReadonlyArray<ServerView>, tunnel: string) {
  const first = servers.find((server) => server.state !== "refused");
  const copied = first ? yield* copyToClipboard(first.url) : false;
  for (const server of servers) yield* Console.log(describeView(server).join("\n"));
  if (copied && first) yield* Console.log(dim(`  copied ${first.url} to the clipboard`));
  yield* Console.log(dim(`  tunnel: ${tunnel} (change with porchlight tunnel)`));
  yield* Console.log("");
  yield* Console.log("  Add the URL as a remote MCP server in your MCP client, then approve the connection.");
  yield* Console.log("");
});

const keepRunning = Effect.fn("Expose.keepRunning")(function* (input: ExposeInput) {
  if (!process.stdin.isTTY || !serviceSupported) {
    yield* Console.log(dim("Running in the foreground. Stop it with Ctrl-C or `kill`."));
    return yield* Effect.never;
  }
  const background = yield* Prompt.run(
    Prompt.confirm({ message: "Keep it on in the background, even after restart?", initial: true }),
  ).pipe(Effect.orElseSucceed(() => false));
  if (!background) {
    yield* Console.log(dim("Running in the foreground. Press Ctrl-C to stop."));
    return yield* Effect.never;
  }
  yield* installService(["--json"]);
  yield* Console.log("✓ Installed. porchlight now starts at login.");
});

const addToRunning = Effect.fn("Expose.addToRunning")(function* (
  input: ExposeInput,
  requested: ReadonlyArray<Requested>,
) {
  const views: Array<ServerView> = [];
  for (const { key, overrides } of requested) {
    const result = yield* addToDaemon(key, overrides);
    if (Option.isNone(result)) {
      return yield* new ExitError({
        code: ExitCode.internal,
        message: "porchlight is running but didn't answer",
        nextAction: "Run `porchlight status`, or restart it.",
      });
    }
    if (!result.value.ok) {
      return yield* new ExitError({
        code: ExitCode.internal,
        message: result.value.error,
        ...(result.value.nextAction ? { nextAction: result.value.nextAction } : {}),
      });
    }
    views.push(result.value.server);
  }
  if (input.json) return yield* printJson(views, true);
  yield* Console.log(dim("porchlight is already running, added to it:"));
  for (const view of views) yield* Console.log(describeView(view).join("\n"));
});

export const expose = Effect.fn("Expose.run")(function* (input: ExposeInput) {
  const settings = yield* loadSettings();
  const requested = yield* requestedServers(input, settings);
  if (Option.isSome(yield* daemonState())) return yield* addToRunning(input, requested);

  const discovered = yield* discover(settings.servers);
  const specs = yield* Effect.forEach(requested, ({ key, overrides }) =>
    resolveSpec(key, overrides, settings, discovered),
  );
  if (!input.json) {
    for (const spec of specs) {
      const kind = spec.source.kind === "http" ? "" : ` (${kindNote[spec.source.kind]})`;
      yield* Console.log(`✓ Found ${spec.name}${kind}`);
    }
    if (input.targets.length === 0 && specs.length === 1 && interactive(input)) {
      yield* Console.log(dim("  share another with `porchlight apps add`"));
    }
  }

  const store = yield* Store;
  const planned = settings.port ?? (yield* store.ports()).gatewayPort;
  const { publicUrl, asked } = yield* resolveTunnel(input, settings, planned);
  if (publicUrl === undefined && !(yield* ensureOpenTunnel(input, asked, settings.tunnel === "opentunnel"))) {
    return yield* new ExitError({
      code: ExitCode.tunnelFailed,
      message: "OpenTunnel isn't installed",
      nextAction: "Run `bun add -g opentunnel`, or use your own tunnel with `porchlight tunnel <https-url>`.",
    });
  }
  const { gatewayPort, approvalPort } = yield* choosePorts(input, settings, publicUrl !== undefined);
  if (settings.keepAwake === true) yield* keepAwake();

  yield* Effect.scoped(
    Effect.gen(function* () {
      const daemon = yield* serve({ gatewayPort, approvalPort, publicUrl });
      const views = yield* Effect.forEach(requested, ({ key, overrides }) =>
        addServer(daemon.registry, daemon.publicUrl, key, overrides),
      );
      for (const view of views.filter((server) => server.state === "refused")) {
        yield* daemon.registry.remove(view.slug);
      }
      if (views.every((view) => view.state === "refused")) {
        return yield* new ExitError({
          code: ExitCode.internal,
          message: views.map((view) => `${view.name} ${view.detail}`).join("; "),
        });
      }
      if (input.json) {
        yield* printJson(views, false);
        return yield* Effect.never;
      }
      yield* announce(views, publicUrl === undefined ? "OpenTunnel" : "your own");
      yield* keepRunning(input);
    }),
  );
});
