import { Effect } from "effect";
import { UpstreamError } from "../errors.js";
import { decodeToolsList, jsonPayloads } from "./rpc.js";
import type { Upstream } from "./types.js";

export const parseHeaders = (pairs: ReadonlyArray<string>): Record<string, string> =>
  Object.fromEntries(
    pairs.flatMap((pair) => {
      const separator = pair.indexOf(":");
      if (separator <= 0) return [];
      return [[pair.slice(0, separator).trim(), pair.slice(separator + 1).trim()]];
    }),
  );

const rewriteOrigin = (headers: Headers, upstream: URL): void => {
  const origin = headers.get("origin");
  if (origin === null) return;
  if (!URL.canParse(origin)) {
    headers.delete("origin");
    return;
  }
  const { hostname } = new URL(origin);
  if (hostname === "127.0.0.1" || hostname === "localhost") headers.set("origin", upstream.origin);
};

const forwardedHeaders = [
  "accept",
  "content-type",
  "last-event-id",
  "mcp-protocol-version",
  "mcp-session-id",
  "origin",
  "user-agent",
];

const upstreamHeaders = (incoming: Headers, upstream: URL, extra: Record<string, string>): Headers => {
  const headers = new Headers();
  for (const name of forwardedHeaders) {
    const value = incoming.get(name);
    if (value !== null) headers.set(name, value);
  }
  headers.set("host", upstream.host);
  rewriteOrigin(headers, upstream);
  for (const [name, value] of Object.entries(extra)) headers.set(name, value);
  return headers;
};

const forward = Effect.fn("HttpUpstream.forward")(function* (
  url: string,
  request: Request,
  body: ArrayBuffer | undefined,
  extraHeaders: Record<string, string>,
) {
  const upstream = new URL(url);
  const response = yield* Effect.tryPromise({
    try: () =>
      fetch(upstream, {
        method: request.method,
        headers: upstreamHeaders(request.headers, upstream, extraHeaders),
        body,
        redirect: "manual",
      }),
    catch: (cause) => new UpstreamError({ message: `${url} is unreachable`, cause }),
  });
  if (response.status >= 300 && response.status < 400) {
    return new Response("The app answered with a redirect, which isn't forwarded", { status: 502 });
  }
  const headers = new Headers(response.headers);
  headers.delete("content-encoding");
  return new Response(response.body, { status: response.status, headers });
});

const listTools = Effect.fn("HttpUpstream.listTools")(
  function* (url: string, extraHeaders: Record<string, string>) {
    const response = yield* Effect.tryPromise({
      try: (signal) =>
        fetch(url, {
          method: "POST",
          headers: {
            ...extraHeaders,
            "content-type": "application/json",
            accept: "application/json, text/event-stream",
          },
          body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
          redirect: "manual",
          signal,
        }),
      catch: (cause) => new UpstreamError({ message: `${url} is unreachable`, cause }),
    });
    const [payload = ""] = yield* jsonPayloads(response);
    return yield* decodeToolsList(payload, url);
  },
  Effect.timeoutOrElse({
    duration: "10 seconds",
    orElse: () => Effect.fail(new UpstreamError({ message: "tools/list timed out" })),
  }),
);

export const makeHttpUpstream = (url: string, headers: Record<string, string>): Upstream => ({
  describe: url,
  listTools: listTools(url, headers),
  handle: (request, body) => forward(url, request, body, headers),
});
