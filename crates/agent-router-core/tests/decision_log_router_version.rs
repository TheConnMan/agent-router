//! Which build of the router wrote each decision row.
//!
//! `router_version` makes an aggregate spanning several builds visibly mixed rather than
//! silently pooled. These assertions go through SQL rather than the flattened read model
//! wherever the contract under test is the shape of the table.

use agent_router_core::config::Config;
use agent_router_core::decide::decide_explicit;
use agent_router_core::log::{DecisionLog, Entry, Mark};
use agent_router_core::{Provider, UsageSnapshot};
use std::path::Path;

/// One decision written the way a dispatch writes it: through `record`, which is the only INSERT
/// into `decisions` and so the only place a version can be stamped. Returns the row id.
fn record(log: &DecisionLog, task: &str) -> i64 {
    let decision = decide_explicit(
        Provider::Codex,
        None,
        None,
        None,
        UsageSnapshot::full(),
        &Config::default(),
    );
    log.record(&Entry {
        task,
        dir: Path::new("/tmp"),
        requested: "codex",
        decision: &decision,
        dry_run: false,
        job_id: Some("c0ffee42"),
        job_name: None,
        outcome: "dispatched",
        effective_effort: None,
    })
    .expect("records the decision")
}

/// The one column a raw read wants back, so the query names it rather than reading an ordinal: a
/// fresh database and a migrated one disagree on every ordinal, because `ALTER TABLE ADD COLUMN`
/// can only append.
fn router_version(path: &Path, id: i64) -> Option<String> {
    let conn = rusqlite::Connection::open(path).expect("reopen");
    conn.query_row(
        "SELECT router_version FROM decisions WHERE id = ?1",
        [id],
        |row| row.get(0),
    )
    .expect("the router_version column exists and is selectable")
}

/// The write site. Every row `record` appends carries the version of the router that appended it,
/// read straight back out of SQL by a second connection so the value is proved to have reached the
/// file rather than only the writer's own memory.
///
/// The assertion is equality with the crate version rather than "not null" on purpose. A writer
/// bound to a stale literal, to a placeholder, or to another crate's version satisfies `is_some()`
/// and still misattributes every row it writes, which is precisely the failure this column exists
/// to prevent: a row that names the wrong writer is worse than a row that names none.
#[test]
fn a_recorded_decision_stamps_the_router_version_that_wrote_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state/router.db");
    let log = DecisionLog::open_at(&path).expect("opens");

    let id = record(&log, "audit the airtable records");

    assert_eq!(
        router_version(&path, id).as_deref(),
        Some(env!("CARGO_PKG_VERSION")),
        "a recorded row must name the build of the router that wrote it"
    );
}

/// A new binary reconciling or marking an old row must not restamp it.
///
/// This is the real regression the column invites: both writes are UPDATEs on rows that already
/// exist, so a version bound there would relabel a historical decision as having been made by
/// whatever build happened to run `status` or `log --mark` afterwards. History would then re-attribute
/// itself every time the router was upgraded, which is a worse reading than no column.
#[test]
fn reconciling_or_marking_a_row_leaves_the_version_that_wrote_it_alone() {
    const EARLIER_BUILD: &str = "0.0.1-earlier";

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    let log = DecisionLog::open_at(&path).expect("opens");
    let id = record(&log, "a decision an earlier build made");

    // Stand the row down to an earlier build's version, which is the only way to have a row written
    // by code that is not the code running the test.
    {
        let conn = rusqlite::Connection::open(&path).expect("reopen to age the row");
        conn.execute(
            "UPDATE decisions SET router_version = ?1 WHERE id = ?2",
            rusqlite::params![EARLIER_BUILD, id],
        )
        .expect("age the row");
    }

    log.reconcile(id, "completed").expect("reconciles");
    log.mark(id, Mark::Bad, Some("routed to codex, needed connectors"))
        .expect("marks");

    // Both updates really landed, so this is not passing because neither write happened.
    let row = &log.recent(1).expect("reads the row back")[0];
    assert_eq!(row.outcome, "completed");
    assert_eq!(row.mark.as_deref(), Some("bad"));

    assert_eq!(
        router_version(&path, id).as_deref(),
        Some(EARLIER_BUILD),
        "an update must not re-attribute a decision to the build that updated it"
    );
}
