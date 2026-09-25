import { Effect, Layer, ServiceMap } from "effect";
import { approvalCode, normalizeCode, pkceChallenge, randomToken, sha256 } from "./crypto.js";
import { AuthError } from "./errors.js";
import { Store } from "./store.js";

const minute = 60_000;
const hour = 60 * minute;
const day = 24 * hour;

const pendingTtl = 10 * minute;
const ticketTtl = 2 * minute;
const deviceCodeTtl = 5 * minute;
const authorizationCodeTtl = minute;
const accessTokenTtl = hour;
const refreshTokenTtl = 30 * day;
const maxCodeAttempts = 5;
const maxRegistrationsPerHour = 30;
const maxPendingRequests = 20;

export const accessTokenSeconds = accessTokenTtl / 1000;

export interface AuthorizationRequest {
  id: string;
  clientId: string;
  server: string;
  redirectUri: string;
  challenge: string;
  state: string;
}

export interface Approval {
  code: string;
  redirectUri: string;
  state: string;
}

export interface TokenPair {
  access: string;
  refresh: string;
}

interface Ticket {
  requestId: string;
  expiresAt: number;
}

interface AuthorizationCode {
  clientId: string;
  server: string;
  challenge: string;
  expiresAt: number;
}

const fail = (message: string) => Effect.fail(new AuthError({ message }));

const resourceMatches = (resource: string, server: string): boolean => {
  if (resource === "") return true;
  if (!URL.canParse(resource)) return false;
  const path = new URL(resource).pathname.replace(/\/$/, "");
  return path === "" || path === `/${server}/mcp`;
};

export const makeAuth = Effect.gen(function* () {
  const store = yield* Store;
  const tickets = new Map<string, Ticket>();
  const codes = new Map<string, AuthorizationCode>();

  const issueTokens = Effect.fn("Auth.issueTokens")(function* (grantId: string, server: string) {
    const access = randomToken();
    const refresh = randomToken();
    const now = Date.now();
    yield* store.tokens.insert({
      hash: sha256(access),
      kind: "access",
      grantId,
      server,
      expiresAt: now + accessTokenTtl,
    });
    yield* store.tokens.insert({
      hash: sha256(refresh),
      kind: "refresh",
      grantId,
      server,
      expiresAt: now + refreshTokenTtl,
    });
    return { access, refresh } satisfies TokenPair;
  });

  const approve = Effect.fn("Auth.approve")(function* (requestId: string, cookieNonce: string | undefined) {
    const request = yield* store.pending.get(requestId);
    if (!request || request.expiresAt < Date.now()) return yield* fail("Request expired");
    if (!cookieNonce || sha256(cookieNonce) !== request.nonceHash) return yield* fail("Cookie mismatch");
    const code = randomToken();
    codes.set(code, {
      clientId: request.clientId,
      server: request.server,
      challenge: request.challenge,
      expiresAt: Date.now() + authorizationCodeTtl,
    });
    yield* store.pending.remove(requestId);
    yield* store.audit.write("authz.completed", `${request.clientId} -> ${request.server}`);
    return { code, redirectUri: request.redirectUri, state: request.state } satisfies Approval;
  });

  const begin = Effect.fn("Auth.begin")(function* (request: AuthorizationRequest) {
    if ((yield* store.pending.countActive(Date.now())) >= maxPendingRequests) {
      return yield* fail("Too many pending requests");
    }
    const nonce = randomToken(16);
    yield* store.pending.create({ ...request, nonceHash: sha256(nonce), expiresAt: Date.now() + pendingTtl });
    yield* store.audit.write("authz.created", `${request.clientId} -> ${request.server}`);
    return nonce;
  });

  const issueTicket = (requestId: string) =>
    Effect.sync(() => {
      const ticket = randomToken();
      tickets.set(ticket, { requestId, expiresAt: Date.now() + ticketTtl });
      return ticket;
    });

  const approveWithTicket = Effect.fn("Auth.approveWithTicket")(function* (
    requestId: string,
    ticket: string,
    cookieNonce: string | undefined,
  ) {
    const issued = tickets.get(ticket);
    tickets.delete(ticket);
    if (!issued || issued.requestId !== requestId || issued.expiresAt < Date.now()) {
      yield* store.audit.write("authz.complete_failed", requestId);
      return yield* fail("Invalid ticket");
    }
    return yield* approve(requestId, cookieNonce);
  });

  const issueDeviceCode = Effect.fn("Auth.issueDeviceCode")(function* (requestId: string) {
    const code = approvalCode();
    yield* store.pending.setCode(requestId, sha256(code), Date.now() + deviceCodeTtl);
    yield* store.audit.write("authz.code_issued", requestId);
    return code;
  });

  const approveWithCode = Effect.fn("Auth.approveWithCode")(function* (
    requestId: string,
    code: string,
    cookieNonce: string | undefined,
  ) {
    const request = yield* store.pending.get(requestId);
    if (!request?.codeHash || request.expiresAt < Date.now()) return yield* fail("Code expired");
    if (request.attempts >= maxCodeAttempts) {
      yield* store.pending.remove(requestId);
      return yield* fail("Too many attempts");
    }
    yield* store.pending.recordAttempt(requestId);
    if (sha256(normalizeCode(code)) !== request.codeHash) return yield* fail("Invalid code");
    return yield* approve(requestId, cookieNonce);
  });

  const exchangeCode = Effect.fn("Auth.exchangeCode")(function* (code: string, verifier: string, resource: string) {
    const issued = codes.get(code);
    codes.delete(code);
    if (!issued || issued.expiresAt < Date.now()) return yield* fail("invalid_grant");
    if (pkceChallenge(verifier) !== issued.challenge) return yield* fail("PKCE verification failed");
    if (!resourceMatches(resource, issued.server)) return yield* fail("Resource mismatch");
    const grantId = randomToken(8);
    yield* store.grants.insert({ id: grantId, clientId: issued.clientId, server: issued.server });
    return yield* issueTokens(grantId, issued.server);
  });

  const refresh = Effect.fn("Auth.refresh")(function* (refreshToken: string) {
    const token = yield* store.tokens.get(sha256(refreshToken));
    if (!token || token.kind !== "refresh") return yield* fail("invalid_grant");
    if (token.expiresAt < Date.now()) return yield* fail("Refresh token expired");
    if (!(yield* store.tokens.claim(token.hash))) {
      yield* store.tokens.revokeGrant(token.grantId);
      yield* store.audit.write("token.reuse_detected", token.grantId);
      return yield* fail("Refresh token reused; grant revoked");
    }
    return yield* issueTokens(token.grantId, token.server);
  });

  const authorize = Effect.fn("Auth.authorize")(function* (bearer: string, server: string) {
    const token = yield* store.tokens.get(sha256(bearer));
    const valid =
      token !== undefined &&
      token.kind !== "refresh" &&
      !token.revoked &&
      token.expiresAt > Date.now() &&
      token.server === server;
    if (!valid) return undefined;
    yield* store.grants.touch(token.grantId);
    return token.grantId;
  });

  const revoke = (token: string) => store.tokens.revoke(sha256(token));

  const caller = Effect.fn("Auth.caller")(function* (bearer: string) {
    const token = yield* store.tokens.get(sha256(bearer));
    return token ? ((yield* store.grants.clientOf(token.grantId)) ?? undefined) : undefined;
  });

  const allowRegistration = Effect.fn("Auth.allowRegistration")(function* (ip: string) {
    const recent = yield* store.registrations.countSince(ip, Date.now() - hour);
    if (recent.total >= maxRegistrationsPerHour) return false;
    yield* store.registrations.record(ip);
    return true;
  });

  const createStaticToken = Effect.fn("Auth.createStaticToken")(function* (server: string, name: string, ttl: number) {
    const id = randomToken(8);
    const token = randomToken();
    yield* store.grants.insert({ id, clientId: `static:${name}`, server });
    yield* store.tokens.insert({
      hash: sha256(token),
      kind: "static",
      grantId: id,
      server,
      expiresAt: Date.now() + ttl,
    });
    yield* store.audit.write("token.static_created", `${name} -> ${server}`);
    return { id, token };
  });

  const revokeGrant = Effect.fn("Auth.revokeGrant")(function* (id: string) {
    yield* store.tokens.revokeGrant(id);
    const removed = yield* store.grants.remove(id);
    if (removed) yield* store.audit.write("grant.revoked", id);
    return removed;
  });

  const revokeAll = Effect.fn("Auth.revokeAll")(function* () {
    yield* store.tokens.revokeAll();
    yield* store.grants.removeAll();
    yield* store.audit.write("grant.revoked_all");
  });

  return {
    begin,
    issueTicket,
    approveWithTicket,
    issueDeviceCode,
    approveWithCode,
    exchangeCode,
    refresh,
    authorize,
    caller,
    revoke,
    allowRegistration,
    createStaticToken,
    revokeGrant,
    revokeAll,
  };
});

export type AuthApi = Effect.Success<typeof makeAuth>;

export class Auth extends ServiceMap.Service<Auth, AuthApi>()("porchlight/Auth") {
  static layer = Layer.effect(Auth, makeAuth);
}
