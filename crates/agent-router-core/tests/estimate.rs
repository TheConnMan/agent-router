//! Feature 3 at the core level: the dry run projection, and the log query it reads.
//!
//! The plan puts these in an inline `mod tests` inside `estimate.rs`; they live here instead so
//! the projection is exercised through the public API a caller actually has, and so the test pass
//! touches no production source.

use agent_router_core::config::Config;
use agent_router_core::decide::decide_explicit;
use agent_router_core::estimate::{draws, median, project};
use agent_router_core::log::DecisionLog;
use agent_router_core::provider::Provider;
use agent_router_core::usage::UsageSnapshot;
use std::path::Path;

/// The model every comparable row in these fixtures ran on. Named here rather than read from the
/// config defaults, so a retiered catalogue cannot quietly change what "comparable" means.
const MODEL: &str = "gpt-5.6-sol";
/// A second tier on the same provider, which is a different sample key.
const OTHER_MODEL: &str = "gpt-5.6-tiny";

/// One row to write straight into the table: provider, model, claude weekly percent, codex weekly
/// percent, dry run, in the order the INSERT below binds them. `record` reads its percentages from
/// a live usage snapshot, so a chosen series is only writable by binding the columns directly,
/// exactly as the seeding fixture in `tests/stats.rs` does.
type Seed<'a> = (&'a str, Option<&'a str>, f64, f64, bool);

fn seed(path: &Path, rows: &[Seed]) {
    let conn = rusqlite::Connection::open(path).expect("open the seeded log");
    for (provider, model, claude_weekly_pct, codex_weekly_pct, dry_run) in rows {
        conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, model, gates, rationale,
                claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                codex_weekly_pct, codex_weekly_reset, dry_run, outcome
            ) VALUES (
                1000, 'seeded task', '/tmp', 'auto', ?1, ?2, '', 'why', 0, 0, ?3, 0, 0, 0, ?4, 0,
                ?5, 'dispatched'
            )",
            rusqlite::params![
                provider,
                model,
                claude_weekly_pct,
                codex_weekly_pct,
                dry_run
            ],
        )
        .expect("seed a decision row");
    }
}

/// A log holding exactly `rows`, in the order given.
fn seeded_log(directory: &tempfile::TempDir, rows: &[Seed]) -> DecisionLog {
    let path = directory.path().join("router.db");
    DecisionLog::open_at(&path).expect("creates the schema");
    seed(&path, rows);
    DecisionLog::open_at(&path).expect("reopens the seeded log")
}

/// A decision on the codex tier the fixtures record, which is the sample key the projection uses.
fn codex_decision(model: &str) -> agent_router_core::Decision {
    decide_explicit(
        Provider::Codex,
        Some(model.to_string()),
        UsageSnapshot::full(),
        &Config::default(),
    )
}

/// A step down is a weekly window that reset between the two rows, not a job that drew a negative
/// amount, so it is dropped. Folding it in as its magnitude would invent a draw out of a reset.
#[test]
fn draws_keep_positive_steps_and_drop_a_weekly_reset() {
    let series = [10.0, 12.5, 4.0, 9.0];
    assert_eq!(draws(&series), vec![2.5, 5.0]);
    assert!(
        !draws(&series).contains(&8.5),
        "the reset between 12.5 and 4.0 was absolute valued into a draw"
    );
    assert!(
        draws(&[7.0]).is_empty(),
        "a single observation has no step to measure"
    );
}

#[test]
fn the_median_of_an_even_sample_averages_the_two_middles() {
    // Unsorted on the way in, because the median is over a sorted copy rather than over the order
    // the rows happened to arrive in.
    assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    let empty: [f64; 0] = [];
    assert_eq!(
        median(&empty),
        None,
        "an empty sample has no middle, so it has no number"
    );
}

/// Two dispatched jobs on this provider and model is one observed gap, short of the three the
/// projection needs. It must say so rather than answer from what little it has.
#[test]
fn fewer_than_three_comparable_jobs_reports_no_number() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = seeded_log(
        &directory,
        &[
            ("codex", Some(MODEL), 0.0, 10.0, false),
            ("codex", Some(MODEL), 0.0, 20.0, false),
        ],
    );

    let estimate = project(&log, &codex_decision(MODEL)).expect("projects");
    assert_eq!(estimate.provider, "codex");
    assert_eq!(estimate.model.as_deref(), Some(MODEL));
    assert_eq!(
        estimate.weekly_pct, None,
        "a short sample must never carry a number: {estimate:?}"
    );
    assert_eq!(estimate.needed, 3, "the reported requirement: {estimate:?}");
    assert!(
        estimate.samples < estimate.needed,
        "a short sample must report why it is short: {estimate:?}"
    );
    assert!(
        estimate.samples > 0,
        "the comparable rows already in the log must be counted: {estimate:?}"
    );
}

/// The series is what the median is taken over, so anything that did not draw on this provider's
/// weekly window on this exact model has to be out of it.
#[test]
fn weekly_series_excludes_dry_runs_and_other_models() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = seeded_log(
        &directory,
        &[
            ("codex", Some(MODEL), 0.0, 10.0, false),
            // A dry run dispatched nothing, so a step across it is not a draw.
            ("codex", Some(MODEL), 0.0, 20.0, true),
            // Another provider's row, carrying a codex weekly percent of its own.
            ("claude", Some("opus[1m]"), 55.0, 77.0, false),
            // Another tier on the same provider, which is a different sample key.
            ("codex", Some(OTHER_MODEL), 0.0, 90.0, false),
            ("codex", Some(MODEL), 0.0, 30.0, false),
            ("codex", Some(MODEL), 0.0, 45.0, false),
        ],
    );

    assert_eq!(
        log.weekly_series("codex", Some(MODEL), 50).expect("reads"),
        vec![10.0, 30.0, 45.0],
        "oldest first, dry runs and other tiers dropped"
    );
    // The claude row carries both providers' percentages, so a query reading the wrong column
    // would answer with 77.0 here.
    assert_eq!(
        log.weekly_series("claude", Some("opus[1m]"), 50)
            .expect("reads"),
        vec![55.0],
        "each provider's series is read from its own weekly column"
    );
    assert!(
        log.weekly_series("codex", Some("a model no row ran on"), 50)
            .expect("reads")
            .is_empty(),
        "an unseen tier has no series rather than the provider's whole history"
    );
}
