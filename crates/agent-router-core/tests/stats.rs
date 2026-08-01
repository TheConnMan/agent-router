//! Feature 1 at the core level: the report `agent-router stats` prints, over a window of decision
//! log rows.
//!
//! Every assertion here drives the public API, so each one is something a caller can make:
//! `summarize` over a row set a reader can count by eye, `collect` and `stats_rows` over a real
//! database, and `parse_since` over the window strings the CLI accepts.

use agent_router_core::log::{DecisionLog, StatsRow};
use agent_router_core::stats::{Rate, Window, collect, parse_since, summarize};
use std::collections::BTreeMap;
use std::path::Path;

/// One row as the stats reader sees it: only the columns a metric is derived from. Nobody has
/// judged it and no reconciliation has settled it, which is what every rate test below the
/// feedback ones is about; `judged` is the constructor for a row that carries either.
fn row(
    created_at_ms: i64,
    requested: &str,
    provider: &str,
    complexity: Option<&str>,
    gates: &str,
    dry_run: bool,
) -> StatsRow {
    let outcome = if dry_run { "dry-run" } else { "dispatched" };
    judged(
        created_at_ms,
        requested,
        provider,
        complexity,
        gates,
        dry_run,
        None,
        outcome,
    )
}

/// One row carrying the two feedback columns: the human mark, and the outcome reconciliation
/// settled it to.
#[allow(clippy::too_many_arguments)]
fn judged(
    created_at_ms: i64,
    requested: &str,
    provider: &str,
    complexity: Option<&str>,
    gates: &str,
    dry_run: bool,
    mark: Option<&str>,
    outcome: &str,
) -> StatsRow {
    StatsRow {
        created_at_ms,
        requested: requested.to_string(),
        provider: provider.to_string(),
        complexity: complexity.map(str::to_string),
        gates: gates.to_string(),
        dry_run,
        mark: mark.map(str::to_string),
        outcome: outcome.to_string(),
    }
}

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(name, count)| ((*name).to_string(), *count))
        .collect()
}

/// One expected breakdown: key, numerator, denominator. Both counts are stated, never a share, so
/// the arithmetic is checked rather than the division.
fn rates(entries: &[(&str, usize, usize)]) -> BTreeMap<String, Rate> {
    entries
        .iter()
        .map(|(key, numerator, denominator)| {
            (
                (*key).to_string(),
                Rate {
                    numerator: *numerator,
                    denominator: *denominator,
                },
            )
        })
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
/// dry run, mark, outcome, in the order the INSERT below binds them.
type Seed<'a> = (
    i64,
    &'a str,
    &'a str,
    Option<&'a str>,
    &'a str,
    bool,
    Option<&'a str>,
    &'a str,
);

/// Seed rows with chosen timestamps. `record` stamps `now_ms()` itself, so a since floor is only
/// testable by writing the column directly, exactly as the migration fixture in `log.rs` does.
fn seed(path: &Path, rows: &[Seed]) {
    let conn = rusqlite::Connection::open(path).expect("open the seeded log");
    for (created_at_ms, requested, provider, complexity, gates, dry_run, mark, outcome) in rows {
        conn.execute(
            "INSERT INTO decisions (
                created_at_ms, task, dir, requested, provider, complexity, gates, rationale,
                claude_five_hour_pct, claude_five_hour_reset, claude_weekly_pct,
                claude_weekly_reset, codex_five_hour_pct, codex_five_hour_reset,
                codex_weekly_pct, codex_weekly_reset, dry_run, mark, outcome
            ) VALUES (
                ?1, 'seeded task', '/tmp', ?2, ?3, ?4, ?5, 'why', 0, 0, 0, 0, 0, 0, 0, 0, ?6,
                ?7, ?8
            )",
            rusqlite::params![
                created_at_ms,
                requested,
                provider,
                complexity,
                gates,
                dry_run,
                mark,
                outcome
            ],
        )
        .expect("seed a decision row");
    }
}

/// The seeded window every database backed test below reads: five rows, four of them auto routed,
/// two of them dry runs, with timestamps a floor can be placed between. Three carry a human mark
/// and three carry an outcome a reconciliation settled, and the two sets deliberately overlap on
/// only one row.
const SEEDED: [Seed; 5] = [
    (
        1_000,
        "auto",
        "codex",
        Some("low"),
        "",
        false,
        None,
        "completed",
    ),
    (
        2_000,
        "auto",
        "claude",
        Some("high"),
        "headroom_tiebreak",
        false,
        Some("bad"),
        "failed",
    ),
    (
        3_000,
        "claude",
        "claude",
        None,
        "explicit_provider",
        true,
        None,
        "dry-run",
    ),
    (
        4_000,
        "auto",
        "codex",
        None,
        "classifier_failed",
        false,
        Some("good"),
        "unknown",
    ),
    (
        5_000,
        "auto",
        "codex",
        Some("ultra"),
        "",
        true,
        Some("bad"),
        "dry-run",
    ),
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
    // Nobody judged this row and nothing reconciled it, and both facts are read back as they were
    // written rather than as an invented default.
    assert_eq!(explicit.mark, None);
    assert_eq!(explicit.outcome, "dry-run");

    let judged = &since[1];
    assert_eq!(judged.mark, Some("good".to_string()));
    assert_eq!(judged.outcome, "unknown");
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

/// The feedback breakdowns over a window a reader can count by eye. Every numerator and every
/// denominator below is written as a literal, so the arithmetic can be rechecked against the eight
/// rows above it without running anything.
///
/// The rows, newest first, as `(provider, complexity, gates, dry run, mark, outcome)`:
///
/// 1. codex, medium, no gate, live, good, `error: dispatch refused`
/// 2. opencode, unscored, explicit_provider, DRY RUN, bad, dry-run
/// 3. claude, medium, no gate, live, UNMARKED, dispatched
/// 4. codex, unscored, classifier_failed, live, rerouted, unknown
/// 5. claude, high, over_ceiling, live, UNMARKED, failed
/// 6. claude, high, five_hour_pacing + over_ceiling, live, bad, completed
/// 7. codex, low, over_ceiling, live, good, completed
/// 8. codex, low, no gate, live, bad, failed
///
/// Six of the eight carry a mark, and three of those six are `bad`. Five of the eight are settled
/// (two completed, two failed, one dispatch error), and three of those five are failures. The two
/// populations overlap on four rows and neither is the row count, which is the whole point.
#[test]
fn bad_and_failure_rates_break_down_by_gate_provider_and_complexity() {
    let rows = vec![
        judged(
            8_000,
            "auto",
            "codex",
            Some("medium"),
            "",
            false,
            Some("good"),
            "error: dispatch refused",
        ),
        judged(
            7_000,
            "opencode",
            "opencode",
            None,
            "explicit_provider",
            true,
            Some("bad"),
            "dry-run",
        ),
        judged(
            6_000,
            "auto",
            "claude",
            Some("medium"),
            "",
            false,
            None,
            "dispatched",
        ),
        judged(
            5_000,
            "auto",
            "codex",
            None,
            "classifier_failed",
            false,
            Some("rerouted"),
            "unknown",
        ),
        judged(
            4_000,
            "auto",
            "claude",
            Some("high"),
            "over_ceiling",
            false,
            None,
            "failed",
        ),
        judged(
            3_000,
            "auto",
            "claude",
            Some("high"),
            "five_hour_pacing,over_ceiling",
            false,
            Some("bad"),
            "completed",
        ),
        judged(
            2_000,
            "auto",
            "codex",
            Some("low"),
            "over_ceiling",
            false,
            Some("good"),
            "completed",
        ),
        judged(
            1_000,
            "auto",
            "codex",
            Some("low"),
            "",
            false,
            Some("bad"),
            "failed",
        ),
    ];

    let stats = summarize(&rows);

    // A bad rate is denominated on the rows a human actually judged. claude routed three rows and
    // exactly one of them was judged, so its denominator is 1 rather than 3: an unjudged row is
    // absence of evidence, and counting it as good drives every bad rate toward zero as the log
    // grows.
    assert_eq!(
        stats.bad_rate_by_provider,
        rates(&[("claude", 1, 1), ("codex", 1, 4), ("opencode", 1, 1)])
    );
    assert_eq!(
        stats.bad_rate_by_complexity,
        rates(&[
            ("high", 1, 1),
            ("low", 1, 2),
            ("medium", 0, 1),
            ("unscored", 1, 2),
        ])
    );
    // A row carrying two tags counts in both gates, which is deliberately different from the flip
    // rate's "one route moved however many gates fired" rule: a flip is one event, a gate
    // breakdown is per gate by definition. The `five_hour_pacing` and `over_ceiling` row is the
    // one that pins it.
    assert_eq!(
        stats.bad_rate_by_gate,
        rates(&[
            ("classifier_failed", 0, 1),
            ("explicit_provider", 1, 1),
            ("five_hour_pacing", 1, 1),
            ("over_ceiling", 1, 2),
        ])
    );

    // A failure rate is denominated on the rows reconciliation settled. The `dispatched` row and
    // the `unknown` row are not in any denominator: neither has been shown to have succeeded, and
    // counting them as successes reports a perfect record over jobs nobody has heard back about.
    // The dry run is out too, on the flag rather than on its outcome, because it dispatched
    // nothing to succeed or fail.
    assert_eq!(
        stats.failure_rate_by_provider,
        rates(&[("claude", 1, 2), ("codex", 2, 3), ("opencode", 0, 0)])
    );
    assert_eq!(
        stats.failure_rate_by_complexity,
        rates(&[
            ("high", 1, 2),
            ("low", 1, 2),
            ("medium", 1, 1),
            ("unscored", 0, 0),
        ])
    );
    assert_eq!(
        stats.failure_rate_by_gate,
        rates(&[
            ("classifier_failed", 0, 0),
            ("explicit_provider", 0, 0),
            ("five_hour_pacing", 0, 1),
            ("over_ceiling", 1, 3),
        ])
    );

    // Every breakdown carries the same keys as the distribution it is a breakdown of, so the maps
    // reconcile against each other and a key with nothing judged yet is visible rather than
    // missing.
    assert_eq!(
        stats.routes,
        counts(&[("claude", 3), ("codex", 4), ("opencode", 1)])
    );
    assert_eq!(
        stats.complexity,
        counts(&[("high", 2), ("low", 2), ("medium", 2), ("unscored", 2)])
    );
    assert_eq!(
        stats.gates,
        counts(&[
            ("classifier_failed", 1),
            ("explicit_provider", 1),
            ("five_hour_pacing", 1),
            ("over_ceiling", 3),
        ])
    );
    for (label, keys) in [
        ("provider", (&stats.bad_rate_by_provider, &stats.routes)),
        ("gate", (&stats.bad_rate_by_gate, &stats.gates)),
        (
            "complexity",
            (&stats.bad_rate_by_complexity, &stats.complexity),
        ),
    ] {
        let (rate_map, distribution) = keys;
        assert_eq!(
            rate_map.keys().collect::<Vec<_>>(),
            distribution.keys().collect::<Vec<_>>(),
            "the bad rate by {label} must cover the whole distribution"
        );
    }
    for (label, keys) in [
        ("provider", (&stats.failure_rate_by_provider, &stats.routes)),
        ("gate", (&stats.failure_rate_by_gate, &stats.gates)),
        (
            "complexity",
            (&stats.failure_rate_by_complexity, &stats.complexity),
        ),
    ] {
        let (rate_map, distribution) = keys;
        assert_eq!(
            rate_map.keys().collect::<Vec<_>>(),
            distribution.keys().collect::<Vec<_>>(),
            "the failure rate by {label} must cover the whole distribution"
        );
    }

    // Three shares that are three different answers: one in four codex routes judged bad, a real
    // zero over one judged medium row, and no answer at all for the opencode row that dispatched
    // nothing. A zero and a null must not collapse into each other.
    assert_share(
        stats.bad_rate_by_provider["codex"].share(),
        0.25,
        "bad rate for codex",
    );
    assert_share(
        stats.bad_rate_by_complexity["medium"].share(),
        0.0,
        "bad rate for medium",
    );
    assert_eq!(stats.failure_rate_by_provider["opencode"].share(), None);
}

/// An unmarked row is not evidence of a good route. A window nobody has judged has no bad rate at
/// all, and every key reports null rather than the zero that reads as a perfect routing record.
/// The failure rates in the same window are unaffected, because the two denominators are counted
/// over different populations.
#[test]
fn an_unmarked_row_is_not_counted_as_good() {
    let rows = vec![
        judged(
            3_000,
            "auto",
            "claude",
            Some("high"),
            "over_ceiling",
            false,
            None,
            "completed",
        ),
        judged(
            2_000,
            "auto",
            "codex",
            Some("low"),
            "",
            false,
            None,
            "failed",
        ),
        judged(
            1_000,
            "auto",
            "codex",
            None,
            "classifier_failed",
            false,
            None,
            "completed",
        ),
    ];

    let stats = summarize(&rows);

    assert_eq!(
        stats.bad_rate_by_provider,
        rates(&[("claude", 0, 0), ("codex", 0, 0)])
    );
    assert_eq!(
        stats.bad_rate_by_complexity,
        rates(&[("high", 0, 0), ("low", 0, 0), ("unscored", 0, 0)])
    );
    assert_eq!(
        stats.bad_rate_by_gate,
        rates(&[("classifier_failed", 0, 0), ("over_ceiling", 0, 0)])
    );
    for (key, rate) in stats
        .bad_rate_by_provider
        .iter()
        .chain(&stats.bad_rate_by_complexity)
        .chain(&stats.bad_rate_by_gate)
    {
        assert_eq!(rate.share(), None, "{key} must report no bad rate at all");
    }

    // All three rows settled, so the failure rates are real numbers over the same window.
    assert_eq!(
        stats.failure_rate_by_provider,
        rates(&[("claude", 0, 1), ("codex", 1, 2)])
    );
}

/// A row nobody has heard back about has not been shown to have succeeded. `dispatched` is the
/// dispatch time value, `running` is a live job, and `unknown` is reconciliation reporting that it
/// could not tell, so none of the three belongs in a failure denominator. Every row here is
/// marked, so a bad rate that came out null would mean the two populations had been confused.
#[test]
fn an_unsettled_row_is_not_counted_as_a_success() {
    let rows = vec![
        judged(
            3_000,
            "auto",
            "claude",
            Some("high"),
            "over_ceiling",
            false,
            Some("good"),
            "dispatched",
        ),
        judged(
            2_000,
            "auto",
            "codex",
            Some("low"),
            "",
            false,
            Some("bad"),
            "running",
        ),
        judged(
            1_000,
            "auto",
            "codex",
            None,
            "classifier_failed",
            false,
            Some("good"),
            "unknown",
        ),
    ];

    let stats = summarize(&rows);

    assert_eq!(
        stats.failure_rate_by_provider,
        rates(&[("claude", 0, 0), ("codex", 0, 0)])
    );
    assert_eq!(
        stats.failure_rate_by_complexity,
        rates(&[("high", 0, 0), ("low", 0, 0), ("unscored", 0, 0)])
    );
    assert_eq!(
        stats.failure_rate_by_gate,
        rates(&[("classifier_failed", 0, 0), ("over_ceiling", 0, 0)])
    );
    for (key, rate) in stats
        .failure_rate_by_provider
        .iter()
        .chain(&stats.failure_rate_by_complexity)
        .chain(&stats.failure_rate_by_gate)
    {
        assert_eq!(
            rate.share(),
            None,
            "{key} must report no failure rate at all"
        );
    }

    assert_eq!(
        stats.bad_rate_by_provider,
        rates(&[("claude", 0, 1), ("codex", 1, 2)])
    );
}

/// A dispatch that errored is a failure the router observed itself, so it is in both the numerator
/// and the denominator. The outcome is the backend's own message behind an `error: ` prefix, and
/// treating it as unsettled would drop the one class of failure the router is certain about.
#[test]
fn a_dispatch_error_row_counts_as_a_failure() {
    let rows = vec![
        judged(
            2_000,
            "auto",
            "codex",
            Some("low"),
            "",
            false,
            None,
            "error: codex app-server refused the turn",
        ),
        judged(
            1_000,
            "auto",
            "codex",
            Some("low"),
            "",
            false,
            None,
            "completed",
        ),
    ];

    let stats = summarize(&rows);

    assert_eq!(stats.failure_rate_by_provider, rates(&[("codex", 1, 2)]));
    assert_eq!(stats.failure_rate_by_complexity, rates(&[("low", 1, 2)]));
    assert_share(
        stats.failure_rate_by_provider["codex"].share(),
        0.5,
        "failure rate for codex",
    );
}

/// A dry run dispatched nothing, so no outcome on one is evidence about a job. The exclusion is on
/// the flag rather than on the outcome string: the second row below carries a terminal outcome no
/// reconciliation could have written for it, and counting it would let a window of dry runs report
/// a failure rate over jobs that never existed.
#[test]
fn a_dry_run_row_never_enters_a_failure_rate_denominator() {
    let rows = vec![
        judged(
            3_000,
            "auto",
            "codex",
            Some("low"),
            "",
            true,
            None,
            "dry-run",
        ),
        judged(
            2_000,
            "auto",
            "codex",
            Some("low"),
            "",
            true,
            None,
            "failed",
        ),
        judged(
            1_000,
            "auto",
            "codex",
            Some("low"),
            "",
            false,
            None,
            "failed",
        ),
    ];

    let stats = summarize(&rows);

    assert_eq!(stats.failure_rate_by_provider, rates(&[("codex", 1, 1)]));
    assert_eq!(stats.failure_rate_by_complexity, rates(&[("low", 1, 1)]));
    // The dry run share still counts every row, which is what makes the exclusion above visible as
    // a choice rather than as rows going missing.
    assert_eq!(
        (
            stats.dry_run_share.numerator,
            stats.dry_run_share.denominator
        ),
        (2, 3)
    );
}

/// The gates column is a comma joined string, so a substring count reads a tag that is a prefix of
/// another as present on both rows. Both new gate breakdowns split on the comma exactly as the
/// existing distribution does, and an empty column is no gate rather than a tag whose name is
/// empty.
#[test]
fn a_gate_tag_that_is_a_prefix_of_another_gets_its_own_rate() {
    let rows = vec![
        judged(
            4_000,
            "auto",
            "codex",
            Some("high"),
            "over_ceiling,over_ceiling_wide",
            false,
            Some("bad"),
            "failed",
        ),
        judged(
            3_000,
            "auto",
            "codex",
            Some("high"),
            "over_ceiling_wide",
            false,
            Some("good"),
            "completed",
        ),
        judged(
            2_000,
            "auto",
            "claude",
            Some("high"),
            "over_ceiling",
            false,
            Some("good"),
            "completed",
        ),
        judged(
            1_000,
            "auto",
            "codex",
            Some("high"),
            "",
            false,
            Some("bad"),
            "failed",
        ),
    ];

    let stats = summarize(&rows);

    // Two rows carry each tag, one judged bad and one judged good, and the row carrying both is
    // counted under both. A prefix match would read three rows under `over_ceiling`.
    assert_eq!(
        stats.bad_rate_by_gate,
        rates(&[("over_ceiling", 1, 2), ("over_ceiling_wide", 1, 2)])
    );
    assert_eq!(
        stats.failure_rate_by_gate,
        rates(&[("over_ceiling", 1, 2), ("over_ceiling_wide", 1, 2)])
    );
    assert!(!stats.bad_rate_by_gate.contains_key(""));
    assert!(!stats.failure_rate_by_gate.contains_key(""));
    // The ungated row is still in the provider and complexity breakdowns, so it was dropped from
    // the gate maps for want of a tag rather than lost.
    assert_eq!(
        stats.bad_rate_by_provider,
        rates(&[("claude", 0, 1), ("codex", 2, 3)])
    );
}

/// The two feedback columns come off the database rather than out of a struct a test built, which
/// is what proves `stats_rows` selects and reads them. The window is the five `SEEDED` rows: three
/// carry a mark, two of them `bad`, and three carry an outcome, one of them a failure.
#[test]
fn collect_reads_the_mark_and_the_outcome_from_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("router.db");
    DecisionLog::open_at(&path).expect("create the schema");
    seed(&path, &SEEDED);
    let log = DecisionLog::open_at(&path).expect("reopen the seeded log");

    let stats = collect(
        &log,
        Window {
            limit: 200,
            since_ms: None,
        },
    )
    .expect("summarize the whole log");

    // claude routed two rows and one of them is marked, so its bad rate is 1 of 1 rather than 1 of
    // 2. codex routed three, two of them marked, one of those bad.
    assert_eq!(
        stats.bad_rate_by_provider,
        rates(&[("claude", 1, 1), ("codex", 1, 2)])
    );
    // Of the five rows, two are dry runs and one reads `unknown`, so only two are settled: the
    // completed codex row and the failed claude row.
    assert_eq!(
        stats.failure_rate_by_provider,
        rates(&[("claude", 1, 1), ("codex", 0, 1)])
    );
    assert_eq!(
        stats.bad_rate_by_gate,
        rates(&[
            ("classifier_failed", 0, 1),
            ("explicit_provider", 0, 0),
            ("headroom_tiebreak", 1, 1),
        ])
    );
    assert_eq!(
        stats.bad_rate_by_complexity,
        rates(&[
            ("high", 1, 1),
            ("low", 0, 0),
            ("ultra", 1, 1),
            ("unscored", 0, 1),
        ])
    );
}
