import { Effect, Schema } from "effect";
import * as Path from "node:path";
import { expandVariables, logDir, type CliTool, type Settings } from "./config.js";
import { knownServers, type DiscoveredServer } from "./discovery.js";
import { ExitCode, ExitError } from "./errors.js";
import type { Policy } from "./policy.js";

export const Overrides = Schema.Struct({
  allow: Schema.optionalKey(Schema.Array(Schema.String)),
  deny: Schema.optionalKey(Schema.Array(Schema.String)),
  readOnly: Schema.optionalKey(Schema.Boolean),
});

export type Overrides = typeof Overrides.Type;

export type ServerSource =
  | { kind: "http"; url: string; headers: Record<string, string> }
  | { kind: "stdio"; command: ReadonlyArray<string>; env: Record<string, string>; cwd: string | undefined }
  | { kind: "cli"; tools: Record<string, CliTool>; env: Record<string, string>; cwd: string | undefined };

export interface ServerSpec {
  key: string;
  slug: string;
  name: string;
  source: ServerSource;
  policy: Policy;
  allowDangerous: boolean;
}

export const slugify = (value: string): string =>
  value
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "") || "server";

interface Base {
  slug: string;
  name: string;
  url?: string;
  command?: ReadonlyArray<string>;
  env?: Record<string, string>;
  cwd?: string;
  headers?: Record<string, string>;
  tools?: Record<string, CliTool>;
}

const notFound = (key: string) =>
  new ExitError({
    code: ExitCode.noServer,
    message: `No app named "${key}"`,
    nextAction: "Add it with `porchlight apps add`, or run `porchlight apps` to see what porchlight finds.",
  });

const remote = (name: string) =>
  new ExitError({
    code: ExitCode.internal,
    message: `${name} is a remote MCP server, so clients can already connect to it directly`,
    nextAction: "porchlight only shares apps running on this computer or your local network.",
  });

const findBase = Effect.fn("Servers.findBase")(function* (
  key: string,
  settings: Settings,
  discovered: ReadonlyArray<DiscoveredServer>,
) {
  const configured = Object.entries(settings.servers ?? {}).find(
    ([name, server]) => name === key || server.url === key,
  );
  if (configured) {
    const [name, server] = configured;
    if (server.url || server.command || server.tools) return { ...server, slug: slugify(name), name } satisfies Base;
  }
  if (URL.canParse(key) && /^https?:/.test(key)) {
    const { hostname, port } = new URL(key);
    return { slug: slugify(`${hostname}-${port}`), name: key, url: key } satisfies Base;
  }
  const known = knownServers.find((server) => server.id === key);
  if (known) return { slug: known.id, name: known.displayName, url: known.url } satisfies Base;
  const found = discovered.find((server) => server.name === key);
  if (!found) return yield* notFound(key);
  if (!found.local) return yield* remote(found.displayName);
  return {
    slug: slugify(found.name),
    name: found.displayName,
    ...(found.url ? { url: found.url } : {}),
    ...(found.command ? { command: found.command } : {}),
    ...(found.env ? { env: found.env } : {}),
  } satisfies Base;
});

export const resolveSpec = Effect.fn("Servers.resolve")(function* (
  key: string,
  overrides: Overrides,
  settings: Settings,
  discovered: ReadonlyArray<DiscoveredServer>,
) {
  const base = yield* findBase(key, settings, discovered);
  const own = settings.servers?.[key] ?? {};
  const env = yield* expandVariables(base.env ?? {});
  const source: ServerSource = base.url
    ? { kind: "http", url: base.url, headers: yield* expandVariables(base.headers ?? {}) }
    : base.tools && !base.command
      ? { kind: "cli", tools: base.tools, env, cwd: base.cwd }
      : { kind: "stdio", command: base.command ?? [], env, cwd: base.cwd };
  return {
    key,
    slug: base.slug,
    name: base.name,
    source,
    policy: {
      allow: overrides.allow ?? own.allow ?? settings.allow ?? [],
      deny: overrides.deny ?? own.deny ?? settings.deny ?? [],
      readOnly: (overrides.readOnly ?? own.readOnly ?? settings.readOnly) === true,
    },
    allowDangerous: own.allowDangerous === true,
  } satisfies ServerSpec;
});

export const logFileFor = (slug: string): string => Path.join(logDir, `${slug}.log`);
