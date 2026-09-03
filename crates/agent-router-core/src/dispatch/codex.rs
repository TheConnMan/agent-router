use crate::binary::{CODEX_BIN_ENV, Environment};
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::Dispatch;
use crate::runtime::home_dir;
use crate::status::Observation;
use std::collections::BTreeMap;
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
pub(crate) struct Daemon {
    socket_path: PathBuf,
}

pub trait CodexRpc {
    fn request(&mut self, request_id: i64, request: &str) -> Result<String>;
}

#[derive(Debug)]
pub enum SpawnAttempt {
    Started {
        thread_id: String,
        /// The effort the daemon reported the thread resolved to, straight off the `thread/start`
        /// reply. None when that reply said nothing, which means the router does not know rather
        /// than that the job runs at no effort.
        effective_effort: Option<String>,
    },
    TurnFailed {
        thread_id: String,
        error: Error,
    },
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

pub fn thread_set_name_request(id: i64, thread_id: &str, name: &str) -> String {
    request(
        id,
        "thread/name/set",
        serde_json::json!({
            "threadId": thread_id,
            "name": name
        }),
    )
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

/// The read the reconciler runs. `includeTurns` is load bearing and must not be dropped: without it
/// the reply carries no turn record, every thread falls through to the status fallback, and a job
/// that provably finished reports as unknown.
pub fn thread_read_request(id: i64, thread_id: &str) -> String {
    request(
        id,
        "thread/read",
        serde_json::json!({
            "threadId": thread_id,
            "includeTurns": true
        }),
    )
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
    read_field(line, expected_id, "/result/thread/id")
}

/// PURE: the status of the routed turn, which is turn 0.
///
/// Index 0 by design and never the last turn: the router calls `turn/start` exactly once, so turn 0
/// is the job it dispatched. Later turns exist only because a human continued the thread by hand,
/// and a human's failed continuation is a fact about that follow up work rather than about where
/// the task was sent. None when the reply carries no turn, which is what makes the thread status
/// below a fallback rather than a competing source.
pub fn parse_first_turn_status(line: &str, expected_id: i64) -> Option<String> {
    read_field(line, expected_id, "/result/thread/turns/0/status")
}

/// PURE: the thread's own status, read only when there is no turn record to read instead.
pub fn parse_thread_status(line: &str, expected_id: i64) -> Option<String> {
    read_field(line, expected_id, "/result/thread/status/type")
}

/// PURE: the reasoning effort the daemon resolved this thread to, off the `thread/start` reply.
///
/// A SIBLING of `thread` at the result root, not a member of it, which is why the pointer stops at
/// `/result/reasoningEffort`. It rides the reply the spawn already reads for the thread id, so
/// observing it costs no extra call. None when the reply does not carry it: that is the router not
/// knowing, and nothing here may substitute the decided effort, the model, or a config value for an
/// answer the daemon did not give.
pub fn parse_reasoning_effort(line: &str, expected_id: i64) -> Option<String> {
    read_field(line, expected_id, "/result/reasoningEffort")
}

/// PURE: one string out of a reply that is genuinely an answer to this request. A reply to someone
/// else's request and a reply carrying a JSON RPC error are not observations.
fn read_field(line: &str, expected_id: i64, pointer: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("id").and_then(serde_json::Value::as_i64) != Some(expected_id)
        || value.get("error").is_some_and(|error| !error.is_null())
    {
        return None;
    }
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .filter(|field| !field.is_empty())
        .map(str::to_string)
}

/// IMPURE: what the app-server knows about each thread, one `thread/read` apiece.
///
/// This probes for a daemon and never starts one: a command reporting on jobs that already exist
/// must not create the state it is reporting on. No daemon means every thread reports unavailable,
/// which is the honest partial report rather than a command failure.
pub fn thread_states(thread_ids: &[String]) -> BTreeMap<String, Observation> {
    #[cfg(target_os = "linux")]
    {
        if let Some(daemon) = probe_daemon()
            && let Ok(mut client) = Client::connect(&daemon)
        {
            return thread_states_on_rpc(&mut client, thread_ids);
        }
    }
    thread_ids
        .iter()
        .map(|thread_id| (thread_id.clone(), Observation::Unavailable))
        .collect()
}

/// IMPURE only in the RPC it is handed: one read per thread, each folded on its own.
///
/// A failing read records `Unavailable` for that thread and the loop continues, so one bad thread
/// never costs the rest of the window its reconciliation. Request ids count from 2 because the
/// handshake owns id 1.
pub fn thread_states_on_rpc(
    rpc: &mut impl CodexRpc,
    thread_ids: &[String],
) -> BTreeMap<String, Observation> {
    let mut states = BTreeMap::new();
    for (offset, thread_id) in thread_ids.iter().enumerate() {
        let request_id = i64::try_from(offset).unwrap_or(i64::MAX).saturating_add(2);
        let observation = match rpc.request(request_id, &thread_read_request(request_id, thread_id))
        {
            Ok(line) => observe(&line, request_id),
            Err(_) => Observation::Unavailable,
        };
        states.insert(thread_id.clone(), observation);
    }
    states
}

/// PURE: the turn record if there is one, and the thread status only if there is not.
fn observe(line: &str, request_id: i64) -> Observation {
    if let Some(status) = parse_first_turn_status(line, request_id) {
        return Observation::CodexTurn(status);
    }
    match parse_thread_status(line, request_id) {
        Some(status) => Observation::CodexThread(status),
        None => Observation::Unavailable,
    }
}

pub fn spawn_on_initialized_rpc(
    rpc: &mut impl CodexRpc,
    cwd: &Path,
    task: &str,
    name: &str,
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
    // Read off the reply already in hand rather than asking again: the daemon loads the operator's
    // own config, so this is the resolved effort the turn will actually run at.
    let effective_effort = parse_reasoning_effort(&thread_response, 2);
    // The thread is already running, so a rejected name must not cost the caller its identity.
    let _ = rpc.request(3, &thread_set_name_request(3, &thread_id, name));
    match rpc.request(4, &turn_start_request(4, &thread_id, task, effort)) {
        Ok(_) => SpawnAttempt::Started {
            thread_id,
            effective_effort,
        },
        Err(error) => SpawnAttempt::TurnFailed { thread_id, error },
    }
}

pub fn dispatch(
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Dispatch> {
    dispatch_in(&Environment::from_process(), cwd, task, name, model, effort)
}

/// IMPURE in `environment` only: the resolution seam.
///
/// Codex resolves at the top: `probe_daemon` always execs `codex app-server daemon
/// version`, so a daemon that is already running does not spare this path the binary. Resolving
/// once here also keeps `daemon_command`'s poll loop from re-resolving on every iteration.
pub fn dispatch_in(
    environment: &Environment,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Dispatch> {
    let binary = crate::binary::resolve(Provider::Codex, environment)?;
    #[cfg(target_os = "linux")]
    {
        let daemon = ensure_daemon(&binary)?;
        let mut client = Client::connect(&daemon)?;
        match spawn_on_initialized_rpc(&mut client, cwd, task, name, model, effort) {
            SpawnAttempt::Started {
                thread_id,
                effective_effort,
            } => Ok(Dispatch {
                job_id: Some(thread_id),
                job_name: name.to_string(),
                effective_effort,
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
        let _ = (binary, cwd, task, name, model, effort);
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

fn daemon_command(binary: &Path, action: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .arg("app-server")
        .arg("daemon")
        .arg(action)
        .current_dir(stable_daemon_cwd());
    command
}

/// IMPURE: the running app-server daemon, or None when none answers. This only asks, so a caller
/// that must not create the state it is reporting on (doctor, status) uses it rather than
/// `ensure_daemon`.
pub(crate) fn probe_daemon() -> Option<Daemon> {
    // The signature stays argument-free on purpose. `probe_daemon` has three callers — the two in
    // this file and `doctor.rs`, which is another stream's file — so resolving INSIDE it is what
    // keeps this change off doctor's ledger. A resolution failure here is `None`, which is exactly
    // right: to something that is only asking, an unlaunchable codex means no daemon answers. The
    // loud, named failure belongs to `ensure_daemon`, which is trying to start one.
    let binary = crate::binary::resolve(Provider::Codex, &Environment::from_process()).ok()?;
    // The one place a probe's launch failure is discarded, and the only one that may be: this
    // wrapper is the observation-only entry point, and to something that is merely asking, an
    // unlaunchable codex means no daemon answers.
    probe_daemon_with_binary(&binary).ok().flatten()
}

/// IMPURE: the same probe against an already-resolved binary, so a caller inside a poll loop
/// resolves once rather than once per iteration.
///
/// `Ok(None)` is the probe's own answer — "no daemon is up" — and it covers every way the version
/// command can run and decline to name one, including a nonzero exit and the probe deadline.
/// `Err` is reserved for a launch failure, because that is not an answer about the daemon at all:
/// it says the CLI could not be executed. Swallowing it here made `ensure_daemon` poll a binary
/// that could never answer and then persist an `Error::Command` timeout, which is precisely the
/// laundering the decision log's `error: launch failed:` prefix exists to catch.
fn probe_daemon_with_binary(binary: &Path) -> Result<Option<Daemon>> {
    match run_reporting_failure(daemon_command(binary, "version"), PROBE_TIMEOUT) {
        Ok(stdout) => Ok(parse_daemon_version(&stdout)),
        Err(launch @ Error::Launch(_)) => Err(launch),
        Err(_) => Ok(None),
    }
}

/// IMPURE: the running daemon, started if one is not up.
///
/// Carries the crate `Error` rather than a `String`: `dispatch` used to consume this with
/// `.map_err(Error::Command)`, which flattened a launch failure into an undifferentiated
/// `Error::Command` even when its message read correctly — invisible to any substring test, and
/// the exact laundering the decision log's `error: launch failed:` prefix exists to catch.
fn ensure_daemon(binary: &Path) -> Result<Daemon> {
    if let Some(daemon) = probe_daemon_with_binary(binary)? {
        return Ok(daemon);
    }
    let deadline = Instant::now() + START_TIMEOUT;
    if let Err(error) = run_reporting_failure(daemon_command(binary, "start"), START_TIMEOUT) {
        // A daemon another process already started answers regardless of why this start failed.
        return match probe_daemon_with_binary(binary)? {
            Some(daemon) => Ok(daemon),
            None => Err(error),
        };
    }
    loop {
        if let Some(daemon) = probe_daemon_with_binary(binary)? {
            return Ok(daemon);
        }
        if Instant::now() >= deadline {
            return Err(Error::Command(
                "`codex app-server daemon start` reported success but no daemon answered"
                    .to_string(),
            ));
        }
        std::thread::sleep(
            Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// IMPURE: run one codex command to completion under a deadline.
///
/// Carries the crate `Error` so a launch failure survives the trip to `ensure_daemon` and on to the
/// decision row. Every other failure keeps its message byte-identically inside `Error::Command`,
/// whose `Display` is `{0}`, so nothing an operator reads changes.
fn run_reporting_failure(mut command: Command, timeout: Duration) -> Result<String> {
    let description = describe(&command);
    // Read before the spawn borrows the command: the mapper needs the program either way.
    let program = PathBuf::from(command.get_program());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| crate::binary::launch_error(&program, CODEX_BIN_ENV, error))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Command(format!("`{description}` gave no stdout pipe")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Command(format!("`{description}` gave no stderr pipe")))?;
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
                return Err(Error::Command(format!(
                    "`{description}` timed out after {timeout:?}"
                )));
            }
            Err(error) => {
                return Err(Error::Command(format!(
                    "could not wait for `{description}`: {error}"
                )));
            }
        }
    };
    let stdout = stdout_rx
        .recv_timeout(OUTPUT_GRACE)
        .map_err(|_| Error::Command(format!("`{description}` stdout was unreadable")))?
        .map_err(|error| {
            Error::Command(format!("`{description}` stdout was unreadable: {error}"))
        })?;
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
        return Err(Error::Command(format!(
            "`{description}` exited {status}{suffix}"
        )));
    }
    String::from_utf8(stdout)
        .map_err(|_| Error::Command(format!("`{description}` printed non utf8 stdout")))
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

/// The client owns no deadline of its own. `RPC_TIMEOUT` is the budget for ONE request, granted
/// afresh on each call, because a status sweep issues one `thread/read` per codex row down a single
/// connection: on one shared budget a single slow read spends what is left of it and every later
/// read then fails without being asked, reporting unavailable for rows the daemon would have
/// answered. A lone request still gets exactly `RPC_TIMEOUT` either way.
#[cfg(target_os = "linux")]
struct Client {
    socket: tungstenite::WebSocket<std::os::unix::net::UnixStream>,
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
        let mut client = Self { socket };
        client.request(1, &initialize_request(1))?;
        Ok(client)
    }

    fn read_line(&mut self, deadline: Instant) -> Result<String> {
        loop {
            remaining(deadline)?;
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
        let deadline = Instant::now() + RPC_TIMEOUT;
        self.socket
            .send(tungstenite::Message::text(request.to_string()))
            .map_err(|error| Error::Command(format!("app-server write failed: {error}")))?;
        loop {
            let line = self.read_line(deadline)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// PURE-ish: the first of these that exists, so the fixture does not pin one distro's layout.
    fn first_existing(candidates: &[&str]) -> PathBuf {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.is_file())
            .expect("a test fixture binary must exist")
    }

    /// The probe used to answer `Option`, which made an unlaunchable codex indistinguishable from
    /// a codex that answered "no daemon". `ensure_daemon` then polled to its deadline and
    /// persisted `Error::Command`, hiding the launch failure the decision row exists to show.
    /// The variant is the whole assertion: both spellings produce a readable message.
    #[test]
    fn a_probe_of_a_missing_binary_is_a_launch_failure_not_an_absent_daemon() {
        let error = probe_daemon_with_binary(Path::new("/nonexistent/agent-router-codex"))
            .expect_err("a missing binary cannot be probed");

        assert!(
            matches!(error, Error::Launch(_)),
            "an unlaunchable codex is a launch failure, not a silent absence: {error:?}"
        );
    }

    /// The bound: a probe that RAN and did not name a daemon is an answer, not a failure, so
    /// `ensure_daemon` still goes on to start one rather than aborting the dispatch.
    #[test]
    fn a_probe_that_runs_and_names_no_daemon_is_an_absent_daemon() {
        let ran = first_existing(&["/bin/echo", "/usr/bin/echo"]);

        assert_eq!(
            probe_daemon_with_binary(&ran).expect("a probe that ran is not a failure"),
            None
        );
    }

    /// The observation-only wrapper is the one place that discard is correct, and its
    /// zero-argument signature is what keeps `doctor` and `thread_states` off this change.
    #[test]
    fn the_public_probe_stays_argument_free_and_swallows_its_failures() {
        let probe: fn() -> Option<Daemon> = probe_daemon;
        let _ = probe;
    }
}
