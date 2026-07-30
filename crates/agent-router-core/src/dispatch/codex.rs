use crate::error::{Error, Result};
use crate::run::Dispatch;
use crate::runtime::{home_dir, truncated_title};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const READ_SLICE: Duration = Duration::from_millis(250);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
const OUTPUT_GRACE: Duration = Duration::from_millis(250);
const HANDSHAKE_URL: &str = "ws://localhost/rpc";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Daemon {
    socket_path: PathBuf,
}

pub trait CodexRpc {
    fn request(&mut self, request_id: i64, request: &str) -> Result<String>;
}

#[derive(Debug)]
pub enum SpawnAttempt {
    Started(String),
    TurnFailed { thread_id: String, error: Error },
    NotCreated(Error),
}

pub fn initialize_request(id: i64) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "agent-router",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {"experimentalApi": true}
        }
    })
    .to_string()
}

pub fn thread_start_request(id: i64, cwd: &Path, model: Option<&str>) -> String {
    let mut params = serde_json::json!({
        "cwd": cwd.to_string_lossy(),
        "sandbox": "danger-full-access",
        "approvalPolicy": "never"
    });
    if let (Some(model), Some(params)) = (model, params.as_object_mut()) {
        params.insert("model".to_string(), serde_json::json!(model));
    }
    request(id, "thread/start", params)
}

pub fn turn_start_request(id: i64, thread_id: &str, task: &str, effort: Option<&str>) -> String {
    let mut params = serde_json::json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": task}]
    });
    if let (Some(effort), Some(params)) = (effort, params.as_object_mut()) {
        params.insert("effort".to_string(), serde_json::json!(effort));
    }
    request(id, "turn/start", params)
}

fn request(id: i64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
    .to_string()
}

pub fn parse_thread_id(line: &str, expected_id: i64) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("id").and_then(serde_json::Value::as_i64) != Some(expected_id)
        || value.get("error").is_some_and(|error| !error.is_null())
    {
        return None;
    }
    value
        .pointer("/result/thread/id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub fn spawn_on_initialized_rpc(
    rpc: &mut impl CodexRpc,
    cwd: &Path,
    task: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> SpawnAttempt {
    let thread_response = match rpc.request(2, &thread_start_request(2, cwd, model)) {
        Ok(response) => response,
        Err(error) => return SpawnAttempt::NotCreated(error),
    };
    let Some(thread_id) = parse_thread_id(&thread_response, 2) else {
        return SpawnAttempt::NotCreated(Error::Command(format!(
            "app-server thread/start returned no thread id: {thread_response}"
        )));
    };
    match rpc.request(3, &turn_start_request(3, &thread_id, task, effort)) {
        Ok(_) => SpawnAttempt::Started(thread_id),
        Err(error) => SpawnAttempt::TurnFailed { thread_id, error },
    }
}

pub fn dispatch(
    cwd: &Path,
    task: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Dispatch> {
    #[cfg(target_os = "linux")]
    {
        let daemon = ensure_daemon().map_err(Error::Command)?;
        let mut client = Client::connect(&daemon)?;
        match spawn_on_initialized_rpc(&mut client, cwd, task, model, effort) {
            SpawnAttempt::Started(thread_id) => Ok(Dispatch {
                job_id: Some(thread_id),
                job_name: truncated_title(task),
            }),
            SpawnAttempt::TurnFailed { thread_id, error } => Err(Error::Command(format!(
                "app-server started thread {thread_id} but its first turn failed: {error}"
            ))),
            SpawnAttempt::NotCreated(error) => Err(Error::Command(format!(
                "codex app-server spawn failed: {error}"
            ))),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cwd, task, model, effort);
        Err(Error::Command(
            "codex app-server daemon transport is unavailable on this platform".to_string(),
        ))
    }
}

fn parse_daemon_version(stdout: &str) -> Option<Daemon> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    if value.get("status").and_then(serde_json::Value::as_str) != Some("running") {
        return None;
    }
    let socket_path = value
        .get("socketPath")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())?;
    Some(Daemon {
        socket_path: PathBuf::from(socket_path),
    })
}

fn stable_daemon_cwd() -> PathBuf {
    let home = home_dir();
    if home.is_dir() {
        home
    } else {
        PathBuf::from("/")
    }
}

fn daemon_command(action: &str) -> Command {
    let mut command = Command::new("codex");
    command
        .arg("app-server")
        .arg("daemon")
        .arg(action)
        .current_dir(stable_daemon_cwd());
    command
}

fn probe_daemon() -> Option<Daemon> {
    let stdout = run_reporting_failure(daemon_command("version"), PROBE_TIMEOUT).ok()?;
    parse_daemon_version(&stdout)
}

fn ensure_daemon() -> std::result::Result<Daemon, String> {
    if let Some(daemon) = probe_daemon() {
        return Ok(daemon);
    }
    let deadline = Instant::now() + START_TIMEOUT;
    if let Err(error) = run_reporting_failure(daemon_command("start"), START_TIMEOUT) {
        return probe_daemon().ok_or(error);
    }
    loop {
        if let Some(daemon) = probe_daemon() {
            return Ok(daemon);
        }
        if Instant::now() >= deadline {
            return Err(
                "`codex app-server daemon start` reported success but no daemon answered"
                    .to_string(),
            );
        }
        std::thread::sleep(
            Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn run_reporting_failure(
    mut command: Command,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let description = describe(&command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run `{description}`: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("`{description}` gave no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("`{description}` gave no stderr pipe"))?;
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = stdout_tx.send(read_limited(stdout));
    });
    std::thread::spawn(move || {
        let _ = stderr_tx.send(read_limited(stderr));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(20)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`{description}` timed out after {timeout:?}"));
            }
            Err(error) => {
                return Err(format!("could not wait for `{description}`: {error}"));
            }
        }
    };
    let stdout = stdout_rx
        .recv_timeout(OUTPUT_GRACE)
        .map_err(|_| format!("`{description}` stdout was unreadable"))?
        .map_err(|error| format!("`{description}` stdout was unreadable: {error}"))?;
    let stderr = stderr_rx
        .recv_timeout(OUTPUT_GRACE)
        .unwrap_or_else(|_| Ok(Vec::new()))
        .unwrap_or_default();
    if !status.success() {
        let why = String::from_utf8_lossy(&stderr).trim().to_string();
        let suffix = if why.is_empty() {
            String::new()
        } else {
            format!(": {why}")
        };
        return Err(format!("`{description}` exited {status}{suffix}"));
    }
    String::from_utf8(stdout).map_err(|_| format!("`{description}` printed non utf8 stdout"))
}

fn read_limited(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    const LIMIT: u64 = 64 * 1024;
    let mut bytes = Vec::new();
    reader.by_ref().take(LIMIT).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn describe(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().to_string()];
    parts.extend(
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string()),
    );
    parts.join(" ")
}

#[cfg(target_os = "linux")]
struct Client {
    socket: tungstenite::WebSocket<std::os::unix::net::UnixStream>,
    deadline: Instant,
}

#[cfg(target_os = "linux")]
impl Client {
    fn connect(daemon: &Daemon) -> Result<Self> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        let stream = std::os::unix::net::UnixStream::connect(&daemon.socket_path)?;
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let (socket, _) = tungstenite::client::client(HANDSHAKE_URL, stream)
            .map_err(|error| Error::Command(format!("app-server handshake failed: {error}")))?;
        socket.get_ref().set_read_timeout(Some(READ_SLICE))?;
        let mut client = Self { socket, deadline };
        client.request(1, &initialize_request(1))?;
        Ok(client)
    }

    fn read_line(&mut self) -> Result<String> {
        loop {
            remaining(self.deadline)?;
            match self.socket.read() {
                Ok(tungstenite::Message::Text(text)) => return Ok(text.to_string()),
                Ok(tungstenite::Message::Close(_)) => {
                    return Err(Error::Command(
                        "app-server closed the connection".to_string(),
                    ));
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    return Err(Error::Command(format!("app-server read failed: {error}")));
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl CodexRpc for Client {
    fn request(&mut self, request_id: i64, request: &str) -> Result<String> {
        self.socket
            .send(tungstenite::Message::text(request.to_string()))
            .map_err(|error| Error::Command(format!("app-server write failed: {error}")))?;
        loop {
            let line = self.read_line()?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_i64) != Some(request_id) {
                continue;
            }
            if value.get("error").is_some_and(|error| !error.is_null()) {
                return Err(Error::Command(format!("app-server error: {line}")));
            }
            return Ok(line);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.socket.get_ref().set_write_timeout(Some(CLOSE_TIMEOUT));
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

#[cfg(target_os = "linux")]
fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(Error::Command("app-server request timed out".to_string()))
    } else {
        Ok(remaining)
    }
}
