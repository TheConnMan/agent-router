use crate::binary::{CLAUDE_BIN_ENV, Environment};
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::Dispatch;
use crate::runtime::{canonicalize_dir, now_ms, router_log_path, spawn_detached};
use serde::Deserialize;
use std::collections::BTreeMap;
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
    /// What claude says the job is doing: `working`, `done`, or `stopped`. Optional because a
    /// claude that stops printing it must leave the router reading absence rather than failing to
    /// parse the whole list.
    #[serde(default)]
    state: Option<String>,
}

pub fn dispatch(
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
    mcp_configs: &[PathBuf],
    strict_mcp_config: bool,
) -> Result<Dispatch> {
    dispatch_in(
        &Environment::from_process(),
        cwd,
        task,
        name,
        model,
        effort,
        mcp_configs,
        strict_mcp_config,
    )
}

/// IMPURE in `environment` only: the seam the stripped-`PATH` regression tests drive.
///
/// The resolution happens here rather than inside `dispatch_with_binary`, so a test that strips
/// `PATH` exercises the real `resolve` on the real code path. A test that only called
/// `dispatch_with_binary` would stay green with `Path::new("claude")` still at the top of this
/// function, which is the whole defect.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_in(
    environment: &Environment,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
    mcp_configs: &[PathBuf],
    strict_mcp_config: bool,
) -> Result<Dispatch> {
    let binary = crate::binary::resolve(Provider::Claude, environment)?;
    dispatch_with_binary(
        &binary,
        cwd,
        task,
        name,
        model,
        effort,
        mcp_configs,
        strict_mcp_config,
        ID_TIMEOUT,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn dispatch_with_binary(
    binary: &Path,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
    mcp_configs: &[PathBuf],
    strict_mcp_config: bool,
    timeout: Duration,
) -> Result<Dispatch> {
    let resolved_configs = resolve_mcp_configs(mcp_configs)?;

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
    for config in &resolved_configs {
        command.arg("--mcp-config").arg(config);
    }
    if strict_mcp_config {
        // --mcp-config is variadic, so the boolean --strict-mcp-config must follow every path: it
        // terminates the value list and stops --name and the prompt being read as more configs.
        command.arg("--strict-mcp-config");
    }
    command.arg("--name").arg(name).arg(task);
    spawn_detached(command, &router_log_path("claude"), Some(CLAUDE_BIN_ENV))?;

    let job_id = resolve_short_id(binary, name, cwd, dispatched_at, timeout);
    Ok(Dispatch {
        job_id,
        job_name: name.to_string(),
        // Claude reports no effective effort anywhere: it takes `--effort`, warns on a value it
        // does not know, and exits 0 having run at its own default. So nothing was observed, and
        // filling this in from the decided effort or the model would record a guess as a reading.
        effective_effort: None,
    })
}

/// An MCP config can carry server credentials, so it is passed by path only: this proves the path
/// is a readable regular file before a job exists, and never reads a byte of the body. The
/// resolved absolute paths are returned because they, not the paths as typed, are what claude is
/// handed.
fn resolve_mcp_configs(mcp_configs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // Both failure paths below report the same thing, so the message is built in one place and
    // cannot drift.
    let unreadable = |config: &Path, error: std::io::Error| {
        Error::Command(format!(
            "MCP config {} is unreadable: {error}",
            config.display()
        ))
    };
    let mut resolved_configs = Vec::with_capacity(mcp_configs.len());
    for config in mcp_configs {
        // The caller typed the path in their own shell, but the job is spawned with
        // current_dir(cwd), so a relative path would mean two different files. Resolving against
        // the router cwd once, lexically, keeps the preflighted file and the file on the argv the
        // same one, which is why the resolved path is what claude is handed.
        let config = std::path::absolute(config).map_err(|error| {
            Error::Command(format!(
                "MCP config {} is unresolvable: {error}",
                config.display()
            ))
        })?;
        // metadata follows symlinks and never blocks, unlike opening a fifo with no writer, which
        // would hang the router: so the kind is checked before anything is opened.
        let metadata = std::fs::metadata(&config).map_err(|error| unreadable(&config, error))?;
        if !metadata.is_file() {
            return Err(Error::Command(format!(
                "MCP config {} is not a regular file",
                config.display()
            )));
        }
        // A regular file can still be unreadable, and that must fail here rather than inside the
        // detached child.
        std::fs::File::open(&config).map_err(|error| unreadable(&config, error))?;
        resolved_configs.push(config);
    }
    Ok(resolved_configs)
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

/// IMPURE: every job the recent list knows, short id to the state claude reports for it.
///
/// The `--all` flag on the list below is load bearing: without it the list is running jobs only, so
/// every finished job would read as absent. The list is still a bounded recent window, so a job
/// missing from this map is a job the router cannot resolve, never a job that completed.
pub fn agent_states(timeout: Duration) -> Result<BTreeMap<String, String>> {
    agent_states_in(&Environment::from_process(), timeout)
}

/// IMPURE in `environment` only: `agent_states`' resolution seam.
///
/// This is a SECOND claude entry point with its own resolution, reached from `status.rs`. Without
/// it, reconciliation would keep calling `execvp("claude")` while dispatch was fixed, and every job
/// in the window would read as unresolvable rather than as unread.
pub fn agent_states_in(
    environment: &Environment,
    timeout: Duration,
) -> Result<BTreeMap<String, String>> {
    let binary = crate::binary::resolve(Provider::Claude, environment)?;
    let rows = list_agents(&binary, timeout)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id = row.id.filter(|id| !id.is_empty())?;
            Some((id, row.state?))
        })
        .collect())
}

fn list_agents(binary: &Path, timeout: Duration) -> Result<Vec<AgentRow>> {
    let mut command = Command::new(binary);
    command
        .arg("agents")
        .arg("--json")
        .arg("--all")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Not `?`: that conversion is `Error::Io`, whose `Display` is the production string. A binary
    // that resolved and then vanished before the exec is still a launch failure.
    let mut child = command
        .spawn()
        .map_err(|error| crate::binary::launch_error(binary, CLAUDE_BIN_ENV, error))?;
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
