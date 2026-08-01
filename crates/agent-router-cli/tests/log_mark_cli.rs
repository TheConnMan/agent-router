//! Commit 2 through the real binary: `agent-router log --mark <ROW_ID> <good|bad|rerouted>
//! [--note <TEXT>]` against a decision log this fixture seeded itself.
//!
//! The mark is the half of the feedback loop no backend can produce. `status` answers whether the
//! job ran; the mark answers whether routing it there was the right call, so a job that exited zero
//! can still carry a `bad` mark and a job that failed can carry a `good` one.
//!
//! Live data safety: every invocation points HOME at a temp directory, and `default_db_path()` is
//! built on `home_dir()`, so the log under test is this fixture's own file and the real log at
//! `~/.local/state/agent-router/router.db` is never opened. Seeding goes through `open_at` against
//! the same temp path for the same reason.
//!
//! The whole file is unix only because it follows the fixture idiom `status_cli.rs` and
//! `stats_cli.rs` already use.
#![cfg(unix)]

use agent_router_core::config::Config;
use agent_router_core::decide::decide_explicit;
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::provider::Provider;
use agent_router_core::usage::UsageSnapshot;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

/// The task text every seeded row carries. The human listing prints it on its own line, so a
/// `--mark` run that printed the listing rather than short circuiting is visible in stdout.
const SEEDED_TASK: &str = "a seeded routing decision";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-log-mark-cli-{}-{unique}",
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

struct MarkFixture {
    root: TempDir,
}

impl MarkFixture {
    fn new() -> Self {
        let root = TempDir::new();
        fs::create_dir_all(root.path.join("home")).expect("create the fixture home");
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path.join("home")
    }

    fn db_path(&self) -> PathBuf {
        self.home().join(".local/state/agent-router/router.db")
    }

    /// One logged decision, written through the same writer a dispatch uses, so the row under test
    /// is shaped exactly like a real one. Returns its row id.
    fn seed(&self, provider: Provider, dry_run: bool, outcome: &str) -> i64 {
        let decision = decide_explicit(provider, None, UsageSnapshot::full(), &Config::default());
        let log = DecisionLog::open_at(&self.db_path()).expect("open the fixture log");
        log.record(&Entry {
            task: SEEDED_TASK,
            dir: Path::new("/tmp"),
            requested: provider.name(),
            decision: &decision,
            dry_run,
            job_id: Some("c0ffee42"),
            job_name: Some("a routed job"),
            outcome,
        })
        .expect("seed a decision row")
    }

    fn router(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command.env("HOME", self.home());
        command
    }

    /// `agent-router log` with whatever arguments the case needs, run as its own process. Every
    /// assertion below reads the database back through a separate invocation, which is what makes
    /// the reopen real rather than a value held in one process.
    fn log(&self, arguments: &[&str]) -> Output {
        self.router()
            .arg("log")
            .args(arguments)
            .output()
            .expect("run the router")
    }

    fn log_rows(&self) -> Vec<Value> {
        let output = self.log(&["--json", "--limit", "50"]);
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

/// One published field of a logged row. Indexing a `Value` yields null for a key that is not there
/// at all, so a column written and never surfaced in `log --json` would read exactly like a column
/// holding null. Every assertion on a null below goes through this, so the two stay distinct.
fn field<'a>(row: &'a Value, key: &str) -> &'a Value {
    row.get(key)
        .unwrap_or_else(|| panic!("log --json publishes no {key} key at all: {row}"))
}

/// Acceptance criteria 1 and 2. The mark and the note are written by one process and read back by
/// another, so a value that only ever lived in memory fails here, and both appear in the JSON the
/// log publishes rather than only in the confirmation line.
#[test]
fn a_marked_row_survives_a_reopen_and_appears_in_the_log_json() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Codex, false, "dispatched");

    let marked = fixture.log(&[
        "--mark",
        &id.to_string(),
        "bad",
        "--note",
        "routed to codex, needed connectors",
    ]);
    assert!(
        marked.status.success(),
        "marking failed, stderr: {}",
        String::from_utf8_lossy(&marked.stderr)
    );
    let confirmation = String::from_utf8_lossy(&marked.stdout);
    assert!(
        !confirmation.contains(SEEDED_TASK),
        "--mark printed the listing rather than short circuiting on one confirmation line: \
         {confirmation}"
    );

    let logged = fixture.logged(id);
    assert_eq!(
        field(&logged, "mark"),
        "bad",
        "the human judgement did not survive the reopen: {logged}"
    );
    assert_eq!(
        field(&logged, "note"),
        "routed to codex, needed connectors",
        "the note did not survive the reopen: {logged}"
    );
}

/// The trap is a bare `UPDATE ... WHERE id = ?` that changes zero rows and exits 0, which reports
/// a judgement as recorded when nothing was recorded at all. The exit status is the contract, and
/// the untouched neighbour row is the proof that the miss wrote nothing on its way out.
#[test]
fn marking_a_row_id_that_does_not_exist_exits_nonzero() {
    let fixture = MarkFixture::new();
    let present = fixture.seed(Provider::Claude, false, "dispatched");
    let absent = present + 4242;

    let output = fixture.log(&[
        "--mark",
        &absent.to_string(),
        "bad",
        "--note",
        "a judgement",
    ]);

    assert!(
        !output.status.success(),
        "marking a row that does not exist reported success, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&absent.to_string()),
        "the error must name the id that matched nothing: {stderr}"
    );

    let rows = fixture.log_rows();
    assert_eq!(
        rows.len(),
        1,
        "a missing id created a row rather than failing: {rows:?}"
    );
    let untouched = fixture.logged(present);
    assert_eq!(
        field(&untouched, "mark"),
        &Value::Null,
        "a missing id wrote its mark onto another row: {untouched}"
    );
    assert_eq!(
        field(&untouched, "note"),
        &Value::Null,
        "a missing id wrote its note onto another row: {untouched}"
    );
}

/// The accepted set is a closed enum of three and the parse is its only entry point, because the
/// column is TEXT and SQLite enforces nothing. A value outside the set must be refused by name,
/// with the accepted values in the message, rather than reaching the column.
#[test]
fn an_unknown_mark_value_exits_nonzero_naming_the_accepted_values() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Codex, false, "dispatched");

    let output = fixture.log(&["--mark", &id.to_string(), "excellent"]);

    assert!(
        !output.status.success(),
        "an unrecognized mark was accepted, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for accepted in ["good", "bad", "rerouted"] {
        assert!(
            stderr.contains(accepted),
            "the rejection must name {accepted} as an accepted value: {stderr}"
        );
    }

    let logged = fixture.logged(id);
    assert_eq!(
        field(&logged, "mark"),
        &Value::Null,
        "a value outside the enum reached the column: {logged}"
    );
}

/// The note is optional, and its absence is a null rather than an empty string, in the same sense
/// every other nullable column on this row means "nothing was said".
#[test]
fn a_mark_with_no_note_persists_the_mark_and_a_null_note() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Claude, false, "dispatched");

    let output = fixture.log(&["--mark", &id.to_string(), "good"]);
    assert!(
        output.status.success(),
        "marking without a note failed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let logged = fixture.logged(id);
    assert_eq!(field(&logged, "mark"), "good", "the logged row: {logged}");
    assert_eq!(
        field(&logged, "note"),
        &Value::Null,
        "an absent note was stored as something other than null: {logged}"
    );
}

/// A mark is a one time human judgement, so re marking overwrites rather than accumulating. The
/// second half is the one a naive UPDATE of the mark column alone gets wrong: leaving the earlier
/// note attached to a new mark would make the row say something no human ever said.
#[test]
fn re_marking_a_row_overwrites_the_earlier_mark_and_its_note() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Codex, false, "dispatched");

    let first = fixture.log(&[
        "--mark",
        &id.to_string(),
        "bad",
        "--note",
        "the first reading",
    ]);
    assert!(
        first.status.success(),
        "the first mark failed, stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = fixture.log(&["--mark", &id.to_string(), "good"]);
    assert!(
        second.status.success(),
        "re marking failed, stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let rows = fixture.log_rows();
    assert_eq!(
        rows.len(),
        1,
        "re marking added a row rather than overwriting one: {rows:?}"
    );
    let logged = fixture.logged(id);
    assert_eq!(
        field(&logged, "mark"),
        "good",
        "the later judgement did not replace the earlier one: {logged}"
    );
    assert_eq!(
        field(&logged, "note"),
        &Value::Null,
        "a mark with no note left the earlier note attached, which the row did not say: {logged}"
    );
}

/// A note with no mark is rejected rather than silently dropped, mirroring the existing rule that
/// `--model` requires an explicit `--provider`. A caller passing a note believes it recorded one.
#[test]
fn a_note_without_a_mark_is_rejected() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Claude, false, "dispatched");

    let output = fixture.log(&["--note", "a judgement with nothing to attach it to"]);

    assert!(
        !output.status.success(),
        "a note with no mark was accepted, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mark"),
        "the rejection must say which flag the note needed: {stderr}"
    );

    let logged = fixture.logged(id);
    assert_eq!(
        field(&logged, "note"),
        &Value::Null,
        "a note with no mark reached a row anyway: {logged}"
    );
}

/// The resting state of every row in the log. An unmarked row is not a good row, which is the
/// distinction commit 4's bad rate denominator rests on, so both columns read back null rather
/// than as any default judgement.
#[test]
fn a_row_that_was_never_marked_reads_back_with_a_null_mark_and_note() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Codex, false, "dispatched");

    let logged = fixture.logged(id);

    assert_eq!(
        field(&logged, "mark"),
        &Value::Null,
        "an unmarked row carried a judgement nobody made: {logged}"
    );
    assert_eq!(
        field(&logged, "note"),
        &Value::Null,
        "an unmarked row carried a note nobody wrote: {logged}"
    );
}

/// A dry run dispatched nothing, but it still made a routing decision a human can judge, which is
/// why commit 4's bad rate includes dry runs where its failure rate excludes them.
#[test]
fn a_dry_run_row_can_be_marked() {
    let fixture = MarkFixture::new();
    let id = fixture.seed(Provider::Claude, true, "dry-run");

    let output = fixture.log(&[
        "--mark",
        &id.to_string(),
        "rerouted",
        "--note",
        "claude was the wrong call even on paper",
    ]);
    assert!(
        output.status.success(),
        "a dry run could not be judged, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let logged = fixture.logged(id);
    assert_eq!(
        field(&logged, "mark"),
        "rerouted",
        "the logged row: {logged}"
    );
    assert_eq!(
        field(&logged, "outcome"),
        "dry-run",
        "marking a row rewrote what became of the job: {logged}"
    );
}
