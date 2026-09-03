//! What the decision log records once routing is decided on projected weekly draw.
//!
//! The migration is additive because the 108 rows already in the database are the backtest corpus:
//! they have to stay readable and stay queryable by the column names the corpus was captured from,
//! so the columns the classifier no longer scores are kept and left null rather than dropped.
//!
//! These assertions go through SQL rather than the flattened read model on purpose. The contract
//! under test is the shape of the table, which is what a later backtest reads.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::Config;
use agent_router_core::decide::{Decision, decide};
use agent_router_core::log::{DecisionLog, Entry, Mark};
use agent_router_core::stats::{Window, collect};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use std::collections::BTreeMap;
use std::path::Path;

const NOW: i64 = 1_785_400_000;
const HALF_WEEK: i64 = 302_400;

const RETIRED_COLUMNS: [&str; 6] = [
    "verdict",
    "confidence",
    "codex_ready",
    "codex_ready_count",
    "claude_signals",
    "claude_signal_count",
];

fn window(weekly_pct: f64, weekly_remaining_secs: i64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + weekly_remaining_secs,
        weekly_capacity_known: true,
        ..Headroom::full()
    }
}

fn scored(orchestration: bool, task_context_horizon: TaskContextHorizon) -> Classification {
    Classification {
        orchestration,
        missing_connector: false,
        complexity: Complexity::Medium,
        task_context_horizon,
        rationale: "fixture".to_string(),
        classifier_failed: false,
        invokes_implement: false,
        unlaunchable: None,
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
        effective_effort: None,
    })
    .expect("records the decision");
}

/// Both projections travel with the decision that used them. Without them the log says a task was
/// moved but not on what reading, which is the number the next tuning pass needs.
///
/// Claude at 5 percent used with half its window elapsed projects to a 10 percent draw; Codex at
/// 80 percent used over the same fraction projects to 160 percent of an allowance that holds 100,
/// so the task moves.
#[test]
fn a_recorded_decision_writes_the_orchestration_score_and_both_projections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state/router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let usage = UsageSnapshot {
        claude: window(5.0, HALF_WEEK),
        codex: window(80.0, HALF_WEEK),
        grok: window(10.0, HALF_WEEK),
    };

    let flipped = decide(
        scored(false, TaskContextHorizon::Ordinary),
        usage,
        NOW,
        &Config::default(),
    );
    assert_eq!(flipped.provider, Provider::Grok);
    record(&log, &flipped);

    let pinned = decide(
        scored(true, TaskContextHorizon::Ordinary),
        usage,
        NOW,
        &Config::default(),
    );
    record(&log, &pinned);

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let mut statement = conn
        .prepare(
            "SELECT orchestration, claude_projected_draw, codex_projected_draw \
             FROM decisions ORDER BY id",
        )
        .expect("the projection columns exist");
    let rows: Vec<(bool, Option<f64>, Option<f64>)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read the projection columns");

    assert_eq!(rows.len(), 2);
    assert!(!rows[0].0, "the flipped row scored no orchestration");
    assert_eq!(rows[0].1, Some(10.0));
    assert_eq!(rows[0].2, Some(160.0));
    assert!(rows[1].0, "the pinned row scored orchestration");
    drop(statement);

    let (grok_draw, grok_weekly): (Option<f64>, Option<f64>) = conn
        .query_row(
            "SELECT grok_projected_draw, grok_weekly_pct FROM decisions ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("grok columns exist");
    assert_eq!(grok_draw, Some(20.0));
    assert_eq!(grok_weekly, Some(10.0));
}

/// A reset that was never read has no projection, and the column says so rather than carrying a
/// number derived from a zero epoch. A row recording a projection against an unknown window would
/// look exactly like a genuinely measured one to the next backtest.
#[test]
fn an_unread_reset_records_no_projection_for_that_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let decision = decide(
        scored(false, TaskContextHorizon::Ordinary),
        UsageSnapshot {
            claude: Headroom {
                weekly_pct: 10.0,
                weekly_reset_epoch: 0,
                ..Headroom::full()
            },
            codex: window(90.0, HALF_WEEK),
            grok: Headroom::closed(),
        },
        NOW,
        &Config::default(),
    );
    record(&log, &decision);

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let claude_projection: Option<f64> = conn
        .query_row("SELECT claude_projected_draw FROM decisions", [], |row| {
            row.get(0)
        })
        .expect("query");
    assert_eq!(claude_projection, None);
}

/// Schema v2 drops the retired classifier score columns. A fresh database must not declare them.
#[test]
fn a_fresh_database_does_not_declare_the_retired_score_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    DecisionLog::open_at(&path).expect("opens");
    let names = table_columns(&path);
    for column in RETIRED_COLUMNS {
        assert!(
            !names.iter().any(|name| name == column),
            "{column} must not exist on a v2 database: {names:?}"
        );
    }
}

#[test]
fn fresh_auto_rows_persist_and_expose_each_context_horizon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let usage = UsageSnapshot::full();

    for (task, classification) in [
        ("ordinary task", scored(false, TaskContextHorizon::Ordinary)),
        ("extended task", scored(false, TaskContextHorizon::Extended)),
        ("failed task", Classification::fallback("fixture failure")),
    ] {
        let decision = decide(classification, usage, NOW, &Config::default());
        log.record(&Entry {
            task,
            dir: Path::new("/tmp"),
            requested: "auto",
            decision: &decision,
            dry_run: true,
            job_id: None,
            job_name: None,
            outcome: "dry-run",
            effective_effort: None,
        })
        .expect("records");
    }

    let rows = log.recent(10).expect("reads rows");
    assert_eq!(rows[0].task_context_horizon.as_deref(), Some("unknown"));
    assert_eq!(rows[1].task_context_horizon.as_deref(), Some("extended"));
    assert_eq!(rows[2].task_context_horizon.as_deref(), Some("ordinary"));
}

#[test]
fn explicit_provider_rows_keep_context_horizon_null() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let decision = agent_router_core::decide::decide_explicit(
        Provider::Claude,
        None,
        None,
        None,
        UsageSnapshot::full(),
        &Config::default(),
    );
    record(&log, &decision);

    let horizon: Option<String> = rusqlite::Connection::open(&path)
        .expect("reopen")
        .query_row("SELECT task_context_horizon FROM decisions", [], |row| {
            row.get(0)
        })
        .expect("query");
    assert_eq!(horizon, None);
    assert_eq!(log.recent(1).expect("reads")[0].task_context_horizon, None);
}

#[test]
fn reconciliation_and_marking_leave_context_horizon_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let decision = decide(
        scored(false, TaskContextHorizon::Extended),
        UsageSnapshot::full(),
        NOW,
        &Config::default(),
    );
    let id = log
        .record(&Entry {
            task: "sustain synthesis",
            dir: Path::new("/tmp"),
            requested: "auto",
            decision: &decision,
            dry_run: false,
            job_id: Some("thread-1"),
            job_name: None,
            outcome: "dispatched",
            effective_effort: None,
        })
        .expect("records");

    log.reconcile(id, "completed").expect("reconciles");
    log.mark(id, Mark::Good, Some("finished")).expect("marks");

    let horizon: String = rusqlite::Connection::open(&path)
        .expect("reopen")
        .query_row(
            "SELECT task_context_horizon FROM decisions WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(horizon, "extended");
}

/// The shipped schema as of v1, including the six retired classifier score columns. A database in
/// this shape is rewritten to v2 on open: those columns drop, retired gate tags fold, and the
/// orphaned `router_secret` table goes with them.
const SCHEMA_V1: &str = "\
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
CREATE TABLE router_secret (id INTEGER PRIMARY KEY, secret TEXT NOT NULL);
";

fn table_columns(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("reopen");
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('decisions') ORDER BY cid")
        .expect("pragma");
    statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("names")
}

fn insert_v1_row(
    conn: &rusqlite::Connection,
    created_at_ms: i64,
    requested: &str,
    provider: &str,
    gates: &str,
    task: &str,
) {
    conn.execute(
        "INSERT INTO decisions (
            created_at_ms, task, dir, requested, provider, verdict, confidence, complexity,
            codex_ready, codex_ready_count, claude_signals, claude_signal_count,
            missing_connector, gates, rationale, claude_five_hour_pct,
            claude_five_hour_reset, claude_weekly_pct, claude_weekly_reset,
            codex_five_hour_pct, codex_five_hour_reset, codex_weekly_pct,
            codex_weekly_reset, dry_run, outcome
        ) VALUES (
            ?1, ?2, '/tmp', ?3, ?4, 'claude', 'low', 'medium', '111111', 6, '100100', 2,
            0, ?5, 'why', 19.0, 0, 95.0, 1785589200, 0.0, 0, 5.0, 1786184639, 0, 'dispatched'
        )",
        rusqlite::params![created_at_ms, task, requested, provider, gates],
    )
    .expect("a v1 corpus row");
}

/// Seven v1 rows whose pre-migration flip rate is 4 of 6 auto routes: `pace_flip`,
/// `projected_overdraw`, `headroom_tiebreak` (with `flipped_on_exhaustion` on the same row, still
/// one flip), and `five_hour_pacing`. `claude_signals` is a pin, not a flip. After v2 those four
/// moving tags fold to `legacy_flip` and the pin to `legacy_pin`, so flip_rate stays 4/6.
#[test]
fn schema_v2_rewrites_an_old_database_and_preserves_flip_rate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("create the older database");
        conn.execute_batch(SCHEMA_V1).expect("the v1 schema");
        conn.execute("INSERT INTO router_secret (secret) VALUES ('orphaned')", [])
            .expect("the leftover OpenCode table");
        insert_v1_row(&conn, 1_000, "auto", "codex", "", "ungated");
        insert_v1_row(&conn, 2_000, "auto", "claude", "claude_signals", "pinned");
        insert_v1_row(&conn, 3_000, "auto", "codex", "pace_flip", "paced");
        insert_v1_row(
            &conn,
            4_000,
            "auto",
            "codex",
            "projected_overdraw",
            "overdrew",
        );
        insert_v1_row(
            &conn,
            5_000,
            "auto",
            "codex",
            "headroom_tiebreak,flipped_on_exhaustion",
            "double flip",
        );
        insert_v1_row(
            &conn,
            6_000,
            "auto",
            "codex",
            "five_hour_pacing",
            "five hour",
        );
        insert_v1_row(
            &conn,
            7_000,
            "claude",
            "claude",
            "explicit_provider",
            "explicit",
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM decisions", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 7);
    }

    let log = DecisionLog::open_at(&path).expect("migrates on open");
    let rows = log.recent(20).expect("reads the corpus back");
    assert_eq!(rows.len(), 7, "every v1 row must survive the rewrite");

    let names = table_columns(&path);
    for column in RETIRED_COLUMNS {
        assert!(
            !names.iter().any(|name| name == column),
            "{column} must be gone after v2: {names:?}"
        );
    }
    assert!(
        names.iter().any(|name| name == "orchestration"),
        "current columns must exist after the rewrite"
    );

    let mut gates_by_task: BTreeMap<String, String> = BTreeMap::new();
    for row in &rows {
        gates_by_task.insert(row.task.clone(), row.gates.clone());
    }
    assert_eq!(gates_by_task["ungated"], "");
    assert_eq!(gates_by_task["pinned"], "legacy_pin");
    assert_eq!(gates_by_task["paced"], "legacy_flip");
    assert_eq!(gates_by_task["overdrew"], "legacy_flip");
    assert_eq!(
        gates_by_task["double flip"],
        "legacy_flip,flipped_on_exhaustion"
    );
    assert_eq!(gates_by_task["five hour"], "legacy_flip");
    assert_eq!(gates_by_task["explicit"], "explicit_provider");

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    let secret_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'router_secret'",
            [],
            |row| row.get(0),
        )
        .expect("router_secret gone");
    assert_eq!(secret_tables, 0, "the OpenCode leftover table must drop");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 2);

    let stats = collect(
        &log,
        Window {
            limit: 200,
            since_ms: None,
        },
    )
    .expect("stats over the migrated fixture");
    // Pinned from the v1 fixture: 4 of 6 auto rows carried a provider-moving tag. Folding those
    // tags into `legacy_flip` must not change the rate.
    assert_eq!(
        (stats.flip_rate.numerator, stats.flip_rate.denominator),
        (4, 6)
    );
    assert_eq!(
        stats.gates,
        BTreeMap::from([
            ("explicit_provider".to_string(), 1),
            ("flipped_on_exhaustion".to_string(), 1),
            ("legacy_flip".to_string(), 4),
            ("legacy_pin".to_string(), 1),
        ])
    );

    let decision = decide(
        scored(false, TaskContextHorizon::Ordinary),
        UsageSnapshot::full(),
        NOW,
        &Config::default(),
    );
    record(&log, &decision);
    DecisionLog::open_at(&path).expect("reopens idempotently");
}
