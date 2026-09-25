import { Database, type SQLQueryBindings } from "bun:sqlite";
import { Effect, Layer, Schema, ServiceMap } from "effect";
import { chmodSync, closeSync, existsSync, mkdirSync, openSync } from "node:fs";
import * as Path from "node:path";
import { stateDir } from "./config.js";
import { randomToken } from "./crypto.js";
import { StoreError } from "./errors.js";
import { Overrides } from "./servers.js";

export const PendingRequest = Schema.Struct({
  id: Schema.String,
  clientId: Schema.String,
  server: Schema.String,
  redirectUri: Schema.String,
  challenge: Schema.String,
  nonceHash: Schema.String,
  state: Schema.String,
  codeHash: Schema.NullOr(Schema.String),
  attempts: Schema.Number,
  createdAt: Schema.Number,
  expiresAt: Schema.Number,
});

export type PendingRequest = typeof PendingRequest.Type;

export const Grant = Schema.Struct({
  id: Schema.String,
  clientId: Schema.String,
  server: Schema.String,
  createdAt: Schema.Number,
  lastUsed: Schema.NullOr(Schema.Number),
});

export type Grant = typeof Grant.Type;

export const Token = Schema.Struct({
  hash: Schema.String,
  kind: Schema.Literals(["access", "refresh", "static"]),
  grantId: Schema.String,
  server: Schema.String,
  expiresAt: Schema.Number,
  revoked: Schema.Literals([0, 1]),
});

export type Token = typeof Token.Type;

export const StaticToken = Schema.Struct({
  id: Schema.String,
  name: Schema.String,
  server: Schema.String,
  expiresAt: Schema.Number,
});

export type StaticToken = typeof StaticToken.Type;

export const AuditEntry = Schema.Struct({
  id: Schema.Number,
  at: Schema.Number,
  event: Schema.String,
  detail: Schema.String,
});

export type AuditEntry = typeof AuditEntry.Type;

const Exposed = Schema.fromJsonString(Schema.Array(Schema.Struct({ key: Schema.String, overrides: Overrides })));

export type Exposed = typeof Exposed.Type;

const Count = Schema.Struct({ n: Schema.Number });

export const StdioSession = Schema.Struct({
  id: Schema.String,
  server: Schema.String,
  initialize: Schema.String,
  lastUsed: Schema.Number,
});

export type StdioSession = typeof StdioSession.Type;
const Text = Schema.Struct({ value: Schema.NullOr(Schema.String) });

const schema = `
  PRAGMA journal_mode = WAL;
  CREATE TABLE IF NOT EXISTS clients (
    id TEXT PRIMARY KEY,
    redirect_uris TEXT NOT NULL,
    name TEXT,
    created_at INTEGER NOT NULL
  );
  CREATE TABLE IF NOT EXISTS grants (
    id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    server TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used INTEGER
  );
  CREATE TABLE IF NOT EXISTS tokens (
    hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    family TEXT,
    server TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked INTEGER DEFAULT 0
  );
  CREATE TABLE IF NOT EXISTS pending (
    id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    server TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    challenge TEXT NOT NULL,
    nonce_hash TEXT NOT NULL,
    state TEXT DEFAULT '',
    code_hash TEXT DEFAULT NULL,
    attempts INTEGER DEFAULT 0,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
  );
  CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    event TEXT NOT NULL,
    detail TEXT DEFAULT ''
  );
  CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
  );
  CREATE TABLE IF NOT EXISTS dcr_hits (
    ip TEXT NOT NULL,
    at INTEGER NOT NULL
  );
  CREATE TABLE IF NOT EXISTS stdio_sessions (
    id TEXT PRIMARY KEY,
    server TEXT NOT NULL,
    initialize TEXT NOT NULL,
    last_used INTEGER NOT NULL
  );
`;

const pendingColumns = `
  id, client_id AS clientId, server, redirect_uri AS redirectUri, challenge,
  nonce_hash AS nonceHash, state, code_hash AS codeHash, attempts,
  created_at AS createdAt, expires_at AS expiresAt
`;

const tokenColumns = `
  hash, kind, family AS grantId, server, expires_at AS expiresAt, revoked
`;

const grantColumns = `
  id, client_id AS clientId, server, created_at AS createdAt, last_used AS lastUsed
`;

const databaseFiles = (filename: string): ReadonlyArray<string> => [filename, `${filename}-wal`, `${filename}-shm`];

const restrictToOwner = (filename: string): void => {
  closeSync(openSync(filename, "a", 0o600));
  for (const file of databaseFiles(filename)) if (existsSync(file)) chmodSync(file, 0o600);
};

const openDatabase = (filename: string): Database => {
  if (filename !== ":memory:") {
    mkdirSync(Path.dirname(filename), { recursive: true, mode: 0o700 });
    restrictToOwner(filename);
  }
  const db = new Database(filename, { strict: true });
  db.exec(schema);
  const hasName = db.query("SELECT 1 FROM pragma_table_info('clients') WHERE name = 'name'").get() !== null;
  if (!hasName) db.exec("ALTER TABLE clients ADD COLUMN name TEXT");
  return db;
};

export const makeStore = Effect.fn("Store.make")(function* (filename: string) {
  const db = yield* Effect.try({
    try: () => openDatabase(filename),
    catch: (cause) => new StoreError({ message: `Failed to open ${filename}`, cause }),
  });

  const query = <A>(message: string, run: () => A) =>
    Effect.try({ try: run, catch: (cause) => new StoreError({ message, cause }) });

  const one = <A>(schema: Schema.Decoder<A>, sql: string, ...params: Array<SQLQueryBindings>): A | undefined => {
    const row = db.query(sql).get(...params);
    return row === null ? undefined : Schema.decodeUnknownSync(schema)(row);
  };

  const all = <A>(schema: Schema.Decoder<A>, sql: string, ...params: Array<SQLQueryBindings>): Array<A> =>
    db
      .query(sql)
      .all(...params)
      .map((row) => Schema.decodeUnknownSync(schema)(row));

  const insertAudit = db.transaction((event: string, details: ReadonlyArray<string>) => {
    const insert = db.query("INSERT INTO audit (at, event, detail) VALUES ($at, $event, $detail)");
    const at = Date.now();
    for (const detail of details) insert.run({ at, event, detail });
  });

  const audit = {
    write: (event: string, detail = "") => query("Failed to write audit log", () => insertAudit(event, [detail])),
    writeMany: (event: string, details: ReadonlyArray<string>) =>
      query("Failed to write audit log", () => insertAudit(event, details)),
    prune: (olderThan: number) =>
      query("Failed to prune audit log", () => {
        db.query("DELETE FROM audit WHERE at < ?").run(olderThan);
      }),
    list: (limit: number) =>
      query("Failed to read audit log", () =>
        all(AuditEntry, "SELECT id, at, event, detail FROM audit ORDER BY id DESC LIMIT ?", limit),
      ),
    after: (id: number) =>
      query("Failed to read audit log", () =>
        all(AuditEntry, "SELECT id, at, event, detail FROM audit WHERE id > ? ORDER BY id", id),
      ),
  };

  const clients = {
    register: (id: string, redirectUris: ReadonlyArray<string>, name: string | null) =>
      query("Failed to register client", () => {
        db.query("INSERT INTO clients (id, redirect_uris, name, created_at) VALUES ($id, $uris, $name, $at)").run({
          id,
          uris: JSON.stringify(redirectUris),
          name,
          at: Date.now(),
        });
      }),
    name: (id: string) =>
      query("Failed to read client", () => {
        const row = one(Text, "SELECT name AS value FROM clients WHERE id = ?", id);
        if (row?.value) return row.value;
        if (id.startsWith("static:")) return id.slice("static:".length);
        return URL.canParse(id) ? new URL(id).host : "an unnamed client";
      }),
    label: (id: string) =>
      query("Failed to read client", () => {
        const row = one(Text, "SELECT name AS value FROM clients WHERE id = ?", id);
        if (row?.value) return `${row.value} (unverified)`;
        if (id.startsWith("static:")) return id.slice("static:".length);
        return URL.canParse(id) ? new URL(id).host : "an unnamed client";
      }),
    redirectUris: (id: string) =>
      query("Failed to read client", () => {
        const row = one(Text, "SELECT redirect_uris AS value FROM clients WHERE id = ?", id);
        if (!row?.value) return undefined;
        return Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Array(Schema.String)))(row.value);
      }),
    prune: (olderThan: number) =>
      query("Failed to prune clients", () => {
        db.query(
          `
          DELETE FROM clients
          WHERE created_at < ?
            AND id NOT IN (SELECT client_id FROM pending)
            AND id NOT IN (SELECT client_id FROM grants)
        `,
        ).run(olderThan);
      }),
  };

  const pending = {
    create: (request: Omit<PendingRequest, "codeHash" | "attempts" | "createdAt">) =>
      query("Failed to store pending request", () => {
        db.query(
          `
          INSERT INTO pending
            (id, client_id, server, redirect_uri, challenge, nonce_hash, state, created_at, expires_at)
          VALUES
            ($id, $clientId, $server, $redirectUri, $challenge, $nonceHash, $state, $createdAt, $expiresAt)
        `,
        ).run({ ...request, createdAt: Date.now() });
      }),
    get: (id: string) =>
      query("Failed to read pending request", () =>
        one(PendingRequest, `SELECT ${pendingColumns} FROM pending WHERE id = ?`, id),
      ),
    list: () =>
      query("Failed to list pending requests", () =>
        all(PendingRequest, `SELECT ${pendingColumns} FROM pending ORDER BY created_at DESC`),
      ),
    setCode: (id: string, codeHash: string, expiresAt: number) =>
      query("Failed to store approval code", () => {
        db.query(
          `
          UPDATE pending
          SET code_hash = $codeHash, attempts = 0, expires_at = MIN(expires_at, $expiresAt)
          WHERE id = $id
        `,
        ).run({ id, codeHash, expiresAt });
      }),
    recordAttempt: (id: string) =>
      query("Failed to record code attempt", () => {
        db.query("UPDATE pending SET attempts = attempts + 1 WHERE id = ?").run(id);
      }),
    remove: (id: string) =>
      query("Failed to delete pending request", () => {
        db.query("DELETE FROM pending WHERE id = ?").run(id);
      }),
    countActive: (now: number) =>
      query(
        "Failed to count pending requests",
        () => one(Count, "SELECT COUNT(*) AS n FROM pending WHERE expires_at > ?", now)?.n ?? 0,
      ),
    prune: (now: number) =>
      query("Failed to prune pending requests", () => {
        db.query("DELETE FROM pending WHERE expires_at < ?").run(now);
      }),
  };

  const tokens = {
    insert: (token: Omit<Token, "revoked">) =>
      query("Failed to store token", () => {
        db.query(
          `
          INSERT INTO tokens (hash, kind, family, server, expires_at)
          VALUES ($hash, $kind, $grantId, $server, $expiresAt)
        `,
        ).run(token);
      }),
    get: (hash: string) =>
      query("Failed to read token", () => one(Token, `SELECT ${tokenColumns} FROM tokens WHERE hash = ?`, hash)),
    listStatic: () =>
      query("Failed to list static tokens", () =>
        all(
          StaticToken,
          `
          SELECT g.id, substr(g.client_id, 8) AS name, g.server, t.expires_at AS expiresAt
          FROM grants g JOIN tokens t ON t.family = g.id
          WHERE t.kind = 'static' AND t.revoked = 0
          ORDER BY g.created_at DESC
          `,
        ),
      ),
    revoke: (hash: string) =>
      query("Failed to revoke token", () => {
        db.query("UPDATE tokens SET revoked = 1 WHERE hash = ?").run(hash);
      }),
    claim: (hash: string) =>
      query(
        "Failed to rotate token",
        () => db.query("UPDATE tokens SET revoked = 1 WHERE hash = ? AND revoked = 0").run(hash).changes === 1,
      ),
    prune: (now: number) =>
      query("Failed to prune tokens", () => {
        db.query("DELETE FROM tokens WHERE expires_at < ? OR (revoked = 1 AND kind != 'refresh')").run(now);
      }),
    revokeGrant: (grantId: string) =>
      query("Failed to revoke grant tokens", () => {
        db.query("UPDATE tokens SET revoked = 1 WHERE family = ?").run(grantId);
      }),
    revokeAll: () =>
      query("Failed to revoke tokens", () => {
        db.query("UPDATE tokens SET revoked = 1").run();
      }),
  };

  const grants = {
    prune: (createdBefore: number) =>
      query("Failed to prune grants", () => {
        db.query("DELETE FROM grants WHERE created_at < ? AND id NOT IN (SELECT family FROM tokens)").run(
          createdBefore,
        );
      }),
    insert: (grant: Omit<Grant, "createdAt" | "lastUsed">) =>
      query("Failed to record grant", () => {
        db.query("INSERT INTO grants (id, client_id, server, created_at) VALUES ($id, $clientId, $server, $at)").run({
          ...grant,
          at: Date.now(),
        });
      }),
    list: () =>
      query("Failed to list grants", () => all(Grant, `SELECT ${grantColumns} FROM grants ORDER BY created_at DESC`)),
    count: () => query("Failed to count grants", () => one(Count, "SELECT COUNT(*) AS n FROM grants")?.n ?? 0),
    clientOf: (id: string) =>
      query("Failed to read grant", () => one(Text, "SELECT client_id AS value FROM grants WHERE id = ?", id)?.value),
    touch: (id: string) =>
      query("Failed to update grant", () => {
        db.query("UPDATE grants SET last_used = ? WHERE id = ?").run(Date.now(), id);
      }),
    remove: (id: string) =>
      query("Failed to delete grant", () => db.query("DELETE FROM grants WHERE id = ?").run(id).changes > 0),
    removeAll: () =>
      query("Failed to delete grants", () => {
        db.query("DELETE FROM grants").run();
      }),
  };

  const registrations = {
    record: (ip: string) =>
      query("Failed to record registration", () => {
        db.query("INSERT INTO dcr_hits (ip, at) VALUES (?, ?)").run(ip, Date.now());
      }),
    countSince: (ip: string, since: number) =>
      query("Failed to count registrations", () => {
        db.query("DELETE FROM dcr_hits WHERE at < ?").run(since);
        return {
          fromIp: one(Count, "SELECT COUNT(*) AS n FROM dcr_hits WHERE ip = ?", ip)?.n ?? 0,
          total: one(Count, "SELECT COUNT(*) AS n FROM dcr_hits")?.n ?? 0,
        };
      }),
  };

  const randomPort = (base: number): number => base + Math.floor(Math.random() * 1000);

  const ports = () =>
    query("Failed to read ports", () => {
      const stored = (key: string, base: number): number => {
        const row = one(Text, "SELECT value FROM meta WHERE key = ?", key);
        if (row?.value) return Number(row.value);
        const port = randomPort(base);
        db.query("INSERT INTO meta (key, value) VALUES (?, ?)").run(key, String(port));
        return port;
      };
      return { gatewayPort: stored("gatewayPort", 18080), approvalPort: stored("approvalPort", 19080) };
    });

  const touchSessions = db.transaction((sessions: ReadonlyArray<{ id: string; lastUsed: number }>) => {
    const touch = db.query("UPDATE stdio_sessions SET last_used = $lastUsed WHERE id = $id");
    for (const session of sessions) touch.run(session);
  });

  const stdioSessions = {
    save: (session: StdioSession) =>
      query("Failed to save session", () => {
        db.query(
          "INSERT OR REPLACE INTO stdio_sessions (id, server, initialize, last_used) VALUES ($id, $server, $initialize, $lastUsed)",
        ).run(session);
      }),
    get: (id: string, server: string) =>
      query("Failed to read session", () =>
        one(
          StdioSession,
          "SELECT id, server, initialize, last_used AS lastUsed FROM stdio_sessions WHERE id = ? AND server = ?",
          id,
          server,
        ),
      ),
    touch: (sessions: ReadonlyArray<{ id: string; lastUsed: number }>) =>
      query("Failed to update sessions", () => touchSessions(sessions)),
    remove: (id: string) =>
      query("Failed to remove session", () => {
        db.query("DELETE FROM stdio_sessions WHERE id = ?").run(id);
      }),
    prune: (server: string, usedBefore: number) =>
      query("Failed to prune sessions", () => {
        db.query("DELETE FROM stdio_sessions WHERE server = ? AND last_used < ?").run(server, usedBefore);
      }),
  };

  const sessionKey = () =>
    query("Failed to read the session key", () => {
      db.query("INSERT OR IGNORE INTO meta (key, value) VALUES ('sessionKey', ?)").run(randomToken());
      const row = one(Text, "SELECT value FROM meta WHERE key = 'sessionKey'");
      if (!row?.value) throw new Error("The session key is missing");
      return row.value;
    });

  const setPort = (key: "gatewayPort" | "approvalPort", port: number) =>
    query("Failed to save port", () => {
      db.query("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)").run(key, String(port));
    });

  const exposed = {
    get: () =>
      query("Failed to read exposed servers", (): Exposed => {
        const row = one(Text, "SELECT value FROM meta WHERE key = 'exposed'");
        return row?.value ? Schema.decodeUnknownSync(Exposed)(row.value) : [];
      }),
    set: (list: Exposed) =>
      query("Failed to save exposed servers", () => {
        db.query("INSERT OR REPLACE INTO meta (key, value) VALUES ('exposed', ?)").run(JSON.stringify(list));
      }),
  };

  return { audit, clients, pending, tokens, grants, registrations, ports, setPort, sessionKey, stdioSessions, exposed };
});

export type StoreApi = Effect.Success<ReturnType<typeof makeStore>>;

export class Store extends ServiceMap.Service<Store, StoreApi>()("porchlight/Store") {
  static layer = (filename = Path.join(stateDir, "state.db")) => Layer.effect(Store, makeStore(filename));
}
