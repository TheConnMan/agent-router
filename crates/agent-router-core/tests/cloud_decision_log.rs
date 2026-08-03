//! What the decision log records about where a job ran.
//!
//! Two columns, and they are not the same fact. `execution_target` is a property of the ROUTING
//! DECISION, so every row this build writes carries one, cloud or local, dry run or not.
//! `cloud_task_url` is a property of what dispatch OBSERVED, so it is null everywhere no cloud task
//! was submitted. Filling the second in from the first would record an inference as a reading,
//! which is the rule `30c0a7a` established for `effective_effort` and which this column inherits.
//!
//! A None in `execution_target` means "this row does not know", exactly as `router_version`'s does.
//! It is not the same as `local`, and any later aggregate has to count the two separately.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::cloud::{CloudEligibility, CloudIneligible, CloudTarget};
use agent_router_core::config::Config;
use agent_router_core::decide::{Decision, decide};
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::stats::Window;
use agent_router_core::status::{Observation, reconcile};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use std::path::Path;

const NOW: i64 = 1_785_400_000;
const HALF_WEEK: i64 = 302_400;
const TASK_ID: &str = "task-123";
const TASK_URL: &str = "https://chatgpt.com/codex/tasks/task-123";

fn window(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + HALF_WEEK,
        ..Headroom::full()
    }
}

/// Both providers burning at the same rate, so no usage rule moves the task and the decision under
/// test is about the target rather than about routing.
fn usage() -> UsageSnapshot {
    UsageSnapshot {
        claude: window(50.0),
        codex: window(50.0),
    }
}

fn classification() -> Classification {
    Classification {
        orchestration: false,
        missing_connector: false,
        complexity: Complexity::High,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
    }
}

fn cloud_decision() -> Decision {
    decide(
        classification(),
        usage(),
        NOW,
        CloudEligibility::Eligible(CloudTarget::resolved(
            "env-abc".to_string(),
            "task/x".to_string(),
        )),
        &Config::default(),
    )
}

fn local_decision() -> Decision {
    decide(
        classification(),
        usage(),
        NOW,
        CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted),
        &Config::default(),
    )
}

/// AC 1d. A cloud row records where it ran and how the task is found again: the cloud task id in
/// `job_id`, like every other backend's job identity, and the url in its own column because the
/// base path belongs to a third party and is not ours to derive.
#[test]
fn a_cloud_row_records_its_target_and_task_identity() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");
    let decision = cloud_decision();
    assert_eq!(
        decision.provider,
        Provider::Codex,
        "a cloud job is still Codex work drawing on the Codex weekly window"
    );

    log.record(&Entry {
        task: "submit this to the cloud",
        dir: Path::new("/tmp"),
        requested: "auto",
        decision: &decision,
        dry_run: false,
        job_id: Some(TASK_ID),
        job_name: Some("a routed job"),
        outcome: "dispatched",
        effective_effort: None,
        cloud_task_url: Some(TASK_URL),
    })
    .expect("records the cloud row");

    let row = &log.recent(1).expect("reads the row back")[0];
    assert_eq!(row.execution_target.as_deref(), Some("cloud"));
    assert_eq!(
        row.job_id.as_deref(),
        Some(TASK_ID),
        "the cloud task id IS the job identity, so it belongs in the primary column"
    );
    assert_eq!(row.cloud_task_url.as_deref(), Some(TASK_URL));
    assert_eq!(
        row.provider, "codex",
        "the provider column is what the estimate and the reconciler key off"
    );
}

/// AC 1d, the negative half. A local row says so rather than saying nothing, and carries no task
/// url because there is no such thing rather than one that went unread. Without this case a null
/// `execution_target` on every row would satisfy the positive test's absence of an assertion.
#[test]
fn a_local_row_records_local_and_no_task_url() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");
    let decision = local_decision();

    log.record(&Entry {
        task: "run this here",
        dir: Path::new("/tmp"),
        requested: "auto",
        decision: &decision,
        dry_run: false,
        job_id: Some("thread-abc"),
        job_name: None,
        outcome: "dispatched",
        effective_effort: None,
        cloud_task_url: None,
    })
    .expect("records the local row");

    let row = &log.recent(1).expect("reads the row back")[0];
    assert_eq!(
        row.execution_target.as_deref(),
        Some("local"),
        "every row this build writes names its target, so a null is a pre-column row and never a \
         local one"
    );
    assert_eq!(row.cloud_task_url, None);
}

/// The current `decisions` definition, read back out of a database the log itself created, so the
/// older fixture below is derived from the shipped schema rather than duplicated from it. Copied in
/// shape from `effective_effort.rs`, which builds its migration fixture the same way; the constant
/// the inline tests use is private to `log.rs` and unreachable from an integration test crate.
fn current_table_sql(path: &Path) -> String {
    let conn = rusqlite::Connection::open(path).expect("open the database to read its schema");
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'decisions'",
        [],
        |row| row.get(0),
    )
    .expect("the decisions table exists")
}

/// The same definition with the named columns dropped. Whole lines are dropped rather than exact
/// strings replaced, so the fixture does not depend on how a column happens to be spaced, and a
/// comma left dangling before the closing paren is repaired so column position does not matter.
fn without_columns(sql: &str, columns: &[&str]) -> String {
    let mut kept: Vec<String> = sql
        .lines()
        .filter(|line| !columns.iter().any(|column| line.contains(column)))
        .map(str::to_string)
        .collect();
    if let Some(index) = kept
        .iter()
        .rposition(|line| !line.trim().is_empty() && !line.trim().starts_with(')'))
        && let Some(without_comma) = kept[index].trim_end().strip_suffix(',')
    {
        kept[index] = without_comma.to_string();
    }
    kept.join("\n")
}

/// AC 1d, the migration half. A database written before these columns gains them on open, keeps its
/// rows, and reads the old ones back as "this row does not know".
///
/// This is the test that catches `SCHEMA` and `MISSING_COLUMNS` drifting apart. `ALTER TABLE ADD
/// COLUMN` can only append, so the migrated table's physical column order differs from a fresh
/// one's, and a read that took an ordinal rather than naming its columns would return one column's
/// value under another column's name on exactly one of the two shapes.
#[test]
fn a_database_created_before_the_columns_migrates_in_place() {
    let directory = tempfile::tempdir().expect("tempdir");
    let current = directory.path().join("current.db");
    DecisionLog::open_at(&current).expect("creates the current schema");
    let current_sql = current_table_sql(&current);
    for column in ["execution_target", "cloud_task_url"] {
        assert!(
            current_sql.contains(column),
            "the shipped schema does not declare {column}, so this migration is about nothing: \
             {current_sql}"
        );
    }
    let older = without_columns(&current_sql, &["execution_target", "cloud_task_url"]);
    assert!(
        !older.contains("execution_target") && !older.contains("cloud_task_url"),
        "the older schema fixture still carries a column it is meant to predate: {older}"
    );

    let path = directory.path().join("older.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("create the older database");
        conn.execute_batch(&older).expect("the older schema");
        conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, gates, rationale,
                claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                codex_weekly_pct, codex_weekly_reset, dry_run, job_id, outcome
            ) VALUES (1, 'older row', '/tmp', 'auto', 'codex', '', 'why', 0, 0, 0, 0, 0, 0, \
             0, 0, 0, 'thread-old', 'dispatched')",
            [],
        )
        .expect("the older row");
    }

    let log = DecisionLog::open_at(&path).expect("migrates on open");
    let decision = cloud_decision();
    log.record(&Entry {
        task: "a cloud row on a migrated database",
        dir: Path::new("/tmp"),
        requested: "auto",
        decision: &decision,
        dry_run: false,
        job_id: Some(TASK_ID),
        job_name: None,
        outcome: "dispatched",
        effective_effort: None,
        cloud_task_url: Some(TASK_URL),
    })
    .expect("records a cloud row into the migrated table");

    let rows = log.recent(10).expect("reads both rows back");
    assert_eq!(
        rows.iter().map(|row| row.task.as_str()).collect::<Vec<_>>(),
        vec!["a cloud row on a migrated database", "older row"],
        "newest first"
    );
    assert_eq!(rows[0].execution_target.as_deref(), Some("cloud"));
    assert_eq!(rows[0].cloud_task_url.as_deref(), Some(TASK_URL));
    assert_eq!(
        rows[1].execution_target, None,
        "a row written before the column does not know where it ran, which is not the same as \
         having run locally"
    );
    assert_eq!(rows[1].cloud_task_url, None);

    // The migration is idempotent: opening again must not try to add a column twice.
    DecisionLog::open_at(&path).expect("reopens");
}

/// The silent downgrade this guard exists to prevent.
///
/// A cloud row carries `provider = "codex"` and a cloud task id in `job_id`, which is not an
/// app-server thread id. Without the guard the reconciler selects the row, asks the local daemon
/// about an id it has never heard of, reads `unknown`, and `settle` writes that over `dispatched`.
/// That happens on every `agent-router status` run, permanently, and it trades a true fact about
/// the row for a less informative one on the strength of a question asked of the wrong system.
///
/// `Unsupported` is the honest reading, and `reconcile` already leaves an `Unsupported` row
/// entirely alone. Polling the cloud for a real terminal state is deliberately not built here, so
/// the row correctly stays `dispatched` forever until it is.
#[test]
fn a_cloud_row_is_left_alone_by_reconciliation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let decision = cloud_decision();
    let id = log
        .record(&Entry {
            task: "a dispatched cloud task",
            dir: Path::new("/tmp"),
            requested: "auto",
            decision: &decision,
            dry_run: false,
            job_id: Some(TASK_ID),
            job_name: None,
            outcome: "dispatched",
            effective_effort: None,
            cloud_task_url: Some(TASK_URL),
        })
        .expect("records the cloud row");

    let report = reconcile(
        &log,
        Window {
            limit: 10,
            since_ms: None,
        },
    )
    .expect("reconciles the window");

    assert_eq!(report.rows.len(), 1, "the row must be in the window at all");
    let reported = &report.rows[0];
    assert_eq!(reported.id, id);
    assert_eq!(
        reported.observation,
        Observation::Unsupported,
        "the router has no way to ask about a cloud task id, and must say so rather than asking \
         the local daemon"
    );
    assert_eq!(
        reported.persisted, "dispatched",
        "an unsupported observation writes nothing, so the row keeps what dispatch proved"
    );

    let row = &log.recent(1).expect("reads the row back")[0];
    assert_eq!(
        row.outcome, "dispatched",
        "the stored outcome was downgraded by a reading that never happened"
    );
    assert_eq!(
        row.reconciled_at_ms, None,
        "nothing landed, so nothing may claim a reading did"
    );
}
