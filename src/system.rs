use crate::config::log_dir;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ShellError {
    pub message: String,
}

fn failure(message: impl Into<String>) -> ShellError {
    ShellError { message: message.into() }
}

struct Output {
    ok: bool,
    stdout: String,
}

async fn output(program: &str, args: &[&str]) -> Output {
    match Command::new(program).args(args).output().await {
        Ok(result) => {
            Output { ok: result.status.success(), stdout: String::from_utf8_lossy(&result.stdout).into_owned() }
        }
        Err(_) => Output { ok: false, stdout: String::new() },
    }
}

async fn shell(message: &str, program: &str, args: &[&str]) -> Result<(), ShellError> {
    if output(program, args).await.ok { Ok(()) } else { Err(failure(message)) }
}

fn write_file(path: &PathBuf, content: &str) -> Result<(), ShellError> {
    let failed = |cause: std::io::Error| failure(format!("Couldn't write {}: {cause}", path.display()));

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }

    std::fs::write(path, content).map_err(failed)
}

fn remove_file(path: &PathBuf) -> Result<(), ShellError> {
    match std::fs::remove_file(path) {
        Err(cause) if cause.kind() != std::io::ErrorKind::NotFound => {
            Err(failure(format!("Couldn't remove {}: {cause}", path.display())))
        }
        _ => Ok(()),
    }
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

struct ServiceEnvironment {
    search_path: String,
    out_log: String,
    err_log: String,
}

fn service_environment() -> ServiceEnvironment {
    ServiceEnvironment {
        search_path: std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
        out_log: log_dir().join("service.out.log").display().to_string(),
        err_log: log_dir().join("service.err.log").display().to_string(),
    }
}

#[derive(Clone, Copy)]
enum Manager {
    Launchd,
    Systemd,
}

const LABEL: &str = "dev.porchlight.agent";

const UNIT: &str = "porchlight.service";

fn escape_xml(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn quote_systemd(arg: &str) -> String {
    format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$"))
}

fn plist(command: &[String], environment: &ServiceEnvironment) -> String {
    let arguments: Vec<String> =
        command.iter().map(|arg| format!("    <string>{}</string>", escape_xml(arg))).collect();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{}
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>{}</string>
  </dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        arguments.join("\n"),
        escape_xml(&environment.search_path),
        escape_xml(&environment.out_log),
        escape_xml(&environment.err_log)
    )
}

fn unit(command: &[String], environment: &ServiceEnvironment) -> String {
    let exec: Vec<String> = command.iter().map(|arg| quote_systemd(arg)).collect();

    format!(
        "[Unit]\nDescription=porchlight\nAfter=network-online.target\n\n[Service]\nExecStart={}\nRestart=always\nRestartSec=5\nEnvironment={}\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
        exec.join(" "),
        quote_systemd(&format!("PATH={}", environment.search_path)),
        environment.out_log,
        environment.err_log
    )
}

impl Manager {
    fn current() -> Option<Self> {
        match std::env::consts::OS {
            "macos" => Some(Self::Launchd),
            "linux" => Some(Self::Systemd),
            _ => None,
        }
    }

    fn path(self) -> PathBuf {
        match self {
            Self::Launchd => home().join("Library").join("LaunchAgents").join(format!("{LABEL}.plist")),
            Self::Systemd => std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home().join(".config"), PathBuf::from)
                .join("systemd")
                .join("user")
                .join(UNIT),
        }
    }

    async fn install(self, command: &[String]) -> Result<(), ShellError> {
        let path = self.path();
        let shown = path.display().to_string();

        match self {
            Self::Launchd => {
                write_file(&path, &plist(command, &service_environment()))?;
                output("launchctl", &["unload", &shown]).await;
                shell("launchctl couldn't load the agent", "launchctl", &["load", &shown]).await
            }
            Self::Systemd => {
                if !output("systemctl", &["--user", "show-environment"]).await.ok {
                    return Err(failure(
                        "systemd user services aren't available here. Run porchlight directly instead.",
                    ));
                }

                write_file(&path, &unit(command, &service_environment()))?;
                shell("systemctl couldn't reload units", "systemctl", &["--user", "daemon-reload"]).await?;
                shell("systemctl couldn't enable the service", "systemctl", &["--user", "enable", "--now", UNIT])
                    .await?;
                shell("systemctl couldn't restart the service", "systemctl", &["--user", "restart", UNIT]).await
            }
        }
    }

    async fn uninstall(self) -> Result<(), ShellError> {
        let path = self.path();

        match self {
            Self::Launchd => {
                output("launchctl", &["unload", &path.display().to_string()]).await;
                remove_file(&path)
            }
            Self::Systemd => {
                output("systemctl", &["--user", "disable", "--now", UNIT]).await;
                remove_file(&path)?;
                output("systemctl", &["--user", "daemon-reload"]).await;
                Ok(())
            }
        }
    }

    async fn pid(self) -> Option<u32> {
        let found = match self {
            Self::Launchd => {
                let listing = output("launchctl", &["list", LABEL]).await.stdout;
                listing
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("\"PID\" = "))
                    .and_then(|rest| rest.trim_end_matches(';').trim().parse().ok())
            }
            Self::Systemd => output("systemctl", &["--user", "show", "--property", "MainPID", "--value", UNIT])
                .await
                .stdout
                .trim()
                .parse()
                .ok(),
        };

        found.filter(|pid| *pid > 0)
    }
}

#[derive(Clone)]
pub struct ServiceStatus {
    pub installed: bool,
    pub running: bool,
    pub pid: Option<u32>,
}

pub fn service_supported() -> bool {
    Manager::current().is_some()
}

fn self_command(args: &[&str]) -> Vec<String> {
    let exe = std::env::current_exe().map_or_else(|_| "porchlight".to_owned(), |path| path.display().to_string());

    std::iter::once(exe).chain(args.iter().map(|arg| (*arg).to_owned())).collect()
}

pub async fn install_service(args: &[&str]) -> Result<(), ShellError> {
    let manager = Manager::current().ok_or_else(|| {
        failure(format!("A background service isn't supported on {}. Run porchlight directly.", std::env::consts::OS))
    })?;
    let _ = std::fs::create_dir_all(log_dir());

    manager
        .install(&self_command(args))
        .await
        .map_err(|error| failure(format!("Failed to install the background service: {error}")))
}

pub async fn uninstall_service() -> Result<(), ShellError> {
    match Manager::current() {
        Some(manager) => manager
            .uninstall()
            .await
            .map_err(|error| failure(format!("Failed to remove the background service: {error}"))),
        None => Ok(()),
    }
}

pub async fn service_status() -> ServiceStatus {
    match Manager::current() {
        Some(manager) => {
            let pid = manager.pid().await;
            ServiceStatus { installed: manager.path().exists(), running: pid.is_some(), pid }
        }
        None => ServiceStatus { installed: false, running: false, pid: None },
    }
}

fn applescript_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

pub async fn notify(title: &str, message: &str) {
    match Manager::current() {
        Some(Manager::Launchd) => {
            let script = format!(
                "display notification {} with title {}",
                applescript_string(message),
                applescript_string(title)
            );
            output("osascript", &["-e", &script]).await;
        }
        Some(Manager::Systemd) => {
            output("notify-send", &[title, message]).await;
        }
        None => {}
    }
}

pub async fn copy_to_clipboard(text: &str) -> bool {
    let program = match Manager::current() {
        Some(Manager::Launchd) => "pbcopy",
        Some(Manager::Systemd) if std::env::var_os("WAYLAND_DISPLAY").is_some() => "wl-copy",
        Some(Manager::Systemd) => "xclip",
        None => return false,
    };
    let args: &[&str] = if program == "xclip" { &["-selection", "clipboard"] } else { &[] };
    let Ok(mut child) = Command::new(program).args(args).stdin(std::process::Stdio::piped()).spawn() else {
        return false;
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
    }

    child.wait().await.is_ok_and(|status| status.success())
}

pub fn host_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();

    name.split('.').next().unwrap_or(&name).to_owned()
}

pub async fn open_in_browser(url: &str) {
    let program = if std::env::consts::OS == "macos" { "open" } else { "xdg-open" };
    output(program, &[url]).await;
}
