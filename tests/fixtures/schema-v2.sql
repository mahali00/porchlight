
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
    detail TEXT DEFAULT '',
    client TEXT,
    app TEXT,
    subject TEXT,
    source TEXT,
    outcome TEXT,
    duration_ms INTEGER,
    input TEXT
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

PRAGMA user_version = 2;
