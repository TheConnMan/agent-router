//! AC4, on disk: a classifier failure and a launch failure must be distinguishable in the
//! PERSISTED decision row.
//!
//! `Classification::unlaunchable` does not substantiate that claim, and this file exists because an
//! earlier draft thought it did. `log.rs` inserts named SQLite columns — `orchestration`,
//! `missing_connector`, `complexity`, `task_context_horizon`, `gates`, `rationale`, `outcome` — and
//! stores no `Classification` blob and no `unlaunchable` column, so the new field never reaches
//! disk at all. The on-disk discriminator is exactly two things: the `gates` column, written as
//! `decision.gate_tags().join(",")`, and the `outcome` column, written as `error: {e}`.
//!
//! So every assertion below reads the columns back out of a real `DecisionLog`. A test that
//! round-trips the in-memory `Decision` proves nothing about the log.

#![cfg(unix)]

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::Config;
use agent_router_core::decide::{Decision, decide};
use agent_router_core::log::{DecisionLog, Entry};
use agent_router_core::run::{Dispatch, recorded_fields};
use agent_router_core::{Headroom, Provider, Result, UsageSnapshot};
use std::path::Path;

mod common;

const NOW: i64 = 1_785_400_000;
const HALF_WEEK: i64 = 302_400;

/// The prefix `stats.rs`'s `DISPATCH_ERROR` counts a failed dispatch by. Duplicated here as a
/// literal on purpose: if the constant moves, this test is what says the persisted contract moved
/// with it.
const DISPATCH_ERROR_PREFIX: &str = "error: ";

fn window(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + HALF_WEEK,
        weekly_capacity_known: true,
        stale: false,
        ..Headroom::full()
    }
}

/// Grok's normal state on this box: a weekly window nobody read, which makes it ineligible.
fn unread() -> Headroom {
    Headroom {
        weekly_capacity_known: false,
        ..Headroom::full()
    }
}

fn usage() -> UsageSnapshot {
    UsageSnapshot {
        claude: window(10.0),
        codex: window(10.0),
        grok: unread(),
    }
}

fn ran_and_failed() -> Classification {
    Classification::fallback("timed out after 30s")
}

fn could_not_launch() -> Classification {
    Classification {
        unlaunchable: Some(Provider::Codex),
        ..Classification::fallback("could not find the codex executable")
    }
}

fn scored() -> Classification {
    Classification {
        orchestration: false,
        missing_connector: false,
        complexity: Complexity::Medium,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
        invokes_implement: false,
        unlaunchable: None,
    }
}

fn record(log: &DecisionLog, task: &str, decision: &Decision, outcome: &str) {
    log.record(&Entry {
        task,
        dir: Path::new("/tmp"),
        requested: "auto",
        decision,
        dry_run: false,
        job_id: None,
        job_name: None,
        outcome,
        effective_effort: None,
    })
    .expect("records");
}

/// A real production launch failure, produced by driving a dispatch off an environment that
/// resolves nothing. Built rather than hand-written so the persisted string is the one the router
/// actually emits, not one this test invented.
///
/// The environment itself is the shared, drift-proof fixture in `tests/common`; see its doc
/// comment for why the empty system fallback list is load-bearing.
fn real_launch_failure(root: &Path) -> Result<Dispatch> {
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).expect("create the fixture working directory");
    let environment = common::stripped_environment(Some(root));
    // Claude rather than codex: the claude dispatch path is not OS-gated, so the persisted string
    // under test is a real production one on every target this crate builds for.
    agent_router_core::dispatch::claude::dispatch_in(
        &environment,
        &cwd,
        "audit the airtable records",
        "Fixture Job",
        None,
        None,
        &[] as &[std::path::PathBuf],
        false,
    )
}

/// Plan test #23, and the only thing that substantiates AC4's "persisted decision row".
///
/// Three rows, read back through SQL:
///
/// * a classifier that RAN and failed — `gates` carries `classifier_failed` and NOT
///   `classifier_unlaunchable`;
/// * a classifier that could not LAUNCH — `gates` carries both, which is the whole discriminator;
/// * a dispatch whose CLI could not launch — `outcome` reads `error: launch failed: …`, and still
///   starts with `error: `, so `stats.rs` keeps counting it as a dispatch failure rather than
///   losing it out of the failure rate.
#[test]
fn a_launch_failure_and_a_classifier_failure_are_distinguishable_in_the_persisted_row() {
    let root = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&root.path().join("router.db")).expect("opens");
    let config = Config::default();

    let ran = decide(ran_and_failed(), usage(), NOW, &config);
    record(&log, "classifier ran and failed", &ran, "dispatched");

    let unlaunchable = decide(could_not_launch(), usage(), NOW, &config);
    let launch_root = root.path().join("launch");
    std::fs::create_dir_all(&launch_root).expect("create the launch fixture root");
    let dispatched = real_launch_failure(&launch_root);
    let (_, _, _, outcome) = recorded_fields(&dispatched);
    record(&log, "classifier could not launch", &unlaunchable, &outcome);

    let healthy = decide(scored(), usage(), NOW, &config);
    record(&log, "ordinary work", &healthy, "dispatched");

    // Newest first.
    let rows = log.recent(10).expect("reads rows");
    let ordinary_row = &rows[0];
    let launch_row = &rows[1];
    let ran_row = &rows[2];

    // A classifier that ran and failed carries one tag, not two.
    assert!(
        ran_row
            .gates
            .split(',')
            .any(|tag| tag == "classifier_failed"),
        "a failed classifier is still tagged as one: {}",
        ran_row.gates
    );
    assert!(
        !ran_row
            .gates
            .split(',')
            .any(|tag| tag == "classifier_unlaunchable"),
        "a CLI that ran was not unlaunchable, and the row must not say it was: {}",
        ran_row.gates
    );

    // A classifier that could not launch carries both, which is the discriminator itself.
    for tag in ["classifier_failed", "classifier_unlaunchable"] {
        assert!(
            launch_row.gates.split(',').any(|found| found == tag),
            "the persisted row must carry {tag}: {}",
            launch_row.gates
        );
    }

    // The dispatch outcome. Both halves matter: the prefix keeps the row counted as a dispatch
    // failure, and the `launch failed:` body is what tells an operator the CLI was never found
    // rather than that the job ran and broke.
    assert!(
        launch_row.outcome.starts_with(DISPATCH_ERROR_PREFIX),
        "stats counts dispatch failures by this prefix, so it is untouchable: {}",
        launch_row.outcome
    );
    assert!(
        launch_row.outcome.starts_with("error: launch failed: "),
        "a launch failure must be readable as one straight out of the column: {}",
        launch_row.outcome
    );
    assert!(
        !launch_row.outcome.contains("os error 2"),
        "the string 13 lost production rows recorded must not survive: {}",
        launch_row.outcome
    );

    // An ordinary row is unaffected: neither tag, and a normal outcome.
    assert_eq!(ordinary_row.outcome, "dispatched");
    assert!(
        !ordinary_row
            .gates
            .split(',')
            .any(|tag| tag == "classifier_unlaunchable"),
        "a scored row carries no launch evidence: {}",
        ordinary_row.gates
    );
}
