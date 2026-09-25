use crate::core::{link_path, now_ms};
use crate::crypto::{approval_code, normalize_code, pkce_challenge, random_token, sha256};
use crate::store::{AuditFields, NewPending, Store, StoreError, TokenKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const MINUTE: i64 = 60_000;

const HOUR: i64 = 60 * MINUTE;

const DAY: i64 = 24 * HOUR;

const PENDING_TTL: i64 = 10 * MINUTE;

const TICKET_TTL: i64 = 2 * MINUTE;

const DEVICE_CODE_TTL: i64 = 5 * MINUTE;

const AUTHORIZATION_CODE_TTL: i64 = MINUTE;

const ACCESS_TOKEN_TTL: i64 = HOUR;

const REFRESH_TOKEN_TTL: i64 = 30 * DAY;

const MAX_CODE_ATTEMPTS: i64 = 5;

const MAX_REGISTRATIONS_PER_HOUR: i64 = 30;

const MAX_PENDING_REQUESTS: i64 = 20;

pub const ACCESS_TOKEN_SECONDS: i64 = ACCESS_TOKEN_TTL / 1000;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Refused(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct AuthorizationRequest {
    pub id: String,
    pub client_id: String,
    pub server: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub state: String,
}

pub struct Approval {
    pub code: String,
    pub redirect_uri: String,
    pub state: String,
}

pub struct TokenPair {
    pub access: String,
    pub refresh: String,
}

struct Ticket {
    request_id: String,
    expires_at: i64,
}

struct AuthorizationCode {
    client_id: String,
    server: String,
    challenge: String,
    expires_at: i64,
}

type Streams = Arc<Mutex<HashMap<String, Vec<(u64, CancellationToken)>>>>;

pub struct GrantStream {
    pub token: CancellationToken,
    id: u64,
    grant: String,
    streams: Streams,
}

impl Drop for GrantStream {
    fn drop(&mut self) {
        if let Ok(mut streams) = self.streams.lock()
            && let Some(active) = streams.get_mut(&self.grant)
        {
            active.retain(|(id, _)| *id != self.id);

            if active.is_empty() {
                streams.remove(&self.grant);
            }
        }
    }
}

fn refused<T>(message: &'static str) -> Result<T, AuthError> {
    Err(AuthError::Refused(message))
}

fn resource_matches(resource: &str, server: &str) -> bool {
    if resource.is_empty() {
        return true;
    }

    url::Url::parse(resource).is_ok_and(|url| {
        let path = url.path().trim_end_matches('/');
        path.is_empty() || path == link_path(server)
    })
}

#[derive(Clone)]
pub struct Auth {
    store: Store,
    tickets: Arc<Mutex<HashMap<String, Ticket>>>,
    codes: Arc<Mutex<HashMap<String, AuthorizationCode>>>,
    code_lock: Arc<Mutex<()>>,
    streams: Streams,
    next_stream: Arc<AtomicU64>,
}

impl Auth {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            tickets: Arc::default(),
            codes: Arc::default(),
            code_lock: Arc::default(),
            streams: Arc::default(),
            next_stream: Arc::default(),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn close_grant_streams(&self, grants: &[String]) {
        let closed: Vec<CancellationToken> = self
            .streams
            .lock()
            .map(|mut streams| {
                grants.iter().filter_map(|grant| streams.remove(grant)).flatten().map(|(_, token)| token).collect()
            })
            .unwrap_or_default();

        closed.iter().for_each(CancellationToken::cancel);
    }

    pub fn open_grant_stream(&self, grant: &str) -> GrantStream {
        let token = CancellationToken::new();
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);

        if let Ok(mut streams) = self.streams.lock() {
            streams.entry(grant.to_owned()).or_default().push((id, token.clone()));
        }

        GrantStream { token, id, grant: grant.to_owned(), streams: self.streams.clone() }
    }

    fn issue_tokens(&self, grant_id: &str, server: &str) -> Result<TokenPair, AuthError> {
        let access = random_token(32);
        let refresh = random_token(32);
        let now = now_ms();
        self.store.token_insert(&sha256(&access), TokenKind::Access, grant_id, server, now + ACCESS_TOKEN_TTL)?;
        self.store.token_insert(&sha256(&refresh), TokenKind::Refresh, grant_id, server, now + REFRESH_TOKEN_TTL)?;

        Ok(TokenPair { access, refresh })
    }

    fn approve(&self, request_id: &str, cookie_nonce: Option<&str>) -> Result<Approval, AuthError> {
        let now = now_ms();
        let Some(request) = self.store.pending_get(request_id)?.filter(|request| request.expires_at >= now) else {
            return refused("Request expired");
        };

        if cookie_nonce.is_none_or(|nonce| sha256(nonce) != request.nonce_hash) {
            return refused("Cookie mismatch");
        }

        let code = random_token(32);

        let stored = self.codes.lock().map(|mut codes| {
            codes.insert(
                code.clone(),
                AuthorizationCode {
                    client_id: request.client_id.clone(),
                    server: request.server.clone(),
                    challenge: request.challenge.clone(),
                    expires_at: now + AUTHORIZATION_CODE_TTL,
                },
            );
        });

        if stored.is_err() {
            eprintln!("Couldn't keep an authorization code: the code store is unusable after an earlier crash");
            return refused("Code store unavailable");
        }

        self.store.pending_remove(request_id)?;
        self.store.audit_write("authz.completed", &format!("{} -> {}", request.client_id, request.server))?;

        Ok(Approval { code, redirect_uri: request.redirect_uri, state: request.state })
    }

    pub fn begin(&self, request: AuthorizationRequest, source: &str) -> Result<String, AuthError> {
        let now = now_ms();

        if self.store.pending_count_active(now)? >= MAX_PENDING_REQUESTS {
            return refused("Too many pending requests");
        }

        let nonce = random_token(16);
        let detail = format!("{} -> {}", request.client_id, request.server);
        self.store.pending_create(&NewPending {
            id: request.id,
            client_id: request.client_id.clone(),
            server: request.server.clone(),
            redirect_uri: request.redirect_uri,
            challenge: request.challenge,
            nonce_hash: sha256(&nonce),
            state: request.state,
            expires_at: now + PENDING_TTL,
        })?;
        self.store.audit_record(
            "authz.created",
            AuditFields {
                detail: Some(detail),
                client: Some(request.client_id),
                app: Some(request.server),
                source: Some(source.to_owned()),
                ..AuditFields::default()
            },
        )?;

        Ok(nonce)
    }

    pub fn issue_ticket(&self, request_id: &str) -> String {
        let ticket = random_token(32);

        let stored = self.tickets.lock().map(|mut tickets| {
            tickets.insert(
                ticket.clone(),
                Ticket { request_id: request_id.to_owned(), expires_at: now_ms() + TICKET_TTL },
            );
        });

        if stored.is_err() {
            eprintln!("Couldn't keep an approval ticket: the ticket store is unusable after an earlier crash");
        }

        ticket
    }

    pub fn approve_with_ticket(
        &self,
        request_id: &str,
        ticket: &str,
        cookie_nonce: Option<&str>,
    ) -> Result<Approval, AuthError> {
        let issued = self.tickets.lock().ok().and_then(|mut tickets| tickets.remove(ticket));

        if issued.is_none_or(|issued| issued.request_id != request_id || issued.expires_at < now_ms()) {
            self.store.audit_write("authz.complete_failed", request_id)?;
            return refused("Invalid ticket");
        }

        self.approve(request_id, cookie_nonce)
    }

    pub fn issue_device_code(&self, request_id: &str) -> Result<String, AuthError> {
        let code = approval_code();
        self.store.pending_set_code(request_id, &sha256(&code), now_ms() + DEVICE_CODE_TTL)?;
        self.store.audit_write("authz.code_issued", request_id)?;

        Ok(code)
    }

    pub fn approve_with_code(
        &self,
        request_id: &str,
        code: &str,
        cookie_nonce: Option<&str>,
    ) -> Result<Approval, AuthError> {
        let request = self.store.pending_get(request_id)?;
        let Some((code_hash, attempts)) = request
            .filter(|request| request.expires_at >= now_ms())
            .and_then(|request| Some((request.code_hash?, request.attempts)))
        else {
            return refused("Code expired");
        };

        if attempts >= MAX_CODE_ATTEMPTS {
            self.store.pending_remove(request_id)?;
            return refused("Too many attempts");
        }

        self.store.pending_record_attempt(request_id)?;

        if sha256(&normalize_code(code)) != code_hash {
            return refused("Invalid code");
        }

        self.approve(request_id, cookie_nonce)
    }

    pub fn exchange_code(&self, code: &str, verifier: &str, resource: &str) -> Result<TokenPair, AuthError> {
        let _locked = self.code_lock.lock();
        let issued = self.codes.lock().ok().and_then(|mut codes| codes.remove(code));
        let Some(issued) = issued.filter(|issued| issued.expires_at >= now_ms()) else {
            return refused("invalid_grant");
        };

        if pkce_challenge(verifier) != issued.challenge {
            return refused("PKCE verification failed");
        }

        if !resource_matches(resource, &issued.server) {
            return refused("Resource mismatch");
        }

        let grant_id = random_token(8);
        self.store.grant_insert(&grant_id, &issued.client_id, &issued.server)?;

        self.issue_tokens(&grant_id, &issued.server)
    }

    pub fn refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError> {
        let Some(token) =
            self.store.token_get(&sha256(refresh_token))?.filter(|token| token.kind == TokenKind::Refresh)
        else {
            return refused("invalid_grant");
        };

        if token.expires_at < now_ms() {
            return refused("Refresh token expired");
        }

        if !self.store.token_claim(&token.hash)? {
            self.store.tokens_revoke_grant(&token.grant_id)?;
            self.store.audit_write("token.reuse_detected", &token.grant_id)?;
            return refused("Refresh token reused; grant revoked");
        }

        self.issue_tokens(&token.grant_id, &token.server)
    }

    pub fn still_allowed(&self, bearer: &str, server: &str) -> Result<Option<String>, AuthError> {
        let now = now_ms();
        let token = self.store.token_get(&sha256(bearer))?.filter(|token| {
            token.kind != TokenKind::Refresh && !token.revoked && token.expires_at > now && token.server == server
        });

        Ok(token.map(|token| token.grant_id))
    }

    pub fn authorize(&self, bearer: &str, server: &str) -> Result<Option<String>, AuthError> {
        let grant = self.still_allowed(bearer, server)?;

        if let Some(grant) = &grant {
            self.store.grant_touch(grant)?;
        }

        Ok(grant)
    }

    pub fn revoke(&self, token: &str) -> Result<(), AuthError> {
        let Some(found) = self.store.token_get(&sha256(token))? else {
            return Ok(());
        };
        let client = self.store.grant_client(&found.grant_id)?;
        self.store.tokens_revoke_grant(&found.grant_id)?;
        self.store.grant_remove(&found.grant_id)?;
        self.close_grant_streams(std::slice::from_ref(&found.grant_id));
        self.store
            .audit_record("token.revoked", AuditFields { client, app: Some(found.server), ..AuditFields::default() })?;

        Ok(())
    }

    pub fn caller(&self, bearer: &str) -> Result<Option<String>, AuthError> {
        match self.store.token_get(&sha256(bearer))? {
            Some(token) => Ok(self.store.grant_client(&token.grant_id)?),
            None => Ok(None),
        }
    }

    pub fn allow_registration(&self, ip: &str) -> Result<bool, AuthError> {
        let recent = self.store.registrations_since(ip, now_ms() - HOUR)?;

        if recent.total >= MAX_REGISTRATIONS_PER_HOUR {
            return Ok(false);
        }

        self.store.registration_record(ip)?;

        Ok(true)
    }

    pub fn create_static_token(&self, server: &str, name: &str, ttl: i64) -> Result<(String, String), AuthError> {
        let id = random_token(8);
        let token = random_token(32);
        self.store.grant_insert(&id, &format!("static:{name}"), server)?;
        self.store.token_insert(&sha256(&token), TokenKind::Static, &id, server, now_ms() + ttl)?;
        self.store.audit_write("token.static_created", &format!("{name} -> {server}"))?;

        Ok((id, token))
    }

    pub fn revoke_grant(&self, id: &str) -> Result<bool, AuthError> {
        self.store.tokens_revoke_grant(id)?;
        let removed = self.store.grant_remove(id)?;
        self.close_grant_streams(&[id.to_owned()]);

        if removed {
            self.store.audit_write("grant.revoked", id)?;
        }

        Ok(removed)
    }

    pub fn revoke_server(&self, server: &str) -> Result<(), AuthError> {
        let _locked = self.code_lock.lock();
        let grants: Vec<String> = self
            .store
            .grants_list()?
            .into_iter()
            .filter(|grant| grant.server == server)
            .map(|grant| grant.id)
            .collect();
        let removed = self.store.grants_remove_server(server)?;
        self.store.tokens_revoke_server(server)?;
        self.store.stdio_sessions_remove_server(server)?;
        self.store.pending_remove_server(server)?;

        if let Ok(mut codes) = self.codes.lock() {
            codes.retain(|_, code| code.server != server);
        }

        self.close_grant_streams(&grants);

        if removed > 0 {
            self.store.audit_record(
                "grant.revoked_app",
                AuditFields {
                    app: Some(server.to_owned()),
                    detail: Some(format!("{removed} removed")),
                    ..AuditFields::default()
                },
            )?;
        }

        Ok(())
    }

    pub fn revoke_all(&self) -> Result<(), AuthError> {
        let _locked = self.code_lock.lock();
        let grants: Vec<String> = self.store.grants_list()?.into_iter().map(|grant| grant.id).collect();
        self.store.tokens_revoke_all()?;
        self.store.grants_remove_all()?;

        if let Ok(mut codes) = self.codes.lock() {
            codes.clear();
        }

        self.close_grant_streams(&grants);
        self.store.audit_write("grant.revoked_all", "")?;

        Ok(())
    }
}
