pub mod commands;
pub mod mcp_http;
pub mod mcp_stdio;
pub mod process;

use crate::config::log_dir;
use crate::core::{Tool, resource_uri};
use crate::policy::is_dangerous;
use crate::store::Store;
use axum::response::Response;
use bytes::Bytes;
use commands::{CommandSource, CommandTool, CommandsSpec, runs_client_code};
use futures::stream::BoxStream;
use http::{HeaderMap, Method};
use indexmap::{IndexMap, IndexSet};
use mcp_http::HttpSource;
use mcp_stdio::{StdioSource, StdioSpec};
use std::sync::Arc;

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct UpstreamError {
    pub message: String,
}

impl UpstreamError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListChange {
    Tools,
    Resources,
}

impl ListChange {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Resources => "resources",
        }
    }
}

pub type Notices = Arc<dyn Fn() -> BoxStream<'static, ListChange> + Send + Sync>;

pub struct SourceRequest {
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl SourceRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandContribution {
    pub tools: IndexMap<String, CommandTool>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceSpec {
    Http { url: String, headers: IndexMap<String, String> },
    Commands { app: String, contribution: CommandContribution },
    Stdio { app: String, command: Vec<String>, env: IndexMap<String, String>, cwd: Option<String> },
}

impl SourceSpec {
    pub fn refresh(&self) -> bool {
        matches!(self, Self::Http { .. })
    }

    pub fn report_refusal(&self) -> bool {
        matches!(self, Self::Commands { .. })
    }

    pub fn contribution(&self) -> Option<&CommandContribution> {
        match self {
            Self::Commands { contribution, .. } => Some(contribution),
            _ => None,
        }
    }

    pub fn risky(&self, tool: &Tool) -> bool {
        match self {
            Self::Commands { contribution, .. } => {
                contribution.tools.get(&tool.name).is_some_and(|command| runs_client_code(&command.words))
            }
            _ => is_dangerous(tool),
        }
    }

    pub fn resources(&self, allowed: &IndexSet<String>) -> Vec<String> {
        self.contribution()
            .map(|contribution| {
                contribution
                    .tools
                    .iter()
                    .filter(|(name, tool)| tool.resource && allowed.contains(*name))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn connect(&self, notices: Notices, store: &Store) -> Source {
        match self {
            Self::Http { url, headers } => Source::Http(Arc::new(HttpSource::new(url, headers, Some(notices)))),
            Self::Commands { app, contribution } => {
                let owner = app.clone();

                Source::Commands(Arc::new(CommandSource::new(CommandsSpec {
                    name: app.clone(),
                    tools: contribution.tools.clone(),
                    uri_of: Box::new(move |tool| resource_uri(&owner, tool)),
                    instructions: contribution.description.clone(),
                    notices: Some(notices),
                })))
            }
            Self::Stdio { app, command, env, cwd } => Source::Stdio(StdioSource::start(
                StdioSpec {
                    name: app.clone(),
                    command: command.clone(),
                    env: env.clone(),
                    cwd: cwd.clone(),
                    log_file: log_dir().join(format!("{app}.log")),
                },
                store.clone(),
            )),
        }
    }
}

#[derive(Clone)]
pub enum Source {
    Http(Arc<HttpSource>),
    Commands(Arc<CommandSource>),
    Stdio(Arc<StdioSource>),
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Self::Http(source) => source.url.clone(),
            Self::Commands(source) => source.name.clone(),
            Self::Stdio(source) => source.describe(),
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>, UpstreamError> {
        match self {
            Self::Http(source) => source.list_tools().await,
            Self::Commands(source) => Ok(source.shared.clone()),
            Self::Stdio(source) => source.list_tools().await,
        }
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, UpstreamError> {
        match self {
            Self::Http(source) => source.call_tool(name, arguments).await,
            Self::Stdio(source) => source.call_tool(name, arguments).await,
            Self::Commands(_) => Err(UpstreamError::new("Command-line tools run directly, not through the daemon")),
        }
    }

    pub async fn handle(&self, request: SourceRequest) -> Result<Response, UpstreamError> {
        match self {
            Self::Http(source) => source.handle(request).await,
            Self::Commands(source) => Ok(source.handle(request).await),
            Self::Stdio(source) => source.handle(request).await,
        }
    }

    pub async fn shutdown(&self) {
        if let Self::Stdio(source) = self {
            source.shutdown().await;
        }
    }
}
