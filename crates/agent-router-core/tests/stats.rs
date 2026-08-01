//! Feature 1 at the core level: the report `agent-router stats` prints, over a window of decision
//! log rows.
//!
//! Every assertion here drives the public API, so each one is something a caller can make:
//! `summarize` over a row set a reader can count by eye, `collect` and `stats_rows` over a real
//! database, and `parse_since` over the window strings the CLI accepts.

use agent_router_core::log::{DecisionLog, StatsRow};
use agent_router_core::stats::{Window, collect, parse_since, summarize};
use std::collections::BTreeMap;
use std::path::Path;

/// One row as the stats reader sees it: only the columns a metric is derived from.
fn row(
    created_at_ms: i64,
    requested: &str,
    provider: &str,
    complexity: Option<&str>,
    gates: &str,
    dry_run: bool,
) -> StatsRow {
    StatsRow {
        created_at_ms,
        requested: requested.to_string(),
        provider: provider.to_string(),
        complexity: complexity.map(str::to_string),
        gates: gates.to_string(),
        dry_run,
    }
}

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(name, count)| ((*name).to_string(), *count))
        .collect()
}

/// A share is a float, so it is compared within a tolerance rather than for bit equality.
fn assert_share(share: Option<f64>, want: f64, label: &str) {
    let value = share.unwrap_or_else(|| panic!("{label} must carry a share"));
    assert!(
        (value - want).abs() < 1e-12,
        "{label} share is {value}, want {want}"
    );
}

/// One row to write straight into the table: created at, requested, provider, complexity, gates,
/// dry run, in the order the INSERT below binds them.
type Seed<'a> = (i64, &'a str, &'a str, Option<&'a str>, &'a str, bool);

/// Seed rows with chosen timestamps. `record` stamps `now_ms()` itself, so a since floor is only
/// testable by writing the column directly, exactly as the migration fixture in `log.rs` does.
fn seed(path: &Path, rows: &[Seed]) {
    let conn = rusqlite::Connection::open(path).expect("open the seeded log");
    for (created_at_ms, requested, provider, complexity, gates, dry_run) in rows {
        conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, complexity, gates, rationale,
                claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                codex_weekly_pct, codex_weekly_reset, dry_run, outcome
            ) VALUES (
                ?1, 'seeded task', '/tmp', ?2, ?3, ?4, ?5, 'why', 0, 0, 0, 0, 0, 0, 0, 0, ?6,
                'dry-run'
            )",
            rusqlite::params![
                created_at_ms,
                requested,
                provider,
                complexity,
                gates,
                dry_run
            ],
        )
        .expect("seed a decision row");
    }
}

/// The seeded window every database backed test below reads: five rows, four of them auto routed,
/// two of them dry runs, with timestamps a floor can be placed between.
const SEEDED: [Seed; 5] = [
    (1_000, "auto", "codex", Some("low"), "", false),
    (
        2_000,
        "auto",
        "claude",
        Some("high"),
        "headroom_tiebreak",
        false,
    ),
    (3_000, "claude", "claude", None, "explicit_provider", true),
    (4_000, "auto", "codex", None, "classifier_failed", false),
    (5_000, "auto", "codex", Some("ultra"), "", true),
];

/// Eight rows whose every metric can be counted by eye, newest first exactly as the query hands
/// them over. Four routes to codex, three to claude, one to opencode; six of the eight are auto
/// routed; two of those six moved off the provider their verdict named; one of them could not be
/// classified; three of all eight were dry runs.
#[test]
fn the_metric_matrix_over_a_hand_countable_row_set() {
    let rows = vec![
        row(
            8_000,
            "opencode",
            "opencode",
            None,
            "explicit_provider",
            true,
        ),
        row(7_000, "claude", "claude", None, "explicit_provider", false),
        row(6_000, "auto", "codex", Some("low"), "over_ceiling", true),
        row(5_000, "auto", "codex", None, "classifier_failed", false),
        row(
            4_000,
            "auto",
            "claude",
            Some("ultra"),
            "headroom_tiebreak",
            false,
        ),
        row(
            3_000,
            "auto",
            "claude",
            Some("high"),
            "claude_signals",
            true,
        ),
        row(
            2_000,
            "auto",
            "codex",
            Some("medium"),
            "flipped_on_exhaustion",
            false,
        ),
        row(1_000, "auto", "codex", Some("low"), "", false),
    ];

    let stats = summarize(&rows);

    assert_eq!(stats.rows_considered, 8);
    assert_eq!(stats.newest_created_at_ms, Some(8_000));
    assert_eq!(stats.oldest_created_at_ms, Some(1_000));
    assert_eq!(
        stats.routes,
        counts(&[("claude", 3), ("codex", 4), ("opencode", 1)])
    );
    assert_eq!(
        stats.gates,
        counts(&[
            ("claude_signals", 1),
            ("classifier_failed", 1),
            ("explicit_provider", 2),
            ("flipped_on_exhaustion", 1),
            ("headroom_tiebreak", 1),
            ("over_ceiling", 1),
        ])
    );
    // A row with no complexity is unscored rather than absent, so the distribution still sums to
    // the row count and a reader can see how much of the window was never scored.
    assert_eq!(
        stats.complexity,
        counts(&[
            ("high", 1),
            ("low", 2),
            ("medium", 1),
            ("ultra", 1),
            ("unscored", 3),
        ])
    );
    assert_eq!(stats.auto_routes, 6);

    assert_eq!(
        (stats.flip_rate.numerator, stats.flip_rate.denominator),
        (2, 6)
    );
    assert_eq!(
        (
            stats.classifier_failure_rate.numerator,
            stats.classifier_failure_rate.denominator
        ),
        (1, 6)
    );
    assert_eq!(
        (
            stats.dry_run_share.numerator,
            stats.dry_run_share.denominator
        ),
        (3, 8)
    );
    assert_share(stats.flip_rate.share(), 2.0 / 6.0, "flip_rate");
    assert_share(
        stats.classifier_failure_rate.share(),
        1.0 / 6.0,
        "classifier_failure_rate",
    );
    assert_share(stats.dry_run_share.share(), 3.0 / 8.0, "dry_run_share");
}

#[test]
fn an_empty_window_reports_zero_counts_and_no_rates() {
    let stats = summarize(&[]);

    assert_eq!(stats.rows_considered, 0);
    assert_eq!(stats.oldest_created_at_ms, None);
    assert_eq!(stats.newest_created_at_ms, None);
    assert!(stats.routes.is_empty());
    assert!(stats.gates.is_empty());
    assert!(stats.complexity.is_empty());
    assert_eq!(stats.auto_routes, 0);

    // A rate with no denominator has no answer. None is the only honest one: 0.0 reads as
    // "nothing ever flipped", and NaN escapes into whatever formats it.
    for (label, rate) in [
        ("flip_rate", &stats.flip_rate),
        ("classifier_failure_rate", &stats.classifier_failure_rate),
        ("dry_run_share", &stats.dry_run_share),
    ] {
        assert_eq!(rate.numerator, 0, "{label} numerator");
        assert_eq!(rate.denominator, 0, "{label} denominator");
        assert_eq!(rate.share(), None, "{label} share");
    }
}

/// A row that named its provider never had a verdict to flip and never ran the classifier, so
/// counting it in either denominator dilutes both rates with rows that could not have contributed.
/// Denominating on `rows_considered` instead reads 1 in 6 here rather than 1 in 2.
#[test]
fn rates_are_denominated_on_auto_routes_only() {
    let rows = vec![
        row(6_000, "claude", "claude", None, "explicit_provider", false),
        row(5_000, "codex", "codex", None, "explicit_provider", false),
        row(
            4_000,
            "opencode",
            "opencode",
            None,
            "explicit_provider",
            false,
        ),
        row(3_000, "claude", "claude", None, "explicit_provider", true),
        row(
            2_000,
            "auto",
            "claude",
            Some("high"),
            "flipped_on_exhaustion",
            false,
        ),
        row(1_000, "auto", "codex", None, "classifier_failed", false),
    ];

    let stats = summarize(&rows);

    assert_eq!(stats.rows_considered, 6);
    assert_eq!(stats.auto_routes, 2);
    assert_eq!(
        (stats.flip_rate.numerator, stats.flip_rate.denominator),
        (1, 2)
    );
    assert_eq!(
        (
            stats.classifier_failure_rate.numerator,
            stats.classifier_failure_rate.denominator
        ),
        (1, 2)
    );
    // The dry run share is the one rate over every row, explicit routes included: any row can be
    // a dry run.
    assert_eq!(
        (
            stats.dry_run_share.numerator,
            stats.dry_run_share.denominator
        ),
        (1, 6)
    );
}

/// The flip rate counts routes that moved, not tags that fired. A row carrying two provider moving
/// gates is one flipped route: the task moved once. `five_hour_pacing` moves a task off the
/// provider its verdict named exactly as the other two do, so a paced route is a flipped route;
/// leaving it out of the numerator shrinks the measured flip rate the moment the rule starts
/// firing. Eight flip tags fire across these six rows and five routes moved, so dropping any tag
/// from the numerator's gate list fails this, and so does counting tags instead of rows.
#[test]
fn every_provider_moving_gate_counts_toward_the_flip_rate() {
    let rows = vec![
        row(6_000, "auto", "codex", Some("high"), "over_ceiling", false),
        row(
            5_000,
            "auto",
            "codex",
            Some("high"),
            "headroom_tiebreak,flipped_on_exhaustion",
            false,
        ),
        row(
            4_000,
            "auto",
            "claude",
            Some("high"),
            "headroom_tiebreak",
            false,
        ),
        row(
            3_000,
            "auto",
            "claude",
            Some("high"),
            "flipped_on_exhaustion",
            false,
        ),
        row(
            2_000,
            "auto",
            "codex",
            Some("high"),
            "five_hour_pacing",
            false,
        ),
        // The reachable double fire the engine produces: a headroom tiebreak to claude that the
        // pacing rule sends straight back to codex. One route, two provider moving tags.
        row(
            1_000,
            "auto",
            "codex",
            Some("high"),
            "headroom_tiebreak,five_hour_pacing",
            false,
        ),
    ];

    let stats = summarize(&rows);

    assert_eq!(
        (stats.flip_rate.numerator, stats.flip_rate.denominator),
        (5, 6)
    );
    assert_eq!(
        stats.gates,
        counts(&[
            ("five_hour_pacing", 2),
            ("flipped_on_exhaustion", 2),
            ("headroom_tiebreak", 3),
            ("over_ceiling", 1),
        ])
    );
}

/// The gates column is a comma joined string, so counting a tag by substring (a SQL `LIKE '%tag%'`,
/// or a `contains` in Rust) reads a tag that is a prefix of another as present on both rows. No two
/// shipped tags overlap today, which is exactly why the hazard is pinned before one does.
#[test]
fn a_gate_tag_that_is_a_substring_of_another_is_counted_separately() {
    let rows = vec![
        row(4_000, "auto", "codex", Some("high"), "", false),
        row(
            3_000,
            "auto",
            "claude",
            Some("high"),
            "headroom_tiebreak,headroom_tiebreak_wide",
            false,
        ),
        row(
            2_000,
            "auto",
            "codex",
            Some("high"),
            "headroom_tiebreak_wide",
            false,
        ),
        row(
            1_000,
            "auto",
            "claude",
            Some("high"),
            "headroom_tiebreak",
            false,
        ),
    ];

    let stats = summarize(&rows);

    assert_eq!(
        stats.gates,
        counts(&[("headroom_tiebreak", 2), ("headroom_tiebreak_wide", 2)])
    );
    // An empty gates string is no gate at all, not a tag whose name is empty.
    assert!(!stats.gates.contains_key(""));
    // The same hazard in the flip numerator: `headroom_tiebreak_wide` is not a provider moving
    // gate, so the row carrying only it did not flip.
    assert_eq!(
        (stats.flip_rate.numerator, stats.flip_rate.denominator),
        (2, 4)
    );
}

#[test]
fn since_windows_parse_and_reject() {
    let hour = 60 * 60 * 1_000;
    let day = 24 * hour;
    for (value, want) in [("7d", 7 * day), ("24h", 24 * hour), ("2w", 14 * day)] {
        let got = parse_since(value).unwrap_or_else(|error| panic!("{value} must parse: {error}"));
        assert_eq!(got, want, "lookback for {value}");
    }

    // A bare number names no unit, a leading unit is not the grammar, a negative lookback would put
    // the floor in the future, and an empty window names nothing at all. Each must be an error
    // rather than a silently different window.
    for value in ["7", "d7", "-1d", ""] {
        assert!(
            parse_since(value).is_err(),
            "{value:?} must be rejected rather than parsed"
        );
    }
}

#[test]
fn stats_rows_honours_the_limit_and_the_since_floor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    DecisionLog::open_at(&path).expect("create the schema");
    seed(&path, &SEEDED);
    let log = DecisionLog::open_at(&path).expect("reopen the seeded log");

    let stamps = |rows: &[StatsRow]| rows.iter().map(|row| row.created_at_ms).collect::<Vec<_>>();

    let newest = log.stats_rows(2, None).expect("the two newest rows");
    assert_eq!(stamps(&newest), vec![5_000, 4_000]);

    let since = log
        .stats_rows(10, Some(3_000))
        .expect("the rows at or after the floor");
    assert_eq!(stamps(&since), vec![5_000, 4_000, 3_000]);

    // The two compose: the floor filters first, then the limit caps what is left.
    let both = log
        .stats_rows(2, Some(2_000))
        .expect("the floor and the limit together");
    assert_eq!(stamps(&both), vec![5_000, 4_000]);

    // Every column a metric is derived from survives the round trip.
    let explicit = &since[2];
    assert_eq!(explicit.requested, "claude");
    assert_eq!(explicit.provider, "claude");
    assert_eq!(explicit.complexity, None);
    assert_eq!(explicit.gates, "explicit_provider");
    assert!(explicit.dry_run);
}

#[test]
fn collect_summarizes_exactly_the_window_it_was_asked_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    DecisionLog::open_at(&path).expect("create the schema");
    seed(&path, &SEEDED);
    let log = DecisionLog::open_at(&path).expect("reopen the seeded log");

    let all = collect(
        &log,
        Window {
            limit: 200,
            since_ms: None,
        },
    )
    .expect("summarize the whole log");
    assert_eq!(all.rows_considered, 5);
    assert_eq!(all.auto_routes, 4);
    assert_eq!(
        (all.dry_run_share.numerator, all.dry_run_share.denominator),
        (2, 5)
    );
    assert_eq!(all.oldest_created_at_ms, Some(1_000));
    assert_eq!(all.newest_created_at_ms, Some(5_000));

    let capped = collect(
        &log,
        Window {
            limit: 2,
            since_ms: None,
        },
    )
    .expect("summarize the newest two");
    assert_eq!(capped.rows_considered, 2);
    assert_eq!(capped.oldest_created_at_ms, Some(4_000));
    assert_eq!(capped.newest_created_at_ms, Some(5_000));

    let recent = collect(
        &log,
        Window {
            limit: 200,
            since_ms: Some(4_000),
        },
    )
    .expect("summarize since the floor");
    assert_eq!(recent.rows_considered, 2);
    assert_eq!(recent.auto_routes, 2);
    assert_eq!(
        (
            recent.dry_run_share.numerator,
            recent.dry_run_share.denominator
        ),
        (1, 2)
    );
}
