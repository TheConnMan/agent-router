//! Shared fixture helpers reused across the resolver / dispatch / classifier test suites.
//!
//! Every stub-writing fixture in this workspace used to inline the same three steps: `fs::write`
//! the script, `set_mode(0o700)`, then exec it. This module is the single place that shape lives,
//! so the fixture's postcondition can be established once rather than nine times. The same is
//! true of the "stripped environment" fixture below: it used to be written out nearly
//! character-for-character in four separate test files, and had already drifted between them
//! before it was consolidated here.
#![cfg(unix)]
#![allow(dead_code)]

use agent_router_core::Context;
use agent_router_core::binary::Environment;
use agent_router_core::config::Config;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the probe keeps retrying before it gives up and fails the fixture.
///
/// Deliberately generous. See the probe loop for why a tight deadline is the wrong shape here.
const PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// Pause between probe attempts, so a retry yields the CPU to the child that is holding the
/// descriptor instead of spinning against it.
const PROBE_BACKOFF: Duration = Duration::from_millis(2);

/// `ETXTBSY`, the only errno the probe retries on. `ErrorKind` has no name for it.
const TEXT_FILE_BUSY: i32 = 26;

/// The one argument a stub answers by exiting 0 before any side effect in its body.
///
/// The token has to sit outside the whole space of first arguments a stub may be asked to branch
/// on, not merely outside the stubs that exist today. Enumerated from the source rather than from
/// memory:
///
/// * `grep -rn '\$1' crates/*/tests/*.rs` finds two branches, both `[ "$1" = "agents" ]`
///   (`run_json_tests.rs:79`, `provider_contract_tests.rs:547`).
/// * The only shell `case` in the suite is `stats_cli.rs:131-136`, over the *last* argument:
///   `*'CLASSIFIER FAILS'*`, `*'SCORED LOW'*`, `*'SCORED ULTRA'*`, and a `*)` catch-all that
///   matches anything at all, including this sentinel. The guard runs before the `case`, which is
///   what keeps the catch-all out of reach.
/// * The first arguments production actually emits: `-p` and `agents` and `--bg` for claude
///   (`src/classify.rs:136`, `src/dispatch/claude.rs:72,187`), `exec` and `app-server` and
///   `features` for codex (`src/classify.rs:186`, `src/dispatch/codex.rs:312`,
///   `src/classify.rs:663`), plus arbitrary free-text prompts.
///
/// A `--`-prefixed token naming this fixture layer is outside all of it and no production call
/// site can emit it.
pub const PROBE_ARG: &str = "--agent-router-fixture-probe";

/// Write `body` as an executable `/bin/sh` stub at `path`.
///
/// The interpreter line and the probe guard are assembled here rather than by the caller, so the
/// guard structurally precedes every side effect in `body`. Several stubs append their argv to a
/// log on every invocation, and a test that asserts a binary was never invoked reads that log, so
/// a probe reaching it would invert the assertion.
pub fn write_stub(path: &Path, body: &str) {
    assert!(
        !body.starts_with("#!"),
        "write_stub supplies the interpreter line itself, so the body must not carry one; \
         a second shebang would run as a comment and hide the mistake: {}",
        path.display()
    );
    let script = format!("#!/bin/sh\nif [ \"$1\" = \"{PROBE_ARG}\" ]; then exit 0; fi\n{body}");
    fs::write(path, script).expect("write the stub");
    let mut permissions = fs::metadata(path).expect("stub metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("make the stub executable");

    // Establish the fixture's postcondition by exercising it: exec the stub once, and do not
    // return until that exec succeeds.
    //
    // This loop is not a retry of the behaviour under test, and it is not a convenience that
    // waits for a flake to pass. It is a bounded proof, and the bound is a property of the
    // system rather than a hope. `fs::write` above closed its own descriptor before returning,
    // and that instant closes the set of descriptors that can ever hold this inode open for
    // writing: only a child forked before it could have inherited a copy, no child forked after
    // it can see one, and no further write to this path overlaps the probe. Rust opens files
    // `O_CLOEXEC`, so every such child drops its copy at its own `execve`, which it reaches on
    // its own without any help from us. The population of blockers is therefore finite and
    // strictly shrinking, so a successful exec is not merely a good sample: it proves no writable
    // descriptor remains on the inode and that none can appear afterwards. That is exactly the
    // guarantee the caller needs, so the loop terminates as soon as it holds.
    //
    // The closed set bound also assumes every child forked in this process reaches `execve` or
    // `_exit` promptly. Rust's `Command` guarantees it and there is no raw `libc::fork` anywhere
    // in the tree, so it holds today; a fixture using raw fork would break the bound, and
    // `PROBE_DEADLINE` is the backstop for that case.
    //
    // The proof rests on one precondition the caller owns: no write to this path may be
    // concurrent with an exec of it, and every write goes through `write_stub`. `fs::write`
    // truncates in place and keeps the inode, so a write racing an exec would open a fresh
    // writable descriptor on an inode already probed clean and reopen the window this probe
    // closed. Per test temp directories are what make the first clause true; a test that
    // deliberately rewrites the same stub between runs satisfies both, because each rewrite
    // carries its own probe and none of them overlaps an exec.
    //
    // The deadline is generous on purpose. This probe spawns, so it forks, so it is itself a
    // participant in the race it resolves: our fork can inherit some other fixture's in flight
    // write descriptor and block that fixture's probe, exactly as its forks block ours. A tight
    // deadline would turn that mutual interference into a spurious failure under load, while a
    // generous one costs nothing on the overwhelmingly common path where the first exec
    // succeeds. Hitting the deadline is a real failure and is reported as one; the fixture never
    // hands back a stub it could not prove executable.
    let started = Instant::now();
    loop {
        // `status` waits for the child. A bare `spawn` would leave one zombie per stub write and
        // the suite writes hundreds into a single libtest process.
        match Command::new(path).arg(PROBE_ARG).status() {
            Ok(status) => {
                // The probe answers through the guard, whose only path is `exit 0`. Any other
                // status means the guard did not match and the body ran instead, which is the
                // side effect leak the guard exists to prevent.
                assert!(
                    status.success(),
                    "the probe guard did not answer at {}: the stub ran its body instead, \
                     status {status:?}",
                    path.display()
                );
                return;
            }
            Err(error) if error.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                assert!(
                    started.elapsed() < PROBE_DEADLINE,
                    "the stub at {} was still held open for writing after {:?}, so it could not \
                     be proven executable: {error}",
                    path.display(),
                    PROBE_DEADLINE
                );
                std::thread::sleep(PROBE_BACKOFF);
            }
            Err(error) => panic!(
                "the stub at {} failed to exec for a reason other than ETXTBSY: {error}",
                path.display()
            ),
        }
    }
}

/// An `Environment` that resolves no provider CLI at all — the fixture behind every "a stripped
/// environment reports a launch failure" test in this workspace.
///
/// `root` selects which of the two variants this fixture needs:
///
/// * `Some(root)` builds a temp `HOME` holding nothing and a `PATH` holding one empty directory
///   under `root`: the service-manager environment (systemd, cron), expressed as data. This is
///   the shape almost every dispatch and classifier fixture wants, because it exercises "PATH and
///   HOME exist but resolve nothing" rather than "PATH and HOME are unset".
/// * `None` builds an `Environment` with no `PATH`, no `HOME`, and no override at all, for a
///   fixture that specifically targets the HOME-absent / PATH-absent code paths (some systemd
///   units genuinely run with no `HOME`).
///
/// **The explicit empty system fallback list is load-bearing and must not be "simplified" away.**
/// `Environment::new` already defaults to no system fallbacks, so calling
/// `.with_system_fallbacks([])` here looks redundant — but stating it explicitly is what keeps
/// this fixture safe if that default ever changes. Every case built from this helper goes on to
/// DISPATCH a real provider CLI through `execvp`. If a stripped environment ever regained a system
/// fallback (accidentally or via a "helpful" default change), it could resolve a real `claude` /
/// `codex` / `grok` binary actually installed on the host box and start a real,
/// billable background job from a test run. Do not drop this call, and do not let a future
/// `Environment::new` default make it look unnecessary.
pub fn stripped_environment(root: Option<&Path>) -> Environment {
    let no_system_fallbacks: [PathBuf; 0] = [];
    match root {
        Some(root) => {
            let home = root.join("home");
            let empty = root.join("empty-path-dir");
            fs::create_dir_all(&home).expect("create the empty HOME");
            fs::create_dir_all(&empty).expect("create the empty PATH directory");
            Environment::new(
                Some(std::env::join_paths([&empty]).expect("join the fixture PATH")),
                Some(home),
                BTreeMap::new(),
            )
        }
        None => Environment::new(None, None, BTreeMap::new()),
    }
    .with_system_fallbacks(no_system_fallbacks)
}

/// A Context over a stripped environment, with HOME under `root/home` and default config.
pub fn stripped_context(root: &Path) -> Context {
    Context::new(
        stripped_environment(Some(root)),
        root.join("home"),
        Config::default(),
    )
}

/// A Context from an explicit environment, home, and config.
pub fn context(environment: Environment, home: PathBuf, config: Config) -> Context {
    Context::new(environment, home, config)
}

/// One run of a fixture T3 thread.
pub struct T3Run<'a> {
    pub ordinal: i64,
    pub status: &'a str,
    pub requested_at: &'a str,
    pub completed_at: Option<&'a str>,
    /// A queued run the user is holding, which T3's shell never presents as the thread's status.
    pub held: bool,
    /// The message of the run's root `error` item, written only for a failed run.
    pub failure: Option<&'a str>,
}

/// One thread in a fixture T3 state database.
pub struct T3Thread<'a> {
    pub id: &'a str,
    pub updated_at: &'a str,
    pub runs: &'a [T3Run<'a>],
    /// The `lastError` of the thread's bound provider session, or None for no session at all.
    pub session_error: Option<&'a str>,
    pub deleted: bool,
}

/// Write `<home>/.t3/userdata/statev2.sqlite` holding exactly `threads`, with the orchestration
/// protocol 2 projection tables the router reads shaped as T3 0.0.46 creates them (the columns the
/// router reads, plus the NOT NULL ones a row needs). This is the T3 state boundary: nothing past
/// it is faked.
pub fn write_t3_state(home: &Path, threads: &[T3Thread<'_>]) -> PathBuf {
    let dir = home.join(".t3/userdata");
    fs::create_dir_all(&dir).expect("create the T3 userdata directory");
    let path = dir.join("statev2.sqlite");
    let _ = fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).expect("create the T3 state database");
    conn.execute_batch(
        "CREATE TABLE orchestration_v2_projection_threads (
           thread_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
           default_provider TEXT NOT NULL, runtime_mode TEXT NOT NULL,
           interaction_mode TEXT NOT NULL, active_provider_thread_id TEXT,
           created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT,
           deleted_at TEXT, payload_json TEXT NOT NULL, provider_instance_id TEXT);
         CREATE TABLE orchestration_v2_projection_runs (
           run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
           provider TEXT NOT NULL, provider_thread_id TEXT, status TEXT NOT NULL,
           requested_at TEXT NOT NULL, completed_at TEXT, payload_json TEXT NOT NULL,
           provider_instance_id TEXT);
         CREATE TABLE orchestration_v2_projection_provider_sessions (
           provider_session_id TEXT PRIMARY KEY, thread_id TEXT, provider TEXT NOT NULL,
           status TEXT NOT NULL, model TEXT, updated_at TEXT NOT NULL,
           payload_json TEXT NOT NULL, driver TEXT, provider_instance_id TEXT);
         CREATE TABLE orchestration_v2_projection_provider_session_bindings (
           provider_session_id TEXT NOT NULL, thread_id TEXT NOT NULL,
           PRIMARY KEY (provider_session_id, thread_id));
         CREATE TABLE orchestration_v2_projection_turn_items (
           turn_item_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, run_id TEXT, node_id TEXT,
           provider_thread_id TEXT, provider_turn_id TEXT, parent_item_id TEXT,
           ordinal INTEGER NOT NULL, type TEXT NOT NULL, status TEXT NOT NULL,
           updated_at TEXT NOT NULL, payload_json TEXT NOT NULL);",
    )
    .expect("create the T3 projection tables");
    for thread in threads {
        conn.execute(
            "INSERT INTO orchestration_v2_projection_threads (thread_id, project_id, title, \
             default_provider, runtime_mode, interaction_mode, created_at, updated_at, \
             deleted_at, payload_json, provider_instance_id) VALUES (?1, 'proj-1', \
             'a fixture thread', 'claudeAgent', 'full-access', 'default', \
             '2026-10-04T00:00:00.000Z', ?2, ?3, '{}', 'claudeAgent')",
            rusqlite::params![
                thread.id,
                thread.updated_at,
                thread.deleted.then_some("2026-10-04T00:00:01.000Z")
            ],
        )
        .expect("insert a fixture thread");
        for run in thread.runs {
            let run_id = format!("run:{}:{}", thread.id, run.ordinal);
            let root = format!("node:{run_id}:root");
            let mut payload = serde_json::json!({
                "id": run_id,
                "threadId": thread.id,
                "ordinal": run.ordinal,
                "rootNodeId": root,
                "status": run.status,
                "startedAt": run.requested_at,
            });
            if run.held {
                payload["queueHeld"] = serde_json::json!(true);
            }
            conn.execute(
                "INSERT INTO orchestration_v2_projection_runs (run_id, thread_id, ordinal, \
                 provider, status, requested_at, completed_at, payload_json, \
                 provider_instance_id) VALUES (?1, ?2, ?3, 'claudeAgent', ?4, ?5, ?6, ?7, \
                 'claudeAgent')",
                rusqlite::params![
                    run_id,
                    thread.id,
                    run.ordinal,
                    run.status,
                    run.requested_at,
                    run.completed_at,
                    payload.to_string()
                ],
            )
            .expect("insert a fixture run");
            if let Some(message) = run.failure {
                let item = serde_json::json!({
                    "type": "error",
                    "status": "failed",
                    "failure": {"class": "provider_error", "message": message},
                });
                conn.execute(
                    "INSERT INTO orchestration_v2_projection_turn_items (turn_item_id, \
                     thread_id, run_id, node_id, ordinal, type, status, updated_at, \
                     payload_json) VALUES (?1, ?2, ?3, ?4, 1, 'error', 'failed', ?5, ?6)",
                    rusqlite::params![
                        format!("item:{run_id}"),
                        thread.id,
                        run_id,
                        root,
                        run.completed_at.unwrap_or(run.requested_at),
                        item.to_string()
                    ],
                )
                .expect("insert a fixture failure item");
            }
        }
        if let Some(error) = thread.session_error {
            let session = format!("session:{}", thread.id);
            conn.execute(
                "INSERT INTO orchestration_v2_projection_provider_sessions \
                 (provider_session_id, thread_id, provider, status, updated_at, payload_json, \
                 provider_instance_id) VALUES (?1, ?2, 'claudeAgent', 'error', ?3, ?4, \
                 'claudeAgent')",
                rusqlite::params![
                    session,
                    thread.id,
                    thread.updated_at,
                    serde_json::json!({"lastError": error}).to_string()
                ],
            )
            .expect("insert a fixture provider session");
            conn.execute(
                "INSERT INTO orchestration_v2_projection_provider_session_bindings \
                 (provider_session_id, thread_id) VALUES (?1, ?2)",
                rusqlite::params![session, thread.id],
            )
            .expect("bind the fixture provider session");
        }
    }
    path
}
