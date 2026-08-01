//! What the decision log records once routing is decided on pace.
//!
//! The migration is additive because the 108 rows already in the database are the backtest corpus:
//! they have to stay readable and stay queryable by the column names the corpus was captured from,
//! so the columns the classifier no longer scores are kept and left null rather than dropped.
//!
//! These assertions go through SQL rather than the flattened read model on purpose. The contract
//! under test is the shape of the table, which is what a later backtest reads.

use agent_router_core::classify::{Classification, Complexity};
use agent_router_core::config::Config;
use agent_router_core::decide::{Decision, decide};
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use std::path::Path;

const NOW: i64 = 1_785_400_000;
const HALF_WEEK: i64 = 302_400;

/// The six scores the classifier no longer produces, read straight back out of SQL. Named because
/// the tuple is what the additive migration promises the corpus: every one of these still reads,
/// and every one of them is null on a row written after the change.
type RetiredScores = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

fn window(weekly_pct: f64, weekly_remaining_secs: i64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + weekly_remaining_secs,
        ..Headroom::full()
    }
}

fn scored(orchestration: bool) -> Classification {
    Classification {
        orchestration,
        missing_connector: false,
        complexity: Complexity::Medium,
        rationale: "fixture".to_string(),
        classifier_failed: false,
    }
}

fn record(log: &DecisionLog, decision: &Decision) {
    log.record(&Entry {
        task: "audit the airtable records",
        dir: Path::new("/tmp"),
        requested: "auto",
        decision,
        dry_run: false,
        job_id: Some("thread-abc"),
        job_name: None,
        outcome: "dispatched",
    })
    .expect("records the decision");
}

/// Both pace numbers travel with the decision that used them. Without them the log says a task was
/// flipped but not by how much, which is the number the next tuning pass needs.
///
/// Claude at 5 percent used with half its window elapsed is 45 points under pace; Codex at 80
/// percent used over the same fraction is 30 points over it, a 75 point gap that clears the dead
/// zone and moves the task.
#[test]
fn a_recorded_decision_writes_the_orchestration_score_and_both_pace_deltas() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state/router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let usage = UsageSnapshot {
        claude: window(5.0, HALF_WEEK),
        codex: window(80.0, HALF_WEEK),
    };

    let flipped = decide(scored(false), usage, NOW, &Config::default());
    assert_eq!(flipped.provider, Provider::Claude);
    record(&log, &flipped);

    let pinned = decide(scored(true), usage, NOW, &Config::default());
    record(&log, &pinned);

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let mut statement = conn
        .prepare(
            "SELECT orchestration, claude_pace_delta, codex_pace_delta FROM decisions ORDER BY id",
        )
        .expect("the pace columns exist");
    let rows: Vec<(bool, Option<f64>, Option<f64>)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read the pace columns");

    assert_eq!(rows.len(), 2);
    assert!(!rows[0].0, "the flipped row scored no orchestration");
    assert_eq!(rows[0].1, Some(-45.0));
    assert_eq!(rows[0].2, Some(30.0));
    assert!(rows[1].0, "the pinned row scored orchestration");
}

/// A reset that was never read has no pace delta, and the column says so rather than carrying a
/// number derived from a zero epoch. A row recording -100 against an unknown window would look
/// exactly like a genuinely idle provider to the next backtest.
#[test]
fn an_unread_reset_records_no_pace_delta_for_that_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let decision = decide(
        scored(false),
        UsageSnapshot {
            claude: Headroom {
                weekly_pct: 10.0,
                weekly_reset_epoch: 0,
                ..Headroom::full()
            },
            codex: window(90.0, HALF_WEEK),
        },
        NOW,
        &Config::default(),
    );
    record(&log, &decision);

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let claude_delta: Option<f64> = conn
        .query_row("SELECT claude_pace_delta FROM decisions", [], |row| {
            row.get(0)
        })
        .expect("query");
    assert_eq!(claude_delta, None);
}

/// The columns the classifier no longer scores stay in the table and stop being written. Dropping
/// them would take the historical rows' scores with them, and writing a placeholder into them would
/// make a new row indistinguishable from an old one that really did score zero.
#[test]
fn the_scores_the_classifier_no_longer_produces_are_left_null() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let decision = decide(
        scored(false),
        UsageSnapshot {
            claude: window(5.0, HALF_WEEK),
            codex: window(80.0, HALF_WEEK),
        },
        NOW,
        &Config::default(),
    );
    record(&log, &decision);

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let row: RetiredScores = conn
        .query_row(
            "SELECT verdict, confidence, codex_ready, claude_signals, codex_ready_count, \
             claude_signal_count FROM decisions",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("the retired columns are still selectable");
    assert_eq!(
        row,
        (None, None, None, None, None, None),
        "a new row must not write a score the classifier no longer produces"
    );
}

/// The shipped schema, exactly as the recorded corpus was written under it. A database in this
/// shape gains the new columns on open, keeps every row, and keeps every historical score.
const SCHEMA_BEFORE_PACE: &str = "\
CREATE TABLE IF NOT EXISTS decisions (
    id                  INTEGER PRIMARY KEY,
    created_at_ms       INTEGER NOT NULL,
    task                TEXT    NOT NULL,
    dir                 TEXT    NOT NULL,
    requested           TEXT    NOT NULL,
    provider            TEXT    NOT NULL,
    model               TEXT,
    effort              TEXT,
    verdict             TEXT,
    confidence          TEXT,
    complexity          TEXT,
    codex_ready         TEXT,
    codex_ready_count   INTEGER,
    claude_signals      TEXT,
    claude_signal_count INTEGER,
    missing_connector   INTEGER,
    gates               TEXT    NOT NULL,
    rationale           TEXT    NOT NULL,
    claude_five_hour_pct   REAL NOT NULL,
    claude_five_hour_reset INTEGER NOT NULL,
    claude_weekly_pct      REAL NOT NULL,
    claude_weekly_reset    INTEGER NOT NULL,
    codex_five_hour_pct    REAL NOT NULL,
    codex_five_hour_reset  INTEGER NOT NULL,
    codex_weekly_pct       REAL NOT NULL,
    codex_weekly_reset     INTEGER NOT NULL,
    claude_usage_stale     INTEGER,
    codex_usage_stale      INTEGER,
    dry_run             INTEGER NOT NULL,
    job_id              TEXT,
    job_name            TEXT,
    outcome             TEXT    NOT NULL
);
";

#[test]
fn a_database_written_before_pace_gains_the_columns_and_keeps_its_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("create the older database");
        conn.execute_batch(SCHEMA_BEFORE_PACE)
            .expect("the older schema");
        conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, verdict, confidence, complexity,
                codex_ready, codex_ready_count, claude_signals, claude_signal_count,
                missing_connector, gates, rationale, claude_five_hour_pct,
                claude_five_hour_reset, claude_weekly_pct, claude_weekly_reset,
                codex_five_hour_pct, codex_five_hour_reset, codex_weekly_pct,
                codex_weekly_reset, dry_run, outcome
            ) VALUES (
                1785581513867, 'a recorded task', '/tmp', 'auto', 'claude', 'claude', 'low',
                'medium', '111111', 6, '100100', 2, 0, 'claude_signals', 'why', 19.0, 0, 95.0,
                1785589200, 0.0, 0, 5.0, 1786184639, 0, 'dispatched'
            )",
            [],
        )
        .expect("a corpus row");
    }

    let log = DecisionLog::open_at(&path).expect("migrates on open");
    let rows = log.recent(10).expect("reads the corpus row back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task, "a recorded task");
    assert_eq!(rows[0].provider, "claude");
    assert_eq!(rows[0].gates, "claude_signals");

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let (signals, weekly, orchestration, claude_pace, codex_pace): (
        Option<String>,
        f64,
        Option<bool>,
        Option<f64>,
        Option<f64>,
    ) = conn
        .query_row(
            "SELECT claude_signals, claude_weekly_pct, orchestration, claude_pace_delta, \
             codex_pace_delta FROM decisions",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("the migrated table carries both generations of column");
    assert_eq!(signals.as_deref(), Some("100100"));
    assert_eq!(weekly, 95.0);
    assert_eq!(orchestration, None, "an older row scored no orchestration");
    assert_eq!(claude_pace, None);
    assert_eq!(codex_pace, None);

    // The migration is idempotent: opening again must not try to add a column twice.
    DecisionLog::open_at(&path).expect("reopens");
}
