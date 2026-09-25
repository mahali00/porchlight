import { Schema } from "effect";

const fields = { message: Schema.String, cause: Schema.optional(Schema.Defect) };

export class StoreError extends Schema.TaggedErrorClass<StoreError>()("StoreError", fields) {}

export class AuthError extends Schema.TaggedErrorClass<AuthError>()("AuthError", fields) {}

export class ConfigError extends Schema.TaggedErrorClass<ConfigError>()("ConfigError", fields) {}

export class TunnelError extends Schema.TaggedErrorClass<TunnelError>()("TunnelError", fields) {}

export class UpstreamError extends Schema.TaggedErrorClass<UpstreamError>()("UpstreamError", fields) {}

export class ClientMetadataError extends Schema.TaggedErrorClass<ClientMetadataError>()(
  "ClientMetadataError",
  fields,
) {}

export class ShellError extends Schema.TaggedErrorClass<ShellError>()("ShellError", fields) {}

export const ExitCode = {
  ok: 0,
  noServer: 2,
  upstreamUnreachable: 3,
  tunnelFailed: 4,
  needsHuman: 5,
  internal: 10,
} as const;

export class ExitError extends Schema.TaggedErrorClass<ExitError>()("ExitError", {
  code: Schema.Number,
  message: Schema.String,
  nextAction: Schema.optional(Schema.String),
}) {}

export class ListenError extends Schema.TaggedErrorClass<ListenError>()("ListenError", fields) {}
