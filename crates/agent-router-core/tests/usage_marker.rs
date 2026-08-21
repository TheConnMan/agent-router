//! Feature 4 at the core level: the fail open usage marker on `Headroom`, on the decision log row,
//! across the schema migration that older databases take, and the writability probe that doctor's
//! `log_writable` check reports from.
//!
//! The plan puts these in inline `mod tests` blocks inside `usage.rs` and `log.rs`; they live here
//! instead so the marker is exercised through the public API a caller actually has, and so the
//! test pass touches no production source.

use agent_router_core::config::Config;
use agent_router_core::decide::decide_explicit;
use agent_router_core::doctor::{self, Health};
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::provider::Provider;
use agent_router_core::usage::{
    Headroom, UsageSnapshot, parse_claude_usage, parse_codex_rate_limits,
};
use std::path::Path;

/// The live shape of the Claude usage payload, trimmed to the fields the reader uses. Reproduced
/// here rather than imported, because the reader's own fixture is private to its module.
const CLAUDE_LIVE: &str = r#"{
  "five_hour": {"utilization": 10.0, "resets_at": "2026-07-30T01:40:00.492061+00:00"},
  "seven_day": {"utilization": 50.0, "resets_at": "2026-08-01T13:00:00.492085+00:00"}
}"#;

/// A Claude payload from a provider that has genuinely consumed nothing. Numerically it is the
/// fail open value; it is a live read all the same.
const CLAUDE_IDLE: &str = r#"{
  "five_hour": {"utilization": 0.0},
  "seven_day": {"utilization": 0.0}
}"#;

/// The `now` every codex fixture below is parsed against.
const NOW: i64 = 1_000_000;

/// The live shape of a codex rollout `rate_limits` event: weekly window only, as this box's plan
/// emits, resetting after the `now` it is read at so its number is not expired away.
fn codex_live_line() -> String {
    format!(
        r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":67,"resets_at":{}}},"secondary":null}}}}}}"#,
        NOW + 3_600
    )
}

/// Both providers read live, through the real parsers, which is what makes this snapshot's marker
/// the parsers' own answer rather than a value the test chose.
fn live_snapshot() -> UsageSnapshot {
    UsageSnapshot {
        claude: parse_claude_usage(CLAUDE_LIVE).expect("the live claude payload parses"),
        codex: parse_codex_rate_limits(&codex_live_line(), NOW)
            .expect("the live codex event parses"),
    }
}

fn decision(usage: UsageSnapshot) -> agent_router_core::Decision {
    decide_explicit(Provider::Codex, None, None, None, usage, &Config::default())
}

fn entry<'a>(task: &'a str, decision: &'a agent_router_core::Decision) -> Entry<'a> {
    Entry {
        task,
        dir: Path::new("/tmp"),
        requested: "codex",
        decision,
        dry_run: true,
        job_id: None,
        job_name: None,
        outcome: "dry-run",
        effective_effort: None,
    }
}

/// A fail open read and a genuinely idle provider report the same numbers, so nothing in the
/// numbers can tell them apart. The marker is the only thing that can, which is the whole reason
/// it exists.
#[test]
fn a_live_usage_read_is_not_stale_and_the_fail_open_value_is() {
    let live = live_snapshot();
    assert!(!live.claude.stale, "a parsed claude payload is a live read");
    assert!(!live.codex.stale, "a parsed codex event is a live read");

    let fail_open = Headroom::full();
    assert!(
        fail_open.stale,
        "the fail open value is what a missing credential or an unreachable API reads as"
    );

    let idle = parse_claude_usage(CLAUDE_IDLE).expect("an idle provider still returns a payload");
    assert!(
        !idle.stale,
        "a provider that has consumed nothing was still read live"
    );
    assert_eq!(idle.five_hour_pct, fail_open.five_hour_pct);
    assert_eq!(idle.weekly_pct, fail_open.weekly_pct);
    assert_ne!(
        idle, fail_open,
        "idle and unreadable are identical in every number, so only the marker separates them"
    );
}

/// The decision log is where the distinction survives the dispatch, so a row decided on a read
/// nobody could trust says so rather than leaving it to be inferred from two zeroes.
#[test]
fn a_fail_open_usage_read_is_recorded_as_stale_and_a_live_one_is_not() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&directory.path().join("router.db")).expect("opens");

    let fail_open = decision(UsageSnapshot::full());
    log.record(&entry("decided on a fail open read", &fail_open))
        .expect("records the fail open row");
    let live = decision(live_snapshot());
    log.record(&entry("decided on a live read", &live))
        .expect("records the live row");

    let rows = log.recent(2).expect("reads both rows back");
    assert_eq!(
        rows.iter().map(|row| row.task.as_str()).collect::<Vec<_>>(),
        vec!["decided on a live read", "decided on a fail open read"],
        "newest first"
    );

    assert_eq!(rows[0].claude_usage_stale, Some(false));
    assert_eq!(rows[0].codex_usage_stale, Some(false));
    assert_eq!(rows[1].claude_usage_stale, Some(true));
    assert_eq!(rows[1].codex_usage_stale, Some(true));

    // The percentages the fail open row carries are the ones that make it look like the most
    // rested provider on the box, which is what the marker beside them is answering.
    assert_eq!(rows[1].claude_weekly_pct, 0.0);
    assert_eq!(rows[1].codex_weekly_pct, 0.0);
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

/// The same definition with the two marker columns dropped, which is what a database written
/// before the marker looks like. Whole lines are dropped rather than exact strings replaced, so
/// the fixture does not depend on how the columns happen to be spaced, and a comma left dangling
/// before the closing paren is repaired so the position of the columns does not matter either.
fn without_marker_columns(sql: &str) -> String {
    let mut kept: Vec<String> = sql
        .lines()
        .filter(|line| !line.contains("claude_usage_stale") && !line.contains("codex_usage_stale"))
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

/// A database written before the marker gains both columns on open, keeps its rows, and reads them
/// back as "this row does not know" rather than failing the SELECT or claiming a live read.
#[test]
fn an_older_database_gains_the_stale_columns_and_reads_them_back_as_unknown() {
    let directory = tempfile::tempdir().expect("tempdir");
    let current = directory.path().join("current.db");
    DecisionLog::open_at(&current).expect("creates the current schema");
    let older = without_marker_columns(&current_table_sql(&current));
    assert!(
        !older.contains("claude_usage_stale") && !older.contains("codex_usage_stale"),
        "the older schema fixture still carries the marker columns: {older}"
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
             0, 0, 1, 'dry-run')",
            [],
        )
        .expect("the older row");
    }

    let log = DecisionLog::open_at(&path).expect("migrates on open");
    let rows = log.recent(10).expect("reads the older row back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task, "older row");
    assert_eq!(
        rows[0].claude_usage_stale, None,
        "a row written before the marker does not know, which is not the same as a live read"
    );
    assert_eq!(rows[0].codex_usage_stale, None);

    // The migration is idempotent: opening again must not try to add either column twice.
    DecisionLog::open_at(&path).expect("reopens");
}

/// Several agents write this log at once, so the probe losing a race for the write lock is an
/// ordinary event on this box. Contention is not unwritability: the lock is released and the next
/// dispatch takes it, so reporting it as a failed check would exit doctor 1 over nothing. The test
/// below it holds the other half: a genuinely readonly database is what Fail is reserved for, so
/// neither answer can be given to both cases.
#[test]
fn a_busy_log_is_contention_rather_than_an_unwritable_one() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("state/router.db");
    let log = DecisionLog::open_at(&path).expect("creates the log");

    // A second connection holding the RESERVED lock for longer than the probe's busy timeout,
    // which is what a concurrent `record()` on this box looks like.
    let holder = rusqlite::Connection::open(&path).expect("second connection");
    holder
        .execute_batch("BEGIN IMMEDIATE; CREATE TABLE another_writer_is_here (id INTEGER);")
        .expect("take the write lock");

    let busy = log
        .probe_writable()
        .expect_err("a held write lock must stop the probe from writing");
    assert_eq!(
        doctor::write_probe_health(&busy),
        Health::Warn,
        "a database another agent is writing was reported as unwritable: {busy}"
    );
    holder.execute_batch("ROLLBACK;").expect("release the lock");

    assert!(
        log.probe_writable().is_ok(),
        "the probe fails once the lock is released, so the fixture proved nothing about busy"
    );
}

/// The other half of the severity split: a database the owner cannot write is the case doctor
/// exits nonzero for, and no retry or later dispatch will fix it.
#[cfg(unix)]
#[test]
fn a_read_only_log_is_a_failure_rather_than_contention() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().expect("tempdir");
    if !writes_can_be_denied_by_mode(directory.path()) {
        eprintln!("skipped: this user is not denied writes by the permission bits");
        return;
    }

    let path = directory.path().join("state/router.db");
    DecisionLog::open_at(&path).expect("creates the log");
    let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).expect("make the database read only");

    let log = DecisionLog::open_at(&path).expect("open() succeeds on a read only database");
    let error = log
        .probe_writable()
        .expect_err("a read only database cannot take the probe write");
    assert_eq!(
        doctor::write_probe_health(&error),
        Health::Fail,
        "a log that will never take a row was reported as passing contention: {error}"
    );
}

/// Root ignores the permission bits, so a mode based read only fixture proves nothing there.
#[cfg(unix)]
fn writes_can_be_denied_by_mode(directory: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let probe = directory.join("write probe");
    std::fs::write(&probe, "x").expect("write the probe file");
    let mut permissions = std::fs::metadata(&probe)
        .expect("probe metadata")
        .permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&probe, permissions).expect("make the probe read only");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&probe)
        .is_err()
}

/// `DecisionLog::open()` succeeding is not evidence the log can be written: its schema batch is
/// entirely `CREATE ... IF NOT EXISTS`, so against a database whose objects all already exist it
/// can satisfy every statement without ever taking a write lock. A `log_writable` check built on
/// `open()` alone therefore passes on a database the next dispatch cannot write, which is the gap
/// the probe exists to close and the reason the second assertion below is the point of this test.
#[cfg(unix)]
#[test]
fn a_read_only_database_is_not_reported_as_writable() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().expect("tempdir");
    if !writes_can_be_denied_by_mode(directory.path()) {
        eprintln!("skipped: this user is not denied writes by the permission bits");
        return;
    }

    let path = directory.path().join("state/router.db");
    let written = decision(UsageSnapshot::full());
    {
        let log = DecisionLog::open_at(&path).expect("creates the log");
        log.record(&entry("a row written while it was writable", &written))
            .expect("records");
    }
    let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).expect("make the database read only");

    let log = DecisionLog::open_at(&path).expect("open() succeeds on a read only database");
    assert_eq!(
        log.recent(10).expect("reads").len(),
        1,
        "the read only database still reads, so open() had every reason to succeed"
    );
    assert!(
        log.record(&entry("a row this database cannot take", &written))
            .is_err(),
        "the fixture is not actually read only, so nothing below proves anything"
    );
    assert!(
        log.probe_writable().is_err(),
        "open() returned Ok and record() returned Err on the same database, so a writability \
         check that trusts open() reports a log the next dispatch will fail to write"
    );
}
