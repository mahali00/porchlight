#!/usr/bin/env bun

import { NodeRuntime, NodeServices } from "@effect/platform-node";
import { Console, Effect, Layer, Option, Schema, Stream } from "effect";
import { Argument, Command, Flag, Prompt } from "effect/unstable/cli";
import * as Os from "node:os";
import * as Path from "node:path";
import packageJson from "../package.json" with { type: "json" };
import { Auth } from "./auth.js";
import {
  configPath,
  isHttpsUrl,
  loadSettings,
  saveSettings,
  type CliInput,
  type CliTool,
  type ServerSettings,
} from "./config.js";
import { parseDuration } from "./crypto.js";
import { discover, isLocalUrl, knownServers, probe } from "./discovery.js";
import { ExitCode, ExitError } from "./errors.js";
import { expose, splitList } from "./expose.js";
import { daemonState, removeFromDaemon, type ServerView } from "./control.js";
import { serviceStatus, uninstallService } from "./system.js";
import { placeholdersOf, runsClientCode } from "./policy.js";
import { Store, type AuditEntry } from "./store.js";
import { daysUntil, findOpenTunnel, info as tunnelInfo, parseCertificateExpiry, parsePublicUrl } from "./tunnel.js";
import { parseHeaders } from "./upstream/http.js";

const args = process.argv.slice(2);
const jsonOutput = args.includes("--json");
const interactive = !jsonOutput && process.stdin.isTTY;

const formatDate = (timestamp: number | null): string =>
  timestamp === null ? "never" : new Date(timestamp).toLocaleString();

const isHttpUrl = (value: string): boolean => URL.canParse(value) && /^https?:/.test(value);

const root = Command.make(
  "porchlight",
  {
    apps: Argument.string("app").pipe(
      Argument.variadic(),
      Argument.withDescription("App to share: a name like paper, a saved app, or a URL (default: asks)"),
    ),
    allow: Flag.string("allow").pipe(Flag.optional, Flag.withDescription("Only share these tools, comma-separated")),
    deny: Flag.string("deny").pipe(Flag.optional, Flag.withDescription("Hide these tools, comma-separated")),
    readOnly: Flag.boolean("read-only").pipe(Flag.withDescription("Only share tools marked read-only")),
  },
  ({ apps, ...input }) => expose({ ...input, targets: apps, json: jsonOutput }),
).pipe(
  Command.withDescription("Share apps on this computer with remote MCP clients. You approve every client."),
  Command.withExamples([
    { command: "porchlight", description: "Pick an app and share it" },
    { command: "porchlight paper --read-only", description: "Share Paper with read-only tools" },
    { command: "porchlight apps add", description: "Add an app porchlight doesn't find on its own" },
    { command: "porchlight logs --follow", description: "Watch what clients do" },
    { command: "porchlight clients revoke", description: "Take access away from a client" },
    { command: "porchlight stop", description: "Stop sharing everything" },
  ]),
  Command.withSharedFlags({
    json: Flag.boolean("json").pipe(Flag.withDescription("Print one JSON object and nothing else")),
  }),
);

const output = Effect.fn("Cli.output")(function* (value: Record<string, unknown>, lines: ReadonlyArray<string>) {
  const { json } = yield* root;
  yield* Console.log(json ? JSON.stringify({ ok: true, ...value }) : lines.join("\n"));
});

const cancelled = () => new ExitError({ code: ExitCode.internal, message: "Cancelled" });

const ask = <A>(prompt: Prompt.Prompt<A>) => Prompt.run(prompt).pipe(Effect.mapError(cancelled));

const askOrFail = <A, E, R>(missing: string, usage: string, question: Effect.Effect<A, E, R>) =>
  interactive
    ? question
    : Effect.fail(new ExitError({ code: ExitCode.internal, message: `Missing ${missing}`, nextAction: usage }));

const askText = (message: string, problem: (value: string) => string | undefined) =>
  ask(
    Prompt.text({
      message,
      validate: (value) => {
        const trimmed = value.trim();
        const found = problem(trimmed);
        return found === undefined ? Effect.succeed(trimmed) : Effect.fail(found);
      },
    }),
  );

const lastSeen = (timestamp: number | null, serving: boolean): string => {
  if (timestamp === null) return "approved, not used yet";
  const minutes = Math.round((Date.now() - timestamp) / 60_000);
  if (minutes < 2) return serving ? "active now" : "last used just now";
  if (minutes < 60) return `last used ${minutes} min ago`;
  if (minutes < 60 * 24) return `last used ${Math.round(minutes / 60)} h ago`;
  return `last used ${Math.round(minutes / (60 * 24))} days ago`;
};

const stateText = (server: ServerView): string =>
  server.state === "live" ? `(${server.tools} ${server.tools === 1 ? "tool" : "tools"})` : `(${server.detail})`;

const approvedClients = Effect.fn("Cli.approvedClients")(function* () {
  const store = yield* Store;
  return yield* Effect.forEach(yield* store.grants.list(), (grant) =>
    store.clients
      .label(grant.clientId)
      .pipe(Effect.map((name) => ({ id: grant.id, client: name, app: grant.server, lastUsed: grant.lastUsed }))),
  );
});

type ApprovedClient = Effect.Success<ReturnType<typeof approvedClients>>[number];

const clientLine = (client: ApprovedClient, serving: boolean): string =>
  `${client.client.padEnd(20)} ${lastSeen(client.lastUsed, serving).padEnd(24)} id ${client.id}`;

const status = Command.make(
  "status",
  {},
  Effect.fn(function* () {
    const live = Option.getOrUndefined(yield* daemonState());
    const service = yield* serviceStatus();
    const clients = yield* approvedClients();
    const apps = (live?.servers ?? []).map((server) => ({
      ...server,
      clients: clients.filter((client) => client.app === server.slug),
    }));
    const shared = new Set(apps.map((server) => server.slug));
    const others = clients.filter((client) => !shared.has(client.app));
    const serviceText = service.running ? "running" : service.installed ? "installed, not running" : "not installed";
    const clientLines = (list: ReadonlyArray<ApprovedClient>, serving: boolean) =>
      list.length === 0 ? ["    no approved clients yet"] : list.map((client) => `    ${clientLine(client, serving)}`);
    yield* output(
      {
        running: live !== undefined,
        pid: live?.pid,
        tunnel: live?.tunnel,
        publicUrl: live?.publicUrl,
        service: serviceText,
        apps,
        otherClients: others,
      },
      [
        live
          ? `● porchlight is running (pid ${live.pid}, since ${new Date(live.startedAt).toLocaleTimeString()})`
          : service.running
            ? "○ The background service is starting, or can't start. See ~/.porchlight/logs/service.out.log"
            : "○ porchlight is not running. Start it with porchlight",
        ...(live ? [`  tunnel: ${live.tunnel}, ${live.publicUrl}`] : []),
        `  background service: ${serviceText}`,
        ...apps.flatMap((server) => [
          "",
          `  ${server.name}  ${server.url}  ${stateText(server)}`,
          ...clientLines(server.clients, true),
        ]),
        ...(others.length === 0
          ? []
          : ["", "  Approved for apps that aren't shared right now:", ...clientLines(others, false)]),
        "",
        `  settings: ${configPath}  ·  take access away with porchlight clients revoke`,
      ],
    );
  }),
).pipe(Command.withDescription("Show what's shared and which clients are connected"));

const approve = Command.make(
  "approve",
  {},
  Effect.fn(function* () {
    const { json } = yield* root;
    if (json || !process.stdin.isTTY) {
      return yield* new ExitError({
        code: ExitCode.needsHuman,
        message: "porchlight approve only shows codes in a terminal",
        nextAction: "Run it in a terminal on this computer. Agents and scripts can't approve access.",
      });
    }
    const store = yield* Store;
    const auth = yield* Auth;
    const pending = (yield* store.pending.list()).filter((request) => request.expiresAt > Date.now());
    if (pending.length === 0) return yield* Console.log("No pending requests.");
    const choices = yield* Effect.forEach(pending, (request) =>
      store.clients.label(request.clientId).pipe(
        Effect.map((client) => ({
          title: `${client} → ${request.server}  (${formatDate(request.createdAt)})`,
          value: request,
        })),
      ),
    );
    const request = yield* Prompt.run(Prompt.select({ message: "Which request is yours?", choices }));
    const code = yield* auth.issueDeviceCode(request.id);
    yield* Console.log(`Type this code on your other device: ${code.slice(0, 4)}-${code.slice(4)}`);
    yield* Console.log("It expires in 5 minutes. Never share it. porchlight will never ask you for it.");
  }),
).pipe(Command.withDescription("Show a code to approve a request from another device"));

const serverNames = Effect.fn("Cli.serverNames")(function* () {
  const names = new Map(knownServers.map((server) => [server.id, server.displayName]));
  for (const server of Option.getOrUndefined(yield* daemonState())?.servers ?? []) names.set(server.slug, server.name);
  return names;
});

const revokeOne = Effect.fn("Cli.revokeOne")(function* (id: string) {
  const auth = yield* Auth;
  if (!(yield* auth.revokeGrant(id))) {
    return yield* new ExitError({
      code: ExitCode.internal,
      message: `No client or token with id ${id}`,
      nextAction: "Run `porchlight clients` to see client ids.",
    });
  }
  yield* output({ id }, [`Revoked ${id}.`]);
});

const tokenCreate = Command.make(
  "create",
  {
    server: Argument.string("server").pipe(Argument.withDescription("App name, like paper")),
    name: Flag.string("name").pipe(Flag.withDefault("automation"), Flag.withDescription("Token label")),
    expires: Flag.string("expires").pipe(Flag.withDefault("30d"), Flag.withDescription("Lifetime, like 30d or 12h")),
  },
  Effect.fn(function* ({ server, name, expires }) {
    const { json } = yield* root;
    if (json || !process.stdin.isTTY) {
      return yield* new ExitError({
        code: ExitCode.needsHuman,
        message: "Tokens can only be created in a terminal",
        nextAction: "Run it in a terminal on this computer. Agents and scripts can't create access for themselves.",
      });
    }
    const ttl = parseDuration(expires);
    if (ttl === undefined) {
      return yield* new ExitError({
        code: ExitCode.internal,
        message: `Invalid --expires "${expires}". Use 30d, 12h or 60m.`,
      });
    }
    const auth = yield* Auth;
    const { id, token } = yield* auth.createStaticToken(server, name, ttl);
    yield* output({ id, token, server, name }, [`Token ${id} for ${server}. Keep it secret:`, token]);
  }),
).pipe(Command.withDescription("Create a static bearer token for automations"));

const tokenList = Command.make(
  "list",
  {},
  Effect.fn(function* () {
    const store = yield* Store;
    const tokens = yield* store.tokens.listStatic();
    yield* output(
      { tokens },
      tokens.length === 0
        ? ["No static tokens."]
        : tokens.map((token) => {
            const expires = formatDate(token.expiresAt);
            return `${token.id}  ${token.name} → ${token.server}  expires ${expires}`;
          }),
    );
  }),
).pipe(Command.withDescription("List static tokens"));

const tokenRevoke = Command.make(
  "revoke",
  { id: Argument.string("id").pipe(Argument.withDescription("Token id from porchlight token list")) },
  ({ id }) => revokeOne(id),
).pipe(Command.withDescription("Revoke a static token"));

const token = Command.make("token").pipe(
  Command.withDescription("For automations: create, list and revoke static tokens"),
  Command.withSubcommands([tokenCreate, tokenList, tokenRevoke]),
);

const describeEvent = (
  event: string,
  detail: string,
  client: string,
  server: string,
  name: string,
  tool: string,
): string => {
  switch (event) {
    case "tool.called":
      return `${client} used ${tool} on ${server}`;
    case "tool.blocked":
      return `Blocked ${client} from ${tool} on ${server}`;
    case "method.blocked":
      return `Refused ${tool} from ${client} on ${server}, porchlight doesn't pass that request on`;
    case "dcr.register":
      return `${client} registered`;
    case "authz.created":
      return `${client} asked to use ${server}`;
    case "authz.ticket_issued":
      return "Approved on this computer";
    case "authz.code_issued":
      return "Showed a code to approve from another device";
    case "authz.completed":
      return `${client} was approved for ${server}`;
    case "authz.denied":
      return "A request was denied";
    case "authz.complete_failed":
      return "An approval link was used twice or had expired";
    case "grant.revoked":
      return `Access ${detail} was revoked`;
    case "grant.revoked_all":
      return "All access was revoked";
    case "token.revoked":
      return "A client signed out";
    case "token.reuse_detected":
      return "Blocked a reused sign-in token and revoked its access";
    case "token.static_created":
      return `Created token "${name}" for ${server}`;
    default:
      return `${event} ${detail}`;
  }
};

const describeEntries = Effect.fn("Cli.describeEntries")(function* (entries: ReadonlyArray<AuditEntry>) {
  const store = yield* Store;
  const names = yield* serverNames();
  const ids = new Set(
    entries.flatMap((entry) =>
      entry.event === "dcr.register" ? [entry.detail] : [entry.detail.split(" -> ")[0] ?? ""],
    ),
  );
  const labels = new Map(
    yield* Effect.forEach(ids, (id) =>
      store.clients.label(id).pipe(Effect.map((label): [string, string] => [id, label])),
    ),
  );
  const when = (at: number) => new Date(at).toLocaleString(undefined, { dateStyle: "short", timeStyle: "medium" });
  return entries.map((entry) => {
    const [who = "", rest = ""] = entry.detail.split(" -> ");
    const [server = "", tool = ""] = rest.split(" | ");
    const client = labels.get(entry.event === "dcr.register" ? entry.detail : who) ?? who;
    const serverName = names.get(server) ?? server;
    return `${when(entry.at)}  ${describeEvent(entry.event, entry.detail, client, serverName, who, tool)}`;
  });
});

const logs = Command.make(
  "logs",
  { follow: Flag.boolean("follow").pipe(Flag.withDescription("Keep printing new events as they happen")) },
  Effect.fn(function* ({ follow }) {
    const store = yield* Store;
    if (!follow) {
      const entries = yield* store.audit.list(100);
      const lines = yield* describeEntries(entries);
      return yield* output({ log: entries }, lines.length === 0 ? ["Nothing has happened yet."] : lines);
    }
    const recent = [...(yield* store.audit.list(20))].reverse();
    for (const line of yield* describeEntries(recent)) yield* Console.log(line);
    yield* Console.log("Watching for new events. Press Ctrl-C to stop.");
    let last = recent.at(-1)?.id ?? 0;
    while (true) {
      yield* Effect.sleep("1 second");
      const fresh = yield* store.audit.after(last);
      for (const line of yield* describeEntries(fresh)) yield* Console.log(line);
      last = fresh.at(-1)?.id ?? last;
    }
  }),
).pipe(Command.withDescription("Show what clients did, or watch it live with --follow"));

const doctor = Command.make(
  "doctor",
  {},
  Effect.fn(function* () {
    const ports = yield* (yield* Store).ports();
    const upstreams = yield* Effect.forEach(knownServers, (server) =>
      probe(server.url).pipe(
        Effect.map((running) => ({
          name: `upstream/${server.id}`,
          ok: running,
          detail: running ? `reachable at ${server.url}` : "not running",
        })),
      ),
    );
    const tunnel = yield* tunnelInfo().pipe(Effect.option);
    const tunnelUrl = Option.flatMap(tunnel, (output) => Option.fromUndefinedOr(parsePublicUrl(output)));
    const expiry = Option.flatMap(tunnel, (output) => Option.fromUndefinedOr(parseCertificateExpiry(output)));
    const service = yield* serviceStatus();
    const checks = [
      ...upstreams,
      {
        name: "tunnel",
        ok: Option.isSome(tunnelUrl),
        detail: Option.getOrElse(tunnelUrl, () => "opentunnel is not installed or has no tunnel"),
      },
      ...Option.match(expiry, {
        onNone: () => [],
        onSome: (date) => [
          {
            name: "certificate",
            ok: daysUntil(date) >= 14,
            detail: `OpenTunnel certificate expires in ${daysUntil(date)} days (${date.toLocaleDateString()})`,
          },
        ],
      }),
      {
        name: "service",
        ok: true,
        detail: service.running ? "running" : service.installed ? "installed, not running" : "not installed",
      },
    ];
    const healthy = checks.every((check) => check.ok);
    yield* output({ healthy, checks, ports }, [
      ...checks.map((check) => `${check.ok ? "✓" : "✗"} ${check.name}: ${check.detail}`),
      ...(healthy ? [] : ["", "Fix the ✗ rows above, then run porchlight doctor again."]),
    ]);
    if (!healthy) process.exitCode = ExitCode.internal;
  }),
).pipe(Command.withDescription("Check apps, tunnel and background service"));

const schemaUrl = "https://raw.githubusercontent.com/mahali00/porchlight/main/porchlight.schema.json";

const appsList = Effect.fn("Cli.appsList")(function* () {
  const found = yield* discover((yield* loadSettings()).servers);
  const shared = new Set((Option.getOrUndefined(yield* daemonState())?.servers ?? []).map((server) => server.key));
  const note = (server: (typeof found)[number]): string => {
    if (shared.has(server.name)) return " · shared";
    if (!server.local) return " · remote, clients can connect to it directly";
    if (server.kind === "stdio") return " · starts when a client connects";
    if (server.kind === "cli") return " · command-line tools";
    return server.running ? "" : " · not running";
  };
  yield* output(
    { apps: found.map((server) => ({ ...server, shared: shared.has(server.name) })) },
    found.length === 0
      ? ["No apps found. Add one with porchlight apps add"]
      : [
          ...found.map(
            (server) => `${server.running ? "●" : "○"} ${server.name.padEnd(16)} ${server.source}${note(server)}`,
          ),
          "",
          "Share one with porchlight <name>, or add another with porchlight apps add",
        ],
  );
});

const expandHome = (word: string): string =>
  word === "~" || word.startsWith("~/") ? Path.join(Os.homedir(), word.slice(1)) : word;

const commandWords = (command: string): ReadonlyArray<string> =>
  [...command.matchAll(/"([^"]*)"|'([^']*)'|(\S+)/g)].map((match) =>
    expandHome(match[1] ?? match[2] ?? match[3] ?? ""),
  );

const commandProblem = (run: ReadonlyArray<string>, allowDangerous = false): string | undefined => {
  const [program] = run;
  if (program === undefined) return "Enter a command";
  if (runsClientCode(run) && !allowDangerous)
    return `That hands client input to ${Path.basename(program)}, which can run anything`;
  if (Bun.which(program) === null) return `Can't find ${program}. Check the spelling, or use its full path`;
  return undefined;
};

const appNameProblem = (value: string): string | undefined =>
  /^[a-z0-9][a-z0-9-]{0,39}$/.test(value) ? undefined : "Use lowercase letters, numbers and -, like my-notes";

const urlProblem = (value: string): string | undefined => {
  if (!isHttpUrl(value)) return "Enter an http:// or https:// URL";
  if (!isLocalUrl(value)) return "That's a remote server, so clients can connect to it directly";
  return undefined;
};

const toolNamePattern = /^[\w.-]{1,64}$/;

const askChoices = (input: string) =>
  askText(`Allowed values for {${input}}, comma-separated (empty allows any)`, () => undefined).pipe(
    Effect.map((value): Array<[string, CliInput]> => {
      const choices = splitList(value);
      return choices.length === 0 ? [] : [[input, { choices }]];
    }),
  );

const askCliTool = (taken: Set<string>) =>
  Effect.gen(function* () {
    const name = yield* askText("Tool name, like get_status", (value) => {
      if (!toolNamePattern.test(value)) return "Use letters, numbers, _ . or -";
      return taken.has(value) ? "You already added that tool" : undefined;
    });
    taken.add(name);
    const command = yield* askText("Command, like garage status", (value) => commandProblem(commandWords(value)));
    const run = commandWords(command);
    const inputs = (yield* Effect.forEach(placeholdersOf(run), askChoices)).flat();
    const description = yield* askText("Description the client sees (optional)", () => undefined);
    const readOnly = yield* ask(Prompt.confirm({ message: "Read-only? It never changes anything", initial: false }));
    const tool: CliTool = {
      run,
      ...(description ? { description } : {}),
      ...(readOnly ? { readOnly } : {}),
      ...(inputs.length > 0 ? { inputs: Object.fromEntries(inputs) } : {}),
    };
    const another = yield* ask(Prompt.confirm({ message: "Add another tool?", initial: false }));
    return { name, tool, another };
  });

const askCliTools = (existing: Record<string, CliTool>) =>
  Effect.gen(function* () {
    yield* Console.log(
      "Each command becomes an MCP tool. Write {name} for an argument the client fills in, like lights on {room}",
    );
    const answers = yield* Stream.fromEffectRepeat(askCliTool(new Set(Object.keys(existing)))).pipe(
      Stream.takeUntil((answer) => !answer.another),
      Stream.runCollect,
    );
    return { tools: Object.fromEntries(answers.map((answer) => [answer.name, answer.tool])) } satisfies ServerSettings;
  });

const askSource = Effect.gen(function* () {
  const how = yield* ask(
    Prompt.select({
      message: "How does it run?",
      choices: [
        { title: "HTTP", value: "url", description: "Streamable HTTP MCP server, already running at a URL" },
        {
          title: "stdio",
          value: "command",
          description: "stdio MCP server, porchlight launches one process per session",
        },
        {
          title: "CLI",
          value: "cli",
          description: "Plain command-line program, porchlight wraps each command as an MCP tool",
        },
      ],
    }),
  );
  if (how === "cli") return yield* askCliTools({});
  if (how === "url") {
    const url = yield* askText("Server URL, like http://127.0.0.1:8080/mcp", urlProblem);
    return { url } satisfies ServerSettings;
  }
  const command = yield* askText(
    "Launch command, like npx -y @modelcontextprotocol/server-filesystem ~/Documents",
    (value) => commandProblem(commandWords(value)),
  );
  return { command: commandWords(command) } satisfies ServerSettings;
});

const askChange = (name: string, existing: ServerSettings) =>
  Effect.gen(function* () {
    const question = existing.tools ? `Add tools to ${name}?` : `${name} already exists. Change how it runs?`;
    const change = yield* ask(Prompt.confirm({ message: question, initial: true }));
    if (!change) return {} satisfies ServerSettings;
    return existing.tools ? yield* askCliTools(existing.tools) : yield* askSource;
  });

const ToolFlag = Schema.String.check(Schema.isPattern(/^[\w.-]{1,64}=\s*\S/));

const invalid = (message: string) => new ExitError({ code: ExitCode.internal, message });

const parseTools = Effect.fn("Cli.parseTools")(function* (pairs: ReadonlyArray<string>, allowDangerous: boolean) {
  const valid = yield* Schema.decodeUnknownEffect(Schema.Array(ToolFlag))(pairs).pipe(
    Effect.mapError(() => invalid('Use --tool name="command", like --tool status="garage status"')),
  );
  const tools = valid.map((pair): [string, CliTool] => {
    const separator = pair.indexOf("=");
    return [pair.slice(0, separator), { run: commandWords(pair.slice(separator + 1)) }];
  });
  const problem = tools.flatMap(([name, tool]) => {
    const found = commandProblem(tool.run, allowDangerous);
    return found === undefined ? [] : [`${name}: ${found}`];
  });
  if (problem.length > 0) return yield* invalid(problem.join("; "));
  return Object.fromEntries(tools);
});

const givenSource = Effect.fn("Cli.givenSource")(function* (
  url: string | undefined,
  command: Option.Option<string>,
  tools: ReadonlyArray<string>,
  allowDangerous: boolean,
) {
  if (url !== undefined) {
    const problem = urlProblem(url);
    if (problem !== undefined) return yield* invalid(`${url}: ${problem}`);
    return { url } satisfies ServerSettings;
  }
  if (Option.isSome(command)) {
    const run = commandWords(command.value);
    const problem = commandProblem(run);
    if (problem !== undefined) return yield* invalid(problem);
    return { command: run } satisfies ServerSettings;
  }
  if (tools.length > 0) return { tools: yield* parseTools(tools, allowDangerous) } satisfies ServerSettings;
  return undefined;
});

const withSource = (existing: ServerSettings | undefined, source: ServerSettings): ServerSettings => {
  if (source.url) return { ...existing, command: undefined, tools: undefined, url: source.url };
  if (source.command)
    return { ...existing, url: undefined, headers: undefined, tools: undefined, command: source.command };
  if (source.tools) {
    return {
      ...existing,
      url: undefined,
      headers: undefined,
      command: undefined,
      tools: { ...existing?.tools, ...source.tools },
    };
  }
  return { ...existing };
};

const addUsage =
  'porchlight apps add <name> <url>, --command "npx -y some-mcp-server", or --tool status="garage status"';

const appsAdd = Command.make(
  "add",
  {
    name: Argument.string("name").pipe(
      Argument.withDescription("App name, also its URL path /<name>/mcp"),
      Argument.optional,
    ),
    url: Argument.string("url").pipe(
      Argument.withDescription("Streamable HTTP URL, like http://127.0.0.1:8080/mcp"),
      Argument.optional,
    ),
    command: Flag.string("command").pipe(
      Flag.optional,
      Flag.withDescription('Launch command for a stdio MCP server, like "npx -y some-mcp-server"'),
    ),
    tool: Flag.string("tool").pipe(
      Flag.atLeast(0),
      Flag.withDescription('Share a CLI command as an MCP tool, like status="garage status" (repeatable)'),
    ),
    header: Flag.string("header").pipe(
      Flag.atLeast(0),
      Flag.withDescription('Header for an HTTP server, like "Authorization: Bearer ${TOKEN}" (repeatable)'),
    ),
    allow: Flag.string("allow").pipe(Flag.optional, Flag.withDescription("Only share these tools, comma-separated")),
    deny: Flag.string("deny").pipe(Flag.optional, Flag.withDescription("Hide these tools, comma-separated")),
    readOnly: Flag.boolean("read-only").pipe(Flag.withDescription("Only share tools marked read-only")),
    allowDangerous: Flag.boolean("allow-dangerous").pipe(
      Flag.withDescription("Share tools that can run shell commands or code"),
    ),
  },
  Effect.fn(function* (input) {
    if (Option.isSome(input.name) && appNameProblem(input.name.value) !== undefined) {
      return yield* invalid(`"${input.name.value}": ${appNameProblem(input.name.value)}`);
    }
    const given = yield* givenSource(Option.getOrUndefined(input.url), input.command, input.tool, input.allowDangerous);
    const name = Option.isSome(input.name)
      ? input.name.value
      : yield* askOrFail("a name", addUsage, askText("App name, also its URL path /<name>/mcp", appNameProblem));
    const settings = yield* loadSettings();
    const existing = settings.servers?.[name];
    const found = (yield* discover(settings.servers)).some((server) => server.name === name);
    const rulesGiven =
      input.header.length > 0 ||
      Option.isSome(input.allow) ||
      Option.isSome(input.deny) ||
      input.readOnly ||
      input.allowDangerous;
    const source: ServerSettings =
      given ??
      (existing && interactive && !rulesGiven
        ? yield* askChange(name, existing)
        : existing || found
          ? {}
          : yield* askOrFail("a URL, --command or --tool", addUsage, askSource));
    if (source.url && interactive && !(yield* probe(source.url))) {
      yield* Console.log(`Nothing answers at ${source.url} yet. porchlight will share it once it starts.`);
    }
    const entry: ServerSettings = {
      ...withSource(existing, source),
      ...(input.header.length === 0 ? {} : { headers: parseHeaders(input.header) }),
      ...Option.match(input.allow, { onNone: () => ({}), onSome: (value) => ({ allow: splitList(value) }) }),
      ...Option.match(input.deny, { onNone: () => ({}), onSome: (value) => ({ deny: splitList(value) }) }),
      ...(input.readOnly ? { readOnly: true } : {}),
      ...(input.allowDangerous ? { allowDangerous: true } : {}),
    };
    const changed = JSON.stringify(entry) !== JSON.stringify(existing ?? {});
    if (changed) {
      yield* saveSettings({
        $schema: settings.$schema ?? schemaUrl,
        ...settings,
        servers: { ...settings.servers, [name]: entry },
      });
    }
    const status = changed ? `Saved ${name}.` : `Nothing to change, porchlight already knows ${name}.`;
    if (!interactive) {
      return yield* output({ name, changed, path: configPath }, [`${status} Share it with porchlight ${name}`]);
    }
    yield* Console.log(`✓ ${status}`);
    const shared = Option.exists(yield* daemonState(), (state) => state.servers.some((server) => server.key === name));
    const question = shared ? (changed ? "Apply the changes now?" : undefined) : "Share it now?";
    if (question === undefined) return;
    const share = yield* ask(Prompt.confirm({ message: question, initial: true }));
    if (!share) return yield* Console.log(`Share it later with porchlight ${name}`);
    yield* expose({ targets: [name], allow: Option.none(), deny: Option.none(), readOnly: false, json: false });
  }),
).pipe(Command.withDescription("Add an HTTP, stdio or CLI app, or change one (asks for anything you leave out)"));

const stopSharing = Effect.fn("Cli.stopSharing")(function* (name: string) {
  const live = yield* removeFromDaemon(name);
  const store = yield* Store;
  const saved = yield* store.exposed.get();
  const kept = saved.filter((item) => item.key !== name);
  if (kept.length < saved.length) yield* store.exposed.set(kept);
  return Option.exists(live, (result) => result.removed) || kept.length < saved.length;
});

const appsRemove = Command.make(
  "remove",
  { name: Argument.string("name").pipe(Argument.withDescription("A saved app"), Argument.optional) },
  Effect.fn(function* (input) {
    const settings = yield* loadSettings();
    const saved = Object.keys(settings.servers ?? {});
    if (Option.isNone(input.name) && saved.length === 0) {
      return yield* new ExitError({ code: ExitCode.internal, message: "You haven't added any apps" });
    }
    const name = Option.isSome(input.name)
      ? input.name.value
      : yield* askOrFail(
          "a name",
          "porchlight apps remove <name>",
          ask(
            Prompt.select({
              message: "Which app?",
              choices: saved.map((value) => ({ title: value, value })),
            }),
          ),
        );
    const { [name]: removed, ...servers } = settings.servers ?? {};
    if (removed !== undefined) yield* saveSettings({ ...settings, servers });
    const stopped = yield* stopSharing(name);
    if (removed === undefined && !stopped) {
      return yield* new ExitError({
        code: ExitCode.internal,
        message: `No saved app named "${name}"`,
        nextAction: "Run `porchlight apps` to see your apps.",
      });
    }
    yield* output({ name, stopped }, [
      removed === undefined
        ? `Stopped sharing ${name}.`
        : `Removed ${name}${stopped ? " and stopped sharing it" : ""}.`,
    ]);
  }),
).pipe(Command.withDescription("Remove an app you added, and stop sharing it"));

const apps = Command.make("apps", {}, appsList).pipe(
  Command.withDescription("List apps you can share, or add and remove them"),
  Command.withSubcommands([appsAdd, appsRemove]),
);

const clientsList = Effect.fn("Cli.clientsList")(function* () {
  const clients = yield* approvedClients();
  const names = yield* serverNames();
  yield* output(
    { clients },
    clients.length === 0
      ? ["No approved clients yet."]
      : [
          ...clients.map(
            (client) => `${(names.get(client.app) ?? client.app).padEnd(12)} ${clientLine(client, false)}`,
          ),
          "",
          "Take access away with porchlight clients revoke",
        ],
  );
});

const clientsRevoke = Command.make(
  "revoke",
  {
    id: Argument.string("id").pipe(Argument.withDescription("Client id from porchlight clients"), Argument.optional),
    all: Flag.boolean("all").pipe(Flag.withDescription("Revoke every client and token")),
  },
  Effect.fn(function* ({ id, all }) {
    if (Option.isSome(id)) return yield* revokeOne(id.value);
    const clients = yield* approvedClients();
    if (!all && clients.length === 0) return yield* output({}, ["No approved clients."]);
    const names = yield* serverNames();
    const choice = all
      ? "all"
      : yield* askOrFail(
          "a client id",
          "porchlight clients revoke <id>, or --all",
          ask(
            Prompt.select({
              message: "Take access away from",
              choices: [
                ...clients.map((client) => ({
                  title: `${client.client} on ${names.get(client.app) ?? client.app}  (${lastSeen(client.lastUsed, false)})`,
                  value: client.id,
                })),
                { title: "Everyone", value: "all" },
              ],
            }),
          ),
        );
    if (choice !== "all") return yield* revokeOne(choice);
    yield* (yield* Auth).revokeAll();
    yield* output({}, ["Revoked every client and token."]);
  }),
).pipe(Command.withDescription("Take access away from a client (asks which)"));

const clients = Command.make("clients", {}, clientsList).pipe(
  Command.withDescription("List the clients you approved, or revoke them"),
  Command.withSubcommands([clientsRevoke]),
);

const stop = Command.make(
  "stop",
  { app: Argument.string("app").pipe(Argument.withDescription("Stop sharing just this app"), Argument.optional) },
  Effect.fn(function* ({ app }) {
    if (Option.isSome(app)) {
      if (!(yield* stopSharing(app.value))) {
        return yield* new ExitError({ code: ExitCode.internal, message: `${app.value} isn't shared` });
      }
      return yield* output({ app: app.value }, [`Stopped sharing ${app.value}.`]);
    }
    const before = Option.getOrUndefined(yield* daemonState());
    yield* uninstallService();
    yield* (yield* Store).exposed.set([]);
    if (before && Option.isSome(yield* daemonState())) {
      yield* Effect.try({ try: () => process.kill(before.pid, "SIGTERM"), catch: () => "gone" }).pipe(Effect.ignore);
    }
    yield* output({}, ["Stopped porchlight. Approved clients can reconnect when you share again."]);
  }),
).pipe(Command.withDescription("Stop sharing one app, or everything"));

const tunnel = Command.make(
  "tunnel",
  {
    choice: Argument.string("choice").pipe(
      Argument.withDescription('"opentunnel", or your own tunnel\'s HTTPS URL'),
      Argument.optional,
    ),
  },
  Effect.fn(function* ({ choice }) {
    const settings = yield* loadSettings();
    const store = yield* Store;
    const gatewayPort = settings.port ?? (yield* store.ports()).gatewayPort;
    if (Option.isNone(choice)) {
      const current = settings.tunnel ?? "opentunnel";
      const installed = findOpenTunnel() !== undefined;
      return yield* output({ tunnel: current, chosen: settings.tunnel !== undefined, opentunnelInstalled: installed }, [
        current === "opentunnel"
          ? `OpenTunnel${settings.tunnel === undefined ? " (default, you'll be asked on the next run)" : ""}`
          : `Your own tunnel: ${current}, pointed at http://127.0.0.1:${gatewayPort}`,
        ...(current === "opentunnel" && !installed ? ["OpenTunnel isn't installed. Run `bun add -g opentunnel`."] : []),
      ]);
    }
    const value = choice.value.trim().replace(/\/$/, "");
    if (value !== "opentunnel" && !isHttpsUrl(value)) {
      return yield* new ExitError({ code: ExitCode.internal, message: 'Use "opentunnel" or an https:// URL' });
    }
    yield* saveSettings({ ...settings, tunnel: value });
    yield* output({ tunnel: value }, [
      value === "opentunnel" ? "Using OpenTunnel." : `Using ${value}. Point it at http://127.0.0.1:${gatewayPort}`,
    ]);
  }),
).pipe(Command.withDescription("Show or choose how MCP clients reach this computer"));

const app = root.pipe(Command.withSubcommands([status, logs, apps, clients, stop, tunnel, approve, doctor, token]));

const report = (error: ExitError) =>
  Effect.gen(function* () {
    process.exitCode = error.code;
    if (jsonOutput) {
      return yield* Console.log(JSON.stringify({ ok: false, error: error.message, next_action: error.nextAction }));
    }
    yield* Console.error(`✗ ${error.message}`);
    if (error.nextAction) yield* Console.error(`  ${error.nextAction}`);
  });

const internal = (error: { message: string }) =>
  report(new ExitError({ code: ExitCode.internal, message: error.message }));

Command.runWith(app, { version: packageJson.version })(args).pipe(
  Effect.provide(Auth.layer.pipe(Layer.provideMerge(Store.layer()))),
  Effect.catchTags({
    ExitError: report,
    TunnelError: (error) =>
      report(
        new ExitError({ code: ExitCode.tunnelFailed, message: error.message, nextAction: "Run `porchlight doctor`." }),
      ),
    StoreError: internal,
    ConfigError: internal,
    ShellError: internal,
    ListenError: internal,
  }),
  Effect.provide(NodeServices.layer),
  NodeRuntime.runMain,
);
