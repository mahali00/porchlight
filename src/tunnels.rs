pub mod opentunnel;

use crate::core::is_https_url;
use opentunnel::OpenTunnel;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TunnelError {
    pub message: String,
}

impl TunnelError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

pub struct TunnelWarning {
    pub id: String,
    pub message: String,
}

pub const DEFAULT_TUNNEL: &str = "opentunnel";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenTunnel,
}

pub enum Opened {
    OpenTunnel(OpenTunnel),
}

impl Opened {
    pub fn public_url(&self) -> &str {
        match self {
            Self::OpenTunnel(tunnel) => &tunnel.public_url,
        }
    }

    pub async fn close(self) {
        match self {
            Self::OpenTunnel(tunnel) => tunnel.close().await,
        }
    }
}

impl Provider {
    pub const ALL: [Self; 1] = [Self::OpenTunnel];

    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| provider.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::OpenTunnel => "opentunnel",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenTunnel => "OpenTunnel",
        }
    }

    pub async fn installed(self) -> bool {
        match self {
            Self::OpenTunnel => opentunnel::find().await.is_some(),
        }
    }

    pub async fn install(self, quiet: bool) -> bool {
        match self {
            Self::OpenTunnel => opentunnel::install(quiet).await,
        }
    }

    pub async fn warnings(self) -> Vec<TunnelWarning> {
        match self {
            Self::OpenTunnel => opentunnel::warnings().await,
        }
    }

    pub async fn open(self, local_port: u16) -> Result<Opened, TunnelError> {
        match self {
            Self::OpenTunnel => opentunnel::open(local_port).await.map(Opened::OpenTunnel),
        }
    }
}

pub enum TunnelChoice {
    Url(String),
    Provider(Provider),
}

pub fn tunnel_choice(value: &str) -> Option<TunnelChoice> {
    if is_https_url(value) {
        return Some(TunnelChoice::Url(value.to_owned()));
    }

    Provider::named(value).map(TunnelChoice::Provider)
}

pub fn days_until(date: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> i64 {
    (date - now).num_days()
}
