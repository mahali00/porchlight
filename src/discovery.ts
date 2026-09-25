import { Effect, Option, Schema } from "effect";
import { BlockList, isIP } from "node:net";
import type { Settings } from "./config.js";
import * as Os from "node:os";
import * as Path from "node:path";

export interface KnownServer {
  id: string;
  displayName: string;
  url: string;
}

export const knownServers: ReadonlyArray<KnownServer> = [
  { id: "paper", displayName: "Paper Desktop", url: "http://127.0.0.1:29979/mcp" },
];

export interface DiscoveredServer {
  name: string;
  displayName: string;
  source: string;
  kind: "http" | "stdio" | "cli";
  url?: string;
  command?: ReadonlyArray<string>;
  env?: Record<string, string>;
  running: boolean;
  local: boolean;
}

const localRanges = new BlockList();
localRanges.addSubnet("127.0.0.0", 8, "ipv4");
localRanges.addSubnet("10.0.0.0", 8, "ipv4");
localRanges.addSubnet("172.16.0.0", 12, "ipv4");
localRanges.addSubnet("192.168.0.0", 16, "ipv4");
localRanges.addSubnet("::1", 128, "ipv6");
localRanges.addSubnet("fc00::", 7, "ipv6");

export const isLocalUrl = (url: string): boolean => {
  if (!URL.canParse(url)) return false;
  const host = new URL(url).hostname.replace(/^\[|\]$/g, "");
  if (host === "localhost" || host.endsWith(".local")) return true;
  const family = isIP(host);
  return family !== 0 && localRanges.check(host, family === 6 ? "ipv6" : "ipv4");
};

const ServerEntries = Schema.Record(
  Schema.String,
  Schema.Struct({
    url: Schema.optional(Schema.String),
    command: Schema.optional(Schema.String),
    args: Schema.optional(Schema.Array(Schema.String)),
    env: Schema.optional(Schema.Record(Schema.String, Schema.String)),
  }),
);

const McpConfig = Schema.fromJsonString(
  Schema.Struct({
    mcpServers: Schema.optional(ServerEntries),
    servers: Schema.optional(ServerEntries),
    mcp: Schema.optional(Schema.Struct({ servers: Schema.optional(ServerEntries) })),
  }),
);

const configFiles = (cwd: string) => {
  const home = Os.homedir();
  return [
    {
      source: "claude-desktop",
      path: Path.join(home, "Library/Application Support/Claude/claude_desktop_config.json"),
    },
    { source: "claude-desktop", path: Path.join(home, ".config/Claude/claude_desktop_config.json") },
    { source: "claude-code", path: Path.join(home, ".claude.json") },
    { source: "claude-code-project", path: Path.join(cwd, ".mcp.json") },
    { source: "cursor", path: Path.join(home, ".cursor/mcp.json") },
    { source: "cursor-project", path: Path.join(cwd, ".cursor/mcp.json") },
    { source: "vscode-project", path: Path.join(cwd, ".vscode/mcp.json") },
  ];
};

const initializeRequest = JSON.stringify({
  jsonrpc: "2.0",
  id: 1,
  method: "initialize",
  params: {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "porchlight", version: "0.1.0" },
  },
});

export const probe = (url: string) =>
  Effect.tryPromise(() =>
    fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json, text/event-stream" },
      body: initializeRequest,
      signal: AbortSignal.timeout(3_000),
    }),
  ).pipe(
    Effect.map((response) => response.ok),
    Effect.orElseSucceed(() => false),
  );

const readConfig = (path: string, source: string) =>
  Effect.tryPromise(() => Bun.file(path).text()).pipe(
    Effect.option,
    Effect.map((text) => {
      const config = Option.flatMap(text, Schema.decodeUnknownOption(McpConfig));
      if (Option.isNone(config)) return [];
      const { mcpServers, servers, mcp } = config.value;
      const entries = Object.entries({ ...mcp?.servers, ...servers, ...mcpServers });
      return entries.flatMap(([name, entry]): Array<DiscoveredServer> => {
        if (entry.url) {
          return [
            {
              name,
              displayName: name,
              source,
              kind: "http",
              url: entry.url,
              running: false,
              local: isLocalUrl(entry.url),
            },
          ];
        }
        if (entry.command) {
          const command = [entry.command, ...(entry.args ?? [])];
          return [
            { name, displayName: name, source, kind: "stdio", command, env: entry.env, running: false, local: true },
          ];
        }
        return [];
      });
    }),
  );

const identity = (server: DiscoveredServer): string => server.url ?? server.command?.join(" ") ?? server.name;

export const discover = Effect.fn("Discovery.discover")(function* (servers: Settings["servers"] = {}) {
  const own = Object.entries(servers).flatMap(([name, settings]): Array<DiscoveredServer> => {
    const base = { name, displayName: name, source: "porchlight.json", running: false, local: true };
    if (settings.url) return [{ ...base, kind: "http", url: settings.url }];
    if (settings.command) return [{ ...base, kind: "stdio", command: settings.command }];
    if (settings.tools) return [{ ...base, kind: "cli", running: true }];
    return [];
  });
  const known = knownServers.map((server): DiscoveredServer => ({
    name: server.id,
    displayName: server.displayName,
    source: "known",
    kind: "http",
    url: server.url,
    running: false,
    local: true,
  }));
  const configured = yield* Effect.forEach(configFiles(process.cwd()), (file) => readConfig(file.path, file.source), {
    concurrency: "unbounded",
  });
  const unique = new Map<string, DiscoveredServer>();
  for (const server of [...own, ...known, ...configured.flat()]) {
    if (!unique.has(identity(server))) unique.set(identity(server), server);
  }
  const probed = yield* Effect.forEach(
    unique.values(),
    (server) =>
      server.url ? probe(server.url).pipe(Effect.map((running) => ({ ...server, running }))) : Effect.succeed(server),
    { concurrency: "unbounded" },
  );
  return probed.filter((server) => server.source !== "known" || server.running);
});
