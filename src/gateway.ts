import type { BunRequest } from "bun";
import { Effect, Option, Predicate, Schema } from "effect";
import { accessTokenSeconds, Auth, type TokenPair } from "./auth.js";
import { fetchClientMetadata, isValidRedirectUri } from "./client-metadata.js";
import { hmac, randomToken, safeEqual } from "./crypto.js";
import { ListenError } from "./errors.js";
import { authorizePage, htmlResponse, messageResponse, redirectTo, type ClientIdentity } from "./pages.js";
import type { Policy } from "./policy.js";
import type { Entry, Registry } from "./registry.js";
import { Store } from "./store.js";
import { filterToolsList } from "./upstream/rpc.js";

export interface GatewayOptions {
  port: number;
  publicUrl: string;
  approvalPort: number;
  registry: Registry;
}

const RegistrationRequest = Schema.Struct({
  redirect_uris: Schema.Array(Schema.String),
  token_endpoint_auth_method: Schema.optional(Schema.String),
  client_name: Schema.optional(Schema.String),
});

const RpcMessage = Schema.Struct({
  id: Schema.optional(Schema.Unknown),
  method: Schema.optional(Schema.String),
  params: Schema.optional(Schema.Unknown),
});

type RpcMessage = typeof RpcMessage.Type;

const RpcBody = Schema.fromJsonString(Schema.Union([RpcMessage, Schema.Array(RpcMessage)]));

const json = (body: unknown, status = 200): Response => Response.json(body, { status });

const text = (body: string, status: number): Response => new Response(body, { status });

const maxRequestBodySize = 4 * 1024 * 1024;

const cookieName = (requestId: string): string => `__Host-pl_${requestId}`;

const handshakeMethods = new Set(["initialize", "server/discover", "ping"]);

const clientMethods = new Set([
  ...handshakeMethods,
  "tools/list",
  "tools/call",
  "resources/list",
  "resources/templates/list",
  "resources/read",
  "resources/subscribe",
  "resources/unsubscribe",
  "prompts/list",
  "prompts/get",
  "completion/complete",
  "logging/setLevel",
]);

const refusal = (message: RpcMessage, policy: Policy): string | undefined => {
  const { method } = message;
  if (method === undefined || method.startsWith("notifications/")) return undefined;
  if (!clientMethods.has(method)) return `Method not allowed: ${method}`;
  const toolsOnly = policy.allow.length > 0 && !method.startsWith("tools/") && !handshakeMethods.has(method);
  if (toolsOnly) return `Only the allowed tools are shared, not ${method}`;
  return undefined;
};

const toolName = (message: RpcMessage): string =>
  Predicate.isObject(message.params) && typeof message.params.name === "string" ? message.params.name : "";

const tokenResponse = (pair: TokenPair): Response =>
  json({
    access_token: pair.access,
    refresh_token: pair.refresh,
    token_type: "Bearer",
    expires_in: accessTokenSeconds,
  });

const FormFields = Schema.Record(Schema.String, Schema.Unknown);

const readFields = (request: Request) =>
  (request.headers.get("content-type") ?? "").includes("application/json")
    ? Effect.tryPromise(() => request.json())
    : Effect.tryPromise(() => request.formData()).pipe(Effect.map((form) => Object.fromEntries(form)));

const readParams = (request: Request) =>
  readFields(request).pipe(
    Effect.flatMap(Schema.decodeUnknownEffect(FormFields)),
    Effect.map(
      (fields): ReadonlyMap<string, string> =>
        new Map(
          Object.entries(fields).flatMap(([key, value]): Array<[string, string]> =>
            typeof value === "string" ? [[key, value]] : [],
          ),
        ),
    ),
    Effect.orElseSucceed((): ReadonlyMap<string, string> => new Map()),
  );

const readBody = (request: Request) =>
  Effect.tryPromise(() => request.arrayBuffer()).pipe(Effect.orElseSucceed(() => new ArrayBuffer(0)));

const corsHeaders = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, DELETE, OPTIONS",
  "access-control-allow-headers":
    "authorization, content-type, accept, mcp-session-id, mcp-protocol-version, last-event-id",
  "access-control-expose-headers": "www-authenticate, mcp-session-id, mcp-protocol-version",
  "access-control-max-age": "86400",
};

const withCors = (response: Response): Response => {
  const headers = new Headers(response.headers);
  for (const [name, value] of Object.entries(corsHeaders)) headers.set(name, value);
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
};

const recover = (effect: Effect.Effect<Response, unknown>): Effect.Effect<Response> =>
  effect.pipe(Effect.catchCause((cause) => Effect.logError(cause).pipe(Effect.as(text("Internal error", 500)))));

export const startGateway = Effect.fn("Gateway.start")(function* (options: GatewayOptions) {
  const store = yield* Store;
  const auth = yield* Auth;
  const run = Effect.runPromiseWith(yield* Effect.services());
  const { publicUrl } = options;

  const route =
    <R extends Request>(methods: Partial<Record<string, (request: R) => Effect.Effect<Response, unknown>>>) =>
    (request: R): Promise<Response> => {
      const handler = methods[request.method];
      const response =
        request.method === "OPTIONS"
          ? Effect.succeed(new Response(null, { status: 204 }))
          : handler
            ? recover(handler(request))
            : Effect.succeed(text("Method not allowed", 405));
      return run(
        response.pipe(
          Effect.tap((result) =>
            Effect.logDebug(`${request.method} ${new URL(request.url).pathname} → ${result.status}`).pipe(
              Effect.annotateLogs({
                agent: request.headers.get("user-agent") ?? "-",
                auth: request.headers.has("authorization") ? "bearer" : "none",
              }),
            ),
          ),
          Effect.map(withCors),
        ),
      );
    };
  const { registry } = options;
  const sessionKey = yield* store.sessionKey();

  const sealSession = (grant: string, session: string): string =>
    `${session}.${hmac(sessionKey, `${grant}:${session}`)}`;

  const unsealSession = (grant: string, sealed: string): string | undefined => {
    const session = sealed.slice(0, Math.max(sealed.lastIndexOf("."), 0));
    return session !== "" && safeEqual(sealed, sealSession(grant, session)) ? session : undefined;
  };

  const withSession = (request: Request, session: string): Request => {
    const headers = new Headers(request.headers);
    headers.set("mcp-session-id", session);
    return new Request(request, { headers });
  };

  const sealResponse = (grant: string, response: Response): Response => {
    const session = response.headers.get("mcp-session-id");
    if (session === null) return response;
    const headers = new Headers(response.headers);
    headers.set("mcp-session-id", sealSession(grant, session));
    return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
  };

  const onlyServer = (): Entry | undefined => {
    const entries = registry.list();
    return entries.length === 1 ? entries[0] : undefined;
  };

  const protectedResource = (slug: string | undefined): Response => {
    if (!slug || !registry.get(slug)) return json({ error: "unknown_resource" }, 404);
    return json({
      resource: `${publicUrl}/${slug}/mcp`,
      authorization_servers: [publicUrl],
      bearer_methods_supported: ["header"],
    });
  };

  const authorizationServer = (): Response =>
    json({
      issuer: publicUrl,
      authorization_endpoint: `${publicUrl}/oauth/authorize`,
      token_endpoint: `${publicUrl}/oauth/token`,
      revocation_endpoint: `${publicUrl}/oauth/revoke`,
      registration_endpoint: `${publicUrl}/oauth/register`,
      code_challenge_methods_supported: ["S256"],
      grant_types_supported: ["authorization_code", "refresh_token"],
      response_types_supported: ["code"],
      token_endpoint_auth_methods_supported: ["none"],
      client_id_metadata_document_supported: true,
      authorization_response_iss_parameter_supported: true,
    });

  const redirectWithCode = (redirectUri: string, code: string, state: string): Response =>
    redirectTo(redirectUri, { code, state, iss: publicUrl });

  const unauthorized = (slug: string): Response =>
    new Response(null, {
      status: 401,
      headers: {
        "www-authenticate": `Bearer resource_metadata="${publicUrl}/.well-known/oauth-protected-resource/${slug}/mcp"`,
      },
    });

  const register = Effect.fn("Gateway.register")(function* (request: Request) {
    const ip = request.headers.get("x-forwarded-for")?.split(",")[0]?.trim() ?? "unknown";
    if (!(yield* auth.allowRegistration(ip))) return json({ error: "rate_limited" }, 429);
    const body = yield* Effect.tryPromise(() => request.text()).pipe(
      Effect.flatMap(Schema.decodeUnknownEffect(Schema.fromJsonString(RegistrationRequest))),
      Effect.option,
    );
    if (Option.isNone(body)) return json({ error: "invalid_client_metadata" }, 400);
    const { redirect_uris: redirectUris, token_endpoint_auth_method: authMethod } = body.value;
    if (redirectUris.length === 0 || !redirectUris.every(isValidRedirectUri)) {
      return json({ error: "invalid_redirect_uri" }, 400);
    }
    if (authMethod !== undefined && authMethod !== "none") return json({ error: "public_clients_only" }, 400);
    const clientId = randomToken(16);
    const name = body.value.client_name?.trim().slice(0, 80) || null;
    yield* store.clients.register(clientId, redirectUris, name);
    yield* store.audit.write("dcr.register", clientId);
    return json({ client_id: clientId, redirect_uris: redirectUris, token_endpoint_auth_method: "none" }, 201);
  });

  const identifyClient = Effect.fn("Gateway.identifyClient")(function* (clientId: string, redirectUri: string) {
    const registered = yield* store.clients.redirectUris(clientId);
    if (registered) {
      if (!registered.includes(redirectUri)) return Option.none<ClientIdentity>();
      return Option.some<ClientIdentity>({ name: yield* store.clients.name(clientId), verifiedHost: undefined });
    }
    const metadata = yield* fetchClientMetadata(clientId);
    return metadata.pipe(
      Option.filter((doc) => doc.redirectUris.includes(redirectUri)),
      Option.map((doc): ClientIdentity => ({ name: doc.name ?? doc.host, verifiedHost: doc.host })),
    );
  });

  const authorize = Effect.fn("Gateway.authorize")(function* (request: BunRequest) {
    const params = new URL(request.url).searchParams;
    const clientId = params.get("client_id") ?? "";
    const redirectUri = params.get("redirect_uri") ?? "";
    const challenge = params.get("code_challenge") ?? "";
    const pkceValid = challenge !== "" && params.get("code_challenge_method") === "S256";
    if (params.get("response_type") !== "code" || !pkceValid) {
      yield* Effect.logDebug("authorize rejected: missing response_type=code or S256 PKCE");
      return json(
        { error: "invalid_request", error_description: "response_type=code with S256 PKCE is required" },
        400,
      );
    }
    const resource = (params.get("resource") ?? "").replace(/\/$/, "");
    const server =
      registry.list().find((candidate) => resource === `${publicUrl}/${candidate.spec.slug}/mcp`) ??
      (resource === "" || resource === publicUrl ? onlyServer() : undefined);
    if (!server) {
      yield* Effect.logDebug(`authorize rejected: unknown resource "${resource}"`);
      const path = URL.canParse(resource) ? new URL(resource).pathname : "";
      const moved = registry.list().find((candidate) => path === `/${candidate.spec.slug}/mcp`);
      if (!moved) return json({ error: "invalid_target" }, 400);
      return messageResponse(400, {
        label: "moved",
        title: "this connector uses an old address",
        detail: `${moved.spec.name} moved to a new url. remove this connector from your mcp client and add it again.`,
        next: `${publicUrl}/${moved.spec.slug}/mcp`,
      });
    }
    const client = yield* identifyClient(clientId, redirectUri);
    if (Option.isNone(client)) {
      yield* Effect.logDebug(`authorize rejected: redirect "${redirectUri}" not allowed for client "${clientId}"`);
      return json({ error: "invalid_redirect_uri" }, 400);
    }
    const requestId = randomToken(12);
    const begun = yield* auth
      .begin({
        id: requestId,
        clientId,
        server: server.spec.slug,
        redirectUri,
        challenge,
        state: params.get("state") ?? "",
      })
      .pipe(Effect.option);
    if (Option.isNone(begun)) {
      return messageResponse(429, {
        label: "slow down",
        title: "too many connection requests",
        detail: "porchlight limits new requests so nobody can guess their way in.",
        next: "wait a few minutes, then connect again from your mcp client",
      });
    }
    const nonce = begun.value;
    request.cookies.set(cookieName(requestId), nonce, { secure: true, httpOnly: true, sameSite: "lax", path: "/" });
    return htmlResponse(
      authorizePage({
        requestId,
        summary: {
          client: client.value,
          serverName: server.spec.name,
          toolCount: server.state === "live" ? server.allowed.size : undefined,
          redirectUri,
        },
        approvalPort: options.approvalPort,
      }),
    );
  });

  const completeWithTicket = Effect.fn("Gateway.completeWithTicket")(function* (request: BunRequest) {
    const params = new URL(request.url).searchParams;
    const requestId = params.get("req") ?? "";
    const nonce = request.cookies.get(cookieName(requestId)) ?? undefined;
    return yield* auth.approveWithTicket(requestId, params.get("g") ?? "", nonce).pipe(
      Effect.map((approval) => redirectWithCode(approval.redirectUri, approval.code, approval.state)),
      Effect.catchTag("AuthError", () =>
        Effect.succeed(
          messageResponse(403, {
            label: "approval failed",
            title: "approval failed",
            detail: "each approval link works once, and this one was already used or ran out.",
            next: "go back to your mcp client and connect again",
          }),
        ),
      ),
    );
  });

  const completeWithCode = Effect.fn("Gateway.completeWithCode")(function* (request: BunRequest) {
    const params = yield* readParams(request);
    const requestId = params.get("req") ?? "";
    const nonce = request.cookies.get(cookieName(requestId)) ?? undefined;
    return yield* auth.approveWithCode(requestId, params.get("code") ?? "", nonce).pipe(
      Effect.map((approval) => redirectWithCode(approval.redirectUri, approval.code, approval.state)),
      Effect.catchTag("AuthError", () =>
        Effect.succeed(
          messageResponse(403, {
            label: "wrong code",
            title: "that code didn't work",
            detail: "each code works once and only for a few minutes. check for a typo, or get a fresh one.",
            next: "run porchlight approve on the computer for a new code",
          }),
        ),
      ),
    );
  });

  const token = Effect.fn("Gateway.token")(function* (request: Request) {
    const params = yield* readParams(request);
    const grantType = params.get("grant_type");
    yield* Effect.logDebug(`token request: grant_type=${grantType ?? "none"}`).pipe(
      Effect.annotateLogs({ fields: [...params.keys()].join(",") }),
    );
    const pair =
      grantType === "authorization_code"
        ? auth.exchangeCode(params.get("code") ?? "", params.get("code_verifier") ?? "", params.get("resource") ?? "")
        : grantType === "refresh_token"
          ? auth.refresh(params.get("refresh_token") ?? "")
          : undefined;
    if (!pair) return json({ error: "unsupported_grant_type" }, 400);
    return yield* pair.pipe(
      Effect.map(tokenResponse),
      Effect.catchTag("AuthError", (error) =>
        Effect.logDebug(`token request rejected: ${error.message}`).pipe(
          Effect.as(json({ error: "invalid_grant" }, 400)),
        ),
      ),
    );
  });

  const revoke = Effect.fn("Gateway.revoke")(function* (request: Request) {
    const params = yield* readParams(request);
    const value = params.get("token");
    if (value) {
      yield* auth.revoke(value);
      yield* store.audit.write("token.revoked");
    }
    return new Response(null, { status: 200 });
  });

  const recordTools = Effect.fn("Gateway.recordTools")(function* (
    event: "tool.called" | "tool.blocked" | "method.blocked",
    caller: string,
    server: Entry,
    names: ReadonlyArray<string>,
  ) {
    if (names.length === 0) return;
    yield* store.audit.writeMany(
      event,
      names.map((name) => `${caller} -> ${server.spec.slug} | ${name}`),
    );
  });

  const proxyPost = Effect.fn("Gateway.proxyPost")(function* (request: Request, server: Entry, caller: string) {
    const body = yield* readBody(request);
    const decoded = Schema.decodeUnknownOption(RpcBody)(new TextDecoder().decode(body));
    if (Option.isNone(decoded)) {
      return json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } }, 400);
    }
    const messages = Array.isArray(decoded.value) ? decoded.value : [decoded.value];
    const visible = server.allowed;
    const policy = server.spec.policy;
    const refused = messages.find((message) => refusal(message, policy) !== undefined);
    if (refused) {
      yield* recordTools("method.blocked", caller, server, [refused.method ?? "unknown"]);
      return json({
        jsonrpc: "2.0",
        id: refused.id ?? null,
        error: { code: -32601, message: refusal(refused, policy) },
      });
    }
    const blocked = messages.find((message) => message.method === "tools/call" && !visible.has(toolName(message)));
    if (blocked) {
      yield* recordTools("tool.blocked", caller, server, [toolName(blocked)]);
      return json({
        jsonrpc: "2.0",
        id: blocked.id ?? null,
        error: { code: -32602, message: `Tool not allowed: ${toolName(blocked)}` },
      });
    }
    const response = yield* server.upstream.handle(request, body);
    yield* recordTools(
      "tool.called",
      caller,
      server,
      messages.filter((message) => message.method === "tools/call").map(toolName),
    );
    if (!messages.some((message) => message.method === "tools/list")) return response;
    return yield* filterToolsList(response, visible);
  });

  const mcp = Effect.fn("Gateway.mcp")(function* (request: BunRequest<"/:slug/mcp">) {
    const server = registry.get(request.params.slug);
    if (!server) return json({ error: "unknown_server" }, 404);
    const bearer = /^Bearer (.+)$/.exec(request.headers.get("authorization") ?? "")?.[1];
    const grant = bearer ? yield* auth.authorize(bearer, server.spec.slug) : undefined;
    if (!bearer || !grant) return unauthorized(server.spec.slug);
    const sealed = request.headers.get("mcp-session-id");
    const session = sealed === null ? undefined : unsealSession(grant, sealed);
    if (sealed !== null && session === undefined) {
      return json({ jsonrpc: "2.0", id: null, error: { code: -32001, message: "Session not found" } }, 404);
    }
    const upstreamRequest = session === undefined ? request : withSession(request, session);
    if (server.state !== "live") {
      return json(
        {
          jsonrpc: "2.0",
          id: null,
          error: { code: -32000, message: `${server.spec.name} isn't available yet: ${server.detail}` },
        },
        503,
      );
    }
    const caller = (yield* auth.caller(bearer)) ?? "an unknown client";
    const response =
      request.method === "POST"
        ? proxyPost(upstreamRequest, server, caller)
        : server.upstream.handle(upstreamRequest, undefined);
    return yield* response.pipe(
      Effect.map((upstream) => sealResponse(grant, upstream)),
      Effect.catchTag("UpstreamError", () =>
        registry.markDown(server.spec.slug).pipe(Effect.as(text(`${server.spec.name} is unreachable`, 502))),
      ),
    );
  });

  const server = yield* Effect.acquireRelease(
    Effect.try({
      try: () =>
        Bun.serve({
          hostname: "127.0.0.1",
          port: options.port,
          maxRequestBodySize,
          routes: {
            "/.well-known/oauth-protected-resource": route({
              GET: () => Effect.succeed(protectedResource(onlyServer()?.spec.slug)),
            }),
            "/.well-known/oauth-protected-resource/:slug/mcp": route({
              GET: (request: BunRequest<"/.well-known/oauth-protected-resource/:slug/mcp">) =>
                Effect.succeed(protectedResource(request.params.slug)),
            }),
            "/.well-known/oauth-authorization-server": route({ GET: () => Effect.succeed(authorizationServer()) }),
            "/.well-known/openid-configuration": route({ GET: () => Effect.succeed(authorizationServer()) }),
            "/oauth/register": route({ POST: register }),
            "/oauth/authorize": route({ GET: authorize }),
            "/oauth/complete": route({ GET: completeWithTicket }),
            "/oauth/complete-code": route({ POST: completeWithCode }),
            "/oauth/token": route({ POST: token }),
            "/oauth/revoke": route({ POST: revoke }),
            "/:slug/mcp": route({ GET: mcp, POST: mcp, DELETE: mcp }),
          },
          fetch: () => text("Not found", 404),
        }),
      catch: (cause) => new ListenError({ message: `Port ${options.port} is in use`, cause }),
    }),
    (server) => Effect.promise(() => server.stop()),
  );
  return server;
});
