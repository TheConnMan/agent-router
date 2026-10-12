//! Commit 1 through the real binary: `agent-router status` against a decision log this fixture
//! seeded itself, reconciled against stubbed backends.
//!
//! Live data safety: every invocation points HOME at a temp directory, and `default_db_path()` is
//! built on `home_dir()`, so the log under test is this fixture's own file. The trace probe
//! resolves through the same `home_dir()`, so the transcripts it sweeps are this fixture's too. PATH
//! holds only what the fixture put on it, so no `claude` and no codex daemon this box happens to be
//! running can decide an assertion.
//!
//! The whole file is unix only because the fixture stubs its provider binaries as shell scripts,
//! which is the idiom `stats_cli.rs` and `doctor_cli.rs` already use.
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
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// One copy of the stub helper, included by path from the core crate's tests. A workspace
// `test-support` dev-dependency crate is the intended follow-up; this shape keeps the helper single
// sourced without editing Cargo.toml while another stream owns it.
#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

/// A claude short id is the first segment of a session UUID, which is what the transcript glob
/// anchors on.
const TRACED_JOB: &str = "a384f762";
const UNTRACED_JOB: &str = "b1c2d3e4";

/// Makes every temp directory this file creates distinct from every other one, whatever the clock
/// does. `fs::create_dir_all` succeeds silently on a path that already exists, so two tests deriving
/// the same path would share one HOME and therefore one `router.db`, and one fixture's `Drop` would
/// delete a live sibling's directories mid run.
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
            "agent-router-status-cli-{}-{serial}-{label}-{unique}",
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// The rows `claude agents --json --all` prints, one per running or recently finished job.
fn agent_rows(jobs: &[(&str, &str)]) -> String {
    let rows: Vec<Value> = jobs
        .iter()
        .map(|(id, state)| {
            json!({
                "id": id,
                "cwd": "/tmp",
                "name": "a routed job",
                "startedAt": 0,
                "kind": "background",
                "state": state,
            })
        })
        .collect();
    Value::Array(rows).to_string()
}

struct StatusFixture {
    root: TempDir,
}

impl StatusFixture {
    /// A fixture whose claude reports an empty agent list, which is the bounded window having aged
    /// every job out, and whose codex refuses every invocation, so the daemon probe answers None.
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        for child in ["home", "bin", "grok-home"] {
            fs::create_dir_all(root.path.join(child)).expect("create fixture directory");
        }
        // The helper writes the interpreter line, so the body starts at the first real command.
        common::write_stub(&root.path.join("bin/codex"), "exit 1\n");
        let fixture = Self { root };
        fixture.claude_reports(&[]);
        fixture
    }

    fn home(&self) -> PathBuf {
        self.root.path.join("home")
    }

    fn db_path(&self) -> PathBuf {
        self.home().join(".local/state/agent-router/router.db")
    }

    /// What the next `claude agents --json --all` call answers. Rewriting it between runs is how
    /// a job aging out of the recent window is reproduced.
    fn claude_reports(&self, jobs: &[(&str, &str)]) {
        // No interpreter line here either: the helper supplies exactly one, and a second would run
        // as a comment and hide the mistake.
        let body = format!("printf '%s\\n' {}\n", shell_quote(&agent_rows(jobs)));
        common::write_stub(&self.root.path.join("bin/claude"), &body);
    }

    /// A transcript on disk under a project directory the glob has to wildcard over, because no
    /// path encoding may be derived from the row's own directory.
    fn write_transcript(&self, short_id: &str) {
        let projects = self.home().join(".claude/projects/a fixture project");
        fs::create_dir_all(&projects).expect("create the projects directory");
        fs::write(
            projects.join(format!("{short_id}-4e0f-4c1a-9b8e-2f1d3c4b5a60.jsonl")),
            "{\"type\":\"system\"}\n",
        )
        .expect("write the transcript");
    }

    /// A projects tree with nothing matching in it, so the sweep runs and finds nothing. That is a
    /// different fact from the sweep being unable to run at all.
    fn write_empty_projects(&self) {
        let projects = self.home().join(".claude/projects/a fixture project");
        fs::create_dir_all(&projects).expect("create the projects directory");
        fs::write(
            projects.join("11111111-2222-3333-4444-555555555555.jsonl"),
            "{\"type\":\"system\"}\n",
        )
        .expect("write an unrelated transcript");
    }

    /// One logged decision, written through the same writer a dispatch uses, so the row under test
    /// is shaped exactly like a real one. Returns its row id.
    fn seed(
        &self,
        provider: Provider,
        job_id: Option<&str>,
        job_name: Option<&str>,
        dry_run: bool,
        outcome: &str,
    ) -> i64 {
        let decision = decide_explicit(
            provider,
            None,
            None,
            None,
            UsageSnapshot::full(),
            &Config::default(),
        );
        let log = DecisionLog::open_at(&self.db_path()).expect("open the fixture log");
        log.record(&Entry {
            task: "a seeded routing decision",
            dir: Path::new("/tmp"),
            requested: provider.name(),
            decision: &decision,
            dry_run,
            job_id,
            job_name,
            outcome,
            effective_effort: None,
            surface: Surface::Background,
            thread_url: None,
        })
        .expect("seed a decision row")
    }

    /// One decision launched on the t3 surface, whose job id is the T3 thread id.
    fn seed_t3(&self, provider: Provider, job_id: &str) -> i64 {
        let decision = decide_explicit(
            provider,
            None,
            None,
            None,
            UsageSnapshot::full(),
            &Config::default(),
        );
        let url = format!("http://127.0.0.1:3773/t/{job_id}");
        DecisionLog::open_at(&self.db_path())
            .expect("open the fixture log")
            .record(&Entry {
                task: "a seeded t3 launch",
                dir: Path::new("/tmp"),
                requested: provider.name(),
                decision: &decision,
                dry_run: false,
                job_id: Some(job_id),
                job_name: Some("A T3 Thread"),
                outcome: "dispatched",
                effective_effort: None,
                surface: Surface::T3,
                thread_url: Some(&url),
            })
            .expect("seed a t3 row")
    }

    fn router(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .env("HOME", self.home())
            .env("GROK_HOME", self.root.path.join("grok-home"))
            .env(
                "GROK_USAGE_CACHE",
                self.root.path.join("grok-usage-cache.json"),
            )
            .env("PATH", self.root.path.join("bin"));
        command
    }

    fn status(&self) -> Output {
        self.router()
            .arg("status")
            .output()
            .expect("run the router")
    }

    fn status_json(&self) -> Value {
        let output = self
            .router()
            .args(["status", "--json"])
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "status --json failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("status json")
    }

    fn log_rows(&self) -> Vec<Value> {
        let output = self
            .router()
            .args(["log", "--json", "--limit", "50"])
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "log --json failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Value = serde_json::from_slice(&output.stdout).expect("log json");
        rows.as_array().expect("log --json prints an array").clone()
    }

    fn logged(&self, id: i64) -> Value {
        self.log_rows()
            .into_iter()
            .find(|row| row["id"] == json!(id))
            .unwrap_or_else(|| panic!("no logged row with id {id}"))
    }
}

/// The rows `status --json` reported. The report carries window bounds alongside them, so it is an
/// object rather than the bare array `log --json` prints.
fn reported(status: &Value) -> Vec<Value> {
    status["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("status --json must carry a rows array, got {status}"))
        .clone()
}

fn reported_row(status: &Value, id: i64) -> Value {
    reported(status)
        .into_iter()
        .find(|row| row["id"] == json!(id))
        .unwrap_or_else(|| panic!("status --json did not report row {id}: {status}"))
}

/// Acceptance criterion 1. No backend knows this job, and an absent job is equally consistent with
/// completed, crashed at startup, and never started, so the honest reading is that the router
/// cannot tell. A classifier that mapped absence to `completed` would pass every other test here.
#[test]
fn an_unresolvable_job_reconciles_to_unknown_rather_than_completed() {
    let fixture = StatusFixture::new("unresolvable");
    let id = fixture.seed(
        Provider::Claude,
        Some("c0ffee42"),
        Some("a routed job"),
        false,
        "dispatched",
    );

    let status = fixture.status_json();

    let row = reported_row(&status, id);
    assert_eq!(row["state"], "unknown", "the reported row: {row}");
    assert_eq!(row["observation"], "absent", "the reported row: {row}");
    assert_eq!(row["job_id"], "c0ffee42", "the reported row: {row}");

    let logged = fixture.logged(id);
    assert_eq!(
        logged["outcome"], "unknown",
        "an absent job was recorded as something the router cannot know: {logged}"
    );
    assert!(
        logged["reconciled_at_ms"].is_i64(),
        "a row the reconciler read must carry the time it read it: {logged}"
    );
}

/// The most consequential cell of the mapping table. An operator killing a healthy job, or stopping
/// one that had already produced what was needed, is not a routing failure, and `failed` here would
/// be a sticky wrong terminal state that the monotonicity rule then protects forever.
#[test]
fn a_stopped_claude_job_settles_to_unknown_rather_than_failed() {
    let fixture = StatusFixture::new("stopped");
    let id = fixture.seed(
        Provider::Claude,
        Some("57099ed0"),
        Some("a routed job"),
        false,
        "dispatched",
    );
    fixture.claude_reports(&[("57099ed0", "stopped")]);

    let output = fixture.status();
    assert_eq!(
        output.status.code(),
        Some(0),
        "a stopped job is not a finding, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let status = fixture.status_json();
    let row = reported_row(&status, id);
    assert_eq!(row["state"], "unknown", "the reported row: {row}");

    let logged = fixture.logged(id);
    assert_eq!(
        logged["outcome"], "unknown",
        "a stopped job says the work ended early, never that it failed: {logged}"
    );
}

/// The monotonicity rule through the real path. A job proven complete today ages out of the bounded
/// recent window tomorrow, and overwriting it then would be a permanent retroactive loss of a fact
/// the router had already established.
#[test]
fn an_aged_out_job_keeps_its_proven_completion_rather_than_regressing_to_unknown() {
    let fixture = StatusFixture::new("aged-out");
    let id = fixture.seed(
        Provider::Claude,
        Some("d0ne1234"),
        Some("a routed job"),
        false,
        "dispatched",
    );

    fixture.claude_reports(&[("d0ne1234", "done")]);
    let first = fixture.status_json();
    assert_eq!(reported_row(&first, id)["state"], "completed");
    let proven = fixture.logged(id);
    assert_eq!(
        proven["outcome"], "completed",
        "claude's own terminal marker must be recorded: {proven}"
    );
    let stamped = proven["reconciled_at_ms"]
        .as_i64()
        .unwrap_or_else(|| panic!("the reconciled row must carry a stamp: {proven}"));

    fixture.claude_reports(&[]);
    let second = fixture.status_json();
    assert_eq!(
        reported_row(&second, id)["state"],
        "unknown",
        "the second reading genuinely found nothing, which is what makes this a regression test"
    );

    let kept = fixture.logged(id);
    assert_eq!(
        kept["outcome"], "completed",
        "an aged out window overwrote a proven completion: {kept}"
    );
    assert_eq!(
        kept["reconciled_at_ms"].as_i64(),
        Some(stamped),
        "nothing was written, so the stamp of the last reading that landed must not move: {kept}"
    );
}

/// The trace is evidence and never a verdict. The transcript answers a question the state cannot,
/// which is whether there is anything to go read, and a job with a transcript is still a job the
/// router cannot resolve.
#[test]
fn a_traced_but_unresolvable_job_still_settles_to_unknown() {
    let fixture = StatusFixture::new("traced");
    let id = fixture.seed(
        Provider::Claude,
        Some(TRACED_JOB),
        Some("a routed job"),
        false,
        "dispatched",
    );
    fixture.write_transcript(TRACED_JOB);

    let status = fixture.status_json();

    let row = reported_row(&status, id);
    assert_eq!(
        row["traced"],
        json!(true),
        "the transcript on disk must be reported: {row}"
    );
    assert_eq!(
        row["state"], "unknown",
        "a transcript read as proof of completion is the fabrication this design exists to \
         prevent: {row}"
    );

    let logged = fixture.logged(id);
    assert_eq!(logged["outcome"], "unknown", "the logged row: {logged}");
}

/// The sibling with nothing on disk. The two tests together prove the trace varies while the state
/// does not, which is what makes the separation real rather than merely asserted.
#[test]
fn an_untraced_unresolvable_job_reports_no_trace_and_the_same_state() {
    let fixture = StatusFixture::new("untraced");
    let id = fixture.seed(
        Provider::Claude,
        Some(UNTRACED_JOB),
        Some("a routed job"),
        false,
        "dispatched",
    );
    fixture.write_empty_projects();

    let status = fixture.status_json();

    let row = reported_row(&status, id);
    assert_eq!(
        row["traced"],
        json!(false),
        "the sweep ran and found nothing, which is a fact worth reporting: {row}"
    );
    assert_eq!(row["state"], "unknown", "the reported row: {row}");
}

/// A codex row carries no trace probe at all, so it reports null rather than false. We did not look
/// and we looked and found nothing are different facts about the router, the same distinction the
/// usage marker draws between a fail open read and an idle provider.
///
/// The fixture has no daemon, so this is also the codex leg reporting unavailable through the real
/// path while the claude row in the same window still reconciles.
#[test]
fn a_codex_row_carries_no_trace_probe() {
    let fixture = StatusFixture::new("codex-no-trace");
    let codex = fixture.seed(
        Provider::Codex,
        Some("019fbce9-ca50-7f32-93d1-54a783a45b51"),
        Some("a routed job"),
        false,
        "dispatched",
    );
    let claude = fixture.seed(
        Provider::Claude,
        Some("d0ne1234"),
        Some("a routed job"),
        false,
        "dispatched",
    );
    fixture.claude_reports(&[("d0ne1234", "done")]);
    // A file the glob would match if the sweep ever ran on a codex row, so `traced` being null is
    // the probe being skipped by provider rather than the disk happening to be empty.
    fixture.write_transcript("019fbce9");

    let status = fixture.status_json();

    let codex_row = reported_row(&status, codex);
    assert_eq!(
        codex_row["traced"],
        Value::Null,
        "a codex row was swept for a claude transcript: {codex_row}"
    );
    assert_eq!(
        codex_row["observation"], "unavailable",
        "no daemon answered, and the report has to say that is why: {codex_row}"
    );
    assert_eq!(codex_row["state"], "unknown");

    let claude_row = reported_row(&status, claude);
    assert_eq!(
        claude_row["state"], "completed",
        "a codex outage cost a claude row in the same window its reconciliation: {claude_row}"
    );
}

/// A dry run dispatched nothing, so it has no job to reconcile and must never be reported as a
/// failure. Treating it as a job nobody has heard from is what would poison the failure rate.
#[test]
fn a_dry_run_row_is_never_reconciled() {
    let fixture = StatusFixture::new("dry-run");
    let id = fixture.seed(Provider::Claude, None, None, true, "dry-run");

    let status = fixture.status_json();
    assert!(
        reported(&status).iter().all(|row| row["id"] != json!(id)),
        "a dry run appeared in the reconciliation window: {status}"
    );

    let logged = fixture.logged(id);
    assert_eq!(logged["outcome"], "dry-run", "the logged row: {logged}");
    assert_eq!(
        logged["reconciled_at_ms"],
        Value::Null,
        "a row nothing was read for must not carry a reading time: {logged}"
    );
}

/// The row that pins the whole safety argument for updating `outcome` in place. A dispatch error
/// binds `job_id` to None, so the exact backend message is outside the reconciliation window by
/// construction, and it is the most valuable text in the tuning data.
#[test]
fn a_failed_dispatch_row_is_never_reconciled_because_it_has_no_job_id() {
    let fixture = StatusFixture::new("failed-dispatch");
    let message = "error: app-server spawn failed: connection refused";
    let id = fixture.seed(Provider::Codex, None, None, false, message);

    let status = fixture.status_json();
    assert!(
        reported(&status).iter().all(|row| row["id"] != json!(id)),
        "a dispatch error was asked about as if it were a job: {status}"
    );

    let logged = fixture.logged(id);
    assert_eq!(
        logged["outcome"], message,
        "the backend's own message was overwritten: {logged}"
    );
    assert_eq!(
        logged["reconciled_at_ms"],
        Value::Null,
        "nothing was read for this row, so nothing may claim it was: {logged}"
    );
}

/// A claude dispatch that succeeded but whose short id never resolved is excluded by the same
/// predicate. Keeping `dispatched` is honest, because the router holds no handle on the job.
#[test]
fn a_row_with_no_job_id_reconciles_to_nothing_and_keeps_dispatched() {
    let fixture = StatusFixture::new("no-job-id");
    let id = fixture.seed(
        Provider::Claude,
        None,
        Some("a routed job"),
        false,
        "dispatched",
    );

    let status = fixture.status_json();
    assert!(
        reported(&status).iter().all(|row| row["id"] != json!(id)),
        "a row with no job id was reported as reconcilable: {status}"
    );

    let logged = fixture.logged(id);
    assert_eq!(logged["outcome"], "dispatched", "the logged row: {logged}");
    assert_eq!(
        logged["reconciled_at_ms"],
        Value::Null,
        "the logged row: {logged}"
    );
}

/// The exit contract. An absence of information is not a finding, so `unknown` never moves the exit
/// code, and neither does a job that finished.
///
/// The `failed` leg is not driven here: with the backends stubbed, the only routes to `failed` are
/// a codex turn record and a codex thread fault, both of which need a live app-server daemon that
/// this fixture deliberately refuses to provide. `tests/status.rs` owns those two classifications.
#[test]
fn status_exits_nonzero_only_when_a_row_reconciled_to_failed() {
    let absent = StatusFixture::new("exit-absent");
    absent.seed(
        Provider::Claude,
        Some("c0ffee42"),
        Some("a routed job"),
        false,
        "dispatched",
    );
    let output = absent.status();
    assert_eq!(
        output.status.code(),
        Some(0),
        "a window of unknowns is an absence of information, not a finding, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let finished = StatusFixture::new("exit-finished");
    let id = finished.seed(
        Provider::Claude,
        Some("d0ne1234"),
        Some("a routed job"),
        false,
        "dispatched",
    );
    finished.claude_reports(&[("d0ne1234", "done")]);
    let output = finished.status();
    assert_eq!(
        output.status.code(),
        Some(0),
        "a completed job is not a finding, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(finished.logged(id)["outcome"], "completed");
}

/// The other half of the exit contract, and the half that only shows up on the second run. A job
/// proven failed ages out of its backend's window, the fresh reading is `unavailable`, and the
/// monotonicity rule keeps the stored `failed`. An exit code taken from the fresh reading alone
/// would report a clean window over a database that still records the failure, so the exit code and
/// the log would disagree about the same row.
#[test]
fn a_persisted_failure_still_exits_nonzero_once_the_backend_can_no_longer_be_asked() {
    let fixture = StatusFixture::new("persisted-failure");
    let id = fixture.seed(
        Provider::Codex,
        Some("019fbce9-ca50-7f32-93d1-54a783a45b51"),
        Some("a routed job"),
        false,
        "dispatched",
    );

    // What an earlier reconciliation left behind when the codex turn record proved the job failed.
    // This fixture keeps no daemon, so the reconciler's own writer stands in for that first run.
    let log = DecisionLog::open_at(&fixture.db_path()).expect("open the fixture log");
    log.reconcile(id, "failed")
        .expect("record the proven failure");
    drop(log);

    let output = fixture.status();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("unknown unavailable"),
        "the second run has to genuinely fail to reach the daemon, which is what makes this a \
         regression test, stdout: {stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "the window still holds a row recorded as failed, stdout: {stdout}"
    );

    let logged = fixture.logged(id);
    assert_eq!(
        logged["outcome"], "failed",
        "the exit code and the log must never disagree about the same row: {logged}"
    );
}

/// Grok has a lifecycle to inspect, even when an isolated home carries no session data. Its
/// absence or temporary unavailability is an unknown fresh observation, and must not replace
/// terminal facts a prior lifecycle observation already proved.
#[test]
fn a_grok_lifecycle_observation_preserves_proven_terminal_outcomes() {
    let fixture = StatusFixture::new("grok-empty-home");
    let completed = fixture.seed(
        Provider::Grok,
        Some("grok-session-completed"),
        Some("a routed job"),
        false,
        "dispatched",
    );
    let failed = fixture.seed(
        Provider::Grok,
        Some("grok-session-failed"),
        Some("a routed job"),
        false,
        "dispatched",
    );

    let log = DecisionLog::open_at(&fixture.db_path()).expect("open the fixture log");
    log.reconcile(completed, "completed")
        .expect("record the proven completion");
    log.reconcile(failed, "failed")
        .expect("record the proven failure");
    drop(log);

    let output = fixture
        .router()
        .args(["status", "--json"])
        .output()
        .expect("run the router");
    assert_eq!(
        output.status.code(),
        Some(1),
        "a persisted failed outcome remains a finding, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("status json");
    for id in [completed, failed] {
        let row = reported_row(&status, id);
        assert!(
            matches!(row["observation"].as_str(), Some("absent" | "unavailable")),
            "an empty GROK_HOME must be observed through Grok lifecycle rather than treated as unsupported: {row}"
        );
        assert_eq!(row["state"], "unknown", "the reported row: {row}");
    }

    assert_eq!(fixture.logged(completed)["outcome"], "completed");
    assert_eq!(fixture.logged(failed)["outcome"], "failed");
}

/// An empty window prints its header and exits zero, in the same shape `stats` reports a window
/// with no denominator.
#[test]
fn status_on_an_empty_database_exits_zero_and_reports_no_rows() {
    let fixture = StatusFixture::new("empty-db");

    let output = fixture.status();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let status = fixture.status_json();
    assert!(
        reported(&status).is_empty(),
        "an empty log reported rows: {status}"
    );
}

/// A T3 thread id is neither a claude short id nor a codex thread id, so no provider backend is
/// asked about a t3 row: the claude stub logs every invocation and must stay silent, and no
/// transcript sweep runs. With no T3 state database under HOME, the router cannot ask T3 either, so
/// both rows observe `unavailable`, classify `unknown`, and carry null run fields.
#[test]
fn t3_surface_rows_are_never_reconciled_as_claude_or_codex_and_are_unavailable_without_t3_state() {
    let fixture = StatusFixture::new("t3-unavailable");
    let invocations = fixture.root.path.join("claude.invocations");
    common::write_stub(
        &fixture.root.path.join("bin/claude"),
        &format!(
            "printf '%s\\n' \"$*\" >> {}\nprintf '[]\\n'\n",
            shell_quote(&invocations.to_string_lossy())
        ),
    );
    // A transcript whose name matches the claude-shaped row's id, so a sweep that wrongly ran on a
    // t3 row would find it and report `traced`.
    fixture.write_transcript("thr_a");
    let claude = fixture.seed_t3(Provider::Claude, "thr_a");
    let codex = fixture.seed_t3(Provider::Codex, "thr_b");

    let status = fixture.status_json();
    for id in [claude, codex] {
        let row = reported_row(&status, id);
        assert_eq!(row["observation"], "unavailable", "the reported row: {row}");
        assert_eq!(row["state"], "unknown", "the reported row: {row}");
        assert_eq!(row["surface"], "t3", "the reported row: {row}");
        assert_eq!(
            row["traced"],
            Value::Null,
            "a t3 row was swept for a claude transcript: {row}"
        );
        for key in ["run_status", "last_activity_at", "last_error"] {
            assert!(
                row.get(key).is_some_and(Value::is_null),
                "a t3 row carries {key} as null when T3 cannot be read: {row}"
            );
        }
    }
    assert!(
        !invocations.exists(),
        "a t3 row made the router ask claude about it: {}",
        fs::read_to_string(&invocations).unwrap_or_default()
    );
    for id in [claude, codex] {
        let logged = fixture.logged(id);
        assert_eq!(
            logged["surface"], "t3",
            "log --json carries the surface: {logged}"
        );
    }

    let output = fixture.status();
    assert!(
        output.status.success(),
        "unknown rows are not failures, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        text.matches(" surface t3").count(),
        2,
        "text mode marks each t3 row: {text}"
    );
}

/// T3 rows read their state from T3's own orchestration protocol 2 state database. A thread's
/// status is its latest unheld run's: a queued or running run is `running`, a completed or
/// interrupted one is `turn_ended` (never `completed`), a failed one is `failed` with its error
/// passed through, and a thread that never ran, or that T3 does not hold (or deleted), is
/// `unknown`. Every t3 row carries `run_status`, `last_activity_at`, and `last_error`, and a
/// background row carries none of them.
#[test]
fn t3_rows_report_running_turn_ended_failed_and_unknown_from_t3_runs() {
    use common::{T3Run, T3Thread};

    let fixture = StatusFixture::new("t3-state");
    const RESTART: &str = "Provider session did not survive a server restart";
    const HANDOFF: &str = "Insufficient context allowance for the provider handoff";
    let run = |ordinal, status, requested_at, completed_at| T3Run {
        ordinal,
        status,
        requested_at,
        completed_at,
        held: false,
        failure: None,
    };
    let thread = |id, updated_at, runs| T3Thread {
        id,
        updated_at,
        runs,
        session_error: None,
        deleted: false,
    };
    common::write_t3_state(
        &fixture.home(),
        &[
            // An earlier completed run does not hide the running one after it.
            thread(
                "thr_running",
                "2026-10-04T10:00:00.000Z",
                &[
                    run(
                        1,
                        "completed",
                        "2026-10-04T09:00:00.000Z",
                        Some("2026-10-04T09:30:00.000Z"),
                    ),
                    run(2, "running", "2026-10-04T10:07:30.000Z", None),
                ],
            ),
            thread(
                "thr_queued",
                "2026-10-04T10:00:00.000Z",
                &[run(1, "queued", "2026-10-04T10:01:00.000Z", None)],
            ),
            // The run completed after the thread's own last update, so it is the newest activity.
            thread(
                "thr_completed",
                "2026-10-04T10:00:00.000Z",
                &[run(
                    1,
                    "completed",
                    "2026-10-04T10:30:00.000Z",
                    Some("2026-10-04T11:00:00.000Z"),
                )],
            ),
            // A held queued run never stands for the thread: its status is the interrupted run's.
            thread(
                "thr_interrupted",
                "2026-10-04T09:00:00.000Z",
                &[
                    run(
                        1,
                        "interrupted",
                        "2026-10-04T08:00:00.000Z",
                        Some("2026-10-04T08:30:00.000Z"),
                    ),
                    T3Run {
                        held: true,
                        ..run(2, "queued", "2026-10-04T08:45:00.000Z", None)
                    },
                ],
            ),
            // The failed run's root error item is the error when no provider session has one.
            thread(
                "thr_failed",
                "2026-10-04T08:00:00.000Z",
                &[T3Run {
                    failure: Some(HANDOFF),
                    ..run(
                        1,
                        "failed",
                        "2026-10-04T07:00:00.000Z",
                        Some("2026-10-04T07:59:00.000Z"),
                    )
                }],
            ),
            // A provider session error takes precedence over the run's own failure item.
            T3Thread {
                session_error: Some(RESTART),
                ..thread(
                    "thr_session_error",
                    "2026-10-04T08:00:00.000Z",
                    &[T3Run {
                        failure: Some(HANDOFF),
                        ..run(
                            1,
                            "failed",
                            "2026-10-04T07:00:00.000Z",
                            Some("2026-10-04T07:30:00.000Z"),
                        )
                    }],
                )
            },
            thread("thr_never_ran", "2026-10-04T06:00:00.000Z", &[]),
            T3Thread {
                deleted: true,
                ..thread(
                    "thr_deleted",
                    "2026-10-04T08:00:00.000Z",
                    &[run(
                        1,
                        "completed",
                        "2026-10-04T07:00:00.000Z",
                        Some("2026-10-04T07:30:00.000Z"),
                    )],
                )
            },
        ],
    );
    let running = fixture.seed_t3(Provider::Claude, "thr_running");
    let queued = fixture.seed_t3(Provider::Codex, "thr_queued");
    let completed = fixture.seed_t3(Provider::Claude, "thr_completed");
    let interrupted = fixture.seed_t3(Provider::Codex, "thr_interrupted");
    let failed = fixture.seed_t3(Provider::Claude, "thr_failed");
    let session_error = fixture.seed_t3(Provider::Claude, "thr_session_error");
    let never_ran = fixture.seed_t3(Provider::Claude, "thr_never_ran");
    let missing = fixture.seed_t3(Provider::Claude, "thr_missing");
    let deleted = fixture.seed_t3(Provider::Claude, "thr_deleted");
    let background = fixture.seed(
        Provider::Codex,
        Some("019c-background"),
        Some("A Background Job"),
        false,
        "dispatched",
    );

    let output = fixture
        .router()
        .args(["status", "--json"])
        .output()
        .expect("run the router");
    // A failed run is a known failure, so status exits 1 exactly as for any failed row.
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("status json");

    let expect = |id: i64,
                  state: &str,
                  observation: &str,
                  run_status: Value,
                  last_activity_at: Value,
                  last_error: Value| {
        let row = reported_row(&status, id);
        assert_eq!(row["state"], state, "the reported row: {row}");
        assert_eq!(row["observation"], observation, "the reported row: {row}");
        assert_eq!(row["run_status"], run_status, "the reported row: {row}");
        assert_eq!(
            row["last_activity_at"], last_activity_at,
            "the reported row: {row}"
        );
        assert_eq!(row["last_error"], last_error, "the reported row: {row}");
        assert_eq!(fixture.logged(id)["outcome"], state, "the logged outcome");
    };
    expect(
        running,
        "running",
        "run running",
        json!("running"),
        json!("2026-10-04T10:07:30.000Z"),
        Value::Null,
    );
    expect(
        queued,
        "running",
        "run queued",
        json!("queued"),
        json!("2026-10-04T10:01:00.000Z"),
        Value::Null,
    );
    expect(
        completed,
        "turn_ended",
        "run completed",
        json!("completed"),
        json!("2026-10-04T11:00:00.000Z"),
        Value::Null,
    );
    expect(
        interrupted,
        "turn_ended",
        "run interrupted",
        json!("interrupted"),
        json!("2026-10-04T09:00:00.000Z"),
        Value::Null,
    );
    expect(
        failed,
        "failed",
        "run failed",
        json!("failed"),
        json!("2026-10-04T08:00:00.000Z"),
        json!(HANDOFF),
    );
    expect(
        session_error,
        "failed",
        "run failed",
        json!("failed"),
        json!("2026-10-04T08:00:00.000Z"),
        json!(RESTART),
    );
    expect(
        never_ran,
        "unknown",
        "no run",
        Value::Null,
        json!("2026-10-04T06:00:00.000Z"),
        Value::Null,
    );
    for id in [missing, deleted] {
        expect(
            id,
            "unknown",
            "absent",
            Value::Null,
            Value::Null,
            Value::Null,
        );
    }

    let background = reported_row(&status, background);
    for key in ["run_status", "last_activity_at", "last_error"] {
        assert!(
            background.get(key).is_none(),
            "a background row grew a T3 field {key}: {background}"
        );
    }
}
