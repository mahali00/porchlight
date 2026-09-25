use crate::approval::ApprovalPages;
use crate::auth::Auth;
use crate::client_metadata::ClientMetadata;
use crate::config::Config;
use crate::control::{self, DaemonState, state_of};
use crate::gateway::{Gateway, ListenError, bind};
use crate::registry::Registry;
use crate::serve::{Daemon, Reload, Reloader, Shared, upkeep};
use crate::store::{Store, StoreError};
use crate::system;
use crate::tunnels::TunnelError;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Tunnel(#[from] TunnelError),
    #[error(transparent)]
    Listen(#[from] ListenError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct Options {
    pub gateway_port: u16,
    pub approval_port: u16,
    pub tunnel: String,
    pub socket: Option<PathBuf>,
}

pub struct Running {
    pub daemon: Arc<Daemon>,
    pub shared: Shared,
    pub reloader: Arc<Reloader>,
    pub gateway_port: u16,
    pub approval_port: u16,
    socket: Option<PathBuf>,
    tasks: Vec<JoinHandle<()>>,
}

pub async fn start(options: Options, config: Config, store: Store) -> Result<Running, StartError> {
    let gateway_listener = bind(options.gateway_port).await?;
    let approval_listener = bind(options.approval_port).await?;
    let gateway_port = gateway_listener.local_addr().map_or(options.gateway_port, |address| address.port());
    let approval_port = approval_listener.local_addr().map_or(options.approval_port, |address| address.port());
    let daemon = Daemon::start(gateway_port, approval_port, &options.tunnel).await?;
    let auth = Auth::new(store.clone());
    let registry = Registry::new(store.clone(), system::host_name());
    let shared = Shared {
        store: store.clone(),
        auth: auth.clone(),
        registry: registry.clone(),
        client_metadata: ClientMetadata::default(),
        config: config.clone(),
    };
    let reloader = Reloader::start(registry.clone(), config, store.clone(), auth).await;
    let reload: Arc<dyn Reload> = reloader.clone();
    let mut tasks = vec![
        Gateway::new(daemon.clone(), shared.clone())?.serve(gateway_listener),
        ApprovalPages::new(daemon.clone(), reload.clone(), shared.clone()).serve(approval_listener),
        upkeep(store, daemon.clone()),
    ];

    if let Some(path) = &options.socket {
        tasks.push(control::serve(path, daemon.clone(), registry, reload)?);
    }

    Ok(Running { daemon, shared, reloader, gateway_port, approval_port, socket: options.socket, tasks })
}

impl Running {
    pub async fn state(&self) -> DaemonState {
        state_of(&self.daemon, &self.shared.registry, self.reloader.as_ref()).await
    }

    pub async fn reload(&self) -> DaemonState {
        self.reloader.reload_now().await;
        self.shared.registry.settle_all().await;
        self.state().await
    }

    pub async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }

        self.shared.registry.shutdown().await;
        self.daemon.close().await;

        if let Some(path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
    }
}
