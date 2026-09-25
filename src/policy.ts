import { Predicate, Schema } from "effect";
import * as Path from "node:path";

export const Tool = Schema.Struct({
  name: Schema.String,
  description: Schema.optional(Schema.String),
  annotations: Schema.optional(
    Schema.Struct({
      readOnlyHint: Schema.optional(Schema.Boolean),
    }),
  ),
  inputSchema: Schema.optional(Schema.Unknown),
});

export type Tool = typeof Tool.Type;

export interface Policy {
  allow: ReadonlyArray<string>;
  deny: ReadonlyArray<string>;
  readOnly: boolean;
}

export const openPolicy: Policy = { allow: [], deny: [], readOnly: false };

const dangerousNames = [
  /(^|[_\-.])(shell|bash|zsh|terminal|subprocess|spawn|eval)([_\-.]|$)/i,
  /^(exec|execute|run)$/i,
  /(exec|execute|run)[_\-.]?(command|cmd|code|script|shell|bash|python|js|javascript)/i,
  /write[_\-.]?file/i,
];

const dangerousPhrases = [
  /\b(runs?|executes?|executing|running)\s+(an?\s+|any\s+|arbitrary\s+)?(shell|terminal|bash)\s+commands?\b/i,
  /\b(runs?|executes?|executing|running)\s+(an?\s+|any\s+|arbitrary\s+)?(system|os|cli)\s+commands?\b/i,
  /\b(runs?|executes?|executing|running|evaluates?)\s+(arbitrary\s+)?(code|scripts?|python|javascript|js)\b/i,
  /\bshell\s+commands?\b/i,
];

const dangerousParameter = /^(command|commands|cmd|script|shell|shell_command|bash)$/i;

const parameterNames = (tool: Tool): ReadonlyArray<string> =>
  Predicate.isObject(tool.inputSchema) && Predicate.isObject(tool.inputSchema.properties)
    ? Object.keys(tool.inputSchema.properties)
    : [];

export const isDangerous = (tool: Tool): boolean =>
  dangerousNames.some((pattern) => pattern.test(tool.name)) ||
  dangerousPhrases.some((pattern) => pattern.test(tool.description ?? "")) ||
  parameterNames(tool).some((name) => dangerousParameter.test(name));

export const placeholder = /^\{(\w+)\}$/;

const runsAnything =
  /^((ba|z|fi|c|k|tc|da)?sh|python[\d.]*|node|bun|deno|ruby|perl|php|lua|osascript|pwsh|powershell|env|xargs|sudo|ssh|eval)$/;

export const placeholdersOf = (run: ReadonlyArray<string>): ReadonlyArray<string> =>
  run.flatMap((word) => placeholder.exec(word)?.slice(1, 2) ?? []);

export const runsClientCode = (run: ReadonlyArray<string>): boolean => {
  const [program = ""] = run;
  if (placeholder.test(program)) return true;
  return runsAnything.test(Path.basename(program)) && run.some((word) => placeholder.test(word));
};

const isPermitted = (tool: Tool, policy: Policy): boolean =>
  (!policy.readOnly || tool.annotations?.readOnlyHint === true) &&
  (policy.allow.length === 0 || policy.allow.includes(tool.name)) &&
  !policy.deny.includes(tool.name);

export const allowedTools = (tools: ReadonlyArray<Tool>, policy: Policy): ReadonlyArray<Tool> =>
  tools.filter((tool) => isPermitted(tool, policy));

export const allowedToolNames = (tools: ReadonlyArray<Tool>, policy: Policy): ReadonlySet<string> =>
  new Set(allowedTools(tools, policy).map((tool) => tool.name));

export const describePolicy = (policy: Policy): string => {
  if (policy.allow.length > 0) return "allow-list";
  if (policy.readOnly) return "read-only";
  if (policy.deny.length > 0) return "deny-list";
  return "all";
};
