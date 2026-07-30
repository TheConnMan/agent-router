use crate::error::{Error, Result};
use crate::run::Dispatch;
use crate::runtime::{canonicalize_dir, now_ms, router_log_path, spawn_detached};
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ID_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_MODEL: &str = "opus[1m]";

#[derive(Debug, Deserialize)]
struct AgentRow {
    #[serde(default)]
    id: Option<String>,
    cwd: PathBuf,
    name: String,
    #[serde(default, rename = "startedAt")]
    started_at: i64,
    #[serde(default)]
    kind: String,
}

pub fn dispatch(
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Dispatch> {
    dispatch_with_binary(
        Path::new("claude"),
        cwd,
        task,
        name,
        model,
        effort,
        ID_TIMEOUT,
    )
}

pub fn dispatch_with_binary(
    binary: &Path,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
    timeout: Duration,
) -> Result<Dispatch> {
    let dispatched_at = now_ms();
    let mut command = Command::new(binary);
    command
        .current_dir(cwd)
        .arg("--bg")
        .arg("--model")
        .arg(model.unwrap_or(DEFAULT_MODEL));
    if let Some(effort) = effort {
        command.arg("--effort").arg(effort);
    }
    command.arg("--name").arg(name).arg(task);
    spawn_detached(command, &router_log_path("claude"))?;

    let job_id = resolve_short_id(binary, name, cwd, dispatched_at, timeout);
    Ok(Dispatch {
        job_id,
        job_name: name.to_string(),
    })
}

fn resolve_short_id(
    binary: &Path,
    name: &str,
    cwd: &Path,
    since_ms: i64,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        if let Ok(rows) = list_agents(binary, remaining)
            && let Some(id) = pick_job(&rows, name, cwd, since_ms)
        {
            return Some(id);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

fn list_agents(binary: &Path, timeout: Duration) -> Result<Vec<AgentRow>> {
    let mut command = Command::new(binary);
    command
        .arg("agents")
        .arg("--json")
        .arg("--all")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Command("claude agents gave no stdout pipe".to_string()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Command("claude agents gave no stderr pipe".to_string()))?;
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stdout_tx.send(result);
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stderr_tx.send(result);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Command(
                    "`claude agents --json --all` timed out".to_string(),
                ));
            }
        }
    };
    let stdout = stdout_rx
        .recv_timeout(Duration::from_millis(200))
        .map_err(|_| Error::Command("claude agents stdout was unreadable".to_string()))??;
    let stderr = stderr_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_else(|_| Ok(Vec::new()))
        .unwrap_or_default();
    if !status.success() {
        return Err(Error::Command(format!(
            "`{} agents --json --all` exited {}: {}",
            binary.display(),
            status,
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    serde_json::from_slice(&stdout).map_err(Error::from)
}

fn pick_job(rows: &[AgentRow], name: &str, cwd: &Path, since_ms: i64) -> Option<String> {
    let cwd = canonicalize_dir(cwd);
    rows.iter()
        .filter(|row| row.kind == "background")
        .filter(|row| row.name == name)
        .filter(|row| canonicalize_dir(&row.cwd) == cwd)
        .filter(|row| row.started_at >= since_ms)
        .max_by_key(|row| row.started_at)
        .and_then(|row| row.id.as_deref())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}
