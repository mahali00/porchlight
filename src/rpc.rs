use crate::core::Tool;
use crate::sources::{ListChange, UpstreamError};
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

pub const MAX_PAYLOAD_BYTES: usize = 256_000;

pub const TOO_LARGE: &str = "Upstream response exceeded the size limit";

pub type ByteStream = BoxStream<'static, Result<Bytes, UpstreamError>>;

pub fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

pub fn with_status(status: StatusCode, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
}

pub fn json(body: &Value, status: StatusCode) -> Response {
    let mut response = with_status(status, Body::from(body.to_string()));
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

pub fn text(body: &str, status: StatusCode) -> Response {
    let mut response = with_status(status, Body::from(body.to_owned()));
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain;charset=utf-8"));
    response
}

pub fn empty(status: StatusCode) -> Response {
    with_status(status, Body::empty())
}

pub fn rpc_error(id: Option<&Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.cloned().unwrap_or(Value::Null), "error": { "code": code, "message": message } })
}

pub fn rpc_result(id: Option<&Value>, result: Value) -> Value {
    let mut reply = serde_json::Map::new();
    reply.insert("jsonrpc".into(), json!("2.0"));
    reply.insert("id".into(), id.cloned().unwrap_or(Value::Null));
    reply.insert("result".into(), result);
    Value::Object(reply)
}

pub fn parse_error() -> Response {
    json(&rpc_error(None, -32700, "Parse error"), StatusCode::BAD_REQUEST)
}

pub fn id_key(id: Option<&Value>) -> String {
    id.cloned().unwrap_or(Value::Null).to_string()
}

pub struct Batch<T> {
    pub items: Vec<T>,
    pub batch: bool,
}

pub fn decode_batch<T: DeserializeOwned>(body: &[u8]) -> Option<Batch<T>> {
    let value: Value = serde_json::from_slice(body).ok()?;

    match value {
        Value::Array(items) => items
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<T>, _>>()
            .ok()
            .map(|items| Batch { items, batch: true }),
        single => serde_json::from_value(single).ok().map(|item| Batch { items: vec![item], batch: false }),
    }
}

#[derive(Deserialize)]
struct ToolsListReply {
    result: ToolsList,
}

#[derive(Deserialize)]
struct ToolsList {
    tools: Vec<Tool>,
}

pub fn decode_tools_list(payload: &str, source: &str) -> Result<Vec<Tool>, UpstreamError> {
    serde_json::from_str::<ToolsListReply>(payload)
        .map(|reply| reply.result.tools)
        .map_err(|cause| UpstreamError::new(format!("{source} returned an invalid tools/list: {cause}")))
}

pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
}

pub fn body_stream(body: Body) -> ByteStream {
    body.into_data_stream()
        .map(|chunk| chunk.map_err(|cause| UpstreamError::new(format!("Upstream closed the connection: {cause}"))))
        .boxed()
}

fn limited(stream: ByteStream, limit: usize) -> ByteStream {
    stream
        .scan(0usize, move |seen, chunk| {
            let next = match chunk {
                Ok(bytes) => {
                    *seen += bytes.len();

                    if *seen > limit { Err(UpstreamError::new(TOO_LARGE)) } else { Ok(bytes) }
                }
                Err(error) => Err(error),
            };

            futures::future::ready(Some(next))
        })
        .boxed()
}

pub struct Limited {
    pub text: String,
    pub too_large: bool,
}

pub async fn read_limited(body: Body, limit: usize) -> Result<Limited, UpstreamError> {
    let mut stream = body_stream(body);
    let mut bytes = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;

        if bytes.len() + chunk.len() > limit {
            return Ok(Limited { text: String::from_utf8_lossy(&bytes).into_owned(), too_large: true });
        }

        bytes.extend_from_slice(&chunk);
    }

    Ok(Limited { text: String::from_utf8_lossy(&bytes).into_owned(), too_large: false })
}

#[derive(Clone, Debug, Default)]
pub struct SseEvent {
    pub fields: Vec<String>,
    pub data: Vec<String>,
}

impl SseEvent {
    fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.data.is_empty()
    }
}

#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    event: SseEvent,
}

impl SseParser {
    fn line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if !self.event.is_empty() {
                events.push(std::mem::take(&mut self.event));
            }

            return;
        }

        match line.strip_prefix("data:") {
            Some(data) => self.event.data.push(data.strip_prefix(' ').unwrap_or(data).to_owned()),
            None => self.event.fields.push(line.to_owned()),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches('\n').trim_end_matches('\r').to_owned();
            self.line(&line, &mut events);
        }

        events
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
        let mut events = Vec::new();

        if !rest.is_empty() {
            self.line(rest.trim_end_matches('\r'), &mut events);
        }

        if !self.event.is_empty() {
            events.push(std::mem::take(&mut self.event));
        }

        events
    }
}

pub fn sse_events(stream: ByteStream) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    let finished = stream.map(Some).chain(stream::once(async { None }));

    finished
        .scan(SseParser::default(), |parser, chunk| {
            let events: Vec<Result<SseEvent, UpstreamError>> = match chunk {
                Some(Ok(bytes)) => parser.feed(&bytes).into_iter().map(Ok).collect(),
                Some(Err(error)) => vec![Err(error)],
                None => parser.finish().into_iter().map(Ok).collect(),
            };

            futures::future::ready(Some(stream::iter(events)))
        })
        .flatten()
        .boxed()
}

#[derive(Clone, Copy)]
pub enum ListKey {
    Tools,
    Resources,
}

impl ListKey {
    fn name(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Resources => "resources",
        }
    }
}

pub type Keep = std::sync::Arc<dyn Fn(&serde_json::Map<String, Value>) -> bool + Send + Sync>;

fn filter_message(message: &mut Value, key: ListKey, keep: &Keep) {
    if let Value::Array(items) = message {
        for item in items {
            filter_message(item, key, keep);
        }

        return;
    }

    if let Some(Value::Array(list)) = message.get_mut("result").and_then(|result| result.get_mut(key.name())) {
        list.retain(|item| item.as_object().is_some_and(|object| keep(object)));
    }
}

fn filter_json(text: &str, key: ListKey, keep: &Keep) -> Option<String> {
    let mut message: Value = serde_json::from_str(text).ok()?;
    filter_message(&mut message, key, keep);

    Some(message.to_string())
}

fn filter_event(event: &SseEvent, key: ListKey, keep: &Keep) -> String {
    let data = if event.data.is_empty() { None } else { filter_json(&event.data.join("\n"), key, keep) };
    let lines: Vec<String> = event.fields.iter().cloned().chain(data.map(|json| format!("data: {json}"))).collect();

    if lines.is_empty() { String::new() } else { format!("{}\n\n", lines.join("\n")) }
}

fn invalid_response(headers: HeaderMap) -> Response {
    let mut response = json(&rpc_error(None, -32603, "Upstream returned an invalid response"), StatusCode::BAD_GATEWAY);
    *response.headers_mut() = headers;
    response
}

pub async fn filter_list(response: Response, key: ListKey, keep: Keep) -> Result<Response, UpstreamError> {
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);

    if is_event_stream(&parts.headers) {
        let events = sse_events(limited(body_stream(body), MAX_PAYLOAD_BYTES)).filter_map(move |event| {
            let text = event.map(|event| filter_event(&event, key, &keep));

            futures::future::ready(match text {
                Ok(text) if text.is_empty() => None,
                Ok(text) => Some(Ok::<Bytes, UpstreamError>(Bytes::from(text))),
                Err(error) => Some(Err(error)),
            })
        });

        return Ok(Response::from_parts(parts, Body::from_stream(events)));
    }

    let read = read_limited(body, MAX_PAYLOAD_BYTES).await?;

    if read.too_large {
        return Ok(invalid_response(parts.headers));
    }

    Ok(match filter_json(&read.text, key, &keep) {
        Some(filtered) => Response::from_parts(parts, Body::from(filtered)),
        None => invalid_response(parts.headers),
    })
}

pub struct CallOutcome {
    pub ok: bool,
    pub message: String,
}

fn first_text(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find_map(|item| item.get("text").and_then(Value::as_str)))
        .unwrap_or_default()
        .to_owned()
}

fn outcome_of(reply: &Value) -> Option<(String, CallOutcome)> {
    let object = reply.as_object()?;
    let key = id_key(object.get("id"));

    if let Some(error) = object.get("error") {
        let message = error.get("message").and_then(Value::as_str)?.to_owned();
        return Some((key, CallOutcome { ok: false, message }));
    }

    let result = object.get("result");

    if result.and_then(|result| result.get("isError")).and_then(Value::as_bool) == Some(true) {
        return Some((key, CallOutcome { ok: false, message: first_text(result.and_then(|r| r.get("content"))) }));
    }

    Some((key, CallOutcome { ok: true, message: String::new() }))
}

pub fn call_outcomes(payload: &str) -> Vec<(String, CallOutcome)> {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Array(replies)) => replies.iter().filter_map(outcome_of).collect(),
        Ok(reply) => outcome_of(&reply).into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

pub fn payloads(headers: &HeaderMap, body: Body) -> BoxStream<'static, Result<String, UpstreamError>> {
    payloads_within(headers, body, MAX_PAYLOAD_BYTES)
}

pub fn payloads_within(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> BoxStream<'static, Result<String, UpstreamError>> {
    if is_event_stream(headers) {
        return sse_events(limited(body_stream(body), limit))
            .filter_map(|event| {
                futures::future::ready(match event {
                    Ok(event) if event.data.is_empty() => None,
                    Ok(event) => Some(Ok(event.data.join("\n"))),
                    Err(error) => Some(Err(error)),
                })
            })
            .boxed();
    }

    stream::once(async move {
        let read = read_limited(body, limit).await?;

        if read.too_large { Err(UpstreamError::new(TOO_LARGE)) } else { Ok(read.text) }
    })
    .boxed()
}

pub fn event_stream(body: Body) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

pub fn list_notification_stream(notices: BoxStream<'static, ListChange>) -> Response {
    let connected = stream::once(async { ": connected\n\n".to_owned() });
    let changes = notices.map(|list| {
        let message = json!({ "jsonrpc": "2.0", "method": format!("notifications/{}/list_changed", list.name()) });
        format!("event: message\ndata: {message}\n\n")
    });
    let keepalive = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    ))
    .map(|_| ": keepalive\n\n".to_owned());
    let merged = stream::select(connected.chain(changes), keepalive)
        .map(|text| Ok::<Bytes, std::convert::Infallible>(Bytes::from(text)));

    event_stream(Body::from_stream(merged))
}
