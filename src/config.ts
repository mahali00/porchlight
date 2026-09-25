import { Console, Effect, Predicate, Schema } from "effect";
import * as Fs from "node:fs/promises";
import * as Os from "node:os";
import * as Path from "node:path";
import { ConfigError } from "./errors.js";

export const stateDir = process.env["PORCHLIGHT_HOME"] ?? Path.join(Os.homedir(), ".porchlight");
export const logDir = Path.join(stateDir, "logs");
export const configPath = Path.join(stateDir, "porchlight.json");

const toolList = (description: string) => Schema.optionalKey(Schema.Array(Schema.String).annotate({ description }));

const flag = (description: string) => Schema.optionalKey(Schema.Boolean.annotate({ description }));

const policyFields = {
  allow: toolList("Only share these tools"),
  deny: toolList("Hide these tools"),
  readOnly: flag("Only share tools marked read-only"),
};

export const CliInput = Schema.Struct({
  description: Schema.optionalKey(Schema.String.annotate({ description: "What to fill in, shown to the client" })),
  choices: Schema.optionalKey(Schema.Array(Schema.String).annotate({ description: "The only values allowed" })),
});

export type CliInput = typeof CliInput.Type;

export const CliTool = Schema.Struct({
  run: Schema.Array(Schema.String).annotate({
    description:
      'Command and its arguments, like ["garage", "open"]. An argument like "{room}" is filled in by the client.',
  }),
  description: Schema.optionalKey(Schema.String.annotate({ description: "What the tool does, shown to the client" })),
  readOnly: flag("It only reads, never changes anything"),
  inputs: Schema.optionalKey(
    Schema.Record(Schema.String, CliInput).annotate({ description: "Describe or limit each {input}, by name" }),
  ),
});

export type CliTool = typeof CliTool.Type;

export const ServerSettings = Schema.Struct({
  url: Schema.optionalKey(Schema.String.annotate({ description: "Streamable HTTP URL of the local MCP server" })),
  command: Schema.optionalKey(
    Schema.Array(Schema.String).annotate({ description: "Command that starts a stdio MCP server, as a list of words" }),
  ),
  env: Schema.optionalKey(
    Schema.Record(Schema.String, Schema.String).annotate({
      description: "Environment for its commands. ${VAR} reads an environment variable.",
    }),
  ),
  cwd: Schema.optionalKey(Schema.String.annotate({ description: "Working folder for its commands" })),
  tools: Schema.optionalKey(
    Schema.Record(Schema.String, CliTool).annotate({ description: "Command-line commands to share as tools, by name" }),
  ),
  headers: Schema.optionalKey(
    Schema.Record(Schema.String, Schema.String).annotate({
      description: "Headers sent to this server only. ${VAR} reads an environment variable.",
    }),
  ),
  ...policyFields,
  allowDangerous: flag("Expose this server even if it has shell-like tools"),
});

export type ServerSettings = typeof ServerSettings.Type;

export const Settings = Schema.Struct({
  $schema: Schema.optionalKey(Schema.String),
  tunnel: Schema.optionalKey(
    Schema.String.annotate({ description: 'How MCP clients reach this computer: "opentunnel" or your own HTTPS URL' }),
  ),
  port: Schema.optionalKey(Schema.Int.annotate({ description: "Local port the tunnel forwards to" })),
  keepAwake: flag("Keep the Mac awake while sharing"),
  ...policyFields,
  servers: Schema.optionalKey(
    Schema.Record(Schema.String, ServerSettings).annotate({ description: "Settings for one server, by name" }),
  ),
});

export type Settings = typeof Settings.Type;

const SettingsJson = Schema.fromJsonString(Settings);

export const loadSettings = Effect.fn("Config.load")(function* () {
  const file = Bun.file(configPath);
  const exists = yield* Effect.tryPromise({
    try: () => file.exists(),
    catch: (cause) => new ConfigError({ message: `Failed to read ${configPath}`, cause }),
  });
  if (!exists) return {} satisfies Settings;
  const text = yield* Effect.tryPromise({
    try: () => file.text(),
    catch: (cause) => new ConfigError({ message: `Failed to read ${configPath}`, cause }),
  });
  const settings = yield* Schema.decodeUnknownEffect(SettingsJson)(text).pipe(
    Effect.mapError((cause) => new ConfigError({ message: `Invalid ${configPath}: ${cause.message}`, cause })),
  );
  for (const warning of unknownKeyWarnings(text)) {
    if (warned.has(warning)) continue;
    warned.add(warning);
    yield* Console.error(`! ${warning}`);
  }
  return settings;
});

const warned = new Set<string>();

const distance = (left: string, right: string): number => {
  const previous = Array.from({ length: right.length + 1 }, (_, index) => index);
  for (const [row, leftChar] of [...left].entries()) {
    let diagonal = previous[0] ?? 0;
    previous[0] = row + 1;
    for (const [column, rightChar] of [...right].entries()) {
      const above = previous[column + 1] ?? 0;
      previous[column + 1] = Math.min(
        above + 1,
        (previous[column] ?? 0) + 1,
        diagonal + (leftChar === rightChar ? 0 : 1),
      );
      diagonal = above;
    }
  }
  return previous[right.length] ?? 0;
};

const suggest = (key: string, known: ReadonlyArray<string>): string => {
  const match = known.find(
    (candidate) => candidate.toLowerCase() === key.toLowerCase() || distance(candidate, key) <= 2,
  );
  return match ? `, did you mean "${match}"?` : "";
};

const unknownKeys = (value: unknown, known: ReadonlyArray<string>, where: string): Array<string> =>
  Predicate.isObject(value)
    ? Object.keys(value)
        .filter((key) => !known.includes(key))
        .map((key) => `Ignoring unknown key "${key}"${where}${suggest(key, known)}`)
    : [];

const unknownKeyWarnings = (text: string): Array<string> => {
  const parsed: unknown = JSON.parse(text);
  const serverKeys = Object.keys(ServerSettings.fields);
  const servers = Predicate.isObject(parsed) ? parsed["servers"] : undefined;
  return [
    ...unknownKeys(parsed, Object.keys(Settings.fields), ""),
    ...(Predicate.isObject(servers)
      ? Object.entries(servers).flatMap(([name, server]) => unknownKeys(server, serverKeys, ` in servers.${name}`))
      : []),
  ];
};

const writeStep = (step: () => Promise<unknown>) =>
  Effect.tryPromise({
    try: step,
    catch: (cause) => new ConfigError({ message: `Failed to write ${configPath}`, cause }),
  });

export const saveSettings = Effect.fn("Config.save")(function* (settings: Settings) {
  yield* writeStep(() => Fs.mkdir(stateDir, { recursive: true, mode: 0o700 }));
  yield* writeStep(() => Bun.write(configPath, `${JSON.stringify(settings, null, 2)}\n`));
  yield* writeStep(() => Fs.chmod(configPath, 0o600));
});

export const expandVariables = Effect.fn("Config.expandVariables")(function* (values: Record<string, string>) {
  const missing = new Set<string>();
  const expanded = Object.fromEntries(
    Object.entries(values).map(([name, value]) => [
      name,
      value.replace(/\$\{(\w+)\}/g, (_, variable: string) => {
        const resolved = process.env[variable];
        if (resolved === undefined) missing.add(variable);
        return resolved ?? "";
      }),
    ]),
  );
  if (missing.size > 0) {
    return yield* new ConfigError({ message: `Set ${[...missing].join(", ")} before starting porchlight` });
  }
  return expanded;
});

export const isHttpsUrl = (value: string): boolean => URL.canParse(value) && new URL(value).protocol === "https:";

export const jsonSchema = () => ({
  $schema: "https://json-schema.org/draft/2020-12/schema",
  title: "porchlight.json",
  ...Schema.toJsonSchemaDocument(Settings).schema,
});
