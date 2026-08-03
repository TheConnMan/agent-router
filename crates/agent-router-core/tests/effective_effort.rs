//! Commit 3 at the core level: the `effective_effort` column, what NULL in it means, and the
//! migration an older database takes to gain it.
//!
//! The plan puts the migration test in an inline `mod tests` block inside `log.rs`; it lives here
//! instead so the column is exercised through the public API a caller actually has, and so the
//! test pass touches no production source, exactly as `usage_marker.rs` already does.

use agent_router_core::config::Config;
use agent_router_core::decide::decide_explicit;
use agent_router_core::error::Error;
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::provider::Provider;
use agent_router_core::run::{Dispatch, recorded_fields};
use agent_router_core::usage::UsageSnapshot;
use std::path::Path;

fn decision(provider: Provider) -> agent_router_core::Decision {
    decide_explicit(provider, None, UsageSnapshot::full(), &Config::default())
}

fn entry<'a>(
    task: &'a str,
    requested: &'a str,
    decision: &'a agent_router_core::Decision,
    effective_effort: Option<&'a str>,
) -> Entry<'a> {
    Entry {
        task,
        dir: Path::new("/tmp"),
        requested,
        decision,
        dry_run: false,
        job_id: Some("job identity"),
        job_name: Some("Bonus: abc 123"),
        outcome: "dispatched",
        effective_effort,
        // Every row this file writes is a LOCAL dispatch, so there is no cloud task to link to
        // rather than one whose url went unread.
        cloud_task_url: None,
    }
}

/// The whole point of the column, in one database. A null claude row read on its own is also what
/// an unimplemented feature looks like, so the codex row beside it is the control: the same log,
/// the same write path, one row carrying the value the backend reported and two carrying nothing
/// because their backends report nothing.
#[test]
fn a_row_written_with_an_effort_reads_it_back_and_rows_written_without_one_stay_null() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");

    let codex = decision(Provider::Codex);
    let claude = decision(Provider::Claude);
    let opencode = decision(Provider::Opencode);
    log.record(&entry("routed to codex", "codex", &codex, Some("high")))
        .expect("records the codex row");
    log.record(&entry("routed to claude", "claude", &claude, None))
        .expect("records the claude row");
    log.record(&entry("routed to opencode", "opencode", &opencode, None))
        .expect("records the opencode row");

    let rows = log.recent(3).expect("reads all three rows back");
    assert_eq!(
        rows.iter().map(|row| row.task.as_str()).collect::<Vec<_>>(),
        vec!["routed to opencode", "routed to claude", "routed to codex"],
        "newest first"
    );

    assert_eq!(
        rows[2].effective_effort.as_deref(),
        Some("high"),
        "the codex daemon reported an effort and the row has to carry it"
    );
    assert_eq!(
        rows[1].effective_effort, None,
        "claude's CLI exposes no effective effort, so the row must not invent one"
    );
    assert_eq!(
        rows[0].effective_effort, None,
        "opencode discards effort, so the row must not invent one either"
    );
}

/// The decided effort and the observed effort are two different facts and two different columns.
/// A reader that collapsed them, or that filled one in from the other, would report the router's
/// intention as the backend's behaviour, which is the defect the second column exists to remove.
#[test]
fn a_decided_effort_and_an_observed_effort_are_two_independent_columns() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");

    let mut decided = decision(Provider::Codex);
    decided.effort = Some("low".to_string());
    log.record(&entry(
        "decided low and ran high",
        "codex",
        &decided,
        Some("high"),
    ))
    .expect("records the row");

    let rows = log.recent(1).expect("reads the row back");
    assert_eq!(
        rows[0].effort.as_deref(),
        Some("low"),
        "the effort the router asked for"
    );
    assert_eq!(
        rows[0].effective_effort.as_deref(),
        Some("high"),
        "the effort the backend reported it will run at"
    );
}

/// The seam between the dispatch and the row: the parsing of the backend's reply is covered at the
/// RPC level and the column is covered above, but neither notices if the value stops being carried
/// across. A dispatch reporting an effort has to reach the log still carrying it.
#[test]
fn a_dispatch_reporting_an_effort_carries_it_into_the_row_it_is_logged_as() {
    let dispatched: agent_router_core::Result<Dispatch> = Ok(Dispatch {
        job_id: Some("thread abc123".to_string()),
        job_name: "Bonus: abc 123".to_string(),
        effective_effort: Some("high".to_string()),
        cloud_task_url: None,
    });

    let recorded = recorded_fields(&dispatched);
    assert_eq!(recorded.job_id.as_deref(), Some("thread abc123"));
    assert_eq!(recorded.job_name.as_deref(), Some("Bonus: abc 123"));
    assert_eq!(
        recorded.effective_effort.as_deref(),
        Some("high"),
        "the backend reported an effort and the fields the row is written from have to carry it"
    );
    assert_eq!(recorded.outcome, "dispatched");

    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");
    let codex = decision(Provider::Codex);
    log.record(&entry(
        "dispatched to codex",
        "codex",
        &codex,
        recorded.effective_effort.as_deref(),
    ))
    .expect("records the row");

    let rows = log.recent(1).expect("reads the row back");
    assert_eq!(
        rows[0].effective_effort.as_deref(),
        Some("high"),
        "the effort the dispatch reported has to survive all the way into the column"
    );
}

/// A dispatch that failed produced no job and observed no effort, so its row carries no identity
/// and no effort, and the outcome carries the backend's own message rather than a generic one.
#[test]
fn a_failed_dispatch_records_no_identity_no_effort_and_the_backend_message() {
    let dispatched: agent_router_core::Result<Dispatch> =
        Err(Error::Command("app-server refused the thread".to_string()));

    let recorded = recorded_fields(&dispatched);
    assert_eq!(recorded.job_id, None);
    assert_eq!(recorded.job_name, None);
    assert_eq!(
        recorded.effective_effort, None,
        "nothing dispatched, so no backend reported an effort"
    );
    assert_eq!(recorded.outcome, "error: app-server refused the thread");
}

/// The same seam, for the second observed value it now carries. `effective_effort` was the field
/// this function's doc comment was written about, and `cloud_task_url` has exactly the same failure
/// mode: it is an observation only the backend can supply, it is the only record of where a
/// submitted cloud task lives, and a row that lost it names a task nobody can find again.
///
/// This is why the function returns a named struct rather than a tuple. A fifth member would have
/// put four consecutive `Option<String>` values in a positional destructure, in the one function
/// whose whole job is not losing a field to its neighbour, and every assertion below would still
/// pass with the effort and the url transposed.
#[test]
fn a_dispatch_reporting_a_cloud_task_url_carries_it_into_the_row_it_is_logged_as() {
    const TASK_URL: &str = "https://chatgpt.com/codex/tasks/task-123";
    let dispatched: agent_router_core::Result<Dispatch> = Ok(Dispatch {
        job_id: Some("task-123".to_string()),
        job_name: "Bonus: abc 123".to_string(),
        // `codex cloud exec` takes no --effort and reports none back, exactly like claude and
        // opencode, so this dispatch observes a url and no effort.
        effective_effort: None,
        cloud_task_url: Some(TASK_URL.to_string()),
    });

    let recorded = recorded_fields(&dispatched);
    assert_eq!(
        recorded.cloud_task_url.as_deref(),
        Some(TASK_URL),
        "the submit reported a task url and the fields the row is written from have to carry it"
    );
    assert_eq!(
        recorded.job_id.as_deref(),
        Some("task-123"),
        "the cloud task id is the job identity, in the same field as every other backend's"
    );
    assert_eq!(
        recorded.effective_effort, None,
        "the url must not have been read out of the neighbouring field"
    );
    assert_eq!(recorded.outcome, "dispatched");

    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");
    let codex = decision(Provider::Codex);
    log.record(&Entry {
        task: "submitted to the cloud",
        dir: Path::new("/tmp"),
        requested: "auto",
        decision: &codex,
        dry_run: false,
        job_id: recorded.job_id.as_deref(),
        job_name: recorded.job_name.as_deref(),
        outcome: &recorded.outcome,
        effective_effort: recorded.effective_effort.as_deref(),
        cloud_task_url: recorded.cloud_task_url.as_deref(),
    })
    .expect("records the row");

    let rows = log.recent(1).expect("reads the row back");
    assert_eq!(
        rows[0].cloud_task_url.as_deref(),
        Some(TASK_URL),
        "the url the dispatch reported has to survive all the way into the column"
    );
    assert_eq!(rows[0].job_id.as_deref(), Some("task-123"));
}

/// A dispatch that failed submitted nothing, so there is no task to link to. A url on a failed row
/// would be a link to a cloud task that was never created, which is worse than no link: it invites
/// an operator to go looking for work that does not exist.
#[test]
fn a_failed_dispatch_records_no_cloud_task_url() {
    let dispatched: agent_router_core::Result<Dispatch> = Err(Error::Command(
        "codex cloud exec refused the submit".to_string(),
    ));

    let recorded = recorded_fields(&dispatched);
    assert_eq!(
        recorded.cloud_task_url, None,
        "nothing was submitted, so there is no task url to record"
    );
    assert_eq!(recorded.job_id, None);
    assert_eq!(
        recorded.outcome,
        "error: codex cloud exec refused the submit"
    );
}

/// The current `decisions` table definition, read back out of a database the log itself created,
/// so the older fixture below is derived from the shipped schema rather than duplicated from it.
fn current_table_sql(path: &Path) -> String {
    let conn = rusqlite::Connection::open(path).expect("open the database to read its schema");
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'decisions'",
        [],
        |row| row.get(0),
    )
    .expect("the decisions table exists")
}

/// The same definition with the effective effort column dropped, which is what a database written
/// before this commit looks like. A whole line is dropped rather than an exact string replaced, so
/// the fixture does not depend on how the column happens to be spaced, and a comma left dangling
/// before the closing paren is repaired so the position of the column does not matter either.
fn without_effective_effort_column(sql: &str) -> String {
    let mut kept: Vec<String> = sql
        .lines()
        .filter(|line| !line.contains("effective_effort"))
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

/// A database written before this commit gains the column on open, keeps its rows, and reads them
/// back as "this row does not know" rather than failing the SELECT or claiming an effort nobody
/// observed.
#[test]
fn an_older_database_gains_the_effective_effort_column_and_keeps_its_rows() {
    let directory = tempfile::tempdir().expect("tempdir");
    let current = directory.path().join("current.db");
    DecisionLog::open_at(&current).expect("creates the current schema");
    let current_sql = current_table_sql(&current);
    assert!(
        current_sql.contains("effective_effort"),
        "the shipped schema does not declare the column this migration is about: {current_sql}"
    );
    let older = without_effective_effort_column(&current_sql);
    assert!(
        !older.contains("effective_effort"),
        "the older schema fixture still carries the column: {older}"
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
                codex_weekly_pct, codex_weekly_reset, dry_run, outcome
            ) VALUES (1, 'older row', '/tmp', 'auto', 'codex', '', 'why', 0, 0, 0, 0, 0, 0, \
             0, 0, 0, 'dispatched')",
            [],
        )
        .expect("the older row");
    }

    let log = DecisionLog::open_at(&path).expect("migrates on open");
    let rows = log.recent(10).expect("reads the older row back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task, "older row");
    assert_eq!(
        rows[0].effective_effort, None,
        "a row written before the column does not know what it ran at, which is not the same as \
         having run at no effort"
    );

    // The migration is idempotent: opening again must not try to add the column twice.
    DecisionLog::open_at(&path).expect("reopens");
}
