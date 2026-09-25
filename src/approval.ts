import type { BunRequest } from "bun";
import { Effect, Option } from "effect";
import * as Os from "node:os";
import { Auth } from "./auth.js";
import { randomToken, sha256 } from "./crypto.js";
import { ListenError } from "./errors.js";
import { notify } from "./system.js";
import { identifyClient } from "./client-metadata.js";
import { approvalPage, htmlResponse, messageResponse, redirectTo, type SharedTool } from "./pages.js";
import { isDangerous } from "./policy.js";
import type { Registry } from "./registry.js";
import { Store } from "./store.js";

export interface ApprovalOptions {
  port: number;
  publicUrl: string;
  registry: Registry;
}

const text = (body: string, status: number): Response => new Response(body, { status });

const expired = (): Response =>
  messageResponse(410, {
    label: "expired",
    title: "this request expired",
    detail: "connection requests only last a few minutes, and this one ran out. nothing was shared.",
    next: "go back to your mcp client and connect again",
  });

const formValue = (form: FormData, name: string): string => {
  const value = form.get(name);
  return typeof value === "string" ? value : "";
};

export const startApproval = Effect.fn("Approval.start")(function* (options: ApprovalOptions) {
  const store = yield* Store;
  const auth = yield* Auth;
  const host = `127.0.0.1:${options.port}`;
  const origin = `http://${host}`;
  const csrfTokens = new Map<string, string>();

  const show = Effect.fn("Approval.show")(function* (request: Request) {
    const requestId = new URL(request.url).searchParams.get("req") ?? "";
    const pending = yield* store.pending.get(requestId);
    if (!pending || pending.expiresAt < Date.now()) return expired();
    const csrf = randomToken(16);
    csrfTokens.set(requestId, csrf);
    const server = options.registry.get(pending.server);
    const tools = (server?.tools ?? [])
      .filter((tool) => server?.allowed.has(tool.name))
      .map((tool): SharedTool => ({ name: tool.name, dangerous: isDangerous(tool) }));
    return htmlResponse(
      approvalPage({
        requestId,
        summary: {
          client: yield* identifyClient(pending.clientId, yield* store.clients.name(pending.clientId)),
          serverName: server?.spec.name ?? pending.server,
          toolCount: server?.allowed.size,
          redirectUri: pending.redirectUri,
        },
        tools,
        hostname: Os.hostname().replace(/\.local$/, ""),
        approvalPort: options.port,
        csrf,
      }),
    );
  });

  const decide = Effect.fn("Approval.decide")(function* (request: Request) {
    const sameOrigin =
      request.headers.get("origin") === origin || request.headers.get("sec-fetch-site") === "same-origin";
    if (!sameOrigin) return text("Bad origin", 403);
    const form = yield* Effect.tryPromise(() => request.formData()).pipe(Effect.option);
    if (Option.isNone(form)) return text("Bad request", 400);
    const requestId = formValue(form.value, "req");
    const csrf = csrfTokens.get(requestId);
    csrfTokens.delete(requestId);
    if (!requestId || csrf === undefined || csrf !== formValue(form.value, "csrf")) return text("Bad CSRF token", 403);
    const pending = yield* store.pending.get(requestId);
    if (!pending || pending.expiresAt < Date.now()) return expired();
    if (formValue(form.value, "decision") !== "allow") {
      yield* store.pending.remove(requestId);
      yield* store.audit.write("authz.denied", requestId);
      return redirectTo(pending.redirectUri, { error: "access_denied", state: pending.state, iss: options.publicUrl });
    }
    const ticket = yield* auth.issueTicket(requestId);
    yield* store.audit.write("authz.ticket_issued", `${requestId}:${sha256(ticket).slice(0, 12)}`);
    const client = yield* identifyClient(pending.clientId, yield* store.clients.name(pending.clientId));
    const serverName = options.registry.get(pending.server)?.spec.name ?? pending.server;
    yield* notify("porchlight", `${client.name} can now use ${serverName}.`);
    return redirectTo(`${options.publicUrl}/oauth/complete`, { req: requestId, g: ticket });
  });

  const guarded =
    (handler: (request: Request) => Effect.Effect<Response, unknown>) =>
    (request: BunRequest): Promise<Response> =>
      request.headers.get("host") === host
        ? Effect.runPromise(
            handler(request).pipe(
              Effect.catchCause((cause) => Effect.logError(cause).pipe(Effect.as(text("Internal error", 500)))),
            ),
          )
        : Promise.resolve(text("Forbidden", 403));

  return yield* Effect.acquireRelease(
    Effect.try({
      try: () =>
        Bun.serve({
          hostname: "127.0.0.1",
          port: options.port,
          routes: {
            "/approve": { GET: guarded(show), POST: guarded(decide) },
          },
          fetch: (request) => text(request.headers.get("host") === host ? "Not found" : "Forbidden", 404),
        }),
      catch: (cause) => new ListenError({ message: `Port ${options.port} is in use`, cause }),
    }),
    (server) => Effect.promise(() => server.stop()),
  );
});
