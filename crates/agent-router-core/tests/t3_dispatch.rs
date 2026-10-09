//! The t3 launch surface: `--surface t3` hands a routed job to the external `t3-thread` launcher
//! instead of a detached provider CLI.
//!
//! Everything here is hermetic. The launcher is always a recording shell stub written into a temp
//! dir: it writes its argv one token per line and its stdin byte for byte, prints canned JSON, and
//! exits with a baked-in code. No real `~/.t3`, no real HOME, no real provider is ever reached, and
//! every `Environment` is built from data with `Environment::new`, never from the process.
//!
//! Routing is not re-tested here: surface is a property of the launch step only, so these tests
//! pin the launch contract (argv shape, stdin task, exit mapping, bounded wait, resolver) and the
//! seams in `run_with` that read the surface back (log row, naming skip, MCP warning).

#![cfg(unix)]

mod common;

use agent_router_core::binary::{
    CLAUDE_BIN_ENV, Environment, T3_THREAD_BIN_ENV, resolve_t3_thread,
};
use agent_router_core::config::{ClassifierEngine, Config, Surface};
use agent_router_core::dispatch::t3::{dispatch_with_binary, launch_args, parse_launch, t3_model};
use agent_router_core::log::DecisionLog;
use agent_router_core::run::{Dispatch, Outcome, Request, run_with};
use agent_router_core::runtime::short_job_name;
use agent_router_core::usage::UsageSnapshot;
use agent_router_core::{Context, Error, Provider, Result};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The thread id and URL every succeeding stub reports.
const THREAD_ID: &str = "thr-123";
const THREAD_URL: &str = "http://127.0.0.1:3773/t/thr-123";

/// A generous launch bound for the cases that are not about the bound. The stub answers at once,
/// so this only has to be long enough never to fire under load.
const GENEROUS: Duration = Duration::from_secs(30);

/// The canned stdout of a successful launch, shaped like t3-thread's real `--json` answer, with
/// extra keys the parser must ignore.
fn launched_json() -> String {
    json!({
        "threadId": THREAD_ID,
        "url": THREAD_URL,
        "projectId": "proj-1",
        "engine": "codex",
        "model": "gpt-6-sol",
        "effort": "high",
    })
    .to_string()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// A recording t3-thread stub and the files it writes.
struct Launcher {
    binary: PathBuf,
    argv: PathBuf,
    stdin: PathBuf,
}

impl Launcher {
    /// The argv the stub recorded, one token per element. Panics when the stub never ran.
    fn argv(&self) -> Vec<String> {
        fs::read_to_string(&self.argv)
            .unwrap_or_else(|error| panic!("the t3-thread stub never ran: {error}"))
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn stdin(&self) -> String {
        fs::read_to_string(&self.stdin).expect("the stub recorded its stdin")
    }

    fn was_invoked(&self) -> bool {
        self.argv.exists()
    }
}

/// Write a t3-thread stub under `dir` that records argv and stdin, prints `stdout` and `stderr`,
/// and exits `code`. Output is held in files the stub cats rather than inlined, so any bytes
/// survive the shell untouched.
fn launcher(dir: &Path, stdout: &str, stderr: &str, code: i32) -> Launcher {
    fs::create_dir_all(dir).expect("create the stub directory");
    let argv = dir.join("t3.argv");
    let stdin = dir.join("t3.stdin");
    let out = dir.join("t3.out");
    let err = dir.join("t3.err");
    fs::write(&out, stdout).expect("write the canned stdout");
    fs::write(&err, stderr).expect("write the canned stderr");
    let binary = dir.join("t3-thread");
    common::write_stub(
        &binary,
        &format!(
            "printf '%s\\n' \"$@\" > {argv}\ncat > {stdin}\ncat {err} >&2\ncat {out}\nexit {code}\n",
            argv = shell_quote(&argv.to_string_lossy()),
            stdin = shell_quote(&stdin.to_string_lossy()),
            err = shell_quote(&err.to_string_lossy()),
            out = shell_quote(&out.to_string_lossy()),
        ),
    );
    Launcher {
        binary,
        argv,
        stdin,
    }
}

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn command_message(error: &Error) -> String {
    match error {
        Error::Command(message) => message.clone(),
        other => panic!("expected Error::Command, got {other:?}"),
    }
}

fn launch_message(error: &Error) -> String {
    match error {
        Error::Launch(message) => message.clone(),
        other => panic!("expected Error::Launch, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------- t3_model (AC2)

/// AC2. T3 claude threads are always 1M, so the router's `[1m]` suffix is stripped; the router's
/// own bare fallback alias `opus` is omitted so t3-thread's default applies; every other claude
/// value passes through; None stays None; codex and grok are verbatim.
#[test]
fn t3_model_strips_the_1m_suffix_omits_the_bare_opus_alias_and_passes_other_values_through() {
    assert_eq!(
        t3_model(Provider::Claude, Some("claude-opus-5-5[1m]")).as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(
        t3_model(Provider::Claude, Some("claude-sonnet-5[1m]")).as_deref(),
        Some("claude-sonnet-5")
    );
    assert_eq!(t3_model(Provider::Claude, Some("opus[1m]")), None);
    assert_eq!(t3_model(Provider::Claude, Some("opus")), None);
    assert_eq!(
        t3_model(Provider::Claude, Some("claude-opus-5-5")).as_deref(),
        Some("claude-opus-5-5")
    );
    assert_eq!(
        t3_model(Provider::Claude, Some("sonnet")).as_deref(),
        Some("sonnet"),
        "only the router's own fallback alias is delegated; other bare aliases pass through"
    );
    assert_eq!(t3_model(Provider::Claude, None), None);
    assert_eq!(
        t3_model(Provider::Codex, Some("gpt-6-sol")).as_deref(),
        Some("gpt-6-sol")
    );
    assert_eq!(
        t3_model(Provider::Grok, Some("grok-4")).as_deref(),
        Some("grok-4")
    );
    assert_eq!(t3_model(Provider::Codex, None), None);
    assert_eq!(t3_model(Provider::Grok, None), None);
}

// ---------------------------------------------------------------------------- launch_args (AC2)

/// AC2. The exact claude argv: every value flag in `--flag=value` form, the model translated, the
/// task nowhere in it (it goes over stdin), and no `--wait`.
#[test]
fn launch_args_for_claude_is_the_exact_equals_form_vector_with_the_translated_model() {
    let args = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Claude,
        Some("claude-opus-5-5[1m]"),
        Some("high"),
        "RS-1 Fixture Job",
    ));
    assert_eq!(
        args,
        vec![
            "--project=/work/repo",
            "--engine=claude",
            "--model=claude-opus-5-5",
            "--effort=high",
            "--title=RS-1 Fixture Job",
            "--prompt=-",
            "--json",
        ]
    );
}

/// AC2. Codex model and effort pass through verbatim.
#[test]
fn launch_args_for_codex_passes_model_and_effort_through_verbatim() {
    let args = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Codex,
        Some("gpt-6-sol"),
        Some("xhigh"),
        "Codex Job",
    ));
    assert_eq!(
        args,
        vec![
            "--project=/work/repo",
            "--engine=codex",
            "--model=gpt-6-sol",
            "--effort=xhigh",
            "--title=Codex Job",
            "--prompt=-",
            "--json",
        ]
    );
}

/// AC2. Grok takes no effort on either surface, so an effort handed in is still omitted.
#[test]
fn launch_args_for_grok_never_carries_an_effort_even_when_one_is_given() {
    let args = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Grok,
        Some("grok-4"),
        Some("high"),
        "Grok Job",
    ));
    assert_eq!(
        args,
        vec![
            "--project=/work/repo",
            "--engine=grok",
            "--model=grok-4",
            "--title=Grok Job",
            "--prompt=-",
            "--json",
        ]
    );
    assert!(
        !args.iter().any(|arg| arg.starts_with("--effort")),
        "grok argv carried an effort: {args:?}"
    );
}

/// AC2. An omitted model (the bare `opus` alias, or no model at all) and an omitted effort drop
/// their flags entirely rather than sending an empty value.
#[test]
fn launch_args_omits_model_and_effort_flags_that_have_no_value() {
    let aliased = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Claude,
        Some("opus[1m]"),
        None,
        "Aliased",
    ));
    assert_eq!(
        aliased,
        vec![
            "--project=/work/repo",
            "--engine=claude",
            "--title=Aliased",
            "--prompt=-",
            "--json",
        ]
    );
    let bare = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Codex,
        None,
        None,
        "Bare",
    ));
    assert_eq!(
        bare,
        vec![
            "--project=/work/repo",
            "--engine=codex",
            "--title=Bare",
            "--prompt=-",
            "--json",
        ]
    );
}

/// A Codex `/implement` task with no `--effort` reaches the launcher as `effort: None`. That has
/// to mean "no override" at the T3 boundary, so t3-thread leaves Codex at its configured default;
/// any `--effort` token here, even an empty one, would pin a level the user never asked for.
#[test]
fn launch_args_for_codex_with_no_effort_sends_no_effort_flag_while_an_effort_does() {
    let unset = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Codex,
        Some("gpt-6-sol"),
        None,
        "Default Effort",
    ));
    assert!(
        !unset.iter().any(|arg| arg.contains("effort")),
        "codex launch with no effort carried an effort argument: {unset:?}"
    );
    let pinned = strings(&launch_args(
        Path::new("/work/repo"),
        Provider::Codex,
        Some("gpt-6-sol"),
        Some("high"),
        "Pinned Effort",
    ));
    assert!(
        pinned.contains(&"--effort=high".to_string()),
        "codex launch with an effort dropped it: {pinned:?}"
    );
}

/// AC2, decision 6. t3-thread rejects a separate value starting with `--`, so a title or project
/// path that starts with dashes must stay inside its own `=` token. The last three tokens are
/// always title, stdin prompt, json; `--wait` and any MCP token never appear.
#[test]
fn launch_args_keeps_dash_leading_values_inside_their_equals_token_and_never_waits() {
    let args = strings(&launch_args(
        Path::new("--weird/project"),
        Provider::Codex,
        Some("gpt-6-sol"),
        Some("high"),
        "--looks like a flag",
    ));
    assert_eq!(args[0], "--project=--weird/project");
    let tail = &args[args.len() - 3..];
    assert_eq!(
        tail,
        ["--title=--looks like a flag", "--prompt=-", "--json"]
    );
    assert!(
        !args.iter().any(|arg| arg.starts_with("--wait")),
        "t3 launches must never wait: {args:?}"
    );
    assert!(
        !args.iter().any(|arg| arg.contains("mcp")),
        "t3 has no MCP flags: {args:?}"
    );
    assert_eq!(
        args.iter().filter(|arg| !arg.starts_with("--")).count(),
        0,
        "every token is a flag, so no value was split off into its own argument: {args:?}"
    );
}

// ---------------------------------------------------------------------------- parse_launch (AC3, AC4)

/// AC4. A well-formed launch answer yields the thread id and URL, ignoring extra keys and
/// surrounding whitespace.
#[test]
fn parse_launch_reads_the_thread_id_and_url_from_a_valid_object() {
    let (thread, url) =
        parse_launch(&format!("\n  {}  \n", launched_json())).expect("a valid answer parses");
    assert_eq!(thread, THREAD_ID);
    assert_eq!(url, THREAD_URL);
}

/// AC3/AC4. A missing or blank thread id is an error naming the missing id, never a job with an
/// empty identity.
#[test]
fn parse_launch_rejects_a_missing_or_blank_thread_id() {
    for stdout in [
        json!({ "url": THREAD_URL }).to_string(),
        json!({ "threadId": "", "url": THREAD_URL }).to_string(),
        json!({ "threadId": "   ", "url": THREAD_URL }).to_string(),
    ] {
        let message = command_message(&parse_launch(&stdout).expect_err("no thread id"));
        assert!(
            message.contains("no thread id"),
            "stdout {stdout} gave: {message}"
        );
    }
}

/// AC4, R4. The URL is required: a launch that cannot say where its thread lives is an error.
#[test]
fn parse_launch_rejects_a_missing_or_blank_url() {
    for stdout in [
        json!({ "threadId": THREAD_ID }).to_string(),
        json!({ "threadId": THREAD_ID, "url": "" }).to_string(),
        json!({ "threadId": THREAD_ID, "url": "  " }).to_string(),
    ] {
        let message = command_message(&parse_launch(&stdout).expect_err("no url"));
        assert!(
            message.contains("no thread url"),
            "stdout {stdout} gave: {message}"
        );
    }
}

/// AC3. Non-JSON stdout is an error, not a panic.
#[test]
fn parse_launch_rejects_stdout_that_is_not_json() {
    for stdout in ["not json", "", "[1,2,3", "{\"threadId\":"] {
        let message = command_message(&parse_launch(stdout).expect_err("not json"));
        assert!(!message.is_empty(), "stdout {stdout:?} gave an empty error");
    }
}

// ---------------------------------------------------------------- dispatch_with_binary (AC2, AC4)

/// AC2/AC4. A successful launch: the thread id is the job id, the URL rides on the dispatch, the
/// surface is T3, no effective effort is claimed, the task reaches stdin byte for byte (including
/// a dash-leading line and the background marker), the task never reaches argv, and `--project`
/// is the canonicalized directory.
#[test]
fn dispatch_with_binary_returns_the_thread_and_sends_the_task_on_stdin_byte_for_byte() {
    let root = tempfile::tempdir().expect("tempdir");
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create the work dir");
    let stub = launcher(&root.path().join("stub"), &launched_json(), "", 0);
    let task = "/implement RS-1 fix the thing\n--not-a-flag line\n\nBACKGROUND_RUN=1";
    let uncanonical = root.path().join("work/../work");

    let dispatch = dispatch_with_binary(
        &stub.binary,
        &uncanonical,
        task,
        "RS-1 Fixture Job",
        Provider::Codex,
        Some("gpt-6-sol"),
        Some("high"),
        GENEROUS,
    )
    .expect("the stub launch succeeds");

    assert_eq!(dispatch.job_id.as_deref(), Some(THREAD_ID));
    assert_eq!(dispatch.url.as_deref(), Some(THREAD_URL));
    assert_eq!(dispatch.surface, Surface::T3);
    assert_eq!(dispatch.effective_effort, None);
    assert_eq!(dispatch.job_name, "RS-1 Fixture Job");
    assert_eq!(
        stub.stdin(),
        task,
        "the task must reach stdin byte for byte"
    );

    let argv = stub.argv();
    let canonical = fs::canonicalize(&work).expect("canonicalize the work dir");
    assert_eq!(
        argv,
        vec![
            format!("--project={}", canonical.display()),
            "--engine=codex".to_string(),
            "--model=gpt-6-sol".to_string(),
            "--effort=high".to_string(),
            "--title=RS-1 Fixture Job".to_string(),
            "--prompt=-".to_string(),
            "--json".to_string(),
        ]
    );
    assert!(
        !argv.iter().any(|arg| arg.contains("fix the thing")),
        "the task leaked into argv: {argv:?}"
    );
}

/// AC3. Exit 3 is t3-thread refusing an unverified T3 server version; the message says what to do
/// about it and carries the launcher's own stderr.
#[test]
fn dispatch_with_binary_maps_exit_three_to_the_unverified_version_message_with_stderr() {
    let root = tempfile::tempdir().expect("tempdir");
    let stub = launcher(root.path(), "", "unverified T3 version 0.0.99\n", 3);

    let error = dispatch_with_binary(
        &stub.binary,
        root.path(),
        "a task",
        "Exit Three",
        Provider::Codex,
        None,
        None,
        GENEROUS,
    )
    .expect_err("exit 3 is a launch failure");

    let Error::NotLaunched(message) = error else {
        panic!("exit 3 must prove the thread was not launched: {error:?}");
    };
    assert!(
        message.contains("VERIFIED_T3_VERSIONS"),
        "exit 3 must name the version allow-list: {message}"
    );
    assert!(
        message.contains("unverified T3 version 0.0.99"),
        "exit 3 must surface t3-thread's stderr: {message}"
    );
}

/// A usage refusal also proves that no thread was launched and retains the diagnostic.
#[test]
fn dispatch_with_binary_maps_exit_two_to_not_launched_with_stderr() {
    let root = tempfile::tempdir().expect("tempdir");
    let stub = launcher(root.path(), "", "unknown option --bad\n", 2);
    let error = dispatch_with_binary(
        &stub.binary,
        root.path(),
        "a task",
        "Usage Error",
        Provider::Codex,
        None,
        None,
        GENEROUS,
    )
    .expect_err("exit 2 is a launch failure");
    let Error::NotLaunched(message) = error else {
        panic!("exit 2 must prove the thread was not launched: {error:?}");
    };
    assert!(message.contains("exited 2"), "{message}");
    assert!(message.contains("unknown option --bad"), "{message}");
}

/// A runtime error before the thread create was sent (no t3 binary, a refused websocket, a 401
/// session ticket) proves that no thread was launched and retains the diagnostic.
#[test]
fn dispatch_with_binary_maps_exit_six_to_not_launched_with_stderr() {
    let root = tempfile::tempdir().expect("tempdir");
    let stub = launcher(
        root.path(),
        "",
        "t3-thread: cannot find the t3 binary; pass --t3-bin or set $T3_BIN\n",
        6,
    );
    let error = dispatch_with_binary(
        &stub.binary,
        root.path(),
        "a task",
        "Before Create",
        Provider::Codex,
        None,
        None,
        GENEROUS,
    )
    .expect_err("exit 6 is a launch failure");
    let Error::NotLaunched(message) = error else {
        panic!("exit 6 must prove the thread was not launched: {error:?}");
    };
    assert!(message.contains("before creating a thread"), "{message}");
    assert!(message.contains("cannot find the t3 binary"), "{message}");
}

/// Printed thread evidence overrides an otherwise safe refusal code.
#[test]
fn refusal_codes_with_thread_evidence_remain_ambiguous() {
    for code in [2, 3, 6] {
        for (stdout, stderr) in [
            (format!("THREAD_ID {THREAD_ID}\n"), "boom".to_string()),
            (launched_json(), "boom".to_string()),
            (
                format!("{{\"threadId\":\"{THREAD_ID}\""),
                "boom".to_string(),
            ),
            (String::new(), format!("THREAD_ID {THREAD_ID}\nboom")),
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            let stub = launcher(root.path(), &stdout, &stderr, code);
            let error = dispatch_with_binary(
                &stub.binary,
                root.path(),
                "a task",
                "Contradictory Refusal",
                Provider::Codex,
                None,
                None,
                GENEROUS,
            )
            .expect_err("a failed exit stays a failure");
            let message = command_message(&error);
            assert!(message.contains("boom"), "{message}");
        }
    }
}

/// AC3. Runtime errors remain ambiguous even after a thread identity was printed.
#[test]
fn dispatch_with_binary_maps_a_nonzero_exit_to_an_error_carrying_stderr() {
    let root = tempfile::tempdir().expect("tempdir");
    let stub = launcher(
        root.path(),
        &format!("THREAD_ID {THREAD_ID}\n"),
        "boom\n",
        1,
    );

    let error = dispatch_with_binary(
        &stub.binary,
        root.path(),
        "a task",
        "Exit One",
        Provider::Codex,
        None,
        None,
        GENEROUS,
    )
    .expect_err("exit 1 is a launch failure");

    let message = command_message(&error);
    assert!(message.contains("boom"), "stderr missing: {message}");
    assert!(message.contains("exited 1"), "exit code missing: {message}");
}

/// AC3/AC4. Exit 0 with unusable stdout is an error, not a panic and not a job with no id.
#[test]
fn dispatch_with_binary_rejects_exit_zero_with_unparseable_or_incomplete_stdout() {
    for stdout in ["not json".to_string(), json!({ "url": "x" }).to_string()] {
        let root = tempfile::tempdir().expect("tempdir");
        let stub = launcher(root.path(), &stdout, "", 0);
        let result = dispatch_with_binary(
            &stub.binary,
            root.path(),
            "a task",
            "Bad Stdout",
            Provider::Codex,
            None,
            None,
            GENEROUS,
        );
        assert!(
            result.is_err(),
            "stdout {stdout:?} was accepted: {result:?}"
        );
    }
}

/// AC3. A launcher path that does not exist is the resolver-family launch error, not a bare Io.
#[test]
fn dispatch_with_binary_reports_a_missing_binary_as_a_launch_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let error = dispatch_with_binary(
        &root.path().join("no-such-t3-thread"),
        root.path(),
        "a task",
        "Missing",
        Provider::Codex,
        None,
        None,
        GENEROUS,
    )
    .expect_err("a missing launcher cannot launch");
    let message = launch_message(&error);
    assert!(
        message.contains(T3_THREAD_BIN_ENV),
        "the launch error must name the override that fixes it: {message}"
    );
}

// ---------------------------------------------------------------- bounded launch wait (R1, R5, R8)

/// Run a launch against `body` with a short bound and return the result and how long it took.
fn timed_launch(root: &Path, body: &str, timeout: Duration) -> (Result<Dispatch>, Duration) {
    let binary = root.join("t3-thread");
    common::write_stub(&binary, body);
    let started = Instant::now();
    let result = dispatch_with_binary(
        &binary,
        root,
        "a task",
        "Bounded",
        Provider::Codex,
        None,
        None,
        timeout,
    );
    (result, started.elapsed())
}

fn assert_timed_out(result: &Result<Dispatch>, elapsed: Duration) {
    assert!(
        elapsed < Duration::from_secs(10),
        "a 1s launch bound took {elapsed:?} to return"
    );
    let error = result
        .as_ref()
        .expect_err("a hung launcher must not succeed");
    let message = command_message(error);
    assert!(
        message.contains("did not finish launching within 1s"),
        "the error must name the bound: {message}"
    );
    assert!(
        message.contains("killed"),
        "the error must say the launcher was killed: {message}"
    );
}

/// R1/R5. A launcher that hangs WITHOUT exec leaves its `sleep` grandchild holding stdout and
/// stderr; killing only the direct child would leave the readers blocked forever. The whole group
/// is signalled, so a 1s bound returns in a few seconds with the timeout error.
#[test]
fn a_hung_launcher_whose_grandchild_holds_the_pipes_times_out_within_the_bound() {
    let root = tempfile::tempdir().expect("tempdir");
    let (result, elapsed) = timed_launch(root.path(), "sleep 30\n", Duration::from_secs(1));
    assert_timed_out(&result, elapsed);
}

/// R5. The same bound when the launcher execs straight into the hang, so the direct child is the
/// sleeper itself.
#[test]
fn a_hung_launcher_that_execs_into_the_hang_times_out_within_the_bound() {
    let root = tempfile::tempdir().expect("tempdir");
    let (result, elapsed) = timed_launch(root.path(), "exec sleep 30\n", Duration::from_secs(1));
    assert_timed_out(&result, elapsed);
}

/// R8. The launcher answered and exited 0, but left a straggler holding stderr open (stdout is
/// closed). That is a successful launch: the thread exists, and reporting a timeout would invite
/// a retry that creates it twice.
#[test]
fn a_launcher_that_exits_zero_but_leaves_stderr_open_still_succeeds() {
    let root = tempfile::tempdir().expect("tempdir");
    let out = root.path().join("t3.out");
    fs::write(&out, launched_json()).expect("write the canned stdout");
    let body = format!(
        "cat > /dev/null\nsleep 30 >/dev/null </dev/null &\ncat {}\nexit 0\n",
        shell_quote(&out.to_string_lossy())
    );
    let (result, elapsed) = timed_launch(root.path(), &body, Duration::from_secs(2));
    assert!(
        elapsed < Duration::from_secs(10),
        "a straggler on stderr held the launch for {elapsed:?}"
    );
    let dispatch = result.expect("stdout closed on a clean exit, so the launch succeeded");
    assert_eq!(dispatch.job_id.as_deref(), Some(THREAD_ID));
    assert_eq!(dispatch.url.as_deref(), Some(THREAD_URL));
}

// ---------------------------------------------------------------------- resolve_t3_thread (AC3)

fn environment(
    path: Option<&Path>,
    home: Option<&Path>,
    overrides: &[(&str, &Path)],
) -> Environment {
    Environment::new(
        path.map(|dir| std::env::join_paths([dir]).expect("join the fixture PATH")),
        home.map(Path::to_path_buf),
        overrides
            .iter()
            .map(|(name, value)| ((*name).to_string(), OsString::from(value)))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn skill_launcher(home: &Path) -> PathBuf {
    let dir = home.join(".claude/skills/t3-thread");
    fs::create_dir_all(&dir).expect("create the skill dir");
    let path = dir.join("t3-thread");
    common::write_stub(&path, "exit 0\n");
    path
}

/// AC3. The env override wins over an installed skill launcher.
#[test]
fn resolve_t3_thread_prefers_the_env_override_over_the_skill_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    skill_launcher(&home);
    let pinned = root.path().join("pinned/t3-thread");
    fs::create_dir_all(pinned.parent().expect("parent")).expect("create pinned dir");
    common::write_stub(&pinned, "exit 0\n");

    let resolved = resolve_t3_thread(&environment(
        None,
        Some(&home),
        &[(T3_THREAD_BIN_ENV, &pinned)],
    ))
    .expect("the override resolves");
    assert_eq!(resolved, pinned);
}

/// AC3. An override pinned to a path that does not exist fails naming the override; it never
/// falls through to the skill path.
#[test]
fn resolve_t3_thread_fails_on_an_override_pinned_to_a_missing_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    skill_launcher(&home);
    let missing = root.path().join("nowhere/t3-thread");

    let error = resolve_t3_thread(&environment(
        None,
        Some(&home),
        &[(T3_THREAD_BIN_ENV, &missing)],
    ))
    .expect_err("a typo in the override must not fall through");
    let message = launch_message(&error);
    assert!(message.contains(T3_THREAD_BIN_ENV), "{message}");
}

/// AC3. With no override, the launcher installed under the Environment's own HOME resolves.
#[test]
fn resolve_t3_thread_falls_back_to_the_skill_path_under_home() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let installed = skill_launcher(&home);

    let resolved =
        resolve_t3_thread(&environment(None, Some(&home), &[])).expect("the skill path resolves");
    assert_eq!(resolved, installed);
}

/// AC3. Neither an override nor an installed skill: the error names the override and the path it
/// looked at.
#[test]
fn resolve_t3_thread_names_the_override_and_skill_path_when_nothing_is_installed() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("create HOME");

    let error =
        resolve_t3_thread(&environment(None, Some(&home), &[])).expect_err("nothing is installed");
    let message = launch_message(&error);
    assert!(message.contains(T3_THREAD_BIN_ENV), "{message}");
    assert!(
        message.contains(".claude/skills/t3-thread/t3-thread"),
        "the error must say where it looked: {message}"
    );
}

/// AC3. No HOME and no override is still a launch error naming the override.
#[test]
fn resolve_t3_thread_without_home_or_override_names_the_override() {
    let error = resolve_t3_thread(&environment(None, None, &[])).expect_err("nothing to resolve");
    let message = launch_message(&error);
    assert!(message.contains(T3_THREAD_BIN_ENV), "{message}");
}

/// AC3, decision 10. PATH is not searched by default: a `t3-thread` that is only on PATH could be
/// an unrelated binary, so it must not be picked up.
#[test]
fn resolve_t3_thread_does_not_search_path_without_an_override() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("create HOME");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    common::write_stub(&bin.join("t3-thread"), "exit 0\n");

    let result = resolve_t3_thread(&environment(Some(&bin), Some(&home), &[]));
    assert!(
        result.is_err(),
        "a t3-thread found only on PATH was picked up: {result:?}"
    );
}

// ------------------------------------------------------ run_with on the t3 surface (AC1, AC4-AC6)

/// A hermetic Context for `run_with`: HOME under `root`, the t3 launcher pinned by override, and
/// any extra overrides (a claude stub for the background and classifier cases).
fn t3_context(root: &Path, launcher: &Path, extra: &[(&str, &Path)], config: Config) -> Context {
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create HOME");
    let mut overrides: Vec<(&str, &Path)> = vec![(T3_THREAD_BIN_ENV, launcher)];
    overrides.extend_from_slice(extra);
    Context::new(environment(None, Some(&home), &overrides), home, config)
        .with_claude_usage_cache(root.join("claude-usage.json"))
        .with_grok_usage_cache(root.join("grok-usage.json"))
        .with_codex_sessions_dir(root.join("codex-sessions"))
}

fn route(ctx: &Context, request: &Request, db: &Path) -> Result<Outcome> {
    run_with(request, ctx, UsageSnapshot::full, || {
        DecisionLog::open_at(db)
    })
}

fn request<'a>(
    task: &'a str,
    dir: &'a Path,
    provider: Provider,
    model: &str,
    effort: Option<&str>,
    surface: Surface,
    mcp_configs: &'a [PathBuf],
) -> Request<'a> {
    Request {
        task,
        dir,
        provider: Some(provider),
        model: Some(model.to_string()),
        effort: effort.map(str::to_string),
        name: None,
        dry_run: false,
        mcp_configs,
        strict_mcp_config: false,
        surface,
    }
}

/// AC1/AC4/AC6. A fully pinned codex route on the t3 surface launches through the stub; the job id
/// is the thread id; the decision row records surface `t3` and the thread URL; with no `--name`
/// the naming worker is not started and the skip reason says T3 threads are not renamed (R2).
#[test]
fn a_t3_route_dispatches_through_t3_thread_and_logs_the_surface_and_thread_url() {
    let root = tempfile::tempdir().expect("tempdir");
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create the work dir");
    let stub = launcher(&root.path().join("stub"), &launched_json(), "", 0);
    let ctx = t3_context(root.path(), &stub.binary, &[], Config::default());
    let db = root.path().join("router.db");
    let task = "tighten the flaky retry test";

    let outcome = route(
        &ctx,
        &request(
            task,
            &work,
            Provider::Codex,
            "gpt-6-sol",
            Some("high"),
            Surface::T3,
            &[],
        ),
        &db,
    )
    .expect("the t3 route dispatches");

    assert_eq!(outcome.surface, Surface::T3);
    let dispatch = outcome.dispatch.as_ref().expect("a dispatch");
    assert_eq!(dispatch.job_id.as_deref(), Some(THREAD_ID));
    assert_eq!(dispatch.url.as_deref(), Some(THREAD_URL));
    assert_eq!(dispatch.surface, Surface::T3);
    assert_eq!(outcome.mcp_warning, None, "no MCP flags were given");
    assert!(!outcome.naming_started, "t3 threads are never renamed");
    let skipped = outcome
        .naming_skipped
        .as_deref()
        .expect("AC6 records why no naming worker ran");
    assert!(
        skipped.contains("does not rename T3 threads"),
        "the skip reason: {skipped}"
    );

    assert_eq!(stub.stdin(), task);
    let argv = stub.argv();
    assert!(argv.contains(&"--engine=codex".to_string()), "{argv:?}");
    assert!(argv.contains(&"--model=gpt-6-sol".to_string()), "{argv:?}");
    assert!(argv.contains(&"--effort=high".to_string()), "{argv:?}");
    assert!(
        argv.contains(&format!("--title={}", short_job_name(task))),
        "an unnamed job launches under its derived name: {argv:?}"
    );

    let rows = DecisionLog::open_at(&db)
        .expect("reopen the log")
        .recent(5)
        .expect("read the log");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.surface.as_deref(), Some("t3"));
    assert_eq!(row.thread_url.as_deref(), Some(THREAD_URL));
    assert_eq!(row.job_id.as_deref(), Some(THREAD_ID));
    assert_eq!(row.outcome, "dispatched");
    assert_eq!(row.effective_effort, None);
}

/// AC6, R2. When the scoring call already produced a title, `wants_async_name` is false, yet an
/// unnamed t3 launch must still record why no worker renames it. The launch title is the scored
/// one.
#[test]
fn a_t3_route_with_a_scored_title_still_records_the_naming_skip_reason() {
    let root = tempfile::tempdir().expect("tempdir");
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create the work dir");
    let stub = launcher(&root.path().join("stub"), &launched_json(), "", 0);
    let classifier = root.path().join("classifier/claude");
    fs::create_dir_all(classifier.parent().expect("parent")).expect("create classifier dir");
    let answer = json!({
        "orchestration": false,
        "missing_connector": false,
        "complexity": "medium",
        "task_context_horizon": "ordinary",
        "rationale": "fixture scored title",
        "job_name": "Scored T3 Title",
    })
    .to_string();
    let envelope = json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": answer,
    })
    .to_string();
    let answer_file = root.path().join("classifier.answer");
    fs::write(&answer_file, envelope).expect("write the classifier answer");
    common::write_stub(
        &classifier,
        &format!(
            "cat {}\nexit 0\n",
            shell_quote(&answer_file.to_string_lossy())
        ),
    );
    let mut config = Config::default();
    config.classifier.engine = ClassifierEngine::Claude;
    let ctx = t3_context(
        root.path(),
        &stub.binary,
        &[(CLAUDE_BIN_ENV, &classifier)],
        config,
    );
    let db = root.path().join("router.db");
    // Codex with a model and no effort still classifies, which is what produces the scored title.
    let outcome = route(
        &ctx,
        &request(
            "tighten the flaky retry test",
            &work,
            Provider::Codex,
            "gpt-6-sol",
            None,
            Surface::T3,
            &[],
        ),
        &db,
    )
    .expect("the t3 route dispatches");

    assert!(!outcome.naming_started);
    let skipped = outcome
        .naming_skipped
        .as_deref()
        .expect("AC6 records the reason even when the scoring call titled the job");
    assert!(
        skipped.contains("does not rename T3 threads"),
        "the skip reason: {skipped}"
    );
    assert!(
        stub.argv().contains(&"--title=Scored T3 Title".to_string()),
        "the scored title is the launch title: {:?}",
        stub.argv()
    );
}

/// AC1. The background surface is unchanged: the claude background dispatcher runs, the t3
/// launcher is never touched, and the row records surface `background` with no thread URL.
#[test]
fn a_background_route_is_unchanged_and_never_touches_t3_thread() {
    let root = tempfile::tempdir().expect("tempdir");
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create the work dir");
    let stub = launcher(&root.path().join("stub"), &launched_json(), "", 0);
    let listing = json!([{
        "id": "bgjob01",
        "cwd": work,
        "name": "Background Fixture",
        "startedAt": i64::MAX,
        "kind": "background",
        "state": "working"
    }])
    .to_string();
    let claude = root.path().join("claude-bin/claude");
    fs::create_dir_all(claude.parent().expect("parent")).expect("create claude dir");
    common::write_stub(
        &claude,
        &format!(
            "if [ \"$1\" = \"agents\" ]; then\n  printf '%s\\n' {}\n  exit 0\nfi\nexit 0\n",
            shell_quote(&listing)
        ),
    );
    let ctx = t3_context(
        root.path(),
        &stub.binary,
        &[(CLAUDE_BIN_ENV, &claude)],
        Config::default(),
    );
    let db = root.path().join("router.db");
    let mut background = request(
        "exercise the background surface",
        &work,
        Provider::Claude,
        "opus[1m]",
        Some("high"),
        Surface::Background,
        &[],
    );
    background.name = Some("Background Fixture".to_string());

    let outcome = route(&ctx, &background, &db).expect("the background route dispatches");

    assert_eq!(outcome.surface, Surface::Background);
    let dispatch = outcome.dispatch.as_ref().expect("a dispatch");
    assert_eq!(dispatch.surface, Surface::Background);
    assert_eq!(dispatch.url, None);
    assert_eq!(dispatch.job_id.as_deref(), Some("bgjob01"));
    assert!(!stub.was_invoked(), "a background route invoked t3-thread");
    let rows = DecisionLog::open_at(&db)
        .expect("reopen the log")
        .recent(1)
        .expect("read the log");
    assert_eq!(rows[0].surface.as_deref(), Some("background"));
    assert_eq!(rows[0].thread_url, None);
}

/// AC5. Claude MCP scoping on t3 is accepted and dropped: the outcome carries the one warning the
/// CLI prints, and nothing MCP-shaped reaches the launcher (the config path is not even
/// preflighted, so a nonexistent one is fine).
#[test]
fn claude_mcp_scoping_on_t3_is_dropped_with_a_warning_and_never_forwarded() {
    let root = tempfile::tempdir().expect("tempdir");
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create the work dir");
    let stub = launcher(&root.path().join("stub"), &launched_json(), "", 0);
    let ctx = t3_context(root.path(), &stub.binary, &[], Config::default());
    let db = root.path().join("router.db");
    let configs = [root.path().join("no-such.mcp.json")];
    let mut scoped = request(
        "use the scoped servers",
        &work,
        Provider::Claude,
        "claude-opus-5-5[1m]",
        Some("high"),
        Surface::T3,
        &configs,
    );
    scoped.strict_mcp_config = true;

    let outcome = route(&ctx, &scoped, &db).expect("claude scoping on t3 is accepted");

    let warning = outcome
        .mcp_warning
        .as_deref()
        .expect("AC5 surfaces a warning for the dropped flags");
    assert!(
        warning.contains("ignored on the t3 surface"),
        "the warning: {warning}"
    );
    let argv = stub.argv();
    assert!(
        !argv.iter().any(|arg| arg.contains("mcp")),
        "an MCP flag reached t3-thread: {argv:?}"
    );
    assert!(argv.contains(&"--engine=claude".to_string()), "{argv:?}");
    assert!(
        argv.contains(&"--model=claude-opus-5-5".to_string()),
        "{argv:?}"
    );
}

/// AC5. Codex refuses MCP scoping on t3 exactly as it does on the background surface, and the
/// launcher is never invoked.
#[test]
fn codex_mcp_scoping_on_t3_is_still_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let work = root.path().join("work");
    fs::create_dir_all(&work).expect("create the work dir");
    let stub = launcher(&root.path().join("stub"), &launched_json(), "", 0);
    let ctx = t3_context(root.path(), &stub.binary, &[], Config::default());
    let db = root.path().join("router.db");
    let configs = [root.path().join("scoped.mcp.json")];

    let error = route(
        &ctx,
        &request(
            "use the scoped servers",
            &work,
            Provider::Codex,
            "gpt-6-sol",
            Some("high"),
            Surface::T3,
            &configs,
        ),
        &db,
    )
    .expect_err("codex cannot take MCP scoping on any surface");

    assert!(
        error.to_string().contains("--mcp-config"),
        "the refusal must name the flag: {error}"
    );
    assert!(!stub.was_invoked(), "t3-thread ran before the refusal");
}
