//! Every dispatch path, driven off a stripped `Environment`, must report a NAMED launch failure.
//!
//! The whole defect this file guards is that a provider CLI resolved through `execvp` against
//! whatever `PATH` the calling process inherited. Under systemd and cron that is `/usr/bin:/bin`,
//! the spawn died `ENOENT`, and the decision log recorded `error: No such file or directory (os
//! error 2)` with no job id and no session — silent loss with a useless diagnosis.
//!
//! Two rules govern every case here, and both are load-bearing.
//!
//! **Each case drives the production `_in(&Environment, …)` seam, never `*_with_binary` alone.**
//! A test that injects a binary path proves the argv is right and proves NOTHING about whether
//! production resolves: someone could leave `Path::new("claude")` at the top of `dispatch` and a
//! `dispatch_with_binary` test would stay green. The `_in` seam runs the real `resolve` on the
//! real code path, which is what makes these regression tests rather than fixtures.
//!
//! **Each case matches on the `Error::Launch` VARIANT, not on a message substring.** The message
//! is the easy half. `dispatch/codex.rs` maps its daemon failure into `Error::Command` and
//! `runtime.rs`'s `spawn_detached` yields `Error::Io`, so a correctly-worded failure of the wrong
//! variant is invisible to a substring test and is exactly the shape AC4 exists to catch.
//!
//! **No fixture here can reach a host binary, and that is structural.** These cases DISPATCH: an
//! environment that resolved a real provider CLI would not merely fail an assertion, it would
//! start a real, billable background job from a test run. The resolver's system fallback list
//! therefore lives on the `Environment` rather than in a constant the search consulted
//! unconditionally, and every fixture below hands it an EMPTY list — so `/usr/local/bin` and every
//! other host directory is unreachable no matter what is installed on the box.
//!
//! No process-environment mutation and no user-namespace isolation: see the header of
//! `binary_resolution.rs` for why both are forbidden here.

#![cfg(unix)]

use agent_router_core::binary::{
    CLAUDE_BIN_ENV, CODEX_BIN_ENV, Environment, GROK_BIN_ENV, OPENCODE_BIN_ENV,
};
use agent_router_core::dispatch::grok::dispatch_with_lifecycle;
use agent_router_core::doctor::{Health, Report, optional_binary_in, required_binary_in};
use agent_router_core::run::Dispatch;
use agent_router_core::{Error, Provider, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod common;

const TIMEOUT: Duration = Duration::from_secs(5);

/// The two ports `ensure_server` probes and then binds. Named here because the opencode ordering
/// guard has to make both of them unavailable, and a third candidate added upstream would make
/// that guard silently vacuous.
const OPENCODE_PORTS: [u16; 2] = [4097, 4098];

/// Serializes the two opencode cases against each other. They state opposite preconditions about
/// the same two fixed ports, and libtest runs a file's tests as concurrent threads.
static OPENCODE_PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The empty system fallback list, named so every fixture below states the guarantee out loud
/// rather than inheriting it: no host directory is searched, so no host binary can be spawned.
const NO_SYSTEM_FALLBACKS: [PathBuf; 0] = [];

/// A temp `HOME` holding nothing, a `PATH` holding one empty directory, no override, and no system
/// fallback directory: the service-manager environment, expressed as data.
///
/// The shared, drift-proof fixture lives in `tests/common`; see its doc comment for why the empty
/// system fallback list is load-bearing.
fn stripped(root: &Path) -> Environment {
    common::stripped_environment(Some(root))
}

/// The guard for the fixtures above, and the reason it is a test rather than a comment.
///
/// Every case in this file drives a real dispatch, so a fixture that resolved a provider CLI would
/// spawn it — a real, billable background job started by `cargo test`. Before the system fallback
/// list moved onto the `Environment`, `resolve` consulted `/usr/local/bin` unconditionally and any
/// box with a provider installed there did exactly that. The positive half is asserted first so
/// the negative cannot pass vacuously.
#[test]
fn the_stripped_fixture_cannot_reach_a_directory_that_would_be_a_system_fallback() {
    let root = tempfile::tempdir().expect("tempdir");
    let would_be_system = root.path().join("system-fallback");
    std::fs::create_dir_all(&would_be_system).expect("create the system fallback directory");
    let stub = would_be_system.join("claude");
    common::write_stub(&stub, "exit 0\n");

    let opted_in = stripped(root.path()).with_system_fallbacks([&would_be_system]);
    assert_eq!(
        agent_router_core::binary::resolve(Provider::Claude, &opted_in)
            .expect("an opted-in system fallback is searched"),
        stub,
        "the stub is genuinely reachable, so the negative below is about the opt-in and nothing else"
    );

    let error = agent_router_core::binary::resolve(Provider::Claude, &stripped(root.path()))
        .expect_err("the stripped fixture must resolve no claude at all");
    assert_named_launch_failure("stripped fixture", error, "claude", CLAUDE_BIN_ENV);
}

/// Assert the shape every stripped-PATH dispatch failure must have: the `Launch` variant, a
/// message naming the binary and the override that would fix it, and never the production string.
///
/// The negative is scoped to this one error rather than to process output, per edge case E13.
fn assert_named_launch_failure(label: &str, error: Error, binary: &str, override_env: &str) {
    let Error::Launch(message) = &error else {
        panic!(
            "{label}: a missing provider CLI must arrive as Error::Launch, not {error:?} — a \
             correctly-worded Command or Io is exactly the laundering AC4 exists to catch"
        );
    };
    assert!(
        message.contains(binary),
        "{label}: the failure must name the binary: {message}"
    );
    assert!(
        message.contains(override_env),
        "{label}: the failure must name the override that would fix it: {message}"
    );
    assert!(
        !message.contains("os error 2"),
        "{label}: the reported production string must not survive: {message}"
    );
    assert!(
        !message.contains("No such file or directory"),
        "{label}: the reported production string must not survive: {message}"
    );
    assert!(
        error.to_string().starts_with("launch failed: "),
        "{label}: the rendered error carries the prefix the decision log discriminates on: {error}"
    );
}

// ------------------------------------------------------------------ claude: two entry points

/// `dispatch/claude.rs`'s `dispatch` is the ordinary claude spawn. Without resolution here a
/// build-tier `/implement` run dispatched from a systemd unit dies before a session exists.
#[test]
fn a_claude_dispatch_off_a_stripped_path_reports_a_launch_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).expect("create the working directory");

    let error = agent_router_core::dispatch::claude::dispatch_in(
        &stripped(root.path()),
        &cwd,
        "score this",
        "Fixture Job",
        None,
        None,
        &[] as &[PathBuf],
        false,
    )
    .expect_err("no claude resolves off a stripped environment");

    assert_named_launch_failure("claude dispatch", error, "claude", CLAUDE_BIN_ENV);
}

/// `agent_states` is a SECOND claude entry point with its own resolution, reached from
/// `status.rs`. Without it, reconciliation keeps calling `execvp("claude")` while dispatch is
/// fixed, and every job in the window reads as unresolvable rather than as unread.
#[test]
fn a_claude_agent_states_read_off_a_stripped_path_reports_a_launch_failure() {
    let root = tempfile::tempdir().expect("tempdir");

    let error: Error =
        agent_router_core::dispatch::claude::agent_states_in(&stripped(root.path()), TIMEOUT)
            .expect_err("no claude resolves off a stripped environment");

    assert_named_launch_failure("claude agent_states", error, "claude", CLAUDE_BIN_ENV);
}

// ------------------------------------------------------------------ codex: the laundering trap

/// The codex path is the D3(b) trap and the AC4 correctness point.
///
/// `ensure_daemon` returns `Result<Daemon, String>` today and `dispatch` consumes it with
/// `.map_err(Error::Command)`, so a launch failure with a perfectly correct message would still
/// persist as an undifferentiated `Error::Command`. Only the variant assertion catches that, which
/// is why `assert_named_launch_failure` matches the variant first and reads the message second.
// The codex daemon transport and the opencode managed server are Linux-only paths.
#[cfg(target_os = "linux")]
#[test]
fn a_codex_dispatch_off_a_stripped_path_reports_a_launch_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).expect("create the working directory");

    let error = agent_router_core::dispatch::codex::dispatch_in(
        &stripped(root.path()),
        &cwd,
        "score this",
        "Fixture Job",
        None,
        None,
    )
    .expect_err("no codex resolves off a stripped environment");

    assert_named_launch_failure("codex dispatch", error, "codex", CODEX_BIN_ENV);
}

// ------------------------------------------------------------------ grok

/// Grok resolves before `GrokLifecycle::new` is constructed, so no external-crate behaviour is
/// under test: the failure is the router's own, with the router's own message.
#[test]
fn a_grok_dispatch_off_a_stripped_path_reports_a_launch_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).expect("create the working directory");

    let error = agent_router_core::dispatch::grok::dispatch_in(
        &stripped(root.path()),
        &cwd,
        "score this",
        "Fixture Job",
        None,
    )
    .expect_err("no grok resolves off a stripped environment");

    assert_named_launch_failure("grok dispatch", error, "grok", GROK_BIN_ENV);
}

/// The residue B10 covers: the binary resolved and the lifecycle's own exec then failed ENOENT.
/// That is a genuine TOCTOU event and must still be a named `Launch`, not
/// `Grok lifecycle spawn failed: No such file or directory (os error 2)` — the production string
/// wearing a prefix.
#[test]
fn a_grok_lifecycle_enoent_after_resolution_is_still_a_launch_failure() {
    let error = dispatch_with_lifecycle(
        Path::new("/tmp"),
        "score this",
        "Fixture Job",
        None,
        |_, _, _| {
            Err(agent_viewer_core::Error::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )))
        },
    )
    .expect_err("a lifecycle ENOENT is a launch failure");

    assert_named_launch_failure("grok lifecycle enoent", error, "grok", GROK_BIN_ENV);
}

/// The other half of B10, and the reason it is a `NotFound` inspection rather than a blanket
/// rewrite: a lifecycle failure that is NOT about a missing binary must keep its existing
/// `Grok lifecycle spawn failed: …` text byte-identically, because other tests assert on it.
#[test]
fn a_grok_lifecycle_failure_that_is_not_a_missing_binary_keeps_its_existing_message() {
    let error = dispatch_with_lifecycle(
        Path::new("/tmp"),
        "score this",
        "Fixture Job",
        None,
        |_, _, _| {
            Err(agent_viewer_core::Error::Command(
                "authoritative leader is unavailable".to_string(),
            ))
        },
    )
    .expect_err("a non-launch lifecycle failure still fails");

    assert!(
        matches!(error, Error::Command(_)),
        "only a missing binary becomes Launch; everything else keeps its variant: {error:?}"
    );
    assert!(
        error.to_string().contains("Grok lifecycle spawn failed: "),
        "the existing message must survive byte-identically: {error}"
    );
}

// ------------------------------------------------------------------ opencode: both directions

/// The opencode spawn arm. Reached only after the probe loop finds no live server and a candidate
/// port binds, which is exactly where D3(a) says resolution belongs.
///
/// Precondition: neither candidate port answers. A live managed OpenCode server on 4097 or 4098
/// would take the probe's early return and this case would have nothing to assert, so the
/// precondition is checked rather than assumed.
// The codex daemon transport and the opencode managed server are Linux-only paths.
#[cfg(target_os = "linux")]
#[test]
fn an_opencode_dispatch_with_no_resolvable_binary_reports_launch() {
    let _guard = OPENCODE_PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for port in OPENCODE_PORTS {
        let probe = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(
            probe.is_ok(),
            "port {port} is occupied, so the spawn arm is unreachable; stop the OpenCode server \
             listening there and re-run"
        );
    }

    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).expect("create the working directory");

    let error = agent_router_core::dispatch::opencode::dispatch_in(
        &stripped(root.path()),
        &cwd,
        "score this",
        "Fixture Job",
        None,
        None,
    )
    .expect_err("no opencode resolves off a stripped environment");

    assert_named_launch_failure("opencode dispatch", error, "opencode", OPENCODE_BIN_ENV);
}

/// The D3(a) regression guard, and the one place in this diff where the correct behaviour is "do
/// not resolve".
///
/// `ensure_server` returns an already-running server before it spawns anything, so a box with a
/// live server on 4097/4098 and no `opencode` on PATH WORKS TODAY. Resolving at the top of
/// `dispatch` — which the first draft of the plan prescribed — would fail that box with
/// `Error::Launch`: a brand-new failure on a working path, shipped by the fix.
///
/// Both candidate ports are held here, so the run cannot reach the spawn. The failure that comes
/// back must therefore be about the ports, never about the binary: a `Launch` here proves
/// resolution was hoisted above the probe.
// The codex daemon transport and the opencode managed server are Linux-only paths.
#[cfg(target_os = "linux")]
#[test]
fn an_already_running_opencode_server_does_not_require_the_binary() {
    let _guard = OPENCODE_PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let held: Vec<_> = OPENCODE_PORTS
        .iter()
        .filter_map(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).ok())
        .collect();

    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).expect("create the working directory");

    let outcome = agent_router_core::dispatch::opencode::dispatch_in(
        &stripped(root.path()),
        &cwd,
        "score this",
        "Fixture Job",
        None,
        None,
    );
    drop(held);

    if let Err(error) = outcome {
        assert!(
            !matches!(error, Error::Launch(_)),
            "no candidate port could be spawned on, so the binary was never needed; a Launch here \
             means resolution was hoisted above the probe and a working path just regressed: \
             {error:?}"
        );
        assert!(
            !error.to_string().contains(OPENCODE_BIN_ENV),
            "the diagnosis must be the ports, not the binary: {error}"
        );
    }
}

// ------------------------------------------------------------------ the four-provider table

/// One row of the parity table below: the provider, its display name, the override env var that
/// pins its binary, and a thunk that drives its `_in(&Environment, …)` dispatch seam.
type DispatchCase<'a> = (
    Provider,
    &'static str,
    &'static str,
    Box<dyn Fn() -> Result<Dispatch> + 'a>,
);

/// The parity gate, stated as a test. A fifth provider added without resolver wiring fails here
/// rather than shipping, which is what the AC1 grep cannot do on its own.
// The codex daemon transport and the opencode managed server are Linux-only paths.
#[cfg(target_os = "linux")]
#[test]
fn every_dispatch_path_reports_a_launch_failure_rather_than_a_bare_io_error() {
    let _guard = OPENCODE_PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).expect("create the working directory");
    let environment = stripped(root.path());

    // Every `Provider` variant appears, so a variant added later must be added here too.
    let cases: [DispatchCase<'_>; 4] = [
        (
            Provider::Claude,
            "claude",
            CLAUDE_BIN_ENV,
            Box::new(|| {
                agent_router_core::dispatch::claude::dispatch_in(
                    &environment,
                    &cwd,
                    "score this",
                    "Fixture Job",
                    None,
                    None,
                    &[] as &[PathBuf],
                    false,
                )
            }),
        ),
        (
            Provider::Codex,
            "codex",
            CODEX_BIN_ENV,
            Box::new(|| {
                agent_router_core::dispatch::codex::dispatch_in(
                    &environment,
                    &cwd,
                    "score this",
                    "Fixture Job",
                    None,
                    None,
                )
            }),
        ),
        (
            Provider::Grok,
            "grok",
            GROK_BIN_ENV,
            Box::new(|| {
                agent_router_core::dispatch::grok::dispatch_in(
                    &environment,
                    &cwd,
                    "score this",
                    "Fixture Job",
                    None,
                )
            }),
        ),
        (
            Provider::Opencode,
            "opencode",
            OPENCODE_BIN_ENV,
            Box::new(|| {
                agent_router_core::dispatch::opencode::dispatch_in(
                    &environment,
                    &cwd,
                    "score this",
                    "Fixture Job",
                    None,
                    None,
                )
            }),
        ),
    ];

    for (provider, binary, override_env, dispatch) in cases {
        let error = match dispatch() {
            Ok(dispatched) => {
                panic!("{provider:?} must not dispatch off a stripped environment: {dispatched:?}")
            }
            Err(error) => error,
        };
        assert_named_launch_failure(provider.name(), error, binary, override_env);
    }
}

// ------------------------------------------------------------------ doctor's severity (D1)

/// D1, first direction. After the fallback lands, a `claude` reachable only through
/// `$HOME/.local/bin` DISPATCHES FINE, so doctor must not Fail on it.
///
/// Fail is doctor's process exit code. Failing here would exit 1 on precisely the machines this
/// ticket teaches the router to handle, turning a preflight into a false alarm on the fixed
/// configuration — a worse defect than the one being fixed.
#[test]
fn doctor_warns_rather_than_fails_when_a_binary_is_off_path_but_resolvable() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let user_local = home.join(".local/bin");
    std::fs::create_dir_all(&user_local).expect("create the user-local bin");
    let stub = user_local.join("claude");
    common::write_stub(&stub, "exit 0\n");
    let empty = root.path().join("empty-path-dir");
    std::fs::create_dir_all(&empty).expect("create the empty PATH directory");
    let environment = Environment::new(
        Some(std::env::join_paths([&empty]).expect("join the fixture PATH")),
        Some(home),
        BTreeMap::new(),
    )
    .with_system_fallbacks(NO_SYSTEM_FALLBACKS);

    let check = required_binary_in(&environment, "claude_on_path", Provider::Claude);
    assert_eq!(
        check.health,
        Health::Warn,
        "a resolvable binary off PATH is degraded, not fatal: {check:?}"
    );
    assert!(
        check.detail.contains(&stub.to_string_lossy().to_string()),
        "the warning names where dispatch will actually find it: {}",
        check.detail
    );
    assert!(
        check.detail.contains(CLAUDE_BIN_ENV),
        "the warning names the variable that pins it: {}",
        check.detail
    );
    assert!(
        !check.detail.contains("any dispatch to it will error"),
        "that consequence is now false, and README quotes it verbatim: {}",
        check.detail
    );
    assert!(
        !Report {
            checks: vec![check]
        }
        .failed(),
        "doctor must exit 0 on a box the router handles"
    );
}

/// D1, the other direction. When the binary resolves NOWHERE the old severity and the old
/// consequence both survive, because both statements are now true.
///
/// Both directions are asserted because D1 is a severity change, and a severity change untested in
/// one direction regresses silently in that direction.
#[test]
fn doctor_fails_when_the_binary_resolves_nowhere() {
    let root = tempfile::tempdir().expect("tempdir");
    let environment = stripped(root.path());

    let required = required_binary_in(&environment, "claude_on_path", Provider::Claude);
    assert_eq!(
        required.health,
        Health::Fail,
        "a claude the router cannot find anywhere is fatal: {required:?}"
    );
    assert!(
        required.detail.contains("no executable claude on PATH"),
        "the existing message survives for the genuinely-missing case: {}",
        required.detail
    );
    assert!(
        Report {
            checks: vec![required]
        }
        .failed(),
        "Fail is doctor's exit code and must still be reachable"
    );

    let optional = optional_binary_in(&environment, "opencode_on_path", Provider::Opencode);
    assert_eq!(
        optional.health,
        Health::Warn,
        "an optional binary never fails the preflight: {optional:?}"
    );
    assert!(
        optional
            .detail
            .contains("no executable opencode on PATH, so any dispatch to it will error"),
        "README quotes this line verbatim, so it is byte-pinned: {}",
        optional.detail
    );
}
