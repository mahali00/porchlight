import type { Effect } from "effect";
import type { UpstreamError } from "../errors.js";
import type { Tool } from "../policy.js";

export interface Upstream {
  describe: string;
  listTools: Effect.Effect<ReadonlyArray<Tool>, UpstreamError>;
  handle: (request: Request, body: ArrayBuffer | undefined) => Effect.Effect<Response, UpstreamError>;
}
