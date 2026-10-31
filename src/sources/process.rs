use crate::rpc::UpstreamError;
use indexmap::IndexMap;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, watch};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

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

struct Log {
    path: PathBuf,
    file: fs::File,
    written: u64,
}

impl Log {
    fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }

        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
        let written = file.metadata().map_or(0, |metadata| metadata.len());

        Ok(Self { path: path.to_owned(), file, written })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let previous = self.path.with_extension("log.1");
        let _ = fs::remove_file(&previous);
        if fs::rename(&self.path, previous).is_err() {
            let _ = fs::remove_file(&self.path);
        }
        self.file = OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path)?;
        self.written = 0;

        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut rest = bytes;

        while !rest.is_empty() {
            if self.written >= MAX_LOG_BYTES {
                self.rotate()?;
            }
            let room = usize::try_from(MAX_LOG_BYTES.saturating_sub(self.written)).unwrap_or(usize::MAX);
            let (now, later) = rest.split_at_checked(rest.len().min(room)).unwrap_or((rest, &[]));
            self.file.write_all(now)?;
            self.written = self.written.saturating_add(u64::try_from(now.len()).unwrap_or(MAX_LOG_BYTES));
            rest = later;
        }

        Ok(())
    }
}

type SharedLog = Arc<StdMutex<Log>>;

static LOGS: LazyLock<StdMutex<HashMap<PathBuf, SharedLog>>> = LazyLock::new(|| StdMutex::new(HashMap::new()));

fn shared_log(path: &Path) -> std::io::Result<SharedLog> {
    let mut logs = LOGS.lock().map_err(|_| std::io::Error::other("log registry unavailable"))?;

    if let Some(log) = logs.get(path) {
        return Ok(log.clone());
    }

    let log = Arc::new(StdMutex::new(Log::open(path)?));
    logs.insert(path.to_owned(), log.clone());

    Ok(log)
}

async fn write_stderr(mut stderr: tokio::process::ChildStderr, log: SharedLog) {
    let mut chunk = [0u8; 8192];
    loop {
        let Ok(read) = stderr.read(&mut chunk).await else { return };
        if read == 0 {
            return;
        }
        let bytes = chunk.get(..read).unwrap_or_default().to_vec();
        let log = log.clone();
        let written =
            tokio::task::spawn_blocking(move || log.lock().is_ok_and(|mut log| log.append(&bytes).is_ok())).await;
        if !matches!(written, Ok(true)) {
            return;
        }
    }
}

async fn emit(buffer: &mut Vec<u8>, lines: &mpsc::Sender<String>) -> bool {
    let line = String::from_utf8_lossy(buffer).trim().to_owned();
    buffer.clear();
    line.is_empty() || lines.send(line).await.is_ok()
}

async fn read_lines(mut stdout: tokio::process::ChildStdout, lines: mpsc::Sender<String>) {
    let mut chunk = [0u8; 8192];
    let mut buffer = Vec::new();

    loop {
        let Ok(read) = stdout.read(&mut chunk).await else { return };
        if read == 0 {
            return;
        }

        let mut start = 0;
        for (index, byte) in chunk.iter().take(read).enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let Some(length) = buffer.len().checked_add(index.saturating_sub(start)) else { return };
            if length > MAX_LINE_BYTES {
                return;
            }
            buffer.extend_from_slice(chunk.get(start..index).unwrap_or_default());
            if !emit(&mut buffer, &lines).await {
                return;
            }
            start = index.saturating_add(1);
        }

        let Some(length) = buffer.len().checked_add(read.saturating_sub(start)) else { return };
        if length > MAX_LINE_BYTES {
            return;
        }
        buffer.extend_from_slice(chunk.get(start..read).unwrap_or_default());
    }
}

impl RunningProcess {
    pub fn start(spec: &ProcessSpec) -> Result<(Self, mpsc::Receiver<String>), UpstreamError> {
        let failed = || UpstreamError::new(format!("Failed to start {}", spec.command.join(" ")));
        let (program, arguments) = spec.command.split_first().ok_or_else(failed)?;
        let log = shared_log(&spec.log_file).map_err(|_| failed())?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env_clear()
            .envs(process_environment(&spec.env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);

        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|_| failed())?;
        let stdout = child.stdout.take().ok_or_else(failed)?;
        let stderr = child.stderr.take().ok_or_else(failed)?;
        let stdin = child.stdin.take();
        tokio::spawn(write_stderr(stderr, log));
        let (sender, receiver) = mpsc::channel(256);
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
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
        }

        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }

        let mut child = self.child.lock().await;
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }

        if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err()
            && let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok())
        {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_oversize_stdout_line_ends_the_process_reader() {
        let log_file = std::env::temp_dir().join(format!("porchlight-line-limit-{}", std::process::id()));
        let spec = ProcessSpec {
            command: vec!["/usr/bin/head".to_owned(), "-c".to_owned(), "16777217".to_owned(), "/dev/zero".to_owned()],
            cwd: None,
            env: IndexMap::new(),
            log_file: log_file.clone(),
        };
        let (process, mut lines) = RunningProcess::start(&spec).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(10), lines.recv()).await.unwrap();

        assert!(next.is_none());
        process.terminate().await;
        let _ = fs::remove_file(log_file);
    }

    #[tokio::test]
    async fn stderr_logs_rotate_with_a_fixed_size_cap() {
        let log_file = std::env::temp_dir().join(format!("porchlight-stderr-cap-{}", std::process::id()));
        let spec = ProcessSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), "while :; do printf '%8192s' x >&2; done".to_owned()],
            cwd: None,
            env: IndexMap::new(),
            log_file: log_file.clone(),
        };
        let (process, _) = RunningProcess::start(&spec).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        process.terminate().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let current = fs::metadata(&log_file).unwrap().len();
        let previous = fs::metadata(log_file.with_extension("log.1")).unwrap().len();

        assert!(current <= MAX_LOG_BYTES);
        assert!(previous <= MAX_LOG_BYTES);
        let _ = fs::remove_file(log_file.with_extension("log.1"));
        let _ = fs::remove_file(log_file);
    }

    #[tokio::test]
    async fn terminate_signals_child_processes_in_the_process_group() {
        let log_file = std::env::temp_dir().join(format!("porchlight-process-group-{}", std::process::id()));
        let spec = ProcessSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), "sleep 60 & printf '%s\\n' \"$!\"; wait".to_owned()],
            cwd: None,
            env: IndexMap::new(),
            log_file: log_file.clone(),
        };
        let (process, mut lines) = RunningProcess::start(&spec).unwrap();
        let child =
            tokio::time::timeout(Duration::from_secs(2), lines.recv()).await.unwrap().unwrap().parse::<i32>().unwrap();
        process.terminate().await;

        assert!(
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if kill(Pid::from_raw(child), None).is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok()
        );
        let _ = fs::remove_file(log_file);
    }
}
