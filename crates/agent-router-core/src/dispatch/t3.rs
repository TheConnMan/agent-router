//! The t3 launch surface: a routed job becomes a T3 Code thread, created through the external
//! `t3-thread` launcher instead of a detached provider CLI.
//!
//! Only the launch differs. Classification, every gate, and the decision ran before this module is
//! reached, so the engine, model, and effort a thread gets are exactly what a background launch
//! would have got. MCP scoping is not forwarded (T3 has no MCP flags; the thread inherits the
//! project's servers), `--wait` is never passed (the router returns once the thread exists), and
//! no effective effort is claimed: t3-thread's reported `effort` is its own client-side
//! resolution, not something the backend observed.
//!
//! The launch wait is bounded. t3-thread's RPC calls carry no timeout of their own, so a down or
//! wedged T3 server would otherwise hold `run` forever. The launcher leads its own process group,
//! and at the deadline the whole group is signalled, because a grandchild still holding stdout or
//! stderr would keep the pipe readers blocked long after the direct child died.

use crate::binary::T3_THREAD_BIN_ENV;
use crate::config::Surface;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::Dispatch;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long one launch may take, end to end: the launcher exiting AND both of its output pipes
/// closing. Creating a thread is a handful of local RPC calls, so this only fires on a hang.
pub const T3_LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a signalled launcher gets to exit on SIGTERM before the group is SIGKILLed, and how
/// long the pipe readers then get to reach EOF before they are detached rather than joined.
const SIGNAL_GRACE: Duration = Duration::from_secs(2);
const READER_GRACE: Duration = Duration::from_secs(2);

/// The longest single wait inside the launch loop, so an exit is noticed promptly even while no
/// pipe event arrives.
const POLL_SLICE: Duration = Duration::from_millis(25);

/// The suffix the router's claude models carry for the 1M context window. A T3 claude thread is
/// always 1M, and t3-thread takes the bare model id.
const ONE_MILLION_SUFFIX: &str = "[1m]";

/// IMPURE: launch one routed job as a T3 thread through the resolved `t3-thread`.
pub fn dispatch(
    ctx: &Context,
    cwd: &Path,
    task: &str,
    name: &str,
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Dispatch> {
    let binary = crate::binary::resolve_t3_thread(&ctx.environment)?;
    dispatch_with_binary(
        &binary,
        cwd,
        task,
        name,
        provider,
        model,
        effort,
        T3_LAUNCH_TIMEOUT,
    )
}

/// IMPURE: the launch against an already resolved launcher, bounded by `timeout`.
///
/// The task goes to the launcher on stdin (`--prompt=-`), never argv: it may carry secrets or the
/// `BACKGROUND_RUN=1` marker, and argv is visible to every process on the box. It never touches
/// disk either, so there is no prompt file to clean up or leak on a crash.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_with_binary(
    binary: &Path,
    cwd: &Path,
    task: &str,
    name: &str,
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
    timeout: Duration,
) -> Result<Dispatch> {
    let project = crate::runtime::canonicalize_dir(cwd);
    let mut command = Command::new(binary);
    command
        .current_dir(&project)
        .args(launch_args(&project, provider, model, effort, name))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // The launcher leads its own group, so the deadline can signal everything it started.
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| crate::binary::launch_error(binary, T3_THREAD_BIN_ENV, error))?;
    let launched = wait_bounded(&mut child, task, timeout)?;
    if launched.timed_out {
        return Err(Error::Command(format!(
            "t3-thread did not finish launching within {}s; it was killed. A thread may already \
             exist in T3. t3-thread said: {}",
            timeout.as_secs_f64(),
            stderr_text(&launched.stderr)
        )));
    }
    if !launched.status.success() {
        return Err(exit_failure(
            launched.status.code(),
            &String::from_utf8_lossy(&launched.stderr),
        ));
    }
    let (thread_id, url) = parse_launch(&String::from_utf8_lossy(&launched.stdout))?;
    Ok(Dispatch {
        job_id: Some(thread_id),
        job_name: name.to_string(),
        // t3-thread's `effort` field is its own guess, including a hardcoded fallback for models
        // it does not list; recording it would record a guess as a reading.
        effective_effort: None,
        surface: Surface::T3,
        url: Some(url),
    })
}

/// PURE: the launcher argv, in a fixed order. Every value flag uses the `--flag=value` form,
/// because t3-thread rejects a separate value that starts with `--`, so a title or project path
/// beginning with dashes would otherwise be a usage error. Built from `OsString`s so a non-UTF8
/// project path survives.
pub fn launch_args(
    project: &Path,
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
    title: &str,
) -> Vec<OsString> {
    let flag = |name: &str, value: &std::ffi::OsStr| {
        let mut arg = OsString::from(format!("--{name}="));
        arg.push(value);
        arg
    };
    let mut args = vec![
        flag("project", project.as_os_str()),
        flag("engine", provider.name().as_ref()),
    ];
    if let Some(model) = t3_model(provider, model) {
        args.push(flag("model", model.as_ref()));
    }
    // Grok takes no effort on either surface: the background grok dispatcher sends none, and `run`
    // already refuses `--effort` for grok.
    if let Some(effort) = effort.filter(|_| provider != Provider::Grok) {
        args.push(flag("effort", effort.as_ref()));
    }
    args.push(flag("title", title.as_ref()));
    args.push(OsString::from("--prompt=-"));
    args.push(OsString::from("--json"));
    args
}

/// PURE: the model id t3-thread should get, or None to omit `--model`.
///
/// Claude drops one trailing `[1m]`, since every T3 claude thread is 1M already. What remains is
/// omitted when it is the router's own bare fallback alias, so t3-thread's default model applies:
/// that keeps one owner for what "opus" means inside T3 rather than a second alias table here.
/// Every other value, including other bare aliases, passes through. Codex and Grok are verbatim.
pub fn t3_model(provider: Provider, model: Option<&str>) -> Option<String> {
    let model = model?;
    match provider {
        Provider::Claude => {
            let bare = model.strip_suffix(ONE_MILLION_SUFFIX).unwrap_or(model);
            let fallback = super::claude::DEFAULT_MODEL;
            let fallback = fallback
                .strip_suffix(ONE_MILLION_SUFFIX)
                .unwrap_or(fallback);
            (bare != fallback).then(|| bare.to_string())
        }
        Provider::Codex | Provider::Grok => Some(model.to_string()),
    }
}

/// PURE: the thread id and URL from t3-thread's `--json` answer. Both are required: a launch with
/// no id has no identity to log, and one with no URL cannot say where its thread lives.
pub fn parse_launch(stdout: &str) -> Result<(String, String)> {
    let trimmed = stdout.trim();
    let answer: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|error| Error::Command(format!("t3-thread printed no parseable JSON: {error}")))?;
    let field = |key: &str| {
        answer
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let thread_id = field("threadId")
        .ok_or_else(|| Error::Command(format!("t3-thread printed no thread id: {trimmed}")))?;
    let url = field("url")
        .ok_or_else(|| Error::Command(format!("t3-thread printed no thread url: {trimmed}")))?;
    Ok((thread_id, url))
}

/// PURE: the error a failed launcher exit maps to. Exit 3 is t3-thread refusing a T3 server version
/// it has not verified its RPC calls against, which needs a person, so it says what to do.
fn exit_failure(code: Option<i32>, stderr: &str) -> Error {
    let said = stderr_text(stderr.as_bytes());
    Error::Command(match code {
        Some(3) => format!(
            "t3-thread refused to launch: the local T3 server version is not one t3-thread has \
             verified (exit 3); re-verify the RPC surface and add the version to \
             VERIFIED_T3_VERSIONS in t3-thread's lib/version.mjs. t3-thread said: {said}"
        ),
        Some(code) => format!("t3-thread exited {code}: {said}"),
        None => format!("t3-thread was terminated by a signal: {said}"),
    })
}

/// PURE: captured stderr for a message, trimmed, with an explicit marker when there was none.
fn stderr_text(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        "(no stderr)".to_string()
    } else {
        text.to_string()
    }
}

/// Which helper thread finished.
enum Pipe {
    Stdin,
    Stdout,
    Stderr,
}

/// What one bounded launch produced. `timed_out` means the launcher was killed at the deadline, in
/// which case the status is the kill's and only `stderr` is worth reporting.
struct Launched {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

/// IMPURE: feed the task, drain both pipes, and wait for the launcher, all under one deadline.
///
/// Each pipe runs on its own detached thread that reports completion over a channel rather than
/// being joined, so a pipe a straggler keeps open can never hold this function past the deadline.
/// The child is only ever reaped after it has been signalled, so the group id the signal names
/// cannot have been recycled by an unrelated process in between.
fn wait_bounded(child: &mut Child, task: &str, timeout: Duration) -> Result<Launched> {
    let deadline = Instant::now() + timeout;
    let (done, events) = mpsc::channel();
    if let Some(mut stdin) = child.stdin.take() {
        let task = task.as_bytes().to_vec();
        let done = done.clone();
        std::thread::spawn(move || {
            // A write error means the launcher exited without reading (a usage error, say); its
            // exit status is what decides the launch, so the error itself is not interesting.
            let _ = stdin.write_all(&task);
            drop(stdin);
            let _ = done.send(Pipe::Stdin);
        });
    }
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let mut stdout_open = match child.stdout.take() {
        Some(pipe) => drain(pipe, Arc::clone(&stdout), done.clone(), Pipe::Stdout),
        None => false,
    };
    let mut stderr_open = match child.stderr.take() {
        Some(pipe) => drain(pipe, Arc::clone(&stderr), done.clone(), Pipe::Stderr),
        None => false,
    };
    drop(done);

    let finished = loop {
        if !stdout_open && !stderr_open && has_exited(child) {
            break true;
        }
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        next_event(
            &events,
            (deadline - now).min(POLL_SLICE),
            &mut stdout_open,
            &mut stderr_open,
        );
    };

    let mut timed_out = false;
    if !finished {
        if has_exited(child) && !stdout_open {
            // The launcher answered and exited; only a straggler still holds stderr. The thread
            // exists, so this is a launch, not a timeout: reporting one would invite a retry that
            // creates the thread twice. The group is signalled only to clean the straggler up.
            terminate_group(child, Duration::ZERO);
        } else {
            terminate_group(child, SIGNAL_GRACE);
            timed_out = true;
        }
        let grace = Instant::now() + READER_GRACE;
        while (stdout_open || stderr_open) && Instant::now() < grace {
            next_event(
                &events,
                grace
                    .saturating_duration_since(Instant::now())
                    .min(POLL_SLICE),
                &mut stdout_open,
                &mut stderr_open,
            );
        }
        // Any reader still blocked here is detached: something outside the group holds its pipe,
        // and waiting on it would be exactly the unbounded wait this function exists to prevent.
    }
    let status = child.wait()?;
    let captured = |buffer: &Arc<Mutex<Vec<u8>>>| {
        buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    };
    Ok(Launched {
        status,
        stdout: captured(&stdout),
        stderr: captured(&stderr),
        timed_out,
    })
}

/// IMPURE: copy one pipe into `buffer` on a detached thread, reporting EOF on `done`. Returns true:
/// the pipe is open until that report arrives.
fn drain<R: Read + Send + 'static>(
    mut pipe: R,
    buffer: Arc<Mutex<Vec<u8>>>,
    done: Sender<Pipe>,
    which: Pipe,
) -> bool {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buffer
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // A read error ends the stream just like EOF: nothing more will arrive on it.
                Err(_) => break,
            }
        }
        let _ = done.send(which);
    });
    true
}

/// IMPURE: wait up to `slice` for one helper thread to finish, recording a closed pipe.
fn next_event(
    events: &Receiver<Pipe>,
    slice: Duration,
    stdout_open: &mut bool,
    stderr_open: &mut bool,
) {
    match events.recv_timeout(slice) {
        Ok(Pipe::Stdout) => *stdout_open = false,
        Ok(Pipe::Stderr) => *stderr_open = false,
        Ok(Pipe::Stdin) | Err(RecvTimeoutError::Timeout) => {}
        // Every helper has reported, so only the exit is left to wait for; sleep the slice rather
        // than spin on a channel that now returns at once.
        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(slice),
    }
}

/// IMPURE: whether the launcher has exited, WITHOUT reaping it. The zombie keeps its pid, and so
/// the group id, reserved until `child.wait()`, which is what makes signalling the group after an
/// exit safe. A failed query reads as still running, which the deadline then settles.
#[cfg(unix)]
fn has_exited(child: &Child) -> bool {
    let pid = libc::id_t::from(child.id());
    // SAFETY: an all-zero siginfo_t is a valid value, and waitid only writes into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live, writable siginfo_t; WNOWAIT leaves the child waitable.
    let queried = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // SAFETY: waitid succeeded, so `info` is initialized; si_pid is 0 when nothing has exited.
    queried == 0 && unsafe { info.si_pid() } != 0
}

#[cfg(not(unix))]
fn has_exited(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

/// IMPURE: SIGTERM the launcher's group, give the launcher up to `grace` to exit, then SIGKILL the
/// group so nothing it started outlives the launch. Always before the reap (see `has_exited`).
#[cfg(unix)]
fn terminate_group(child: &Child, grace: Duration) {
    signal_group(child, libc::SIGTERM);
    let until = Instant::now() + grace;
    while !has_exited(child) && Instant::now() < until {
        std::thread::sleep(POLL_SLICE);
    }
    signal_group(child, libc::SIGKILL);
}

#[cfg(not(unix))]
fn terminate_group(child: &mut Child, _grace: Duration) {
    let _ = child.kill();
}

/// IMPURE: signal every process in the launcher's group. ESRCH (the group is already gone) is the
/// expected answer once everything exited and is ignored; no other failure is possible for a
/// group this process created, so the result is not inspected.
#[cfg(unix)]
fn signal_group(child: &Child, signal: libc::c_int) {
    let Ok(group) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    // SAFETY: kill touches no memory; the negative pid names the group `process_group(0)` made.
    let _ = unsafe { libc::kill(-group, signal) };
}
