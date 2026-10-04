//! `agent-router nudge` through the real binary.
//!
//! The boundary faked is the one `run --surface t3` already uses: the t3-thread launcher pinned
//! with `AGENT_ROUTER_T3_THREAD_BIN`. Nudge runs its helper against the launcher's `lib` directory,
//! so the fixture's launcher directory carries fake lib modules in place of t3-thread's: a
//! `find-node.sh` naming the node this test found, a context that resolves no real `~/.t3`, an
//! auth session that mints nothing, the launcher's run status vocabulary and `message.dispatch`
//! builder, and an RPC client that serves a canned orchestration protocol 2
//! `orchestration.subscribeShell` snapshot and records every call it is asked to make. The helper
//! script, the bash wrapper, and the router's exit code mapping all run for real.
//!
//! HOME is a per-test temp directory, so the decision log consulted is this fixture's own.
#![cfg(unix)]

use agent_router_core::Surface;
use agent_router_core::config::Config;
use agent_router_core::decide::decide_explicit;
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::provider::Provider;
use agent_router_core::usage::UsageSnapshot;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

const T3_THREAD_BIN_ENV: &str = "AGENT_ROUTER_T3_THREAD_BIN";
const THREAD: &str = "thr_nudge";
const MESSAGE: &str = "Your background task finished; continue from its result.";

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-t3-nudge-{}-{serial}-{label}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The node the fake `find-node.sh` hands the wrapper: the first `node` on this test's own PATH.
fn node_binary() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set for the test process");
    std::env::split_paths(&path)
        .map(|dir| dir.join("node"))
        .find(|candidate| candidate.is_file())
        .expect("these tests need node on PATH, as the nudge helper is a node module")
}

fn js_string(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("a JSON string")
}

/// A shell thread in the shape orchestration protocol 2's `orchestration.subscribeShell` returns
/// it: `status` is `idle` before the first run and the latest run's status after.
fn shell_thread(id: &str, status: &str, active_run: Option<&str>) -> Value {
    json!({
        "id": id,
        "createdBy": "user",
        "creationSource": "web",
        "archivedAt": null,
        "deletedAt": null,
        "providerInstanceId": "claudeAgent",
        "modelSelection": {"instanceId": "claudeAgent", "model": "claude-opus-5-5", "options": [{"id": "effort", "value": "high"}]},
        "runtimeMode": "full-access",
        "interactionMode": "plan",
        "latestRunId": if status == "idle" { Value::Null } else { json!("run-1") },
        "activeRunId": active_run,
        "status": status,
        "lastError": null,
    })
}

struct NudgeFixture {
    root: TempDir,
}

impl NudgeFixture {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        let fixture = Self { root };
        fs::create_dir_all(fixture.home()).expect("create home");
        let lib = fixture.launcher_dir().join("lib");
        fs::create_dir_all(&lib).expect("create the fake lib");
        // Never run: nudge only uses the launcher to locate its lib.
        common::write_stub(&fixture.launcher(), "exit 99\n");
        fs::write(
            lib.join("find-node.sh"),
            format!(
                "find_node() {{ printf '%s\\n' '{}'; }}\n",
                node_binary().to_string_lossy()
            ),
        )
        .expect("write find-node.sh");
        fs::write(
            lib.join("context.mjs"),
            "export function resolveContext() {\n\
             \x20 return { baseDir: \"/fixture/t3\", bin: \"/fixture/t3/t3\", origin: \"http://fixture.invalid\" };\n\
             }\n\
             export function missingOriginError(baseDir) { return new Error(`no origin for ${baseDir}`); }\n",
        )
        .expect("write context.mjs");
        fs::write(
            lib.join("t3cli.mjs"),
            "export async function withT3Session(_opts, fn) { return fn(\"fixture-token\"); }\n",
        )
        .expect("write t3cli.mjs");
        fs::write(
            lib.join("version.mjs"),
            format!(
                "import fs from \"node:fs\";\n\
                 export async function ensureCompatibleServer() {{\n\
                 \x20 if (fs.existsSync({fail})) throw new Error(\"T3 server version 9.9.9 failed the structural preflight\");\n\
                 }}\n",
                fail = js_string(&fixture.file("version.fail")),
            ),
        )
        .expect("write version.mjs");
        fs::write(
            lib.join("commands.mjs"),
            "export function buildMessageDispatch({ threadId, messageId, text, modelSelection }) {\n\
             \x20 return { type: \"message.dispatch\", createdBy: \"user\", creationSource: \"web\", threadId, messageId, text, attachments: [], modelSelection, dispatchMode: { type: \"start_immediately\" } };\n\
             }\n",
        )
        .expect("write commands.mjs");
        fs::write(
            lib.join("completion.mjs"),
            "export const TERMINAL_RUN_STATUSES = [\"completed\", \"interrupted\", \"failed\", \"cancelled\", \"rolled_back\"];\n",
        )
        .expect("write completion.mjs");
        fs::write(
            lib.join("rpc.mjs"),
            format!(
                "import fs from \"node:fs\";\n\
                 const record = (tag, payload) => fs.appendFileSync({calls}, JSON.stringify({{ tag, payload }}) + \"\\n\");\n\
                 export async function connect() {{\n\
                 \x20 return {{ call: async (tag, payload) => {{ record(tag, payload); return {{}}; }}, close() {{}} }};\n\
                 }}\n\
                 export async function takeSnapshot(_client, tag) {{\n\
                 \x20 record(tag, null);\n\
                 \x20 return JSON.parse(fs.readFileSync({snapshot}, \"utf8\"));\n\
                 }}\n",
                calls = js_string(&fixture.file("calls.jsonl")),
                snapshot = js_string(&fixture.file("snapshot.json")),
            ),
        )
        .expect("write rpc.mjs");
        fixture.t3_holds(&[]);
        fixture
    }

    fn home(&self) -> PathBuf {
        self.root.path.join("home")
    }

    fn launcher_dir(&self) -> PathBuf {
        self.root.path.join("t3-thread")
    }

    fn launcher(&self) -> PathBuf {
        self.launcher_dir().join("t3-thread")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.path.join(name)
    }

    /// The threads the next shell snapshot lists.
    fn t3_holds(&self, threads: &[Value]) {
        fs::write(
            self.file("snapshot.json"),
            json!({"projects": [], "threads": threads}).to_string(),
        )
        .expect("write the snapshot");
    }

    /// Every RPC call the helper made, in order.
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.file("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a recorded call"))
            .collect()
    }

    fn dispatched(&self) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|call| call["tag"] == "orchestration.dispatchCommand")
            .map(|call| call["payload"].clone())
            .collect()
    }

    fn seed(&self, job_id: &str, surface: Surface) {
        let decision = decide_explicit(
            Provider::Claude,
            None,
            None,
            None,
            UsageSnapshot::full(),
            &Config::default(),
        );
        DecisionLog::open_at(&self.home().join(".local/state/agent-router/router.db"))
            .expect("open the fixture log")
            .record(&Entry {
                task: "a seeded launch",
                dir: Path::new("/tmp"),
                requested: "claude",
                decision: &decision,
                dry_run: false,
                job_id: Some(job_id),
                job_name: Some("A Seeded Job"),
                outcome: "dispatched",
                effective_effort: None,
                surface,
                thread_url: None,
            })
            .expect("seed a row");
    }

    fn nudge(&self, job_id: &str, message: &str) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_agent-router"))
            .args(["nudge", job_id, "--message", message, "--json"])
            .env("HOME", self.home())
            .env(T3_THREAD_BIN_ENV, self.launcher())
            // bash and the coreutils the wrapper needs; node comes from the fake find-node.sh.
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("run the router")
    }
}

fn answer(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "nudge --json printed no JSON ({error}); stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A thread whose latest run completed gets exactly one `message.dispatch`, carrying the message,
/// a fresh message id, and the thread's own model selection.
#[test]
fn a_t3_thread_whose_run_ended_is_sent_one_message_reusing_its_own_selection() {
    let fixture = NudgeFixture::new("sent");
    fixture.seed(THREAD, Surface::T3);
    let thread = shell_thread(THREAD, "completed", None);
    fixture.t3_holds(&[shell_thread("thr_other", "completed", None), thread.clone()]);

    let output = fixture.nudge(THREAD, MESSAGE);

    assert_exit(&output, 0);
    let answer = answer(&output);
    assert_eq!(answer["job_id"], THREAD);
    assert_eq!(answer["sent"], true);
    assert!(answer["reason"].as_str().is_some_and(|r| !r.is_empty()));
    let dispatched = fixture.dispatched();
    assert_eq!(dispatched.len(), 1, "exactly one dispatch: {dispatched:?}");
    let message = &dispatched[0];
    assert_eq!(message["type"], "message.dispatch");
    assert_eq!(message["threadId"], THREAD);
    assert_eq!(message["text"], MESSAGE);
    assert!(
        message["messageId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{message}"
    );
    assert_eq!(message["modelSelection"], thread["modelSelection"]);
}

/// A thread whose latest run failed (a server restart, say), was interrupted, or that has never
/// run, has nothing in flight and may be nudged.
#[test]
fn a_failed_interrupted_or_idle_t3_thread_is_sent_a_message() {
    for status in ["failed", "interrupted", "idle"] {
        let fixture = NudgeFixture::new("ended");
        fixture.seed(THREAD, Surface::T3);
        fixture.t3_holds(&[shell_thread(THREAD, status, None)]);

        let output = fixture.nudge(THREAD, MESSAGE);

        assert_exit(&output, 0);
        assert_eq!(answer(&output)["sent"], true, "a {status} thread");
        assert_eq!(fixture.dispatched().len(), 1, "a {status} thread");
    }
}

/// A follow-up sent into an active run would interleave with it, so nudge refuses with exit 4 and
/// dispatches nothing, whether the activity shows as an active run id or as a latest run that has
/// not reached a terminal status.
#[test]
fn a_t3_thread_with_an_active_run_is_refused_with_exit_four() {
    for (status, run) in [
        ("running", Some("run-7")),
        ("completed", Some("run-8")),
        ("queued", None),
        ("waiting", None),
    ] {
        let fixture = NudgeFixture::new("active");
        fixture.seed(THREAD, Surface::T3);
        fixture.t3_holds(&[shell_thread(THREAD, status, run)]);

        let output = fixture.nudge(THREAD, MESSAGE);

        assert_exit(&output, 4);
        let answer = answer(&output);
        assert_eq!(answer["job_id"], THREAD);
        assert_eq!(answer["sent"], false);
        assert!(
            answer["reason"]
                .as_str()
                .is_some_and(|r| r.contains("active run")),
            "{answer}"
        );
        assert!(
            fixture.dispatched().is_empty(),
            "a {status} thread was sent a message"
        );
    }
}

/// A logged t3 job whose thread T3 no longer holds is exit 3, and nothing is dispatched.
#[test]
fn a_t3_job_whose_thread_is_gone_is_not_found_with_exit_three() {
    let fixture = NudgeFixture::new("thread-gone");
    fixture.seed(THREAD, Surface::T3);
    fixture.t3_holds(&[shell_thread("thr_other", "completed", None)]);

    let output = fixture.nudge(THREAD, MESSAGE);

    assert_exit(&output, 3);
    let answer = answer(&output);
    assert_eq!(answer["sent"], false);
    assert!(fixture.dispatched().is_empty());
}

/// A job id the decision log never dispatched is exit 3 before T3 is asked anything, so nudge
/// cannot be pointed at a thread the router did not launch.
#[test]
fn a_job_the_log_never_dispatched_is_not_found_without_asking_t3() {
    let fixture = NudgeFixture::new("not-logged");
    fixture.t3_holds(&[shell_thread(THREAD, "completed", None)]);

    let output = fixture.nudge(THREAD, MESSAGE);

    assert_exit(&output, 3);
    assert_eq!(answer(&output)["sent"], false);
    assert!(
        fixture.calls().is_empty(),
        "T3 was asked: {:?}",
        fixture.calls()
    );
}

/// A background job has no T3 thread, and an empty message is no turn at all: both are usage.
#[test]
fn a_background_job_or_an_empty_message_is_a_usage_error() {
    let fixture = NudgeFixture::new("usage");
    fixture.seed("bg-job", Surface::Background);
    fixture.seed(THREAD, Surface::T3);
    fixture.t3_holds(&[shell_thread(THREAD, "completed", None)]);

    let background = fixture.nudge("bg-job", MESSAGE);
    assert_exit(&background, 2);
    assert_eq!(answer(&background)["sent"], false);

    let empty = fixture.nudge(THREAD, "   ");
    assert_exit(&empty, 2);
    assert_eq!(answer(&empty)["sent"], false);

    assert!(
        fixture.calls().is_empty(),
        "T3 was asked: {:?}",
        fixture.calls()
    );
}

/// A helper that fails before reaching a verdict (here the version gate) is a runtime error, exit
/// 1, carrying the helper's message, and nothing is dispatched.
#[test]
fn a_helper_failure_is_a_runtime_error_with_exit_one() {
    let fixture = NudgeFixture::new("helper-fails");
    fixture.seed(THREAD, Surface::T3);
    fixture.t3_holds(&[shell_thread(THREAD, "completed", None)]);
    fs::write(fixture.file("version.fail"), "").expect("arm the version gate");

    let output = fixture.nudge(THREAD, MESSAGE);

    assert_exit(&output, 1);
    let answer = answer(&output);
    assert_eq!(answer["sent"], false);
    assert!(
        answer["reason"]
            .as_str()
            .is_some_and(|r| r.contains("structural preflight")),
        "{answer}"
    );
    assert!(fixture.dispatched().is_empty());
}
