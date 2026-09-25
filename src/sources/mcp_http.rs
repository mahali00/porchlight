use super::{Notices, SourceRequest, UpstreamError};
use crate::core::Tool;
use crate::rpc::{decode_tools_list, list_notification_stream, payloads, text, with_status};
use axum::body::Body;
use axum::response::Response;
use futures::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::time::Duration;

const FORWARDED: [&str; 7] =
    ["accept", "content-type", "last-event-id", "mcp-protocol-version", "mcp-session-id", "origin", "user-agent"];

pub fn parse_headers(pairs: &[String]) -> IndexMap<String, String> {
    pairs
        .iter()
        .filter_map(|pair| {
            let (name, value) = pair.split_once(':')?;
            (!name.trim().is_empty()).then(|| (name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default()
}

fn insert(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
        headers.insert(name, value);
    }
}

fn origin_of(url: &url::Url) -> String {
    url.origin().ascii_serialization()
}

fn upstream_headers(incoming: &HeaderMap, upstream: &url::Url, extra: &IndexMap<String, String>) -> HeaderMap {
    let mut headers = HeaderMap::new();

    for name in FORWARDED {
        if let Some(value) = incoming.get(name) {
            headers.insert(name, value.clone());
        }
    }

    if let Some(origin) = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()).map(str::to_owned) {
        match url::Url::parse(&origin) {
            Ok(parsed) if matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")) => {
                insert(&mut headers, "origin", &origin_of(upstream));
            }
            Ok(_) => {}
            Err(_) => {
                headers.remove(header::ORIGIN);
            }
        }
    }

    for (name, value) in extra {
        insert(&mut headers, name, value);
    }

    headers
}

fn into_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let mut headers = response.headers().clone();
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::CONNECTION);
    let mut reply = with_status(status, Body::from_stream(response.bytes_stream()));
    *reply.headers_mut() = headers;
    reply
}

pub struct HttpSource {
    pub url: String,
    headers: IndexMap<String, String>,
    notices: Option<Notices>,
    client: reqwest::Client,
}

struct Reply {
    payload: String,
    session: Option<String>,
}

impl HttpSource {
    pub fn new(url: &str, headers: &IndexMap<String, String>, notices: Option<Notices>) -> Self {
        Self { url: url.to_owned(), headers: headers.clone(), notices, client: client() }
    }

    fn unreachable(&self) -> UpstreamError {
        UpstreamError::new(format!("{} is unreachable", self.url))
    }

    async fn forward(&self, request: SourceRequest) -> Result<Response, UpstreamError> {
        let upstream = url::Url::parse(&self.url).map_err(|_| self.unreachable())?;
        let headers = upstream_headers(&request.headers, &upstream, &self.headers);
        let mut builder = self.client.request(request.method.clone(), upstream).headers(headers);

        if request.method != Method::GET && request.method != Method::DELETE {
            builder = builder.body(request.body);
        }

        let response = builder.send().await.map_err(|_| self.unreachable())?;

        if response.status().is_redirection() {
            return Ok(text("The app answered with a redirect, which isn't forwarded", StatusCode::BAD_GATEWAY));
        }

        Ok(into_response(response))
    }

    async fn request(
        &self,
        method: &str,
        id: Option<i64>,
        params: Value,
        session: Option<&str>,
        version: Option<&str>,
    ) -> Result<Reply, UpstreamError> {
        let mut headers = HeaderMap::new();

        for (name, value) in &self.headers {
            insert(&mut headers, name, value);
        }

        insert(&mut headers, "content-type", "application/json");
        insert(&mut headers, "accept", "application/json, text/event-stream");

        if let Some(session) = session {
            insert(&mut headers, "mcp-session-id", session);
        }

        if let Some(version) = version {
            insert(&mut headers, "mcp-protocol-version", version);
        }

        let mut body = json!({ "jsonrpc": "2.0", "method": method, "params": params });

        if let (Some(id), Some(object)) = (id, body.as_object_mut()) {
            object.insert("id".into(), json!(id));
        }

        let response = self
            .client
            .post(&self.url)
            .headers(headers)
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| self.unreachable())?;

        if !response.status().is_success() {
            return Err(UpstreamError::new(format!("{} answered {}", self.url, response.status().as_u16())));
        }

        let session = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()).map(str::to_owned);
        let reply = into_response(response);
        let (parts, body) = reply.into_parts();
        let payload = payloads(&parts.headers, body).next().await.transpose()?.unwrap_or_default();

        Ok(Reply { payload, session })
    }

    async fn close_session(&self, session: &str) {
        let mut headers = HeaderMap::new();

        for (name, value) in &self.headers {
            insert(&mut headers, name, value);
        }

        insert(&mut headers, "mcp-session-id", session);
        let _ = self.client.delete(&self.url).headers(headers).send().await;
    }

    async fn handshake(&self, method: &str, params: Value) -> Result<String, UpstreamError> {
        let initialize = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "porchlight", "version": "1.0.0" },
        });
        let initialized = self.request("initialize", Some(1), initialize, None, None).await?;
        let negotiated: Value = serde_json::from_str(&initialized.payload)
            .map_err(|_| UpstreamError::new(format!("{} rejected MCP initialization", self.url)))?;
        let version = negotiated
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| UpstreamError::new(format!("{} rejected MCP initialization", self.url)))?
            .to_owned();
        let session = initialized.session;
        let _ = self.request("notifications/initialized", None, json!({}), session.as_deref(), Some(&version)).await;
        let reply = self.request(method, Some(2), params, session.as_deref(), Some(&version)).await;

        if let Some(session) = &session {
            self.close_session(session).await;
        }

        Ok(reply?.payload)
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>, UpstreamError> {
        let payload = tokio::time::timeout(Duration::from_secs(10), self.handshake("tools/list", json!({})))
            .await
            .unwrap_or_else(|_| Err(UpstreamError::new("MCP initialization or tools/list timed out")))?;

        decode_tools_list(&payload, &self.url)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, UpstreamError> {
        let params = json!({ "name": name, "arguments": arguments });
        let payload = tokio::time::timeout(Duration::from_mins(1), self.handshake("tools/call", params))
            .await
            .unwrap_or_else(|_| Err(UpstreamError::new(format!("{name} took longer than a minute"))))?;

        serde_json::from_str(&payload)
            .map_err(|_| UpstreamError::new(format!("{} sent a reply porchlight can't read", self.url)))
    }

    pub async fn handle(&self, request: SourceRequest) -> Result<Response, UpstreamError> {
        if request.method == Method::GET
            && let Some(notices) = &self.notices
        {
            return Ok(list_notification_stream(notices()));
        }

        self.forward(request).await
    }
}
