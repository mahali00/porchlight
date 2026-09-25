use super::UpstreamError;
use indexmap::IndexMap;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, watch};

const INHERITED: [&str; 7] = ["PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR"];

const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

pub fn process_environment(extra: &IndexMap<String, String>) -> IndexMap<String, String> {
    INHERITED
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| ((*name).to_owned(), value)))
        .chain(extra.iter().map(|(name, value)| (name.clone(), value.clone())))
        .collect()
}

pub struct ProcessSpec {
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: IndexMap<String, String>,
    pub log_file: PathBuf,
}

pub struct RunningProcess {
    pub pid: Option<u32>,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    exited: watch::Receiver<bool>,
}

fn open_log(spec: &ProcessSpec) -> std::io::Result<fs::File> {
    if let Some(dir) = spec.log_file.parent() {
        fs::create_dir_all(dir)?;
    }

    OpenOptions::new().create(true).append(true).mode(0o600).open(&spec.log_file)
}

async fn read_lines(stdout: tokio::process::ChildStdout, lines: mpsc::UnboundedSender<String>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();

        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) if buffer.len() > MAX_LINE_BYTES => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer).trim().to_owned();

                if !line.is_empty() && lines.send(line).is_err() {
                    return;
                }
            }
        }
    }
}

impl RunningProcess {
    pub fn start(spec: &ProcessSpec) -> Result<(Self, mpsc::UnboundedReceiver<String>), UpstreamError> {
        let failed = || UpstreamError::new(format!("Failed to start {}", spec.command.join(" ")));
        let (program, arguments) = spec.command.split_first().ok_or_else(failed)?;
        let log = open_log(spec).map_err(|_| failed())?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env_clear()
            .envs(process_environment(&spec.env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true);

        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|_| failed())?;
        let stdout = child.stdout.take().ok_or_else(failed)?;
        let stdin = child.stdin.take();
        let (sender, receiver) = mpsc::unbounded_channel();
        let (exited_sender, exited) = watch::channel(false);
        tokio::spawn(async move {
            read_lines(stdout, sender).await;
            let _ = exited_sender.send(true);
        });

        Ok((Self { pid: child.id(), stdin: Mutex::new(stdin), child: Mutex::new(child), exited }, receiver))
    }

    pub async fn write(&self, line: &str) -> Result<(), UpstreamError> {
        let mut stdin = self.stdin.lock().await;
        let failed = || UpstreamError::new("Failed to write to the server");
        let pipe = stdin.as_mut().ok_or_else(failed)?;
        pipe.write_all(format!("{line}\n").as_bytes()).await.map_err(|_| failed())?;
        pipe.flush().await.map_err(|_| failed())
    }

    pub async fn exited(&self) {
        let mut exited = self.exited.clone();
        let _ = exited.wait_for(|done| *done).await;
    }

    pub async fn terminate(&self) {
        self.stdin.lock().await.take();
        let mut child = self.child.lock().await;

        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }

        if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }

        if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
}
