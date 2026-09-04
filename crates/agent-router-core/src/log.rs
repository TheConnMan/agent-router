//! The SQLite log at `~/.local/state/agent-router/router.db`: one `decisions` row per routing
//! decision, and one `reviews` row per adversarial review. `decisions` is the tuning data for the
//! heuristic and the answer to "why did this route here".

use crate::decide::Decision;
use crate::error::{Error, Result};
use crate::runtime::now_ms;
use rusqlite::Connection;
use std::collections::HashSet;
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
    /// What the backend reported this job will actually run its reasoning at, which is a different
    /// fact from the effort the router decided. None where the backend says nothing, which is every
    /// claude and grok dispatch and every dry run.
    pub effective_effort: Option<&'a str>,
}

/// One adversarial-review row to write: the outcome fields the CLI exit site can produce.
#[derive(Debug, Clone)]
pub struct ReviewEntry<'a> {
    pub exit_status: i64,
    pub primary: &'a str,
    pub reviewer_provider: Option<&'a str>,
    pub reviewer_model: Option<&'a str>,
    pub usage_provenance: &'a str,
    pub rationale: &'a str,
    pub body_bytes: i64,
    pub dir: &'a Path,
}

/// One reviews row read back.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRow {
    pub id: i64,
    pub ts: i64,
    pub exit_status: i64,
    pub primary: String,
    pub reviewer_provider: Option<String>,
    pub reviewer_model: Option<String>,
    pub usage_provenance: String,
    pub rationale: String,
    pub body_bytes: i64,
    pub dir: String,
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
    /// The reasoning effort the router decided. Not the effort the job ran at: that is
    /// `effective_effort` below.
    pub effort: Option<String>,
    /// None for a row written before complexity scaling, and for a fully pinned route.
    pub complexity: Option<String>,
    /// The classifier predicted working context horizon. None on a historical row and on an
    /// explicit provider row.
    pub task_context_horizon: Option<String>,
    /// The capability pin the classifier does still produce. None on a row written before it, and
    /// on an explicit provider.
    pub orchestration: Option<bool>,
    pub missing_connector: Option<bool>,
    /// What each provider's weekly draw projected to at its own window's reset when this decision
    /// was made, as a percent of that provider's own allowance. None on a row written before the
    /// projection override, on an explicit provider, and on a row whose projection could not be
    /// computed. Rows written before router 0.5.0 instead carry a run rate difference in the
    /// retired `claude_pace_delta`/`codex_pace_delta` columns, which are a different measurement on
    /// a different scale; the two series must not be read as one.
    pub claude_projected_draw: Option<f64>,
    pub codex_projected_draw: Option<f64>,
    pub grok_projected_draw: Option<f64>,
    pub gates: String,
    pub claude_weekly_pct: f64,
    pub codex_weekly_pct: f64,
    /// None on a row written before Grok weekly capacity was logged, and on a row whose Grok
    /// weekly window was unknown at record time.
    pub grok_weekly_pct: Option<f64>,
    pub dry_run: bool,
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    /// What became of the job. The dispatch time value until `status` has read a backend for this
    /// row, and the reconciled state afterwards.
    pub outcome: String,
    pub rationale: String,
    /// Whether the Claude usage numbers on this row came from a fail open read rather than a live
    /// one. None for a row written before the marker, which is not the same as a live read.
    pub claude_usage_stale: Option<bool>,
    /// The same for Codex.
    pub codex_usage_stale: Option<bool>,
    /// When the last reading that actually landed was taken. None means never reconciled, which is
    /// not the same as reconciled and found unknown.
    pub reconciled_at_ms: Option<i64>,
    /// The human judgement on the route. None means nobody has judged this row, which is not the
    /// same as judging it good.
    pub mark: Option<String>,
    /// What the human said alongside the mark. None means nothing was said.
    pub note: Option<String>,
    /// The reasoning effort the backend reported this job will run at, as against `effort` above,
    /// which is what the router decided. None means nobody observed one: the backend exposes none
    /// (claude, grok), nothing was dispatched (a dry run), or the row predates the column. None
    /// of those is the same as a job running at no effort.
    pub effective_effort: Option<String>,
    /// The `agent-router` build that made this decision, from `CARGO_PKG_VERSION` at record time.
    /// None on a row written before this column, which is not the same as a version that is
    /// genuinely empty: an aggregate spanning both a None and a real version is mixed regardless.
    pub router_version: Option<String>,
}

/// The human judgement on one routing decision: whether sending the task there was the right call.
///
/// Deliberately a separate column from `outcome`, which says only whether the job ran. A completed
/// job can still be a bad route, and a failed job can be the right route that hit an unrelated
/// fault, so no backend can produce this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Good,
    Bad,
    Rerouted,
}

impl Mark {
    pub const fn tag(self) -> &'static str {
        match self {
            Mark::Good => "good",
            Mark::Bad => "bad",
            Mark::Rerouted => "rerouted",
        }
    }
}

/// PURE: the mark a `--mark` value names.
///
/// The column is TEXT and SQLite enforces nothing about what lands in it, so this is deliberately
/// the only way a value reaches it: a second write path added later is the regression to guard
/// against.
pub fn parse_mark(value: &str) -> Result<Mark> {
    match value {
        "good" => Ok(Mark::Good),
        "bad" => Ok(Mark::Bad),
        "rerouted" => Ok(Mark::Rerouted),
        other => Err(Error::Command(format!(
            "unknown mark {other:?}: expected good, bad, or rerouted"
        ))),
    }
}

/// PURE: the log row a `--mark` value names. The CLI takes the id as a string so the rejection names
/// the value the caller actually passed rather than reporting clap's own parse failure.
pub fn parse_row_id(value: &str) -> Result<i64> {
    value.parse().map_err(|_| {
        Error::Command(format!(
            "unknown row id {value:?}: expected the id of a decision log row"
        ))
    })
}

/// One row as the reconciler needs it: the columns it matches a backend on, nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRow {
    pub id: i64,
    pub created_at_ms: i64,
    pub provider: String,
    /// Never NULL: the query that produces these rows excludes rows carrying no job.
    pub job_id: String,
    pub outcome: String,
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
    /// The human judgement on the route. None means nobody has judged this row, which is why a bad
    /// rate is denominated on the marked rows alone.
    pub mark: Option<String>,
    /// What became of the job: the dispatch time value until `status` has read a backend for this
    /// row, and the reconciled state afterwards. It is the state, so a failure rate reads it
    /// directly rather than a second column.
    pub outcome: String,
    /// The `agent-router` build that made this decision. None on a row written before this column,
    /// which a report must count separately from a row that genuinely carries a known version.
    pub router_version: Option<String>,
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
    complexity          TEXT,
    task_context_horizon TEXT,
    orchestration       INTEGER,
    missing_connector   INTEGER,
    claude_projected_draw REAL,
    codex_projected_draw  REAL,
    grok_projected_draw   REAL,
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
    grok_weekly_pct        REAL,
    grok_weekly_reset      INTEGER,
    claude_usage_stale     INTEGER,
    codex_usage_stale      INTEGER,
    dry_run             INTEGER NOT NULL,
    job_id              TEXT,
    job_name            TEXT,
    reconciled_at_ms    INTEGER,
    mark                TEXT,
    note                TEXT,
    effective_effort    TEXT,
    router_version      TEXT,
    outcome             TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS decisions_created_at ON decisions(created_at_ms);
";

/// A new table, so an existing database picks it up the same way `decisions` did: `CREATE TABLE IF
/// NOT EXISTS` on open. `"primary"` is quoted because it is a SQLite keyword.
const REVIEWS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS reviews (
    id                  INTEGER PRIMARY KEY,
    ts                  INTEGER NOT NULL,
    exit_status         INTEGER NOT NULL,
    \"primary\"         TEXT    NOT NULL,
    reviewer_provider   TEXT,
    reviewer_model      TEXT,
    usage_provenance    TEXT    NOT NULL,
    rationale           TEXT    NOT NULL,
    body_bytes          INTEGER NOT NULL,
    dir                 TEXT    NOT NULL
);
";

const SELECT_COLUMNS: &str = "\
id, created_at_ms, task, dir, requested, provider, model, effort, missing_connector, \
gates, claude_weekly_pct, codex_weekly_pct, dry_run, job_id, job_name, outcome, \
rationale, complexity, task_context_horizon, claude_usage_stale, codex_usage_stale, \
orchestration, claude_projected_draw, codex_projected_draw, reconciled_at_ms, mark, \
note, effective_effort, router_version, grok_weekly_pct, grok_projected_draw";

/// Outcomes whose fate is known. Same set `stats` denominates a failure rate on: `completed`,
/// `failed`, or a dispatch error. A dry run's outcome is `dry-run`, so it does not match; live
/// jobs (`dispatched`, `running`) and `unknown` stay out because a review pass cannot judge a
/// route whose job has not finished.
const SETTLED_OUTCOME_SQL: &str =
    "outcome = 'completed' OR outcome = 'failed' OR outcome LIKE 'error: %'";

/// The narrower list the stats reader needs, so a report never pays for columns it drops.
const STATS_COLUMNS: &str = "created_at_ms, requested, provider, complexity, gates, dry_run, \
mark, outcome, router_version";

/// The narrower list the reconciler needs.
const STATUS_COLUMNS: &str = "id, created_at_ms, provider, job_id, outcome";

/// The rows a reconciliation may touch: the ones that actually produced a job. A dry run dispatched
/// nothing, and a failed dispatch bound its `job_id` to NULL while putting the backend's own
/// message in `outcome`, so this single predicate keeps both out of every reconciliation and leaves
/// that message untouched.
const RECONCILABLE: &str = "dry_run = 0 AND job_id IS NOT NULL";

/// The `agent-router` build writing this process, stamped onto every row at record time so an
/// aggregate spanning code that changed the routing rules is visibly mixed rather than silently
/// pooled. Never touched by `reconcile()` or `mark()`: those are updates to a row already written,
/// and restamping one with the version doing the reconciling would misattribute the decision to a
/// build that never made it.
const ROUTER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct DecisionLog {
    conn: Connection,
}

impl DecisionLog {
    /// IMPURE: the log at the default path under `home`, creating the state directory (0700, it
    /// records task text) and the schema when absent.
    pub fn open_in(home: &Path) -> Result<DecisionLog> {
        DecisionLog::open_at(&default_db_path(home))
    }

    pub fn open_at(path: &Path) -> Result<DecisionLog> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict_to_owner(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        migrate_schema(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(REVIEWS_SCHEMA)?;
        add_missing_columns(&conn)?;
        Ok(DecisionLog { conn })
    }

    /// IMPURE: prove the log can actually be written.
    ///
    /// `open()` alone does not prove it: its schema batch is entirely `CREATE ... IF NOT EXISTS`,
    /// so against a database whose objects all already exist it satisfies every statement without
    /// ever taking a write lock, and returns `Ok` on a database the next `record()` fails on.
    ///
    /// A lock only probe does not prove it either, which is why this writes. Measured against a
    /// 0o444 database file: `CREATE TABLE IF NOT EXISTS` succeeds, `BEGIN IMMEDIATE` succeeds, and
    /// only an actual write raises "attempt to write a readonly database". So the probe creates a
    /// throwaway table inside a transaction and rolls it back, which is a genuine write against a
    /// database that leaves behind no table, no row, and no schema change.
    pub fn probe_writable(&self) -> Result<()> {
        let probe = self.conn.execute_batch(
            "BEGIN IMMEDIATE; CREATE TABLE agent_router_write_probe (id INTEGER); ROLLBACK;",
        );
        if probe.is_err() {
            // The batch stops at the failing statement, so the transaction it opened is still
            // open on this connection and has to be closed before the connection is reused.
            let _ = self.conn.execute_batch("ROLLBACK;");
        }
        probe?;
        Ok(())
    }

    /// Append one decision. Returns its row id.
    pub fn record(&self, entry: &Entry) -> Result<i64> {
        let decision = entry.decision;
        let classification = decision.classification.as_ref();
        let usage = &decision.usage;
        self.conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, model, effort,
                orchestration, missing_connector, gates, rationale,
                claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                codex_weekly_pct, codex_weekly_reset, dry_run, job_id, job_name, outcome,
                complexity, task_context_horizon, claude_usage_stale, codex_usage_stale,
                claude_projected_draw, codex_projected_draw, grok_projected_draw,
                effective_effort, router_version, grok_weekly_pct, grok_weekly_reset
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30,
                ?31, ?32, ?33, ?34
            )",
            rusqlite::params![
                now_ms(),
                entry.task,
                entry.dir.to_string_lossy(),
                entry.requested,
                decision.provider.name(),
                decision.model,
                decision.effort,
                classification.map(|c| c.orchestration),
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
                classification.map(|c| c.task_context_horizon.tag()),
                usage.claude.stale,
                usage.codex.stale,
                decision.claude_projected_draw,
                decision.codex_projected_draw,
                decision.grok_projected_draw,
                entry.effective_effort,
                ROUTER_VERSION,
                usage.grok.weekly_known().then_some(usage.grok.weekly_pct),
                usage.grok.weekly_reset_epoch,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// IMPURE: append one adversarial-review outcome. Returns its row id.
    pub fn record_review(&self, entry: &ReviewEntry) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO reviews (
                ts, exit_status, \"primary\", reviewer_provider, reviewer_model,
                usage_provenance, rationale, body_bytes, dir
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                now_ms(),
                entry.exit_status,
                entry.primary,
                entry.reviewer_provider,
                entry.reviewer_model,
                entry.usage_provenance,
                entry.rationale,
                entry.body_bytes,
                entry.dir.to_string_lossy(),
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
                mark: row.get(6)?,
                outcome: row.get(7)?,
                router_version: row.get(8)?,
            })
        };
        let rows = match since_ms {
            Some(floor) => {
                let sql = format!(
                    "SELECT {STATS_COLUMNS} FROM decisions WHERE created_at_ms >= ?1 \
                     ORDER BY id DESC LIMIT ?2"
                );
                let mut statement = self.conn.prepare(&sql)?;
                let rows =
                    statement.query_map([floor, i64::try_from(limit).unwrap_or(i64::MAX)], read)?;
                rows.collect::<rusqlite::Result<Vec<StatsRow>>>()?
            }
            None => {
                let sql =
                    format!("SELECT {STATS_COLUMNS} FROM decisions ORDER BY id DESC LIMIT ?1");
                let mut statement = self.conn.prepare(&sql)?;
                let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], read)?;
                rows.collect::<rusqlite::Result<Vec<StatsRow>>>()?
            }
        };
        Ok(rows)
    }

    /// IMPURE: the `limit` newest rows a reconciliation may touch, newest first. `since_ms` is an
    /// inclusive floor on `created_at_ms`, filtering first so the limit caps what is left, exactly
    /// as the stats window does.
    pub fn status_rows(&self, limit: usize, since_ms: Option<i64>) -> Result<Vec<StatusRow>> {
        let read = |row: &rusqlite::Row| {
            Ok(StatusRow {
                id: row.get(0)?,
                created_at_ms: row.get(1)?,
                provider: row.get(2)?,
                job_id: row.get(3)?,
                outcome: row.get(4)?,
            })
        };
        let rows = match since_ms {
            Some(floor) => {
                let sql = format!(
                    "SELECT {STATUS_COLUMNS} FROM decisions WHERE created_at_ms >= ?1 \
                     AND {RECONCILABLE} ORDER BY id DESC LIMIT ?2"
                );
                let mut statement = self.conn.prepare(&sql)?;
                let rows =
                    statement.query_map([floor, i64::try_from(limit).unwrap_or(i64::MAX)], read)?;
                rows.collect::<rusqlite::Result<Vec<StatusRow>>>()?
            }
            None => {
                let sql = format!(
                    "SELECT {STATUS_COLUMNS} FROM decisions WHERE {RECONCILABLE} \
                     ORDER BY id DESC LIMIT ?1"
                );
                let mut statement = self.conn.prepare(&sql)?;
                let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], read)?;
                rows.collect::<rusqlite::Result<Vec<StatusRow>>>()?
            }
        };
        Ok(rows)
    }

    /// IMPURE: record what the backend said about one row, and when it said it.
    ///
    /// The caller reaches here only for a state the monotonicity rule admitted, so this writes
    /// whatever it is handed and the rule that protects a proven outcome lives outside SQL where it
    /// is testable with no database.
    pub fn reconcile(&self, id: i64, outcome: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE decisions SET outcome = ?1, reconciled_at_ms = ?2 WHERE id = ?3",
            rusqlite::params![outcome, now_ms(), id],
        )?;
        Ok(())
    }

    /// IMPURE: record the human judgement on one row, replacing any earlier one.
    ///
    /// Both columns are written every time, including the note when there is none. A mark with no
    /// note landing after a mark that had one would otherwise leave the old note attached to a new
    /// judgement, which is a sentence no human said.
    ///
    /// A zero row update is an error rather than a quiet success: a bare UPDATE that matched
    /// nothing exits 0 and reports a judgement as recorded when nothing was recorded at all.
    pub fn mark(&self, id: i64, mark: Mark, note: Option<&str>) -> Result<()> {
        // An empty or whitespace only note is Some, so it would land in the column and read as a
        // note nobody wrote. A loud error is correct, exactly as it is for an empty --name, since a
        // caller passing a note believes it recorded one.
        if let Some(note) = note
            && note.trim().is_empty()
        {
            return Err(Error::Command(
                "--note must not be empty or whitespace only".to_string(),
            ));
        }
        let changed = self.conn.execute(
            "UPDATE decisions SET mark = ?1, note = ?2 WHERE id = ?3",
            rusqlite::params![mark.tag(), note, id],
        )?;
        if changed == 0 {
            return Err(Error::Command(format!("no decision log row with id {id}")));
        }
        Ok(())
    }

    /// IMPURE: this provider's own weekly percent at each dispatched decision on this exact model,
    /// oldest first. Dry runs are excluded: they drew nothing, so a step across one is not a draw.
    ///
    /// The inner query takes the NEWEST `limit` rows and the outer one puts them back in
    /// chronological order. Ordering ascending before the limit would instead take the OLDEST rows
    /// and quietly answer about ancient history as the log grows, while the steps between rows are
    /// only meaningful read oldest first.
    pub fn weekly_series(&self, provider: &str, model: &str, limit: usize) -> Result<Vec<f64>> {
        let Some(column) = weekly_column(provider) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {column} FROM (SELECT id, {column} FROM decisions \
             WHERE provider = ?1 AND model = ?2 AND dry_run = 0 \
             ORDER BY id DESC LIMIT ?3) ORDER BY id ASC"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let series = statement
            .query_map(
                rusqlite::params![provider, model, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row: &rusqlite::Row| row.get::<_, Option<f64>>(0),
            )?
            .filter_map(|value| value.transpose())
            .collect::<rusqlite::Result<Vec<f64>>>()?;
        Ok(series)
    }

    /// The `limit` newest decisions, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Row>> {
        let sql = format!("SELECT {SELECT_COLUMNS} FROM decisions ORDER BY id DESC LIMIT ?1");
        self.query_decision_rows(&sql, limit)
    }

    /// IMPURE: the `limit` newest settled rows nobody has judged, newest first.
    ///
    /// A review pass's worklist. Marked rows are already judged; unsettled rows have no fate to
    /// judge against yet.
    pub fn recent_unmarked(&self, limit: usize) -> Result<Vec<Row>> {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM decisions \
             WHERE mark IS NULL AND ({SETTLED_OUTCOME_SQL}) \
             ORDER BY id DESC LIMIT ?1"
        );
        self.query_decision_rows(&sql, limit)
    }

    fn query_decision_rows(&self, sql: &str, limit: usize) -> Result<Vec<Row>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], map_decision_row)?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(rows)
    }

    /// The `limit` newest adversarial reviews, newest first.
    pub fn recent_reviews(&self, limit: usize) -> Result<Vec<ReviewRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, ts, exit_status, \"primary\", reviewer_provider, reviewer_model, \
             usage_provenance, rationale, body_bytes, dir \
             FROM reviews ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(ReviewRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    exit_status: row.get(2)?,
                    primary: row.get(3)?,
                    reviewer_provider: row.get(4)?,
                    reviewer_model: row.get(5)?,
                    usage_provenance: row.get(6)?,
                    rationale: row.get(7)?,
                    body_bytes: row.get(8)?,
                    dir: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<ReviewRow>>>()?;
        Ok(rows)
    }
}

fn map_decision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: row.get(0)?,
        created_at_ms: row.get(1)?,
        task: row.get(2)?,
        dir: row.get(3)?,
        requested: row.get(4)?,
        provider: row.get(5)?,
        model: row.get(6)?,
        effort: row.get(7)?,
        missing_connector: row.get(8)?,
        gates: row.get(9)?,
        claude_weekly_pct: row.get(10)?,
        codex_weekly_pct: row.get(11)?,
        dry_run: row.get(12)?,
        job_id: row.get(13)?,
        job_name: row.get(14)?,
        outcome: row.get(15)?,
        rationale: row.get(16)?,
        complexity: row.get(17)?,
        task_context_horizon: row.get(18)?,
        claude_usage_stale: row.get(19)?,
        codex_usage_stale: row.get(20)?,
        orchestration: row.get(21)?,
        claude_projected_draw: row.get(22)?,
        codex_projected_draw: row.get(23)?,
        reconciled_at_ms: row.get(24)?,
        mark: row.get(25)?,
        note: row.get(26)?,
        effective_effort: row.get(27)?,
        router_version: row.get(28)?,
        grok_weekly_pct: row.get(29)?,
        grok_projected_draw: row.get(30)?,
    })
}

/// Columns added after schema v2. Empty after the v2 rewrite, which creates every current column
/// in `SCHEMA`. Keep the mechanism: a later column goes here as `ALTER TABLE ADD COLUMN` so an
/// existing v2 database picks it up on open without another table rewrite.
const MISSING_COLUMNS: [(&str, &str); 0] = [];

/// `PRAGMA user_version` stamped once the v2 rewrite has run. 0 is an unstamped pre-v2 database.
const SCHEMA_VERSION: i64 = 2;

/// Every column `SCHEMA` currently declares, in CREATE TABLE order, used by the v2 copy so a
/// source table missing some of them still yields a full v2 row (NULL for the absences).
const V2_COLUMNS: [&str; 38] = [
    "id",
    "created_at_ms",
    "task",
    "dir",
    "requested",
    "provider",
    "model",
    "effort",
    "complexity",
    "task_context_horizon",
    "orchestration",
    "missing_connector",
    "claude_projected_draw",
    "codex_projected_draw",
    "grok_projected_draw",
    "gates",
    "rationale",
    "claude_five_hour_pct",
    "claude_five_hour_reset",
    "claude_weekly_pct",
    "claude_weekly_reset",
    "codex_five_hour_pct",
    "codex_five_hour_reset",
    "codex_weekly_pct",
    "codex_weekly_reset",
    "grok_weekly_pct",
    "grok_weekly_reset",
    "claude_usage_stale",
    "codex_usage_stale",
    "dry_run",
    "job_id",
    "job_name",
    "reconciled_at_ms",
    "mark",
    "note",
    "effective_effort",
    "router_version",
    "outcome",
];

/// IMPURE: bring a database written before any of those columns up to the current schema. Guarded
/// on each column being absent, because `ALTER TABLE ADD COLUMN` is an error when it is not.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    for (name, declared_type) in MISSING_COLUMNS {
        let present: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('decisions') WHERE name = ?1")?
            .exists([name])?;
        if !present {
            conn.execute(
                &format!("ALTER TABLE decisions ADD COLUMN {name} {declared_type}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// IMPURE: one-time v2 rewrite. Guarded by `PRAGMA user_version`, so a database already at 2 is
/// left alone. A v1 `decisions` table is copied into the current shape without the retired score
/// columns, its gate tags are folded, then the old table is dropped. The orphaned `router_secret`
/// table left by the deleted OpenCode provider is dropped either way.
fn migrate_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    if table_exists(conn, "decisions")? {
        rewrite_decisions_v2(conn)?;
    }
    conn.execute("DROP TABLE IF EXISTS router_secret", [])?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
        .exists([name])?)
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut statement = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let names = statement.query_map([], |row| row.get(0))?;
    Ok(names.collect::<rusqlite::Result<HashSet<String>>>()?)
}

fn rewrite_decisions_v2(conn: &Connection) -> Result<()> {
    let existing = table_columns(conn, "decisions")?;
    let create_v2 = SCHEMA
        .replace(
            "CREATE TABLE IF NOT EXISTS decisions (",
            "CREATE TABLE decisions_v2 (",
        )
        .lines()
        .filter(|line| !line.contains("CREATE INDEX"))
        .collect::<Vec<_>>()
        .join("\n");
    conn.execute_batch(&create_v2)?;

    let select_list = V2_COLUMNS
        .iter()
        .map(|column| {
            if existing.contains(*column) {
                (*column).to_string()
            } else {
                format!("NULL AS {column}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!(
            "INSERT INTO decisions_v2 ({}) SELECT {select_list} FROM decisions",
            V2_COLUMNS.join(", ")
        ),
        [],
    )?;

    let rewritten: Vec<(i64, String)> = {
        let mut statement = conn.prepare("SELECT id, gates FROM decisions_v2")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, gates) in rewritten {
        let folded = rewrite_legacy_gates(&gates);
        if folded != gates {
            conn.execute(
                "UPDATE decisions_v2 SET gates = ?1 WHERE id = ?2",
                rusqlite::params![folded, id],
            )?;
        }
    }

    conn.execute_batch(
        "DROP TABLE decisions;
         ALTER TABLE decisions_v2 RENAME TO decisions;
         CREATE INDEX IF NOT EXISTS decisions_created_at ON decisions(created_at_ms);",
    )?;
    Ok(())
}

/// PURE: fold retired gate tags so flip rate over an old window is unchanged. The four provider
/// moving tags become one `legacy_flip` (deduped, so a row that carried two of them still counts
/// once). `claude_signals` becomes `legacy_pin`. Token-aware: `headroom_tiebreak_wide` is left
/// alone.
fn rewrite_legacy_gates(gates: &str) -> String {
    let mut folded = Vec::new();
    let mut saw_flip = false;
    let mut saw_pin = false;
    for tag in gates.split(',').filter(|tag| !tag.is_empty()) {
        match tag {
            "headroom_tiebreak" | "pace_flip" | "projected_overdraw" | "five_hour_pacing" => {
                if !saw_flip {
                    folded.push("legacy_flip");
                    saw_flip = true;
                }
            }
            "claude_signals" => {
                if !saw_pin {
                    folded.push("legacy_pin");
                    saw_pin = true;
                }
            }
            other => folded.push(other),
        }
    }
    folded.join(",")
}

/// PURE: the column a provider's own weekly percentage is recorded in. None for a provider the log
/// keeps no weekly window for, so its series reads as empty rather than as another provider's
/// numbers. Naming the column here rather than interpolating the caller's string also keeps the
/// only formatted identifier in these queries a fixed one.
fn weekly_column(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("claude_weekly_pct"),
        "codex" => Some("codex_weekly_pct"),
        "grok" => Some("grok_weekly_pct"),
        _ => None,
    }
}

pub fn default_db_path(home: &Path) -> PathBuf {
    home.join(".local/state/agent-router/router.db")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Classification, Complexity, TaskContextHorizon};
    use crate::config::Config;
    use crate::usage::{Headroom, UsageSnapshot};

    #[test]
    fn rewrite_legacy_gates_folds_tokens_without_touching_prefixes() {
        assert_eq!(rewrite_legacy_gates(""), "");
        assert_eq!(rewrite_legacy_gates("claude_signals"), "legacy_pin");
        assert_eq!(
            rewrite_legacy_gates("headroom_tiebreak,flipped_on_exhaustion"),
            "legacy_flip,flipped_on_exhaustion"
        );
        assert_eq!(
            rewrite_legacy_gates("headroom_tiebreak,five_hour_pacing"),
            "legacy_flip"
        );
        assert_eq!(
            rewrite_legacy_gates("headroom_tiebreak_wide,headroom_tiebreak"),
            "headroom_tiebreak_wide,legacy_flip"
        );
        assert_eq!(
            rewrite_legacy_gates("pace_flip,projected_overdraw,claude_signals"),
            "legacy_flip,legacy_pin"
        );
        assert_eq!(
            rewrite_legacy_gates("explicit_provider"),
            "explicit_provider"
        );
    }

    /// The instant the fixture decision is made at, chosen to sit before both recorded resets so
    /// each provider's weekly window is genuinely part elapsed. The percentages below then put the
    /// two providers within the dead zone of each other, so this fixture records a decision no
    /// rule moved: what is under test here is the round trip, not the routing.
    const NOW: i64 = 1_785_400_000;

    fn decision() -> Decision {
        let classification = Classification {
            orchestration: false,
            missing_connector: false,
            complexity: Complexity::Ultra,
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "bounded contract".to_string(),
            classifier_failed: false,
            invokes_implement: false,
            unlaunchable: None,
        };
        let usage = UsageSnapshot {
            claude: Headroom {
                five_hour_pct: 11.0,
                five_hour_reset_epoch: 1_785_375_600,
                weekly_pct: 60.0,
                weekly_reset_epoch: 1_785_589_200,
                weekly_capacity_known: true,
                stale: false,
            },
            codex: Headroom {
                // Chosen so neither provider projects past its allowance and no gate fires: this
                // fixture is about what the log stores, so a routing rule firing inside it would
                // make the empty gate vector below assert something other than what it says.
                weekly_pct: 10.0,
                weekly_reset_epoch: 1_785_908_348,
                weekly_capacity_known: true,
                ..Headroom::full()
            },
            grok: Headroom::closed(),
        };
        crate::decide::decide(classification, usage, NOW, &Config::default())
    }

    #[test]
    fn a_recorded_decision_reads_back_with_its_scores_and_usage_snapshot() {
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
                effective_effort: None,
            })
            .expect("records");
        assert!(id > 0);

        let rows = log.recent(10).expect("reads back");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.task, "audit the airtable records");
        assert_eq!(row.provider, "codex");
        assert_eq!(row.complexity.as_deref(), Some("ultra"));
        assert_eq!(row.orchestration, Some(false));
        assert_eq!(row.missing_connector, Some(false));
        assert_eq!(row.claude_weekly_pct, 60.0);
        assert_eq!(row.codex_weekly_pct, 10.0);
        assert_eq!(row.grok_weekly_pct, None);
        // Both resets were read, so both projections travel with the row. Grok was closed, so
        // it has no weekly reading and no projection.
        assert!(row.claude_projected_draw.is_some());
        assert!(row.codex_projected_draw.is_some());
        assert_eq!(row.grok_projected_draw, None);
        assert_eq!(row.job_id.as_deref(), Some("thread-abc"));
        assert_eq!(row.outcome, "dispatched");
        assert!(!row.dry_run);
        assert!(row.created_at_ms > 0);
        assert!(row.rationale.contains("bounded contract"));
    }

    #[test]
    fn the_raw_row_keeps_every_usage_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let log = DecisionLog::open_at(&path).expect("opens");
        let automatic = decision();
        let decision = crate::decide::decide_explicit(
            crate::Provider::Grok,
            None,
            None,
            automatic.classification,
            automatic.usage,
            &Config::default(),
        );
        log.record(&Entry {
            task: "t",
            dir: Path::new("/tmp"),
            requested: "grok",
            decision: &decision,
            dry_run: true,
            job_id: None,
            job_name: Some("t"),
            outcome: "dry-run",
            effective_effort: None,
        })
        .expect("records");
        let conn = rusqlite::Connection::open(&path).expect("reopen");
        let (gates, five_hour, weekly_reset): (String, f64, i64) = conn
            .query_row(
                "SELECT gates, claude_five_hour_pct, codex_weekly_reset FROM decisions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query");
        assert_eq!(gates, "explicit_provider,grok_unavailable");
        assert_eq!(five_hour, 11.0);
        assert_eq!(weekly_reset, 1_785_908_348);
    }

    #[test]
    fn an_explicit_provider_row_has_no_classification_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
        let decision = crate::decide::decide_explicit(
            crate::Provider::Codex,
            None,
            None,
            None,
            UsageSnapshot::full(),
            &Config::default(),
        );
        log.record(&Entry {
            task: "t",
            dir: Path::new("/tmp"),
            requested: "codex",
            decision: &decision,
            dry_run: false,
            job_id: Some("session-1"),
            job_name: None,
            outcome: "dispatched",
            effective_effort: None,
        })
        .expect("records");
        let row = &log.recent(1).expect("reads")[0];
        assert_eq!(row.provider, "codex");
        assert_eq!(row.orchestration, None);
        assert_eq!(row.missing_connector, None);
        assert_eq!(row.complexity, None);
        assert_eq!(row.claude_projected_draw, None);
        assert_eq!(row.codex_projected_draw, None);
        assert_eq!(row.grok_projected_draw, None);
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
                effective_effort: None,
            })
            .expect("records");
        }
        let rows = log.recent(2).expect("reads");
        assert_eq!(
            rows.iter().map(|row| row.task.as_str()).collect::<Vec<_>>(),
            vec!["third", "second"]
        );
    }

    /// `log --unmarked` is a worklist, not a window filter: it must return the settled unmarked
    /// rows even when newer unmarked rows are still live or dry runs, and it must drop a settled
    /// row a human already judged.
    #[test]
    fn recent_unmarked_keeps_settled_null_marks_and_drops_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
        let record = |task: &str, dry_run: bool, outcome: &str| {
            log.record(&Entry {
                task,
                dir: Path::new("/tmp"),
                requested: "auto",
                decision: &decision(),
                dry_run,
                job_id: None,
                job_name: None,
                outcome,
                effective_effort: None,
            })
            .expect("records")
        };

        let completed = record("completed-unmarked", false, "completed");
        let failed = record("failed-unmarked", false, "failed");
        let errored = record("error-unmarked", false, "error: dispatch refused");
        let marked = record("completed-marked", false, "completed");
        log.mark(marked, Mark::Good, None).expect("marks");
        record("dispatched-unmarked", false, "dispatched");
        record("running-unmarked", false, "running");
        record("unknown-unmarked", false, "unknown");
        record("dry-run-unmarked", true, "dry-run");

        let rows = log.recent_unmarked(10).expect("reads");
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![errored, failed, completed]
        );
        assert!(rows.iter().all(|row| row.mark.is_none()));
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

    /// A database written before reconciliation gains the column on open, keeps its rows, and reads
    /// them back as never reconciled rather than failing the SELECT.
    #[test]
    fn an_older_database_gains_the_reconciled_at_column_and_keeps_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let older = SCHEMA.replace("    reconciled_at_ms    INTEGER,\n", "");
        assert!(
            !older.contains("reconciled_at_ms"),
            "the older schema fixture"
        );
        {
            let conn = rusqlite::Connection::open(&path).expect("create older database");
            conn.execute_batch(&older).expect("older schema");
            conn.execute(
                "INSERT INTO decisions (
                    created_at_ms, task, dir, requested, provider, gates, rationale,
                    claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                    claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                    codex_weekly_pct, codex_weekly_reset, dry_run, job_id, outcome
                ) VALUES (1, 'older row', '/tmp', 'auto', 'claude', '', 'why', 0, 0, 0, 0, 0, 0, \
                 0, 0, 0, 'c0ffee42', 'dispatched')",
                [],
            )
            .expect("older row");
        }

        let log = DecisionLog::open_at(&path).expect("migrates on open");
        let rows = log.recent(10).expect("reads the older row back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task, "older row");
        assert_eq!(rows[0].reconciled_at_ms, None);

        // The reconciler reads the migrated database through its own projection, whose columns sit
        // at different ordinals here than in a fresh one.
        let status = log
            .status_rows(10, None)
            .expect("reads the status projection");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].job_id, "c0ffee42");
        assert_eq!(status[0].outcome, "dispatched");

        log.reconcile(status[0].id, "unknown").expect("reconciles");
        let reconciled = &log.recent(10).expect("reads back")[0];
        assert_eq!(reconciled.outcome, "unknown");
        assert!(reconciled.reconciled_at_ms.is_some());

        // The migration is idempotent: opening again must not try to add the column twice.
        DecisionLog::open_at(&path).expect("reopens");
    }

    /// A database written before the human judgement gains both columns on open, keeps its rows,
    /// and reads them back as unjudged rather than failing the SELECT.
    #[test]
    fn an_older_database_gains_the_mark_columns_and_keeps_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let older = SCHEMA
            .replace("    mark                TEXT,\n", "")
            .replace("    note                TEXT,\n", "");
        assert!(!older.contains("mark"), "the older schema fixture");
        assert!(!older.contains("note"), "the older schema fixture");
        {
            let conn = rusqlite::Connection::open(&path).expect("create older database");
            conn.execute_batch(&older).expect("older schema");
            conn.execute(
                "INSERT INTO decisions (
                    created_at_ms, task, dir, requested, provider, gates, rationale,
                    claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                    claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                    codex_weekly_pct, codex_weekly_reset, dry_run, job_id, outcome
                ) VALUES (1, 'older row', '/tmp', 'auto', 'codex', '', 'why', 0, 0, 0, 0, 0, 0, \
                 0, 0, 0, 'c0ffee42', 'dispatched')",
                [],
            )
            .expect("older row");
        }

        let log = DecisionLog::open_at(&path).expect("migrates on open");
        let rows = log.recent(10).expect("reads the older row back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task, "older row");
        assert_eq!(rows[0].mark, None);
        assert_eq!(rows[0].note, None);

        // The judgement is written to the migrated database, whose appended columns sit at
        // different ordinals here than the same columns do in a fresh one.
        log.mark(rows[0].id, Mark::Bad, Some("needed connectors"))
            .expect("marks the older row");
        let marked = &log.recent(10).expect("reads back")[0];
        assert_eq!(marked.mark.as_deref(), Some("bad"));
        assert_eq!(marked.note.as_deref(), Some("needed connectors"));
        assert_eq!(marked.outcome, "dispatched");

        // The migration is idempotent: opening again must not try to add the columns twice.
        DecisionLog::open_at(&path).expect("reopens");
    }

    /// Shared setup for the two router-version migration tests below: an older-schema database
    /// carrying one historical row, opened through the column-adding migration. Returns the
    /// tempdir (kept alive so `router.db` survives for the caller) and the migrated log, which at
    /// this point holds only the historical row.
    fn open_router_version_migration_fixture() -> (tempfile::TempDir, DecisionLog) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        let older = SCHEMA.replace("    router_version      TEXT,\n", "");
        assert!(
            !older.contains("router_version"),
            "the older schema fixture"
        );
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
        (dir, log)
    }

    /// A database written before the router version gains the column on open, keeps its rows, and
    /// reads the historical row's version back as None through the `Row` model rather than failing
    /// the SELECT or guessing one: which build wrote that row is genuinely unknown, and a guess
    /// would misattribute it, which is worse than a null. A fresh row recorded through the migrated
    /// log afterwards is stamped normally, and `recent()` must tell the two apart through its own
    /// projection, whose appended column sits at a different ordinal here than in a fresh database.
    #[test]
    fn an_older_database_gains_the_router_version_column_and_recent_keeps_its_rows() {
        let (dir, log) = open_router_version_migration_fixture();
        let path = dir.path().join("router.db");

        let rows = log.recent(10).expect("reads the older row back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task, "older row");
        assert_eq!(rows[0].router_version, None);

        // A fresh row recorded after the migration is stamped with the running build's version, so
        // the migrated database now holds one row that knows its writer and one that does not.
        let decision = decision();
        log.record(&Entry {
            task: "new row",
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

        // recent() reads the migrated database through its own projection, whose appended column
        // sits at a different ordinal here than in a fresh one, and this is newest first.
        let rows = log.recent(10).expect("reads back");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].router_version.as_deref(), Some(ROUTER_VERSION));
        assert_eq!(rows[1].router_version, None);

        // The migration is idempotent: opening again must not try to add the column twice.
        DecisionLog::open_at(&path).expect("reopens");
    }

    /// A database written before the router version gains the column on open, keeps its rows, and
    /// reads the historical row's version back as None through the `StatsRow` model rather than
    /// failing the SELECT or guessing one: which build wrote that row is genuinely unknown, and a
    /// guess would misattribute it, which is worse than a null. A fresh row recorded through the
    /// migrated log afterwards is stamped normally, and `stats_rows()` must tell the two apart
    /// through its own projection, whose appended column sits at a different ordinal here than in
    /// a fresh database.
    #[test]
    fn an_older_database_gains_the_router_version_column_and_stats_rows_keeps_its_rows() {
        let (dir, log) = open_router_version_migration_fixture();
        let path = dir.path().join("router.db");

        // A fresh row recorded after the migration is stamped with the running build's version, so
        // the migrated database now holds one row that knows its writer and one that does not.
        let decision = decision();
        log.record(&Entry {
            task: "new row",
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

        // stats_rows() reads the migrated database through its own projection, whose appended
        // column sits at a different ordinal here than in a fresh one, and this is newest first.
        let stats = log
            .stats_rows(10, None)
            .expect("reads the stats projection");
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].router_version.as_deref(), Some(ROUTER_VERSION));
        assert_eq!(stats[1].router_version, None);

        // The migration is idempotent: opening again must not try to add the column twice.
        DecisionLog::open_at(&path).expect("reopens");
    }

    /// The accepted set is a closed enum of three and this parse is its only entry point, so every
    /// accepted value round trips through `tag()` and anything else is refused by name with the
    /// accepted values in the message.
    #[test]
    fn an_unknown_mark_is_rejected_by_name() {
        for mark in [Mark::Good, Mark::Bad, Mark::Rerouted] {
            assert_eq!(parse_mark(mark.tag()).expect("round trips"), mark);
        }
        assert_eq!(parse_mark("good").expect("good"), Mark::Good);
        assert_eq!(parse_mark("bad").expect("bad"), Mark::Bad);
        assert_eq!(parse_mark("rerouted").expect("rerouted"), Mark::Rerouted);

        let rejection = parse_mark("great")
            .expect_err("great is not a mark")
            .to_string();
        assert!(rejection.contains("great"), "{rejection}");
        for accepted in ["good", "bad", "rerouted"] {
            assert!(rejection.contains(accepted), "{rejection}");
        }
    }

    /// An empty note is Some, so nothing but this check keeps it out of the column, and a caller
    /// passing one believes it recorded a judgement.
    #[test]
    fn an_empty_note_is_rejected_rather_than_stored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
        let decision = decision();
        let id = log
            .record(&Entry {
                task: "t",
                dir: Path::new("/tmp"),
                requested: "auto",
                decision: &decision,
                dry_run: false,
                job_id: Some("thread-abc"),
                job_name: None,
                outcome: "dispatched",
                effective_effort: None,
            })
            .expect("records");

        log.mark(id, Mark::Good, Some("   "))
            .expect_err("a whitespace only note");

        let row = &log.recent(1).expect("reads")[0];
        assert_eq!(
            row.mark, None,
            "the rejected note carried a mark in with it"
        );
        assert_eq!(row.note, None);
    }

    /// The predicate the whole in place update rests on: a dry run and a dispatch error are outside
    /// the window by construction, so neither can be overwritten by a reconciliation.
    #[test]
    fn the_status_window_holds_only_rows_that_produced_a_job() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
        let decision = decision();
        let mut entry = Entry {
            task: "t",
            dir: Path::new("/tmp"),
            requested: "auto",
            decision: &decision,
            dry_run: true,
            job_id: None,
            job_name: None,
            outcome: "dry-run",
            effective_effort: None,
        };
        log.record(&entry).expect("a dry run row");
        entry.dry_run = false;
        entry.outcome = "error: app-server spawn failed";
        log.record(&entry).expect("a dispatch error row");
        entry.job_id = Some("thread-abc");
        entry.outcome = "dispatched";
        let dispatched = log.record(&entry).expect("a dispatched row");

        let rows = log.status_rows(10, None).expect("reads the window");
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![dispatched]
        );
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
                effective_effort: None,
            })
            .expect("records");
        }
        let reopened = DecisionLog::open_at(&path).expect("reopens");
        assert_eq!(reopened.recent(10).expect("reads").len(), 1);
    }

    /// A database that only has `decisions` gains `reviews` on open, keeps its decision rows, and
    /// can record a review afterwards. This is the migration an existing router.db takes.
    #[test]
    fn an_existing_database_gains_the_reviews_table_and_keeps_its_decision_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("router.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("create older database");
            conn.execute_batch(SCHEMA).expect("decisions schema only");
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
            let names: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .expect("list tables")
                .query_map([], |row| row.get(0))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("names");
            assert_eq!(names, vec!["decisions"]);
        }

        let log = DecisionLog::open_at(&path).expect("migrates on open");
        let rows = log.recent(10).expect("reads the older row back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task, "older row");

        log.record_review(&ReviewEntry {
            exit_status: 0,
            primary: "codex",
            reviewer_provider: Some("claude"),
            reviewer_model: Some("opus"),
            usage_provenance: "[]",
            rationale: "picked claude",
            body_bytes: 4,
            dir: Path::new("/tmp"),
        })
        .expect("records a review");

        let reviews = log.recent_reviews(10).expect("reads reviews");
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].exit_status, 0);
        assert_eq!(reviews[0].primary, "codex");
        assert_eq!(reviews[0].reviewer_provider.as_deref(), Some("claude"));
        assert_eq!(reviews[0].reviewer_model.as_deref(), Some("opus"));
        assert_eq!(reviews[0].usage_provenance, "[]");
        assert_eq!(reviews[0].rationale, "picked claude");
        assert_eq!(reviews[0].body_bytes, 4);
        assert_eq!(reviews[0].dir, "/tmp");
        assert!(reviews[0].ts > 0);

        DecisionLog::open_at(&path).expect("reopens");
    }
}
