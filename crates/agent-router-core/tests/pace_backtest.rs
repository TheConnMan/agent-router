//! The retroactive backtest: every real logged decision replayed through the new engine.
//!
//! The corpus is the 108 rows the router actually wrote, captured as a fixture. Nothing here reads
//! the live database: a backtest that depended on the operator state directory would answer differently
//! on every run and could not fail in CI.
//!
//! Each row carries both providers' weekly percent AND both resets at the instant it was decided,
//! which is what makes a run rate replay possible at all: the row is replayed at its own
//! `created_at_ms`, not at the wall clock.
//!
//! The number that matters is the 40 REAL dispatches (`dry_run = 0`), not the 100 auto routed rows.
//! A dry run drew nothing, so it is evidence about the classifier and not about spend. 39 of those
//! 40 carry both resets and can be replayed through the override.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::{Config, DefaultProvider};
use agent_router_core::decide::{Decision, Gate, decide};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use serde::Deserialize;

const FIXTURE: &str = include_str!("fixtures/decision_history.json");

/// The weekly window, in seconds, for the gap the band tests SELECT rows by. Nothing here asserts
/// this arithmetic: it picks which fixture rows a case is about, and `decide` is what is measured.
const WEEK_SECS: f64 = 604_800.0;

/// One logged decision, with the fields a replay needs. The fixture carries more (task excerpt,
/// model, rationale); serde drops what is not named here.
#[derive(Debug, Deserialize)]
struct HistoricalRow {
    id: i64,
    created_at_ms: i64,
    /// "auto", or the provider the caller named. A named provider never reached `decide`.
    requested: String,
    /// 1 when the decision was projected rather than dispatched. A dry run spent nothing.
    dry_run: i64,
    historical_provider: String,
    complexity: Option<String>,
    /// The six rubric booleans as "010000", or null on a row that was never classified.
    claude_signals: Option<String>,
    missing_connector: Option<i64>,
    historical_gates: Option<String>,
    claude_weekly_pct: f64,
    claude_weekly_reset: i64,
    codex_weekly_pct: f64,
    codex_weekly_reset: i64,
    claude_five_hour_pct: f64,
    codex_five_hour_pct: f64,
}

impl HistoricalRow {
    fn usage(&self) -> UsageSnapshot {
        UsageSnapshot {
            claude: Headroom {
                weekly_pct: self.claude_weekly_pct,
                weekly_reset_epoch: self.claude_weekly_reset,
                five_hour_pct: self.claude_five_hour_pct,
                ..Headroom::full()
            },
            codex: Headroom {
                weekly_pct: self.codex_weekly_pct,
                weekly_reset_epoch: self.codex_weekly_reset,
                five_hour_pct: self.codex_five_hour_pct,
                ..Headroom::full()
            },
        }
    }

    /// The row's decision instant, in seconds. The log records milliseconds; the reset epochs it
    /// records alongside them are seconds.
    fn now(&self) -> i64 {
        self.created_at_ms / 1000
    }

    fn dispatched(&self) -> bool {
        self.requested == "auto" && self.dry_run == 0
    }

    fn resets_known(&self) -> bool {
        self.claude_weekly_reset != 0 && self.codex_weekly_reset != 0
    }

    /// How far Codex is running ahead of Claude, in points of window. This SELECTS rows for the
    /// band cases below; it is never compared against anything the engine produced.
    fn gap(&self) -> f64 {
        let expected = |reset: i64| {
            (100.0 * (1.0 - (reset - self.now()) as f64 / WEEK_SECS)).clamp(0.0, 100.0)
        };
        (self.codex_weekly_pct - expected(self.codex_weekly_reset))
            - (self.claude_weekly_pct - expected(self.claude_weekly_reset))
    }

    fn classifier_failed(&self) -> bool {
        self.historical_gates
            .as_deref()
            .unwrap_or("")
            .split(',')
            .any(|tag| tag == "classifier_failed")
    }

    /// The recorded score, expressed in the new three field shape.
    ///
    /// Orchestration is claude signal 2, "dependent agents must exchange findings mid-run", which
    /// is the second of the six characters. It is the only one of the six the new engine keeps:
    /// the other five never justified a pin.
    fn classification(&self) -> Classification {
        if self.classifier_failed() {
            return Classification::fallback("replayed classifier failure", DefaultProvider::Codex);
        }
        let signals = self
            .claude_signals
            .as_deref()
            .expect("an auto row carries its rubric scores");
        Classification {
            orchestration: signals.as_bytes()[1] == b'1',
            missing_connector: self.missing_connector.unwrap_or(0) != 0,
            complexity: complexity(self.complexity.as_deref()),
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "replayed".to_string(),
            classifier_failed: false,
        }
    }

    fn replay(&self, config: &Config) -> Decision {
        decide(self.classification(), self.usage(), self.now(), config)
    }
}

/// A row written before complexity scaling has none, and an unscored task runs at the high tier.
///
/// Rows 32 to 40 carry "trivial" and "hard", the tier names an earlier build of the rubric emitted
/// before the ladder settled on low/medium/high/ultra. They are mapped onto the nearest current
/// tier rather than panicking. All 8 are dry runs, so they reach only the printed corpus wide
/// report below and no asserted count: nothing in this file selects them.
fn complexity(tag: Option<&str>) -> Complexity {
    match tag {
        Some("trivial") | Some("low") => Complexity::Low,
        Some("medium") => Complexity::Medium,
        Some("ultra") => Complexity::Ultra,
        None | Some("hard") | Some("high") => Complexity::High,
        Some(other) => panic!("unknown complexity {other} in the fixture"),
    }
}

fn corpus() -> Vec<HistoricalRow> {
    serde_json::from_str(FIXTURE).expect("parse the decision history fixture")
}

fn row(corpus: &[HistoricalRow], id: i64) -> &HistoricalRow {
    corpus
        .iter()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("row {id} is missing from the fixture"))
}

/// A row whose caller named a provider never reached `decide`, so it is not replayable: there is no
/// classification to replay and no usage rule ever ran on it. The two conditions are asserted to be
/// the same set, so a future fixture cannot quietly drop rows out of the replay by nulling a score.
#[test]
fn only_explicitly_routed_rows_are_outside_the_replay() {
    let corpus = corpus();
    assert_eq!(corpus.len(), 108, "the fixture is the full recorded corpus");

    let unscored: Vec<i64> = corpus
        .iter()
        .filter(|row| row.claude_signals.is_none())
        .map(|row| row.id)
        .collect();
    let explicit: Vec<i64> = corpus
        .iter()
        .filter(|row| row.requested != "auto")
        .map(|row| row.id)
        .collect();
    assert_eq!(unscored, explicit);
    assert_eq!(explicit, vec![9, 10, 11, 24, 26, 27, 105, 106]);
    assert_eq!(corpus.len() - explicit.len(), 100);
}

/// A row the classifier could not score still routes, and still says so. These are the rows the
/// pin can never fire on, because there is no score to pin against, so they are the rows most
/// exposed to the usage rules.
#[test]
fn every_classifier_failure_row_replays_as_a_failure_and_still_routes() {
    let config = Config::default();
    let corpus = corpus();
    let failures: Vec<&HistoricalRow> = corpus
        .iter()
        .filter(|row| row.requested == "auto" && row.classifier_failed())
        .collect();
    assert_eq!(failures.len(), 9, "the corpus records nine failed scores");

    for row in failures {
        let decision = row.replay(&config);
        assert!(
            decision.gates.contains(&Gate::ClassifierFailed),
            "row {} lost its classifier failure gate",
            row.id
        );
        assert!(matches!(
            decision.provider,
            Provider::Codex | Provider::Claude
        ));
    }
}

/// The blowout the override exists for. At rows 6, 7 and 8 Codex was 74 percent spent with 12
/// percent of its window gone, a 77 point gap, and it still held 26 points of allowance against
/// Claude's 50. Codex is below the ceiling on all three, so the ceiling cannot be what moved them.
///
/// Row 8 reaches Claude by a different road: it scored orchestration, so the pin takes it before
/// any usage rule runs. Asserting that separately is the point, because "row 8 is on Claude" is
/// true under both routes and only the gate vector distinguishes them.
#[test]
fn the_blowout_rows_route_to_claude() {
    let config = Config::default();
    let corpus = corpus();

    for id in [6, 7] {
        let row = row(&corpus, id);
        assert!(row.dispatched(), "row {id} is a real dispatch");
        assert!(
            row.codex_weekly_pct < config.hard_ceiling_pct,
            "row {id} must not be a ceiling case"
        );
        let decision = row.replay(&config);
        assert_eq!(decision.provider, Provider::Claude, "row {id}");
        assert!(
            decision.gates.contains(&Gate::PaceFlip),
            "row {id} must move on run rate, not on something else: {:?}",
            decision.gates
        );
    }

    let pinned = row(&corpus, 8);
    let decision = pinned.replay(&config);
    assert_eq!(decision.provider, Provider::Claude);
    assert!(
        pinned.classification().orchestration,
        "row 8 scored orchestration"
    );
    assert!(
        !decision.gates.contains(&Gate::PaceFlip),
        "the pin runs before the override, so it cannot also fire: {:?}",
        decision.gates
    );
}

/// Row 28 is the extreme: Codex 100 percent spent with 13 percent of its window elapsed, a 104
/// point gap, against a Claude with 57 points left. Both the ceiling and the override point the
/// same way here, so the override is deliberately switched off (by a gap wider than any real one)
/// to prove the ceiling catches it alone. A correct route for two reasons is only one guarantee if
/// exactly one of them is load bearing.
#[test]
fn the_fully_spent_codex_row_is_caught_by_the_ceiling_without_the_override() {
    let config = Config::default();
    let corpus = corpus();
    let row = row(&corpus, 28);
    assert!(row.codex_weekly_pct >= config.hard_ceiling_pct);

    let with_override = row.replay(&config);
    assert_eq!(with_override.provider, Provider::Claude);

    let override_disabled = Config {
        pace_flip_gap: 1000.0,
        ..Config::default()
    };
    let decision = row.replay(&override_disabled);
    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
    assert!(!decision.gates.contains(&Gate::PaceFlip));
}

/// The dead zone boundary, and the most valuable rows in the set. Rows 68, 69 and 76 sit at gaps of
/// 62 to 67, above the chronic band and below the threshold, so they must NOT move. They are what
/// fails if anyone tunes the threshold back down toward the chronic band.
#[test]
fn the_rows_just_under_the_threshold_stay_on_codex() {
    let config = Config::default();
    let corpus = corpus();

    for id in [68, 69, 76] {
        let row = row(&corpus, id);
        assert!(row.dispatched(), "row {id} is a real dispatch");
        let decision = row.replay(&config);
        assert_eq!(decision.provider, Provider::Codex, "row {id} must not move");
        assert!(
            !decision.gates.contains(&Gate::PaceFlip),
            "row {id}: {:?}",
            decision.gates
        );
    }
}

/// The dead zone itself, over every row that lands in it rather than a hand picked few. A gap of 43
/// to 58 is what two Claude 20x plans against one Codex 5x plan produce all week: the same work
/// shows as about four times the percentage on the smaller allowance, so it is a plan size artifact
/// and not a signal. Nothing in that band may move.
///
/// This is the assertion the whole redesign turns on. Under the first threshold the band WAS the
/// rule: it moved 27 of 39 real dispatches, 25 of them onto the provider with less absolute
/// allowance left.
#[test]
fn no_row_in_the_chronic_band_is_moved_by_the_override() {
    let config = Config::default();
    let corpus = corpus();
    let band: Vec<&HistoricalRow> = corpus
        .iter()
        .filter(|row| row.dispatched() && row.resets_known())
        .filter(|row| (43.0..=58.0).contains(&row.gap()))
        .collect();
    assert!(
        band.len() >= 15,
        "the chronic band is the bulk of the corpus, found {}",
        band.len()
    );

    for row in band {
        let decision = row.replay(&config);
        assert_eq!(
            decision.provider,
            Provider::Codex,
            "row {} at gap {:.1} is chronic, not a blowout",
            row.id,
            row.gap()
        );
        assert!(!decision.gates.contains(&Gate::PaceFlip), "row {}", row.id);
    }
}

/// Rows 85 to 88 by name, because the first version of this rule inverted them to Claude and that
/// was the wrong answer. Their gap is 50 to 51, squarely chronic. At those rows Codex held 29
/// points of allowance against Claude's 10 to 14, so moving them handed work to the emptier tank
/// while the percentages said the opposite.
#[test]
fn the_rows_the_first_threshold_wrongly_inverted_stay_on_codex() {
    let config = Config::default();
    let corpus = corpus();

    for id in [85, 86, 87, 88] {
        let row = row(&corpus, id);
        assert_eq!(
            row.historical_provider, "codex",
            "row {id} was recorded on codex"
        );
        assert!(
            row.claude_weekly_pct > row.codex_weekly_pct,
            "row {id}: Claude is the fuller tank by percentage, which is what misled the old rule"
        );
        let decision = row.replay(&config);
        assert_eq!(decision.provider, Provider::Codex, "row {id} must not move");
        assert!(!decision.gates.contains(&Gate::PaceFlip), "row {id}");
    }
}

/// The quiet band, right after a Codex reset: gaps of 6 to 9, nowhere near the threshold. Rows 89,
/// 91, 93 and 94 were historically routed to Claude by the retired two-signal pin, so they DO
/// invert, just in the direction that spends less.
///
/// Rows 93 and 94 land on Codex for a different reason than the other four, and the split is
/// asserted rather than left implicit. Claude sits at 95 percent on both, which is exactly the
/// reserve, so those two take the one-eligible-provider arm and the run rate gap is never consulted
/// at all. Neither arm records a gate, so the gate vector cannot tell them apart, which is why the
/// eligibility of each row is stated here directly.
#[test]
fn the_quiet_band_rows_route_to_codex() {
    let config = Config::default();
    let corpus = corpus();

    for id in [89, 90, 91, 92, 93, 94] {
        let historical = row(&corpus, id);
        let decision = historical.replay(&config);
        assert_eq!(decision.provider, Provider::Codex, "row {id}");
        assert!(
            !decision.gates.contains(&Gate::PaceFlip),
            "row {id}: {:?}",
            decision.gates
        );

        let claude_eligible = historical.claude_weekly_pct < config.hard_ceiling_pct;
        assert_eq!(
            claude_eligible,
            !matches!(id, 93 | 94),
            "row {id}: only 93 and 94 reach Codex by Claude being inside the reserve, at {} percent",
            historical.claude_weekly_pct
        );
    }
}

/// Row 94 is the shape that makes the ceiling load bearing: 95 percent used against 99 percent
/// elapsed is a NEGATIVE pace delta, so run rate reads Claude as running cold while exactly the
/// reserve remains. Replayed with Codex hot enough to clear the dead zone, the task must still not
/// land on a provider that is out of weekly budget.
///
/// The row is on the ceiling as recorded, so the assignment below changes nothing at the current
/// default and is kept for what it states: the case under test is a Claude sitting on the ceiling,
/// whatever the ceiling is set to. A build that raises the ceiling back above 95 gets the same
/// coverage from this test rather than silently losing it.
#[test]
fn a_cold_pace_delta_never_routes_into_a_provider_at_the_ceiling() {
    let config = Config::default();
    let corpus = corpus();
    let row = row(&corpus, 94);

    let as_recorded = row.replay(&config);
    assert_eq!(as_recorded.provider, Provider::Codex);

    let mut exhausted = row.usage();
    exhausted.claude.weekly_pct = config.hard_ceiling_pct;
    // Codex at 90 percent used with almost none of its window elapsed is about 90 points over
    // pace, a gap past the dead zone, against a Claude that reads as colder still.
    exhausted.codex.weekly_pct = 90.0;

    let decision = decide(row.classification(), exhausted, row.now(), &config);
    assert_eq!(decision.provider, Provider::Codex);
    assert!(!decision.gates.contains(&Gate::PaceFlip));
}

/// The one recorded dispatch whose Codex reset was never read, and the row the fail closed rule was
/// written for. Its Codex weekly number is the two zeroes an unread window produces, so the old
/// rule read it as a completely idle provider and kept the task there; now the provider that
/// reported nothing is ineligible and the task goes to the Claude that did report.
///
/// Replay agrees with what actually happened on this row: it was dispatched to Claude. That is
/// corroboration and not the assertion, since the historical route came from the retired
/// two-signal pin rather than from this rule.
#[test]
fn the_row_with_an_unread_reset_routes_to_the_provider_that_reported() {
    let config = Config::default();
    let corpus = corpus();
    let row = row(&corpus, 31);
    assert!(row.dispatched(), "row 31 is a real dispatch");
    assert_eq!(row.codex_weekly_reset, 0, "row 31 has no codex reset");
    assert_eq!(
        row.codex_weekly_pct, 0.0,
        "row 31's unread window reports as idle, which is the whole problem"
    );

    let decision = row.replay(&config);
    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::WeeklyUnknown));
    assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
    assert!(!decision.gates.contains(&Gate::PaceFlip));
    assert_eq!(row.historical_provider, "claude");
}

/// The threshold is a property of how this box is provisioned, not of the algorithm, so it has to
/// come from config on every decision. Rows 68, 69 and 76 sit between the two values, so the SAME
/// usage inputs must route differently under 60 and under 70. A `decide` that hardcoded either
/// number would answer identically for both and fail one half of this.
#[test]
fn the_flip_gap_is_read_from_config_and_never_hardcoded() {
    let corpus = corpus();
    let shipped = Config::default();
    let tighter = Config {
        pace_flip_gap: 60.0,
        ..Config::default()
    };

    for id in [68, 69, 76] {
        let row = row(&corpus, id);
        let held = row.replay(&shipped);
        assert_eq!(held.provider, Provider::Codex, "row {id} at gap 70");

        let moved = row.replay(&tighter);
        assert_eq!(moved.provider, Provider::Claude, "row {id} at gap 60");
        assert!(
            moved.gates.contains(&Gate::PaceFlip),
            "row {id} at gap 60: {:?}",
            moved.gates
        );
    }
}

/// The replay summary, over the 39 real dispatches whose resets were both read: 5 on
/// Claude and 34 on Codex, with the override itself firing exactly twice.
///
/// This one IS pinned to a golden number, unlike the corpus wide split below, because it is the
/// result the threshold was chosen to produce. The sweep behind it, on these same 39 rows: a gap of
/// 25 gives 30 Claude and 9 Codex with the override firing 27 times, which is not an override at
/// all; 60 gives 8 and 31; 70 gives 5 and 34; 80 gives 3 and 36 with the override never firing, so
/// the run rate signal would be dead config.
#[test]
fn the_real_dispatches_replay_to_five_claude_and_thirty_four_codex() {
    let config = Config::default();
    let corpus = corpus();
    let dispatched: Vec<&HistoricalRow> = corpus
        .iter()
        .filter(|row| row.dispatched() && row.resets_known())
        .collect();
    assert_eq!(dispatched.len(), 39);

    let (mut claude, mut codex) = (0, 0);
    let mut fired = Vec::new();
    for row in &dispatched {
        let decision = row.replay(&config);
        match decision.provider {
            Provider::Claude => claude += 1,
            Provider::Codex => codex += 1,
            Provider::Opencode => panic!("row {} routed to opencode", row.id),
        }
        if decision.gates.contains(&Gate::PaceFlip) {
            fired.push(row.id);
        }
    }

    assert_eq!((claude, codex), (5, 34));
    assert_eq!(fired, vec![6, 7], "the override is rare by construction");
}

/// The corpus wide shift, printed rather than asserted: a golden count over all 100 auto routed
/// rows would have to be re-typed every time a threshold is tuned, and most of those rows are dry
/// runs that spent nothing. Run with `--nocapture` to see it.
#[test]
fn the_corpus_replays_and_reports_its_before_and_after_split() {
    let config = Config::default();
    let corpus = corpus();

    let (mut was_claude, mut was_codex, mut now_claude, mut now_codex) = (0, 0, 0, 0);
    let (mut real_was_claude, mut real_now_claude, mut real_total) = (0, 0, 0);
    let (mut pace_flips, mut pace_unavailable, mut pinned) = (0, 0, 0);
    for row in corpus.iter().filter(|row| row.requested == "auto") {
        let decision = row.replay(&config);
        match row.historical_provider.as_str() {
            "claude" => was_claude += 1,
            "codex" => was_codex += 1,
            other => panic!("row {} recorded an unroutable provider {other}", row.id),
        }
        match decision.provider {
            Provider::Claude => now_claude += 1,
            Provider::Codex => now_codex += 1,
            Provider::Opencode => panic!("row {} routed to opencode", row.id),
        }
        if row.dry_run == 0 {
            real_total += 1;
            if row.historical_provider == "claude" {
                real_was_claude += 1;
            }
            if decision.provider == Provider::Claude {
                real_now_claude += 1;
            }
        }
        if decision.gates.contains(&Gate::PaceFlip) {
            pace_flips += 1;
        }
        if decision.gates.contains(&Gate::PaceUnavailable) {
            pace_unavailable += 1;
        }
        if decision.gates.contains(&Gate::MissingConnector) || row.classification().orchestration {
            pinned += 1;
        }
    }

    let total = was_claude + was_codex;
    assert_eq!(total, 100);
    println!("backtest over {total} auto routed decisions ({real_total} real dispatches)");
    println!(
        "  all rows before: claude {was_claude} ({:.0}%), codex {was_codex} ({:.0}%)",
        100.0 * f64::from(was_claude) / f64::from(total),
        100.0 * f64::from(was_codex) / f64::from(total),
    );
    println!(
        "  all rows after:  claude {now_claude} ({:.0}%), codex {now_codex} ({:.0}%)",
        100.0 * f64::from(now_claude) / f64::from(total),
        100.0 * f64::from(now_codex) / f64::from(total),
    );
    println!(
        "  real dispatches: claude {real_was_claude} ({:.0}%) before, {real_now_claude} ({:.0}%) after",
        100.0 * f64::from(real_was_claude) / f64::from(real_total),
        100.0 * f64::from(real_now_claude) / f64::from(real_total),
    );
    println!("  pace flips {pace_flips}, pace unavailable {pace_unavailable}, pinned {pinned}");
}
