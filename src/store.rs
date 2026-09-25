use crate::config::in_state_dir;
use crate::core::now_ms;
use crate::crypto::random_token;
use rusqlite::{Connection, OptionalExtension, Row, named_params, params};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StoreError {
    pub message: String,
    #[source]
    pub cause: Option<rusqlite::Error>,
}

fn failed(message: &str) -> impl FnOnce(rusqlite::Error) -> StoreError + '_ {
    move |cause| StoreError { message: message.to_owned(), cause: Some(cause) }
}

const SCHEMA: &str = "
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
";

const SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRequest {
    pub id: String,
    pub client_id: String,
    pub server: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub nonce_hash: String,
    pub state: String,
    pub code_hash: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Clone, Debug)]
pub struct NewPending {
    pub id: String,
    pub client_id: String,
    pub server: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub nonce_hash: String,
    pub state: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub id: String,
    pub client_id: String,
    pub server: String,
    pub created_at: i64,
    pub last_used: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Refresh,
    Static,
}

impl TokenKind {
    fn name(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Refresh => "refresh",
            Self::Static => "static",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "access" => Some(Self::Access),
            "refresh" => Some(Self::Refresh),
            "static" => Some(Self::Static),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub hash: String,
    pub kind: TokenKind,
    pub grant_id: String,
    pub server: String,
    pub expires_at: i64,
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticToken {
    pub id: String,
    pub name: String,
    pub server: String,
    pub expires_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Error,
    Refused,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Refused => "refused",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "ok" => Some(Self::Ok),
            "error" => Some(Self::Error),
            "refused" => Some(Self::Refused),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub event: String,
    pub detail: String,
    pub client: Option<String>,
    pub app: Option<String>,
    pub subject: Option<String>,
    pub source: Option<String>,
    pub outcome: Option<Outcome>,
    pub duration_ms: Option<i64>,
    pub input: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AuditFields {
    pub detail: Option<String>,
    pub client: Option<String>,
    pub app: Option<String>,
    pub subject: Option<String>,
    pub source: Option<String>,
    pub outcome: Option<Outcome>,
    pub duration_ms: Option<i64>,
    pub input: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StdioSession {
    pub id: String,
    pub server: String,
    pub initialize: String,
    pub last_used: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub gateway: u16,
    pub approval: u16,
}

#[derive(Clone, Copy, Debug)]
pub enum PortKey {
    Gateway,
    Approval,
}

impl PortKey {
    fn name(self) -> &'static str {
        match self {
            Self::Gateway => "gatewayPort",
            Self::Approval => "approvalPort",
        }
    }
}

pub struct Registrations {
    pub from_ip: i64,
    pub total: i64,
}

const PENDING_COLUMNS: &str =
    "id, client_id, server, redirect_uri, challenge, nonce_hash, state, code_hash, attempts, created_at, expires_at";

const TOKEN_COLUMNS: &str = "hash, kind, family, server, expires_at, revoked";

const GRANT_COLUMNS: &str = "id, client_id, server, created_at, last_used";

const AUDIT_COLUMNS: &str = "id, at, event, detail, client, app, subject, source, outcome, duration_ms, input";

fn pending_row(row: &Row) -> rusqlite::Result<PendingRequest> {
    Ok(PendingRequest {
        id: row.get(0)?,
        client_id: row.get(1)?,
        server: row.get(2)?,
        redirect_uri: row.get(3)?,
        challenge: row.get(4)?,
        nonce_hash: row.get(5)?,
        state: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        code_hash: row.get(7)?,
        attempts: row.get::<_, Option<i64>>(8)?.unwrap_or_default(),
        created_at: row.get(9)?,
        expires_at: row.get(10)?,
    })
}

fn token_row(row: &Row) -> rusqlite::Result<Option<Token>> {
    let kind: String = row.get(1)?;

    TokenKind::parse(&kind)
        .map(|kind| -> rusqlite::Result<Token> {
            Ok(Token {
                hash: row.get(0)?,
                kind,
                grant_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                server: row.get(3)?,
                expires_at: row.get(4)?,
                revoked: row.get::<_, Option<i64>>(5)?.unwrap_or_default() != 0,
            })
        })
        .transpose()
}

fn grant_row(row: &Row) -> rusqlite::Result<Grant> {
    Ok(Grant {
        id: row.get(0)?,
        client_id: row.get(1)?,
        server: row.get(2)?,
        created_at: row.get(3)?,
        last_used: row.get(4)?,
    })
}

fn audit_row(row: &Row) -> rusqlite::Result<AuditEntry> {
    Ok(AuditEntry {
        id: row.get(0)?,
        at: row.get(1)?,
        event: row.get(2)?,
        detail: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        client: row.get(4)?,
        app: row.get(5)?,
        subject: row.get(6)?,
        source: row.get(7)?,
        outcome: row.get::<_, Option<String>>(8)?.as_deref().and_then(Outcome::parse),
        duration_ms: row.get(9)?,
        input: row.get(10)?,
    })
}

fn restrict_to_owner(file: &Path) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }

    OpenOptions::new().create(true).append(true).mode(0o600).open(file)?;

    for suffix in ["", "-wal", "-shm"] {
        let path = Path::new(&format!("{}{suffix}", file.display())).to_path_buf();

        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
    }

    Ok(())
}

fn prepare(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version != SCHEMA_VERSION {
        let tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name != 'sqlite_sequence'")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        for table in tables {
            connection.execute_batch(&format!("DROP TABLE IF EXISTS \"{}\"", table.replace('"', "\"\"")))?;
        }

        connection.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    }

    connection.execute_batch(SCHEMA)
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(file: &Path) -> Result<Self, StoreError> {
        let message = format!("Failed to open {}", file.display());
        restrict_to_owner(file).map_err(|_| StoreError { message: message.clone(), cause: None })?;
        let connection = Connection::open(file).map_err(failed(&message))?;
        prepare(&connection).map_err(failed(&message))?;

        Ok(Self { db: Arc::new(Mutex::new(connection)) })
    }

    pub fn open_default() -> Result<Self, StoreError> {
        Self::open(&in_state_dir("state.db"))
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory().map_err(failed("Failed to open the database"))?;
        prepare(&connection).map_err(failed("Failed to open the database"))?;

        Ok(Self { db: Arc::new(Mutex::new(connection)) })
    }

    fn with<T>(
        &self,
        message: &str,
        run: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T, StoreError> {
        let mut connection = self.db.lock().map_err(|_| StoreError { message: message.to_owned(), cause: None })?;

        run(&mut connection).map_err(failed(message))
    }

    pub fn audit_write(&self, event: &str, detail: &str) -> Result<(), StoreError> {
        self.audit_record(event, AuditFields { detail: Some(detail.to_owned()), ..AuditFields::default() })
    }

    pub fn audit_record(&self, event: &str, fields: AuditFields) -> Result<(), StoreError> {
        self.with("Failed to write audit log", |db| {
            db.execute(
                "INSERT INTO audit (at, event, detail, client, app, subject, source, outcome, duration_ms, input)
                 VALUES (:at, :event, :detail, :client, :app, :subject, :source, :outcome, :duration, :input)",
                named_params! {
                    ":at": now_ms(),
                    ":event": event,
                    ":detail": fields.detail.unwrap_or_default(),
                    ":client": fields.client,
                    ":app": fields.app,
                    ":subject": fields.subject,
                    ":source": fields.source,
                    ":outcome": fields.outcome.map(Outcome::name),
                    ":duration": fields.duration_ms,
                    ":input": fields.input,
                },
            )
            .map(drop)
        })
    }

    pub fn audit_prune(&self, older_than: i64) -> Result<(), StoreError> {
        self.with("Failed to prune audit log", |db| {
            db.execute("DELETE FROM audit WHERE at < ?", [older_than]).map(drop)
        })
    }

    pub fn audit_list(&self, limit: i64) -> Result<Vec<AuditEntry>, StoreError> {
        self.with("Failed to read audit log", |db| {
            db.prepare(&format!("SELECT {AUDIT_COLUMNS} FROM audit ORDER BY id DESC LIMIT ?"))?
                .query_map([limit], audit_row)?
                .collect()
        })
    }

    pub fn audit_after(&self, id: i64) -> Result<Vec<AuditEntry>, StoreError> {
        self.with("Failed to read audit log", |db| {
            db.prepare(&format!("SELECT {AUDIT_COLUMNS} FROM audit WHERE id > ? ORDER BY id"))?
                .query_map([id], audit_row)?
                .collect()
        })
    }

    fn display_name(&self, id: &str, unverified: &str) -> Result<String, StoreError> {
        let name: Option<String> = self.with("Failed to read client", |db| {
            db.query_row("SELECT name FROM clients WHERE id = ?", [id], |row| row.get(0))
                .optional()
                .map(Option::flatten)
        })?;

        if let Some(name) = name.filter(|name| !name.is_empty()) {
            return Ok(format!("{name}{unverified}"));
        }

        if let Some(label) = id.strip_prefix("static:") {
            return Ok(label.to_owned());
        }

        Ok(url::Url::parse(id)
            .ok()
            .and_then(|url| {
                url.host_str().map(|host| match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_owned(),
                })
            })
            .unwrap_or_else(|| "an unnamed client".to_owned()))
    }

    pub fn client_register(&self, id: &str, redirect_uris: &[String], name: Option<&str>) -> Result<(), StoreError> {
        let uris = serde_json::to_string(redirect_uris).unwrap_or_else(|_| "[]".to_owned());

        self.with("Failed to register client", |db| {
            db.execute(
                "INSERT INTO clients (id, redirect_uris, name, created_at) VALUES (?, ?, ?, ?)",
                params![id, uris, name, now_ms()],
            )
            .map(drop)
        })
    }

    pub fn client_name(&self, id: &str) -> Result<String, StoreError> {
        self.display_name(id, "")
    }

    pub fn client_label(&self, id: &str) -> Result<String, StoreError> {
        self.display_name(id, " (unverified)")
    }

    pub fn client_redirect_uris(&self, id: &str) -> Result<Option<Vec<String>>, StoreError> {
        let stored: Option<String> = self.with("Failed to read client", |db| {
            db.query_row("SELECT redirect_uris FROM clients WHERE id = ?", [id], |row| row.get(0)).optional()
        })?;

        Ok(stored.filter(|value| !value.is_empty()).and_then(|value| serde_json::from_str(&value).ok()))
    }

    pub fn clients_prune(&self, older_than: i64) -> Result<(), StoreError> {
        self.with("Failed to prune clients", |db| {
            db.execute(
                "DELETE FROM clients WHERE created_at < ?
                   AND id NOT IN (SELECT client_id FROM pending)
                   AND id NOT IN (SELECT client_id FROM grants)",
                [older_than],
            )
            .map(drop)
        })
    }

    pub fn pending_create(&self, request: &NewPending) -> Result<(), StoreError> {
        self.with("Failed to store pending request", |db| {
            db.execute(
                "INSERT INTO pending (id, client_id, server, redirect_uri, challenge, nonce_hash, state, created_at, expires_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    request.id,
                    request.client_id,
                    request.server,
                    request.redirect_uri,
                    request.challenge,
                    request.nonce_hash,
                    request.state,
                    now_ms(),
                    request.expires_at
                ],
            )
            .map(drop)
        })
    }

    pub fn pending_get(&self, id: &str) -> Result<Option<PendingRequest>, StoreError> {
        self.with("Failed to read pending request", |db| {
            db.query_row(&format!("SELECT {PENDING_COLUMNS} FROM pending WHERE id = ?"), [id], pending_row).optional()
        })
    }

    pub fn pending_list(&self) -> Result<Vec<PendingRequest>, StoreError> {
        self.with("Failed to list pending requests", |db| {
            db.prepare(&format!("SELECT {PENDING_COLUMNS} FROM pending ORDER BY created_at DESC"))?
                .query_map([], pending_row)?
                .collect()
        })
    }

    pub fn pending_set_code(&self, id: &str, code_hash: &str, expires_at: i64) -> Result<(), StoreError> {
        self.with("Failed to store approval code", |db| {
            db.execute(
                "UPDATE pending SET code_hash = ?, attempts = 0, expires_at = MIN(expires_at, ?) WHERE id = ?",
                params![code_hash, expires_at, id],
            )
            .map(drop)
        })
    }

    pub fn pending_record_attempt(&self, id: &str) -> Result<(), StoreError> {
        self.with("Failed to record code attempt", |db| {
            db.execute("UPDATE pending SET attempts = attempts + 1 WHERE id = ?", [id]).map(drop)
        })
    }

    pub fn pending_remove(&self, id: &str) -> Result<(), StoreError> {
        self.with("Failed to delete pending request", |db| {
            db.execute("DELETE FROM pending WHERE id = ?", [id]).map(drop)
        })
    }

    pub fn pending_remove_server(&self, server: &str) -> Result<(), StoreError> {
        self.with("Failed to delete pending requests", |db| {
            db.execute("DELETE FROM pending WHERE server = ?", [server]).map(drop)
        })
    }

    pub fn pending_count_active(&self, now: i64) -> Result<i64, StoreError> {
        self.with("Failed to count pending requests", |db| {
            db.query_row("SELECT COUNT(*) FROM pending WHERE expires_at > ?", [now], |row| row.get(0))
        })
    }

    pub fn pending_prune(&self, now: i64) -> Result<(), StoreError> {
        self.with("Failed to prune pending requests", |db| {
            db.execute("DELETE FROM pending WHERE expires_at < ?", [now]).map(drop)
        })
    }

    pub fn token_insert(
        &self,
        hash: &str,
        kind: TokenKind,
        grant_id: &str,
        server: &str,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        self.with("Failed to store token", |db| {
            db.execute(
                "INSERT INTO tokens (hash, kind, family, server, expires_at) VALUES (?, ?, ?, ?, ?)",
                params![hash, kind.name(), grant_id, server, expires_at],
            )
            .map(drop)
        })
    }

    pub fn token_get(&self, hash: &str) -> Result<Option<Token>, StoreError> {
        self.with("Failed to read token", |db| {
            db.query_row(&format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE hash = ?"), [hash], token_row)
                .optional()
                .map(Option::flatten)
        })
    }

    pub fn tokens_list_static(&self) -> Result<Vec<StaticToken>, StoreError> {
        self.with("Failed to list static tokens", |db| {
            db.prepare(
                "SELECT g.id, substr(g.client_id, 8), g.server, t.expires_at
                 FROM grants g JOIN tokens t ON t.family = g.id
                 WHERE t.kind = 'static' AND t.revoked = 0
                 ORDER BY g.created_at DESC",
            )?
            .query_map([], |row| {
                Ok(StaticToken { id: row.get(0)?, name: row.get(1)?, server: row.get(2)?, expires_at: row.get(3)? })
            })?
            .collect()
        })
    }

    pub fn token_revoke(&self, hash: &str) -> Result<(), StoreError> {
        self.with("Failed to revoke token", |db| {
            db.execute("UPDATE tokens SET revoked = 1 WHERE hash = ?", [hash]).map(drop)
        })
    }

    pub fn token_claim(&self, hash: &str) -> Result<bool, StoreError> {
        self.with("Failed to rotate token", |db| {
            db.execute("UPDATE tokens SET revoked = 1 WHERE hash = ? AND revoked = 0", [hash])
                .map(|changed| changed == 1)
        })
    }

    pub fn tokens_prune(&self, now: i64) -> Result<(), StoreError> {
        self.with("Failed to prune tokens", |db| {
            db.execute("DELETE FROM tokens WHERE expires_at < ? OR (revoked = 1 AND kind != 'refresh')", [now])
                .map(drop)
        })
    }

    pub fn tokens_revoke_grant(&self, grant_id: &str) -> Result<(), StoreError> {
        self.with("Failed to revoke grant tokens", |db| {
            db.execute("UPDATE tokens SET revoked = 1 WHERE family = ?", [grant_id]).map(drop)
        })
    }

    pub fn tokens_revoke_all(&self) -> Result<(), StoreError> {
        self.with("Failed to revoke tokens", |db| db.execute("UPDATE tokens SET revoked = 1", []).map(drop))
    }

    pub fn tokens_revoke_server(&self, server: &str) -> Result<(), StoreError> {
        self.with("Failed to revoke tokens", |db| {
            db.execute("UPDATE tokens SET revoked = 1 WHERE server = ?", [server]).map(drop)
        })
    }

    pub fn grants_prune(&self, created_before: i64) -> Result<(), StoreError> {
        self.with("Failed to prune grants", |db| {
            db.execute(
                "DELETE FROM grants WHERE created_at < ? AND id NOT IN (SELECT family FROM tokens WHERE family IS NOT NULL)",
                [created_before],
            )
            .map(drop)
        })
    }

    pub fn grant_insert(&self, id: &str, client_id: &str, server: &str) -> Result<(), StoreError> {
        self.with("Failed to record grant", |db| {
            db.execute(
                "INSERT INTO grants (id, client_id, server, created_at) VALUES (?, ?, ?, ?)",
                params![id, client_id, server, now_ms()],
            )
            .map(drop)
        })
    }

    pub fn grants_list(&self) -> Result<Vec<Grant>, StoreError> {
        self.with("Failed to list grants", |db| {
            db.prepare(&format!("SELECT {GRANT_COLUMNS} FROM grants ORDER BY created_at DESC"))?
                .query_map([], grant_row)?
                .collect()
        })
    }

    pub fn grants_count(&self) -> Result<i64, StoreError> {
        self.with("Failed to count grants", |db| db.query_row("SELECT COUNT(*) FROM grants", [], |row| row.get(0)))
    }

    pub fn grant_client(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with("Failed to read grant", |db| {
            db.query_row("SELECT client_id FROM grants WHERE id = ?", [id], |row| row.get(0)).optional()
        })
    }

    pub fn grant_touch(&self, id: &str) -> Result<(), StoreError> {
        self.with("Failed to update grant", |db| {
            db.execute("UPDATE grants SET last_used = ? WHERE id = ?", params![now_ms(), id]).map(drop)
        })
    }

    pub fn grant_remove(&self, id: &str) -> Result<bool, StoreError> {
        self.with("Failed to delete grant", |db| db.execute("DELETE FROM grants WHERE id = ?", [id]).map(|n| n > 0))
    }

    pub fn grants_remove_all(&self) -> Result<(), StoreError> {
        self.with("Failed to delete grants", |db| db.execute("DELETE FROM grants", []).map(drop))
    }

    pub fn grants_remove_server(&self, server: &str) -> Result<usize, StoreError> {
        self.with("Failed to delete grants", |db| db.execute("DELETE FROM grants WHERE server = ?", [server]))
    }

    pub fn registration_record(&self, ip: &str) -> Result<(), StoreError> {
        self.with("Failed to record registration", |db| {
            db.execute("INSERT INTO dcr_hits (ip, at) VALUES (?, ?)", params![ip, now_ms()]).map(drop)
        })
    }

    pub fn registrations_since(&self, ip: &str, since: i64) -> Result<Registrations, StoreError> {
        self.with("Failed to count registrations", |db| {
            db.execute("DELETE FROM dcr_hits WHERE at < ?", [since])?;
            let from_ip = db.query_row("SELECT COUNT(*) FROM dcr_hits WHERE ip = ?", [ip], |row| row.get(0))?;
            let total = db.query_row("SELECT COUNT(*) FROM dcr_hits", [], |row| row.get(0))?;

            Ok(Registrations { from_ip, total })
        })
    }

    fn meta_value(db: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
        db.query_row("SELECT value FROM meta WHERE key = ?", [key], |row| row.get(0)).optional()
    }

    pub fn ports(&self) -> Result<Ports, StoreError> {
        self.with("Failed to read ports", |db| {
            let stored = |key: PortKey, base: u16| -> rusqlite::Result<u16> {
                if let Some(port) = Self::meta_value(db, key.name())?.and_then(|value| value.parse().ok()) {
                    return Ok(port);
                }

                let port = base + rand::random_range(0..1000);
                db.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
                    params![key.name(), port.to_string()],
                )?;

                Ok(port)
            };

            Ok(Ports { gateway: stored(PortKey::Gateway, 18080)?, approval: stored(PortKey::Approval, 19080)? })
        })
    }

    pub fn set_port(&self, key: PortKey, port: u16) -> Result<(), StoreError> {
        self.meta_set(key.name(), &port.to_string())
    }

    pub fn session_key(&self) -> Result<String, StoreError> {
        self.with("Failed to read the session key", |db| {
            db.execute("INSERT OR IGNORE INTO meta (key, value) VALUES ('sessionKey', ?)", [random_token(32)])?;
            db.query_row("SELECT value FROM meta WHERE key = 'sessionKey'", [], |row| row.get(0))
        })
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.with("Failed to read state", |db| Self::meta_value(db, key))
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.with("Failed to save state", |db| {
            db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)", params![key, value]).map(drop)
        })
    }

    pub fn stdio_session_save(&self, session: &StdioSession) -> Result<(), StoreError> {
        self.with("Failed to save session", |db| {
            db.execute(
                "INSERT OR REPLACE INTO stdio_sessions (id, server, initialize, last_used) VALUES (?, ?, ?, ?)",
                params![session.id, session.server, session.initialize, session.last_used],
            )
            .map(drop)
        })
    }

    pub fn stdio_session_get(&self, id: &str, server: &str) -> Result<Option<StdioSession>, StoreError> {
        self.with("Failed to read session", |db| {
            db.query_row(
                "SELECT id, server, initialize, last_used FROM stdio_sessions WHERE id = ? AND server = ?",
                [id, server],
                |row| {
                    Ok(StdioSession {
                        id: row.get(0)?,
                        server: row.get(1)?,
                        initialize: row.get(2)?,
                        last_used: row.get(3)?,
                    })
                },
            )
            .optional()
        })
    }

    pub fn stdio_sessions_touch(&self, sessions: &[(String, i64)]) -> Result<(), StoreError> {
        self.with("Failed to update sessions", |db| {
            let transaction = db.transaction()?;

            for (id, last_used) in sessions {
                transaction.execute("UPDATE stdio_sessions SET last_used = ? WHERE id = ?", params![last_used, id])?;
            }

            transaction.commit()
        })
    }

    pub fn stdio_session_remove(&self, id: &str) -> Result<(), StoreError> {
        self.with("Failed to remove session", |db| {
            db.execute("DELETE FROM stdio_sessions WHERE id = ?", [id]).map(drop)
        })
    }

    pub fn stdio_sessions_remove_server(&self, server: &str) -> Result<(), StoreError> {
        self.with("Failed to remove sessions", |db| {
            db.execute("DELETE FROM stdio_sessions WHERE server = ?", [server]).map(drop)
        })
    }

    pub fn stdio_sessions_prune(&self, server: &str, used_before: i64) -> Result<(), StoreError> {
        self.with("Failed to prune sessions", |db| {
            db.execute("DELETE FROM stdio_sessions WHERE server = ? AND last_used < ?", params![server, used_before])
                .map(drop)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_database_and_its_wal_files_are_readable_only_by_the_owner() {
        let dir = std::env::temp_dir().join(format!("porchlight-db-{}", random_token(6)));
        let file = dir.join("state.db");
        let store = Store::open(&file).unwrap();
        store.audit_write("test", "").unwrap();
        let modes: Vec<u32> = ["", "-wal", "-shm"]
            .iter()
            .map(|suffix| fs::metadata(format!("{}{suffix}", file.display())).unwrap().permissions().mode() & 0o777)
            .collect();

        assert_eq!(modes, [0o600, 0o600, 0o600]);
    }
}
