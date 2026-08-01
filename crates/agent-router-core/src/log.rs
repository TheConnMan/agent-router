//! The decision log: one SQLite row per routing decision at
//! `~/.local/state/agent-router/router.db`. This is the tuning data for the heuristic and the
//! answer to "why did this route here".

use crate::decide::Decision;
use crate::error::Result;
use crate::runtime::{home_dir, now_ms};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// One row to write: the decision plus what the caller asked for and what dispatch did.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub task: &'a str,
    pub dir: &'a Path,
    /// "auto" or the provider the caller named.
    pub requested: &'a str,
    pub decision: &'a Decision,
    pub dry_run: bool,
    /// The backend's own identity for the new job (codex thread id, claude short id), when one
    /// was resolved.
    pub job_id: Option<&'a str>,
    /// The job name a claude dispatch is findable by when no short id came back.
    pub job_name: Option<&'a str>,
    /// "dispatched", "dry-run", or "error: ...".
    pub outcome: &'a str,
}

/// One row read back, flattened for display.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: i64,
    pub created_at_ms: i64,
    pub task: String,
    pub dir: String,
    pub requested: String,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub verdict: Option<String>,
    pub confidence: Option<String>,
    /// None for a row written before complexity scaling, and for an explicit provider.
    pub complexity: Option<String>,
    pub codex_ready_count: Option<i64>,
    pub claude_signal_count: Option<i64>,
    pub missing_connector: Option<bool>,
    pub gates: String,
    pub claude_weekly_pct: f64,
    pub codex_weekly_pct: f64,
    pub dry_run: bool,
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    pub outcome: String,
    pub rationale: String,
}

/// One row as the stats reader needs it: the columns a metric is derived from, nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsRow {
    pub created_at_ms: i64,
    /// "auto" or the provider the caller named.
    pub requested: String,
    pub provider: String,
    /// None for a row written before complexity scaling, and for an explicit provider.
    pub complexity: Option<String>,
    /// The gate tags that fired, comma joined. Empty when none did.
    pub gates: String,
    pub dry_run: bool,
}

const SCHEMA: &str = "\
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
    dry_run             INTEGER NOT NULL,
    job_id              TEXT,
    job_name            TEXT,
    outcome             TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS decisions_created_at ON decisions(created_at_ms);
";

const SELECT_COLUMNS: &str = "\
id, created_at_ms, task, dir, requested, provider, model, effort, verdict, confidence, \
codex_ready_count, claude_signal_count, missing_connector, gates, claude_weekly_pct, \
codex_weekly_pct, dry_run, job_id, job_name, outcome, rationale, complexity";

/// The narrower list the stats reader needs, so a report never pays for columns it drops.
const STATS_COLUMNS: &str = "created_at_ms, requested, provider, complexity, gates, dry_run";

pub struct DecisionLog {
    conn: Connection,
}

impl DecisionLog {
    /// IMPURE: the log at the default path, creating the state directory (0700, it records task
    /// text) and the schema when absent.
    pub fn open() -> Result<DecisionLog> {
        DecisionLog::open_at(&default_db_path())
    }

    pub fn open_at(path: &Path) -> Result<DecisionLog> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict_to_owner(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        conn.execute_batch(SCHEMA)?;
        add_missing_columns(&conn)?;
        Ok(DecisionLog { conn })
    }

    /// Append one decision. Returns its row id.
    pub fn record(&self, entry: &Entry) -> Result<i64> {
        let decision = entry.decision;
        let classification = decision.classification.as_ref();
        let usage = &decision.usage;
        self.conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, model, effort, verdict,
                confidence, codex_ready, codex_ready_count, claude_signals,
                claude_signal_count, missing_connector, gates, rationale,
                claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                codex_weekly_pct, codex_weekly_reset, dry_run, job_id, job_name, outcome,
                complexity
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29
            )",
            rusqlite::params![
                now_ms(),
                entry.task,
                entry.dir.to_string_lossy(),
                entry.requested,
                decision.provider.name(),
                decision.model,
                decision.effort,
                classification.map(|c| format!("{:?}", c.verdict).to_lowercase()),
                classification.map(|c| format!("{:?}", c.confidence).to_lowercase()),
                classification.map(|c| bits(&c.codex_ready)),
                classification.map(|c| c.codex_ready_count() as i64),
                classification.map(|c| bits(&c.claude_signals)),
                classification.map(|c| c.claude_signal_count() as i64),
                classification.map(|c| c.missing_connector),
                decision.gate_tags().join(","),
                decision.rationale,
                usage.claude.five_hour_pct,
                usage.claude.five_hour_reset_epoch,
                usage.claude.weekly_pct,
                usage.claude.weekly_reset_epoch,
                usage.codex.five_hour_pct,
                usage.codex.five_hour_reset_epoch,
                usage.codex.weekly_pct,
                usage.codex.weekly_reset_epoch,
                entry.dry_run,
                entry.job_id,
                entry.job_name,
                entry.outcome,
                classification.map(|c| c.complexity.tag()),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// IMPURE: the `limit` newest rows the stats reader needs, newest first. `since_ms` is an
    /// inclusive floor on `created_at_ms`: it filters first, then the limit caps what is left.
    pub fn stats_rows(&self, limit: usize, since_ms: Option<i64>) -> Result<Vec<StatsRow>> {
        let read = |row: &rusqlite::Row| {
            Ok(StatsRow {
                created_at_ms: row.get(0)?,
                requested: row.get(1)?,
                provider: row.get(2)?,
                complexity: row.get(3)?,
                gates: row.get(4)?,
                dry_run: row.get(5)?,
            })
        };
        let rows = match since_ms {
            Some(floor) => {
                let sql = format!(
                    "SELECT {STATS_COLUMNS} FROM decisions WHERE created_at_ms >= ?1 \
                     ORDER BY id DESC LIMIT ?2"
                );
                let mut statement = self.conn.prepare(&sql)?;
                let rows = statement.query_map([floor, limit as i64], read)?;
                rows.collect::<rusqlite::Result<Vec<StatsRow>>>()?
            }
            None => {
                let sql =
                    format!("SELECT {STATS_COLUMNS} FROM decisions ORDER BY id DESC LIMIT ?1");
                let mut statement = self.conn.prepare(&sql)?;
                let rows = statement.query_map([limit as i64], read)?;
                rows.collect::<rusqlite::Result<Vec<StatsRow>>>()?
            }
        };
        Ok(rows)
    }

    /// The `limit` newest decisions, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Row>> {
        let sql = format!("SELECT {SELECT_COLUMNS} FROM decisions ORDER BY id DESC LIMIT ?1");
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map([limit as i64], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    created_at_ms: row.get(1)?,
                    task: row.get(2)?,
                    dir: row.get(3)?,
                    requested: row.get(4)?,
                    provider: row.get(5)?,
                    model: row.get(6)?,
                    effort: row.get(7)?,
                    verdict: row.get(8)?,
                    confidence: row.get(9)?,
                    codex_ready_count: row.get(10)?,
                    claude_signal_count: row.get(11)?,
                    missing_connector: row.get(12)?,
                    gates: row.get(13)?,
                    claude_weekly_pct: row.get(14)?,
                    codex_weekly_pct: row.get(15)?,
                    dry_run: row.get(16)?,
                    job_id: row.get(17)?,
                    job_name: row.get(18)?,
                    outcome: row.get(19)?,
                    rationale: row.get(20)?,
                    complexity: row.get(21)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(rows)
    }
}

/// IMPURE: bring a database written before complexity scaling up to the current schema. Guarded
/// on the column being absent, because `ALTER TABLE ADD COLUMN` is an error when it is not.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    let present: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('decisions') WHERE name = 'complexity'")?
        .exists([])?;
    if !present {
        conn.execute("ALTER TABLE decisions ADD COLUMN complexity TEXT", [])?;
    }
    Ok(())
}

pub fn default_db_path() -> PathBuf {
    home_dir().join(".local/state/agent-router/router.db")
}

/// The log holds full task text, so its directory is the owner's alone.
#[cfg(unix)]
fn restrict_to_owner(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_dir: &Path) -> Result<()> {
    Ok(())
}

/// PURE: the six rubric booleans as "101010", the compact shape a human reads in a sqlite dump.
fn bits(flags: &[bool; 6]) -> String {
    flags
        .iter()
        .map(|held| if *held { '1' } else { '0' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Classification, Complexity, Confidence, Verdict};
    use crate::config::Config;
    use crate::usage::{Headroom, UsageSnapshot};

    fn decision() -> Decision {
        let classification = Classification {
            codex_ready: [true, true, true, true, true, false],
            claude_signals: [false, true, false, false, false, false],
            missing_connector: false,
            verdict: Verdict::Codex,
            confidence: Confidence::High,
            complexity: Complexity::Ultra,
            rationale: "bounded contract".to_string(),
            classifier_failed: false,
        };
        let usage = UsageSnapshot {
            claude: Headroom {
                five_hour_pct: 11.0,
                five_hour_reset_epoch: 1_785_375_600,
                weekly_pct: 50.0,
                weekly_reset_epoch: 1_785_589_200,
            },
            codex: Headroom {
                weekly_pct: 71.0,
                weekly_reset_epoch: 1_785_908_348,
                ..Headroom::full()
            },
        };
        crate::decide::decide(classification, usage, &Config::default())
    }

    #[test]
    fn a_recorded_decision_reads_back_with_its_rubric_scores_and_usage_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("state/router.db")).expect("opens");
        let decision = decision();
        let id = log
            .record(&Entry {
                task: "audit the airtable records",
                dir: Path::new("/tmp"),
                requested: "auto",
                decision: &decision,
                dry_run: false,
                job_id: Some("thread-abc"),
                job_name: None,
                outcome: "dispatched",
            })
            .expect("records");
        assert!(id > 0);

        let rows = log.recent(10).expect("reads back");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.task, "audit the airtable records");
        assert_eq!(row.provider, "codex");
        assert_eq!(row.verdict.as_deref(), Some("codex"));
        assert_eq!(row.confidence.as_deref(), Some("high"));
        assert_eq!(row.complexity.as_deref(), Some("ultra"));
        assert_eq!(row.codex_ready_count, Some(5));
        assert_eq!(row.claude_signal_count, Some(1));
        assert_eq!(row.missing_connector, Some(false));
        assert_eq!(row.claude_weekly_pct, 50.0);
        assert_eq!(row.codex_weekly_pct, 71.0);
        assert_eq!(row.job_id.as_deref(), Some("thread-abc"));
        assert_eq!(row.outcome, "dispatched");
        assert!(!row.dry_run);
        assert!(row.created_at_ms > 0);
        assert!(row.rationale.contains("bounded contract"));
    }

    #[test]
    fn the_raw_row_keeps_both_rubric_arrays_and_every_usage_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let log = DecisionLog::open_at(&path).expect("opens");
        let decision = decision();
        log.record(&Entry {
            task: "t",
            dir: Path::new("/tmp"),
            requested: "auto",
            decision: &decision,
            dry_run: true,
            job_id: None,
            job_name: Some("t"),
            outcome: "dry-run",
        })
        .expect("records");
        let conn = rusqlite::Connection::open(&path).expect("reopen");
        let (codex_ready, claude_signals, gates, five_hour, weekly_reset): (
            String,
            String,
            String,
            f64,
            i64,
        ) = conn
            .query_row(
                "SELECT codex_ready, claude_signals, gates, claude_five_hour_pct, \
                 codex_weekly_reset FROM decisions",
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
            .expect("query");
        assert_eq!(codex_ready, "111110");
        assert_eq!(claude_signals, "010000");
        assert_eq!(gates, "");
        assert_eq!(five_hour, 11.0);
        assert_eq!(weekly_reset, 1_785_908_348);
    }

    #[test]
    fn an_explicit_provider_row_has_no_classification_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
        let decision = crate::decide::decide_explicit(
            crate::Provider::Opencode,
            None,
            UsageSnapshot::full(),
            &Config::default(),
        );
        log.record(&Entry {
            task: "t",
            dir: Path::new("/tmp"),
            requested: "opencode",
            decision: &decision,
            dry_run: false,
            job_id: Some("session-1"),
            job_name: None,
            outcome: "dispatched",
        })
        .expect("records");
        let row = &log.recent(1).expect("reads")[0];
        assert_eq!(row.provider, "opencode");
        assert_eq!(row.verdict, None);
        assert_eq!(row.confidence, None);
        assert_eq!(row.codex_ready_count, None);
        assert_eq!(row.gates, "explicit_provider");
    }

    #[test]
    fn recent_returns_newest_first_and_honours_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
        let decision = decision();
        for task in ["first", "second", "third"] {
            log.record(&Entry {
                task,
                dir: Path::new("/tmp"),
                requested: "auto",
                decision: &decision,
                dry_run: true,
                job_id: None,
                job_name: None,
                outcome: "dry-run",
            })
            .expect("records");
        }
        let rows = log.recent(2).expect("reads");
        assert_eq!(
            rows.iter().map(|row| row.task.as_str()).collect::<Vec<_>>(),
            vec!["third", "second"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_state_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state/router.db");
        DecisionLog::open_at(&path).expect("opens");
        let mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// A database written before complexity scaling gains the column on open, keeps its rows, and
    /// reads them back with no complexity rather than failing the SELECT.
    #[test]
    fn an_older_database_gains_the_complexity_column_and_keeps_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let older = SCHEMA.replace("    complexity          TEXT,\n", "");
        assert!(!older.contains("complexity"), "the older schema fixture");
        {
            let conn = rusqlite::Connection::open(&path).expect("create older database");
            conn.execute_batch(&older).expect("older schema");
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
            .expect("older row");
        }

        let log = DecisionLog::open_at(&path).expect("migrates on open");
        let rows = log.recent(10).expect("reads the older row back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task, "older row");
        assert_eq!(rows[0].complexity, None);

        // The migration is idempotent: opening again must not try to add the column twice.
        DecisionLog::open_at(&path).expect("reopens");
    }

    #[test]
    fn opening_an_existing_log_twice_keeps_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let decision = decision();
        {
            let log = DecisionLog::open_at(&path).expect("opens");
            log.record(&Entry {
                task: "kept",
                dir: Path::new("/tmp"),
                requested: "auto",
                decision: &decision,
                dry_run: true,
                job_id: None,
                job_name: None,
                outcome: "dry-run",
            })
            .expect("records");
        }
        let reopened = DecisionLog::open_at(&path).expect("reopens");
        assert_eq!(reopened.recent(10).expect("reads").len(), 1);
    }
}
