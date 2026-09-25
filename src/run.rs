use crate::config::Config;
use crate::control::{self, DaemonState, daemon_state, socket_path};
use crate::core::is_https_url;
use crate::daemon::{self, Options};
use crate::exit::{ExitCode, ExitError};
use crate::store::{PortKey, Ports, Store};
use crate::system;
use crate::tunnels::{DEFAULT_TUNNEL, Provider};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;

fn internal(error: &dyn std::fmt::Display) -> ExitError {
    ExitError::new(ExitCode::Internal, error.to_string())
}

pub fn resolve_tunnel(config: &Config) -> Result<String, ExitError> {
    let settings = config.load().map_err(|error| internal(&error))?.settings;

    if let Some(tunnel) = settings.tunnel {
        return Ok(tunnel);
    }

    config.edit(&["tunnel"], Some(&json!(DEFAULT_TUNNEL))).map_err(|error| internal(&error))?;

    Ok(DEFAULT_TUNNEL.to_owned())
}

pub async fn ensure_tunnel(choice: &str, quiet: bool) -> Result<(), ExitError> {
    let Some(provider) = Provider::named(choice) else {
        return Ok(());
    };

    if provider.installed().await || provider.install(quiet).await {
        return Ok(());
    }

    Err(ExitError::new(ExitCode::TunnelFailed, format!("{} isn't installed", provider.label()))
        .next("Install it with `npm i -g opentunnel`, or use your own tunnel's https:// address."))
}

async fn port_is_free(port: u16) -> bool {
    tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok()
}

async fn free_port() -> u16 {
    match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => listener.local_addr().map_or(0, |address| address.port()),
        Err(_) => 0,
    }
}

async fn choose_ports(store: &Store, fixed: Option<u16>, own_tunnel: bool) -> Result<Ports, ExitError> {
    let stored = store.ports().map_err(|error| internal(&error))?;
    let mut ports = Ports { gateway: fixed.unwrap_or(stored.gateway), approval: stored.approval };

    if (fixed.is_some() || own_tunnel) && !port_is_free(ports.gateway).await {
        return Err(ExitError::new(ExitCode::Internal, format!("Port {} is in use by another app", ports.gateway))
            .next(if own_tunnel {
                "Free that port, or set another \"port\" in porchlight.json and point your tunnel at it."
            } else {
                "Free that port, or set another \"port\" in porchlight.json."
            }));
    }

    if !port_is_free(ports.gateway).await {
        ports.gateway = free_port().await;
        store.set_port(PortKey::Gateway, ports.gateway).map_err(|error| internal(&error))?;
    }

    if !port_is_free(ports.approval).await {
        ports.approval = free_port().await;
        store.set_port(PortKey::Approval, ports.approval).map_err(|error| internal(&error))?;
    }

    Ok(ports)
}

fn servers_json(state: &DaemonState, status: &str) -> Value {
    json!({
        "ok": state.servers.iter().any(|server| server.state != crate::links::ServerState::Refused),
        "servers": state.servers.iter().map(|server| json!({
            "name": server.name,
            "url": server.url,
            "state": server.state,
            "detail": server.detail,
            "tools": server.tools,
        })).collect::<Vec<_>>(),
        "status": status,
        "next_action": "Give share the url so they add it as a remote MCP server in their MCP client and approve the connection on this computer. You cannot approve on their behalf.",
    })
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        () = terminate => {},
    }
}

pub async fn serve() -> Result<(), ExitError> {
    if let Some(state) = daemon_state().await {
        out!("{}", servers_json(&state, "already_running"));
        return Ok(());
    }

    let config = Config::default_location();
    let store = Store::open_default().map_err(|error| internal(&error))?;
    let tunnel = resolve_tunnel(&config)?;
    ensure_tunnel(&tunnel, true).await?;
    let fixed = config.load().map_err(|error| internal(&error))?.settings.port;
    let ports = choose_ports(&store, fixed, is_https_url(&tunnel)).await?;
    let options =
        Options { gateway_port: ports.gateway, approval_port: ports.approval, tunnel, socket: Some(socket_path()) };
    let running = daemon::start(options, config, store).await.map_err(|error| match error {
        daemon::StartError::Tunnel(error) => {
            ExitError::new(ExitCode::TunnelFailed, error.message).next("Check the Tunnel tab in porchlight.")
        }
        other => internal(&other),
    })?;
    running.shared.registry.settle_all().await;
    out!("{}", servers_json(&running.state().await, "awaiting_owner_approval"));
    shutdown_signal().await;
    running.shutdown().await;

    Ok(())
}

pub fn tunnel_trouble() -> Option<String> {
    let log = std::fs::read_to_string(crate::config::log_dir().join("opentunnel.log")).ok()?;
    let recent: Vec<&str> = log.lines().rev().take(80).collect();
    let error = recent.iter().find(|line| line.trim_start().starts_with("error:"))?;

    Some(if error.contains("route_conflict") {
        "OpenTunnel says another tunnel still holds this address. That usually clears within a minute or two, or when the old porchlight stops.".to_owned()
    } else {
        format!("OpenTunnel says: {}", error.trim().trim_start_matches("error:").trim())
    })
}

pub async fn start(say: &(dyn Fn(&str) + Sync)) -> Result<(DaemonState, bool), ExitError> {
    say("Checking the tunnel");
    let config = Config::default_location();
    let tunnel = resolve_tunnel(&config)?;

    if Provider::named(&tunnel).is_some() {
        say("Installing OpenTunnel if it's missing");
    }

    ensure_tunnel(&tunnel, true).await?;
    say("Starting in the background");
    let installed = system::service_supported() && system::install_service(&["serve"]).await.is_ok();

    if !installed {
        let exe = std::env::current_exe().map_err(|error| internal(&error))?;
        std::process::Command::new(exe)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| internal(&error))?;
    }

    say("Opening the tunnel");

    for _ in 0..240 {
        if let Some(state) = daemon_state().await {
            return Ok((state, installed));
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    Err(ExitError::new(ExitCode::Internal, "The tunnel didn't open in 2 minutes")
        .next("porchlight keeps trying in the background. Check again with porchlight status."))
}

pub async fn status(json_output: bool) -> Result<(), ExitError> {
    let Some(state) = daemon_state().await else {
        let service = system::service_status().await;

        if json_output {
            out!(
                "{}",
                json!({ "ok": true, "running": false, "service": { "installed": service.installed, "running": service.running } })
            );
        } else {
            out!("○ porchlight isn't running. Start it with porchlight");
        }

        return Ok(());
    };

    if json_output {
        let mut value = serde_json::to_value(&state).unwrap_or(Value::Null);

        if let Some(object) = value.as_object_mut() {
            object.insert("ok".into(), json!(true));
            object.insert("running".into(), json!(true));
        }

        out!("{value}");
        return Ok(());
    }

    out!("● porchlight · {}", system::host_name());

    for server in &state.servers {
        out!("  {:<12} {}  {}", server.name, server.url, server.detail);
    }

    for problem in &state.problems {
        out!("  ✗ {}", problem.message);
    }

    Ok(())
}

pub async fn stop() -> Result<(), ExitError> {
    let before = control::daemon_state().await;
    system::uninstall_service().await.map_err(|error| internal(&error))?;

    if let Some(before) = before
        && control::daemon_state().await.is_some()
        && let Ok(pid) = i32::try_from(before.pid)
    {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
    }

    Ok(())
}
