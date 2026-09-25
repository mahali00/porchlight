import { afterAll, describe, expect, test } from "bun:test";
import { Effect, Exit, Layer, Predicate, Scope, Stream } from "effect";
import * as Path from "node:path";
import { startApproval } from "../src/approval.js";
import { Auth } from "../src/auth.js";
import { pkceChallenge } from "../src/crypto.js";
import { startGateway } from "../src/gateway.js";
import { openPolicy } from "../src/policy.js";
import { makeRegistry, type Registry, type ServerState } from "../src/registry.js";
import type { CliTool } from "../src/config.js";
import type { ServerSpec } from "../src/servers.js";
import { Store } from "../src/store.js";

const tools = [
  { name: "get_basic_info", annotations: { readOnlyHint: true } },
  { name: "write_html" },
  { name: "delete_nodes" },
];

const shellTools = [{ name: "run_shell" }];

const received: Array<Headers> = [];
let landed = 0;

const toolsResponse = (pathname: string, message: unknown): Response => {
  if (Array.isArray(message)) return Response.json(message.map((_, id) => ({ jsonrpc: "2.0", id, result: { tools } })));
  const result = { tools: pathname === "/shell/mcp" ? shellTools : tools, echo: message };
  const body = JSON.stringify({ jsonrpc: "2.0", id: 1, result }, null, pathname === "/split/mcp" ? 2 : undefined);
  const data = body
    .split("\n")
    .map((line) => `data: ${line}`)
    .join("\n");
  const initializing = Predicate.isObject(message) && message["method"] === "initialize";
  const headers = { "content-type": "text/event-stream", ...(initializing ? { "mcp-session-id": "upstream-1" } : {}) };
  return new Response(`event: message\n${data}\n\n`, { headers });
};

const serveUpstream = Effect.acquireRelease(
  Effect.sync(() =>
    Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      fetch: async (request) => {
        const { pathname } = new URL(request.url);
        if (pathname === "/landing") {
          landed++;
          return new Response("landed");
        }
        received.push(request.headers);
        const message: unknown = await request.json();
        if (JSON.stringify(message).includes("redirect_me")) return Response.redirect(new URL("/landing", request.url));
        return toolsResponse(pathname, message);
      },
    }),
  ),
  (server) => Effect.promise(() => server.stop()),
);

const freePort = Effect.acquireUseRelease(
  Effect.sync(() => Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response() })),
  (server) => Effect.fromNullishOr(server.port),
  (server) => Effect.promise(() => server.stop()),
);

const reaches = (registry: Registry, slug: string, state: ServerState) =>
  Stream.suspend(() => registry.changes(slug)).pipe(
    Stream.filter((entry) => entry.state === state),
    Stream.runHead,
    Effect.timeoutOrElse({ duration: "10 seconds", orElse: () => Effect.die(`${slug} never became ${state}`) }),
    Effect.asVoid,
  );

const httpSpec = (slug: string, url: string, overrides: Partial<ServerSpec> = {}): ServerSpec => ({
  key: slug,
  slug,
  name: slug,
  source: { kind: "http", url, headers: {} },
  policy: openPolicy,
  allowDangerous: false,
  ...overrides,
});

const fixtureSpec: ServerSpec = {
  key: "fixture",
  slug: "fixture",
  name: "Fixture",
  source: {
    kind: "stdio",
    command: [process.execPath, Path.join(import.meta.dir, "fixtures", "stdio-server.ts")],
    env: {},
    cwd: undefined,
  },
  policy: { ...openPolicy, deny: ["wipe"] },
  allowDangerous: false,
};

const cliSpec = (slug: string, tools: Record<string, CliTool>): ServerSpec => ({
  key: slug,
  slug,
  name: slug,
  source: { kind: "cli", tools, env: {}, cwd: undefined },
  policy: openPolicy,
  allowDangerous: false,
});

const shelf = cliSpec("shelf", {
  say: { run: ["echo", "said", "{words}"] },
  light: { run: ["echo", "light", "{room}"], inputs: { room: { choices: ["kitchen"] } } },
});

const harness = Effect.gen(function* () {
  const upstream = yield* serveUpstream;
  const registry = yield* makeRegistry();
  const url = (path: string) => `http://127.0.0.1:${upstream.port}${path}`;
  const hideDelete = { policy: { ...openPolicy, deny: ["delete_nodes"] } };
  const httpServers = [
    httpSpec("paper", url("/mcp"), hideDelete),
    httpSpec("split", url("/split/mcp"), hideDelete),
    httpSpec("readonly", url("/mcp"), { policy: { ...openPolicy, readOnly: true } }),
    httpSpec("allowlist", url("/mcp"), { policy: { ...openPolicy, allow: ["write_html"] } }),
  ];
  yield* Effect.forEach(httpServers, (spec) => registry.add(spec, {}));
  yield* registry.add(fixtureSpec, {});
  yield* registry.add(shelf, {});
  const slugs = [...httpServers.map((spec) => spec.slug), "fixture", "shelf"];
  yield* Effect.forEach(slugs, (slug) => reaches(registry, slug, "live"), { concurrency: "unbounded" });
  const approvalPort = yield* freePort;
  const gatewayServer = yield* startGateway({ port: 0, publicUrl: "https://tunnel.example", approvalPort, registry });
  yield* startApproval({ port: approvalPort, publicUrl: "https://tunnel.example", registry });
  const auth = yield* Auth;
  const issued = yield* Effect.forEach(slugs, (slug) =>
    auth.createStaticToken(slug, "test", 60_000).pipe(Effect.map(({ token }) => [slug, token] as const)),
  );
  const expired = yield* auth.createStaticToken("paper", "expired", -1_000);
  return {
    registry,
    upstreamPort: yield* Effect.fromNullishOr(upstream.port),
    gateway: `http://127.0.0.1:${yield* Effect.fromNullishOr(gatewayServer.port)}`,
    approvalPort,
    approval: `http://127.0.0.1:${approvalPort}`,
    tokens: Object.fromEntries(issued),
    expiredToken: expired.token,
  };
});

const layer = Auth.layer.pipe(Layer.provideMerge(Store.layer(":memory:")));
const scope = Effect.runSync(Scope.make("parallel"));
const services = await Effect.runPromise(Layer.buildWithScope(layer, scope));
const run = Effect.runPromiseWith(services);
const { registry, upstreamPort, gateway, approvalPort, approval, tokens, expiredToken } = await run(
  harness.pipe(Scope.provide(scope)),
);

afterAll(() => run(Scope.close(scope, Exit.void)));

const rpc = (
  method: string,
  params: object = {},
  options: { token?: string; server?: string; session?: string; headers?: Record<string, string>; id?: number } = {},
) =>
  fetch(`${gateway}/${options.server ?? "paper"}/mcp`, {
    method: "POST",
    headers: {
      ...options.headers,
      authorization: `Bearer ${options.token ?? tokens[options.server ?? "paper"]}`,
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      ...(options.session ? { "mcp-session-id": options.session } : {}),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: options.id ?? 1, method, params }),
  });

describe("gateway", () => {
  test("rejects requests without a valid bearer token and points at resource metadata", async () => {
    const response = await rpc("tools/list", {}, { token: "nope" });
    expect(response.status).toBe(401);
    expect(response.headers.get("www-authenticate")).toContain("/.well-known/oauth-protected-resource/paper/mcp");
  });

  test("rejects an expired token", async () => {
    expect((await rpc("tools/list", {}, { token: expiredToken })).status).toBe(401);
  });

  test("filters tools/list in SSE events, events split across data lines, and JSON batches", async () => {
    const batch = await fetch(`${gateway}/paper/mcp`, {
      method: "POST",
      headers: { authorization: `Bearer ${tokens.paper}`, "content-type": "application/json" },
      body: JSON.stringify([{ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }]),
    });
    const bodies = [
      await (await rpc("tools/list", {}, { server: "paper" })).text(),
      await (await rpc("tools/list", {}, { server: "split" })).text(),
      await batch.text(),
    ];
    for (const body of bodies) {
      expect(body).toContain("get_basic_info");
      expect(body).toContain("write_html");
      expect(body).not.toContain("delete_nodes");
    }
  });

  test("a read-only server shares only tools marked read-only", async () => {
    const listed = await (await rpc("tools/list", {}, { server: "readonly" })).text();
    const called = await (await rpc("tools/call", { name: "write_html" }, { server: "readonly" })).json();
    expect(listed).toContain("get_basic_info");
    expect(listed).not.toContain("write_html");
    expect(called).toMatchObject({ error: { code: -32602 } });
  });

  test("an allow-list server shares only the listed tools and no other methods", async () => {
    const listed = await (await rpc("tools/list", {}, { server: "allowlist" })).text();
    const resources = await (await rpc("resources/list", {}, { server: "allowlist" })).json();
    expect(listed).toContain("write_html");
    expect(listed).not.toContain("get_basic_info");
    expect(resources).toMatchObject({ error: { code: -32601 } });
  });

  test("rejects request bodies over the size limit", async () => {
    const response = await fetch(`${gateway}/paper/mcp`, {
      method: "POST",
      headers: { authorization: `Bearer ${tokens.paper}`, "content-type": "application/json" },
      body: "x".repeat(5 * 1024 * 1024),
    });
    expect(response.status).toBe(413);
  });

  test("does not follow redirects from an upstream", async () => {
    const response = await rpc("tools/call", { name: "write_html", arguments: { html: "redirect_me" } });
    expect(response.status).toBe(502);
    expect(response.headers.get("location")).toBeNull();
    expect(landed).toBe(0);
  });

  test("does not serve the approval page", async () => {
    expect((await fetch(`${gateway}/approve?req=x`)).status).toBe(404);
  });

  test("blocks calls to hidden or unknown tools before they reach upstream", async () => {
    const before = received.length;
    for (const name of ["delete_nodes", "secret_tool"]) {
      const body = await (await rpc("tools/call", { name })).json();
      expect(body).toMatchObject({ error: { code: -32602 } });
    }
    expect(received.length).toBe(before);
  });

  test("refuses unknown MCP methods before they reach upstream", async () => {
    const before = received.length;
    const body = await (await rpc("admin/shutdown")).json();
    expect(body).toMatchObject({ error: { code: -32601 } });
    expect(received.length).toBe(before);
  });

  test("forwards allowed calls without credentials or unknown headers", async () => {
    const headers = { cookie: "__Host-pl_x=nonce", "x-forwarded-for": "10.0.0.1", "x-internal-admin": "yes" };
    const response = await rpc("tools/call", { name: "write_html" }, { headers });
    expect(response.status).toBe(200);
    const forwarded = received.at(-1);
    expect(["authorization", ...Object.keys(headers)].map((name) => forwarded?.get(name) ?? null)).toEqual([
      null,
      null,
      null,
      null,
    ]);
    expect(forwarded?.get("mcp-protocol-version") ?? forwarded?.get("content-type")).not.toBeNull();
  });
});

describe("oauth", () => {
  const redirectUri = "https://claude.ai/callback";
  const verifier = "a-verifier-that-only-this-client-knows";
  const resource = "https://tunnel.example/paper/mcp";

  const startConnecting = async () => {
    const registration = await fetch(`${gateway}/oauth/register`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ redirect_uris: [redirectUri] }),
    });
    const { client_id: clientId } = await registration.json();
    const query = new URLSearchParams({
      response_type: "code",
      client_id: clientId,
      redirect_uri: redirectUri,
      code_challenge: pkceChallenge(verifier),
      code_challenge_method: "S256",
      resource,
      state: "opaque",
    });
    const page = await fetch(`${gateway}/oauth/authorize?${query}`);
    const cookie = page.headers.get("set-cookie")?.split(";")[0] ?? "";
    return { cookie, requestId: /^__Host-pl_([^=]+)=/.exec(cookie)?.[1] ?? "" };
  };

  const requestAccess = async () => {
    const { cookie, requestId } = await startConnecting();
    const ticket = await run(Effect.flatMap(Auth.asEffect(), (auth) => auth.issueTicket(requestId)));
    return { cookie, link: completeLink(requestId, ticket) };
  };

  const completeLink = (requestId: string, ticket: string) =>
    `${gateway}/oauth/complete?${new URLSearchParams({ req: requestId, g: ticket })}`;

  const openLink = (link: string, cookie: string | undefined) =>
    fetch(link, { redirect: "manual", headers: cookie ? { cookie } : {} });

  const token = async (fields: Record<string, string>) => {
    const response = await fetch(`${gateway}/oauth/token`, { method: "POST", body: new URLSearchParams(fields) });
    return { status: response.status, body: await response.json() };
  };

  const connect = async () => {
    const { cookie, link } = await requestAccess();
    const redirect = new URL((await openLink(link, cookie)).headers.get("location") ?? "");
    const code = redirect.searchParams.get("code") ?? "";
    const { body } = await token({ grant_type: "authorization_code", code, code_verifier: verifier, resource });
    return { redirect, access: String(body.access_token), refresh: String(body.refresh_token) };
  };

  test("a connected client can use only the server it was approved for", async () => {
    const { redirect, access } = await connect();
    expect(redirect.origin + redirect.pathname).toBe(redirectUri);
    expect(redirect.searchParams.get("state")).toBe("opaque");
    expect((await rpc("tools/list", {}, { token: access })).status).toBe(200);
    expect((await rpc("tools/list", {}, { token: access, server: "fixture" })).status).toBe(401);
  });

  test("an approval link fails without the requester's cookie and is burned", async () => {
    const { cookie, link } = await requestAccess();
    expect((await openLink(link, undefined)).status).toBe(403);
    expect((await openLink(link, cookie)).status).toBe(403);
  });

  test("a code can't be exchanged without the PKCE verifier", async () => {
    const { cookie, link } = await requestAccess();
    const code = new URL((await openLink(link, cookie)).headers.get("location") ?? "").searchParams.get("code") ?? "";
    const stolen = await token({ grant_type: "authorization_code", code, code_verifier: "guess", resource });
    expect(stolen).toEqual({ status: 400, body: { error: "invalid_grant" } });
  });

  test("the approval page lists the tools that will be shared and no hidden ones", async () => {
    const { requestId } = await startConnecting();
    const page = await (await fetch(`${approval}/approve?req=${requestId}`)).text();
    expect(page).toContain("<code>get_basic_info</code>");
    expect(page).toContain("<code>write_html</code>");
    expect(page).not.toContain("delete_nodes");
  });

  test("denying on the approval page sends the client back with access_denied and ends the request", async () => {
    const { cookie, requestId } = await startConnecting();
    const page = await (await fetch(`${approval}/approve?req=${requestId}`)).text();
    const csrf = /name="csrf" value="([^"]+)"/.exec(page)?.[1] ?? "";
    const denied = await fetch(`${approval}/approve`, {
      method: "POST",
      redirect: "manual",
      headers: { origin: approval },
      body: new URLSearchParams({ req: requestId, csrf, decision: "deny" }),
    });
    const redirect = new URL(denied.headers.get("location") ?? "");
    expect(redirect.origin + redirect.pathname).toBe(redirectUri);
    expect(redirect.searchParams.get("error")).toBe("access_denied");
    const ticket = await run(Effect.flatMap(Auth.asEffect(), (auth) => auth.issueTicket(requestId)));
    expect((await openLink(completeLink(requestId, ticket), cookie)).status).toBe(403);
  });

  test("reusing a refresh token revokes the grant", async () => {
    const { refresh } = await connect();
    const rotated = await token({ grant_type: "refresh_token", refresh_token: refresh });
    const reused = await token({ grant_type: "refresh_token", refresh_token: refresh });
    expect(rotated.status).toBe(200);
    expect(reused).toEqual({ status: 400, body: { error: "invalid_grant" } });
    expect((await rpc("tools/list", {}, { token: String(rotated.body.access_token) })).status).toBe(401);
  });
});

describe("registry", () => {
  test("refuses a server that exposes a command tool unless dangerous tools are allowed", async () => {
    const url = `http://127.0.0.1:${upstreamPort}/shell/mcp`;
    const states = await run(
      Effect.gen(function* () {
        yield* registry.add(httpSpec("shell", url), {});
        const refused = yield* registry.settled("shell");
        yield* registry.add(httpSpec("shell", url, { allowDangerous: true }), {});
        const allowed = yield* registry.settled("shell");
        yield* registry.remove("shell");
        return [refused?.state, allowed?.state];
      }),
    );
    expect(states).toEqual(["refused", "live"]);
  });
});

describe("command-line tools", () => {
  test("run without a shell, pass each input as one argument, and refuse option-like inputs", async () => {
    const call = (name: string, input: object) =>
      rpc("tools/call", { name, arguments: input }, { server: "shelf" }).then((response) => response.json());
    expect(await call("say", { words: "a; echo pwned $(id)" })).toMatchObject({
      result: { content: [{ text: "said a; echo pwned $(id)" }], isError: false },
    });
    const rejected = { error: { code: -32602, message: expect.stringContaining("Invalid arguments") } };
    expect(await call("say", { words: "--version" })).toMatchObject(rejected);
    expect(await call("light", { room: "garage" })).toMatchObject(rejected);
  });

  test("refuse to share a command that hands client input to a shell", async () => {
    const state = await run(
      Effect.gen(function* () {
        yield* registry.add(cliSpec("risky", { go: { run: ["sh", "-c", "{script}"] } }), {});
        const settled = yield* registry.settled("risky");
        yield* registry.remove("risky");
        return settled?.state;
      }),
    );
    expect(state).toBe("refused");
  });
});

describe("stdio servers", () => {
  test("run per session through the gateway, apply the policy, and stop when the session ends", async () => {
    const options = { server: "fixture", token: tokens.fixture };
    const initialized = await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {} }, options);
    const session = initialized.headers.get("mcp-session-id") ?? "";
    expect(session).not.toBe("");

    const listed = await (await rpc("tools/list", {}, { ...options, session })).text();
    expect(listed).toContain("echo");
    expect(listed).not.toContain("wipe");

    const hidden = await (await rpc("tools/call", { name: "wipe" }, { ...options, session })).json();
    expect(hidden).toMatchObject({ error: { code: -32602 } });

    const called = await (await rpc("tools/call", { name: "echo" }, { ...options, session })).text();
    const pid = Number(/pid (\d+)/.exec(called)?.[1]);
    expect(pid).toBeGreaterThan(0);

    const ended = await fetch(`${gateway}/fixture/mcp`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${tokens.fixture}`, "mcp-session-id": session },
    });
    expect(ended.status).toBe(204);
    expect(() => process.kill(pid, 0)).toThrow();
  });
});

describe("sessions", () => {
  test("a session only works with a token from the grant that opened it", async () => {
    const other = await run(
      Effect.flatMap(Auth.asEffect(), (auth) => auth.createStaticToken("fixture", "other", 60_000)),
    );
    const owner = { server: "fixture", token: tokens.fixture };
    const initialized = await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {} }, owner);
    const session = initialized.headers.get("mcp-session-id") ?? "";
    const status = async (token: string, id: string) =>
      (await rpc("tools/list", {}, { server: "fixture", token, session: id })).status;
    expect(await status(other.token, session)).toBe(404);
    expect(await status(tokens.fixture, `${session}x`)).toBe(404);
    expect(await status(tokens.fixture, session.slice(0, session.lastIndexOf(".")))).toBe(404);
    expect(await status(tokens.fixture, session)).toBe(200);
    await fetch(`${gateway}/fixture/mcp`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${tokens.fixture}`, "mcp-session-id": session },
    });
  });
});

describe("restarts", () => {
  test("a stdio session comes back under the same id after its process is gone, until it is ended", async () => {
    const options = { server: "fixture", token: tokens.fixture };
    const params = { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "test", version: "1" } };
    const session = (await rpc("initialize", params, options)).headers.get("mcp-session-id") ?? "";
    const call = (id = 1) => rpc("tools/call", { name: "echo" }, { ...options, session, id });
    const pidOf = async (id = 1) => Number(/pid (\d+)/.exec(await (await call(id)).text())?.[1]);
    const before = await pidOf();
    await run(registry.add(fixtureSpec, {}).pipe(Effect.andThen(reaches(registry, "fixture", "live"))));
    const [after, alongside] = await Promise.all([pidOf(1), pidOf(2)]);
    expect(alongside).toBe(after);
    expect(after).toBeGreaterThan(0);
    expect(after).not.toBe(before);
    expect(() => process.kill(before, 0)).toThrow();
    const ended = await fetch(`${gateway}/fixture/mcp`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${tokens.fixture}`, "mcp-session-id": session },
    });
    expect(ended.status).toBe(204);
    expect((await call()).status).toBe(404);
  });

  test("a session opened before a restart keeps working with the same grant", async () => {
    const initialized = await rpc("initialize", {}, { server: "paper" });
    const session = initialized.headers.get("mcp-session-id") ?? "";
    const restarted = await run(
      startGateway({ port: 0, publicUrl: "https://tunnel.example", approvalPort, registry }).pipe(Scope.provide(scope)),
    );
    const response = await fetch(`http://127.0.0.1:${restarted.port}/paper/mcp`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${tokens.paper}`,
        "content-type": "application/json",
        "mcp-session-id": session,
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
    });
    expect(session).not.toBe("");
    expect(response.status).toBe(200);
  });
});

describe("approval listener", () => {
  test("serves only its exact loopback Host", async () => {
    const statusFor = async (host: string) => (await fetch(`${approval}/approve?req=x`, { headers: { host } })).status;
    expect(await statusFor(`127.0.0.1:${approvalPort}`)).not.toBe(403);
    for (const host of [`localhost:${approvalPort}`, "tunnel.example", `127.0.0.1:${approvalPort + 1}`]) {
      expect(await statusFor(host)).toBe(403);
    }
  });

  test("rejects a cross-origin POST", async () => {
    const response = await fetch(`${approval}/approve`, {
      method: "POST",
      headers: { origin: "https://tunnel.example" },
      body: new URLSearchParams({ req: "x", csrf: "y", decision: "allow" }),
    });
    expect(response.status).toBe(403);
    expect(await response.text()).toBe("Bad origin");
  });
});
