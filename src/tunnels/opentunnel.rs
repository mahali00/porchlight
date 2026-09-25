use super::{TunnelError, TunnelWarning, days_until};
use crate::config::log_dir;
use chrono::{DateTime, Utc};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;

const ROUTE: &str = "porchlight";

const MISSING: &str = "OpenTunnel isn't installed. Run `bun add -g opentunnel` or `npm i -g opentunnel`.";

async fn npm_global_bin() -> Option<PathBuf> {
    let output = Command::new("npm").args(["prefix", "-g"]).output().await.ok()?;
    let prefix = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    (output.status.success() && !prefix.is_empty()).then(|| PathBuf::from(prefix).join("bin").join("opentunnel"))
}

pub async fn find() -> Option<PathBuf> {
    if let Ok(found) = which::which("opentunnel") {
        return Some(found);
    }

    let bun_install = std::env::var_os("BUN_INSTALL")
        .map_or_else(|| std::env::home_dir().unwrap_or_default().join(".bun"), PathBuf::from);
    let bun_bin = bun_install.join("bin").join("opentunnel");

    if bun_bin.exists() {
        return Some(bun_bin);
    }

    npm_global_bin().await.filter(|path| path.exists())
}

pub async fn install(quiet: bool) -> bool {
    let installers: [(&str, &[&str]); 2] = [("bun", &["add", "-g", "opentunnel"]), ("npm", &["i", "-g", "opentunnel"])];

    for (program, args) in installers {
        let Ok(path) = which::which(program) else {
            continue;
        };
        let (stdout, stderr) =
            if quiet { (Stdio::null(), Stdio::null()) } else { (Stdio::inherit(), Stdio::inherit()) };
        let status = Command::new(path).args(args).stdout(stdout).stderr(stderr).status().await;

        if status.is_ok_and(|status| status.success()) && find().await.is_some() {
            return true;
        }
    }

    false
}

struct Called {
    ok: bool,
    output: String,
}

async fn call(args: &[&str]) -> Result<Called, TunnelError> {
    let bin = find().await.ok_or_else(|| TunnelError::new(MISSING))?;
    let result = Command::new(bin).args(args).output().await.map_err(|_| TunnelError::new(MISSING))?;

    Ok(Called {
        ok: result.status.success(),
        output: format!("{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr)),
    })
}

async fn run(args: &[&str]) -> Result<String, TunnelError> {
    let result = call(args).await?;

    if !result.ok {
        return Err(TunnelError::new(format!("opentunnel {} failed: {}", args.join(" "), result.output)));
    }

    Ok(result.output)
}

fn parse_certificate_expiry(output: &str) -> Option<DateTime<Utc>> {
    let lower = output.to_lowercase();
    let at = lower.find("certificate expiry:")?;
    let rest = output.get(at + "certificate expiry:".len()..)?;
    let value = rest.split_whitespace().next()?;

    DateTime::parse_from_rfc3339(value).map(|date| date.with_timezone(&Utc)).ok().or_else(|| {
        chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|date| date.and_utc())
    })
}

fn parse_public_url(output: &str) -> Option<String> {
    output.split(|c: char| c.is_whitespace() || c == '/').find_map(|word| {
        let host = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
        let label = host.strip_suffix(".opentunnel.xyz")?;

        (!label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            .then(|| format!("https://{}", host.to_lowercase()))
    })
}

async fn ensure_tunnel() -> Result<(), TunnelError> {
    let result = call(&["create"]).await?;

    if !result.ok && !result.output.to_lowercase().contains("already has a tunnel") {
        return Err(TunnelError::new(format!("opentunnel create failed: {}", result.output)));
    }

    Ok(())
}

async fn ensure_route(local_port: u16) -> Result<(), TunnelError> {
    let target = format!("127.0.0.1:{local_port}");
    let routes = run(&["route", "list"]).await?;
    let has_route = routes.contains(&format!("{ROUTE}."));

    if has_route && routes.contains(&target) {
        return Ok(());
    }

    if has_route {
        run(&["route", "remove", ROUTE]).await?;
    }

    run(&["route", "add", ROUTE, &target]).await.map(drop)
}

async fn service_running() -> bool {
    call(&["service", "status"])
        .await
        .is_ok_and(|result| result.ok && result.output.to_lowercase().contains("is running"))
}

async fn serve_once() -> Result<(), TunnelError> {
    let bin = find().await.ok_or_else(|| TunnelError::new(MISSING))?;
    let failed = |_| TunnelError::new("Couldn't start opentunnel serve");
    let dir = log_dir();
    std::fs::create_dir_all(&dir).map_err(failed)?;
    let log =
        OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("opentunnel.log")).map_err(failed)?;
    let err = log.try_clone().map_err(failed)?;
    let mut child = Command::new(bin)
        .arg("serve")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let status = child.wait().await.map_err(failed)?;
    eprintln!("opentunnel serve exited with {status}, restarting");

    Ok(())
}

async fn keep_serving() {
    loop {
        if service_running().await {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }

        if let Err(error) = serve_once().await {
            eprintln!("{error}");
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub struct OpenTunnel {
    pub public_url: String,
    serving: JoinHandle<()>,
}

impl OpenTunnel {
    pub async fn close(self) {
        self.serving.abort();
        let _ = self.serving.await;
        let _ = run(&["route", "remove", ROUTE]).await;
    }
}

pub async fn open(local_port: u16) -> Result<OpenTunnel, TunnelError> {
    ensure_tunnel().await?;
    ensure_route(local_port).await?;
    let details = run(&["info"]).await?;
    let tunnel_url =
        parse_public_url(&details).ok_or_else(|| TunnelError::new("opentunnel info did not print a hostname"))?;

    if let Some(days) = parse_certificate_expiry(&details).map(|expiry| days_until(expiry, Utc::now()))
        && days < 14
    {
        eprintln!(
            "! OpenTunnel's certificate expires in {} days, and OpenTunnel doesn't renew it yet. Use your own tunnel for an address that has to last: `porchlight tunnel use <https-url>`.",
            days.max(0)
        );
    }

    Ok(OpenTunnel {
        public_url: tunnel_url.replacen("https://", &format!("https://{ROUTE}."), 1),
        serving: tokio::spawn(keep_serving()),
    })
}

pub async fn warnings() -> Vec<TunnelWarning> {
    let Some(expiry) = run(&["info"]).await.ok().and_then(|details| parse_certificate_expiry(&details)) else {
        return Vec::new();
    };
    let days = days_until(expiry, Utc::now());

    [14, 7, 1]
        .into_iter()
        .filter(|limit| days <= *limit)
        .map(|limit| TunnelWarning {
            id: format!("certificate-{limit}"),
            message: format!(
                "The OpenTunnel certificate expires in {days} days and doesn't renew automatically; switch to your own tunnel in porchlight's Tunnel tab."
            ),
        })
        .collect()
}
