//! The retroactive backtest: every captured decision replayed through the new engine.
//!
//! The corpus is 108 captured rows with synthetic task excerpts. Nothing here reads a live
//! database, so the backtest is deterministic and safe to publish.
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
                weekly_capacity_known: self.claude_weekly_reset != 0,
                five_hour_pct: self.claude_five_hour_pct,
                ..Headroom::full()
            },
            codex: Headroom {
                weekly_pct: self.codex_weekly_pct,
                weekly_reset_epoch: self.codex_weekly_reset,
                weekly_capacity_known: self.codex_weekly_reset != 0,
                five_hour_pct: self.codex_five_hour_pct,
                ..Headroom::full()
            },
            grok: Headroom::closed(),
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
            invokes_implement: false,
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
/// percent of its window gone, which projects to a 637 percent draw on a plan that holds 100,
/// against a Claude projecting 78 and finishing its week inside its allowance. Codex is below the
/// ceiling on all three, so the ceiling cannot be what moved them.
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
            decision.gates.contains(&Gate::ProjectedOverdraw),
            "row {id} must move on its projection, not on something else: {:?}",
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
        !decision.gates.contains(&Gate::ProjectedOverdraw),
        "the pin runs before the override, so it cannot also fire: {:?}",
        decision.gates
    );
}

/// Row 28 is the extreme: Codex 100 percent spent with 13 percent of its window elapsed. Both the
/// ceiling and the override point the same way here, so the override is deliberately switched off
/// (by a threshold no real projection reaches) to prove the ceiling catches it alone. A correct
/// route for two reasons is only one guarantee if exactly one of them is load bearing.
#[test]
fn the_fully_spent_codex_row_is_caught_by_the_ceiling_without_the_override() {
    let config = Config::default();
    let corpus = corpus();
    let row = row(&corpus, 28);
    assert!(row.codex_weekly_pct >= config.hard_ceiling_pct);

    let with_override = row.replay(&config);
    assert_eq!(with_override.provider, Provider::Claude);

    let override_disabled = Config {
        projection_overdraw_pct: 100_000.0,
        ..Config::default()
    };
    let decision = row.replay(&override_disabled);
    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
}

/// Rows the OLD rule's dead zone held on Codex and this one moves, which is the whole redesign in
/// one assertion. Rows 77 to 84 sit at run rate gaps inside the 43 to 58 point band that the
/// retired `pace_flip_gap` was widened to ignore as a plan size artifact. Their projections are
/// 399 to 452 percent of a Codex allowance against 92 to 99 on Claude: Codex was going to run out
/// with most of its week left while Claude was going to finish just inside its plan.
///
/// The band was never an artifact to be tolerated. It was a small plan being spent at a rate that
/// would empty it early, which is a fact about the box and not a distortion of the measurement, and
/// a rule tuned to look past it was tuned to look past the only thing worth acting on.
#[test]
fn the_rows_the_old_dead_zone_ignored_now_move_on_their_projection() {
    let config = Config::default();
    let corpus = corpus();

    for id in [77, 78, 79, 80, 81, 82, 83, 84] {
        let row = row(&corpus, id);
        assert!(row.dispatched(), "row {id} is a real dispatch");
        assert!(
            (43.0..=58.0).contains(&row.gap()),
            "row {id} sits in the old dead zone at gap {:.1}",
            row.gap()
        );
        let decision = row.replay(&config);
        assert_eq!(decision.provider, Provider::Claude, "row {id} must move");
        assert!(
            decision.gates.contains(&Gate::ProjectedOverdraw),
            "row {id}: {:?}",
            decision.gates
        );
    }
}

/// Rows 85 to 88, where BOTH providers project past their allowance: Codex at 393 to 401 percent
/// and Claude at 102 to 104. The task goes to Claude, because the question when neither is safe is
/// which one runs out later, and Claude finishing a fraction over its plan at the end of its week
/// is a different problem from Codex emptying a plan with five sixths of its window still to run.
///
/// An earlier rule held these on Codex, reasoning that Codex had 29 points of allowance left
/// against Claude's 10 to 14 and that moving them handed work to the emptier tank. That reasoning
/// is the plan size confusion in its purest form: a point of a 5x Codex plan and a point of a 20x
/// Claude plan are not the same quantity, so the two numbers were never comparable and the tank
/// with more points left was in fact the smaller one. A projection is comparable, because it is
/// each provider measured against its own plan and its own clock.
#[test]
fn the_rows_where_both_providers_overdraw_go_to_the_one_that_lasts_longer() {
    let config = Config::default();
    let corpus = corpus();

    for id in [85, 86, 87, 88] {
        let row = row(&corpus, id);
        assert_eq!(
            row.historical_provider, "codex",
            "row {id} was recorded on codex"
        );
        let decision = row.replay(&config);
        let codex = decision.codex_projected_draw.expect("codex projects");
        let claude = decision.claude_projected_draw.expect("claude projects");
        assert!(
            codex > config.projection_overdraw_pct && claude > config.projection_overdraw_pct,
            "row {id}: both providers must be overdrawing, got codex {codex:.0} claude {claude:.0}"
        );
        assert!(claude < codex, "row {id}: claude must be the lighter draw");
        assert_eq!(decision.provider, Provider::Claude, "row {id}");
        assert!(
            decision.gates.contains(&Gate::ProjectedOverdraw),
            "row {id}"
        );
    }
}

/// The quiet band, right after a Codex reset. Rows 89 to 94 were historically routed to Claude by
/// the retired two-signal pin, and they route to Codex now, but NOT because a projection said so:
/// Codex's window had barely started on each, so there is nothing to extrapolate and the override
/// declines to run. This is the guard rail earning its keep, not the rule choosing.
///
/// Claude remains eligible in every row at the 98 percent default ceiling, including rows 93 and
/// 94 at 95 percent. Codex therefore holds the configured default while the unavailable projection
/// rules out an override.
#[test]
fn the_quiet_band_rows_route_to_codex_with_nothing_to_project() {
    let config = Config::default();
    let corpus = corpus();

    for id in [89, 90, 91, 92, 93, 94] {
        let historical = row(&corpus, id);
        let decision = historical.replay(&config);
        assert_eq!(decision.provider, Provider::Codex, "row {id}");
        assert!(
            !decision.gates.contains(&Gate::ProjectedOverdraw),
            "row {id}: {:?}",
            decision.gates
        );
        assert_eq!(
            decision.codex_projected_draw, None,
            "row {id}: codex's window is too fresh to project across"
        );

        let claude_eligible = historical.claude_weekly_pct < config.hard_ceiling_pct;
        assert!(
            claude_eligible,
            "row {id}: Claude at {} percent remains eligible below the 98 percent default ceiling",
            historical.claude_weekly_pct
        );
        if matches!(id, 93 | 94) {
            assert!(
                decision.gates.contains(&Gate::ProjectionUnavailable),
                "row {id}: the unavailable Codex projection keeps the default route"
            );
        }
    }
}

/// Row 94 records Claude at 95 percent used against 99 percent elapsed, which projects to 96 and
/// remains below the 98 percent default ceiling. The mutation below puts Claude on the ceiling and
/// makes Codex overdraw, proving an otherwise healthy projection cannot route into an exhausted
/// provider.
#[test]
fn a_healthy_projection_never_routes_into_a_provider_at_the_ceiling() {
    let config = Config::default();
    let corpus = corpus();
    let row = row(&corpus, 94);

    let as_recorded = row.replay(&config);
    assert_eq!(as_recorded.provider, Provider::Codex);

    let mut exhausted = row.usage();
    exhausted.claude.weekly_pct = config.hard_ceiling_pct;
    // Codex at 90 percent used with almost none of its window elapsed projects far past its
    // allowance, against a Claude projecting inside its own.
    exhausted.codex.weekly_pct = 90.0;

    let decision = decide(row.classification(), exhausted, row.now(), &config);
    assert_eq!(decision.provider, Provider::Codex);
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
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
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
    assert!(!decision.gates.contains(&Gate::ProjectionUnavailable));
    assert_eq!(row.historical_provider, "claude");
}

/// The threshold has to come from config on every decision rather than be baked into `decide`.
/// Rows 85 to 88 project Claude at 102 to 104, so a threshold of 110 puts Claude back inside its
/// allowance and, since the destination must be an improvement on an overdrawing provider, the
/// SAME usage inputs must still move: what changes is that a threshold above every projection in
/// the corpus stops the override entirely. Both halves are asserted, because a `decide` that
/// hardcoded 100 would pass the first alone.
#[test]
fn the_overdraw_threshold_is_read_from_config_and_never_hardcoded() {
    let corpus = corpus();
    let shipped = Config::default();
    let unreachable = Config {
        projection_overdraw_pct: 100_000.0,
        ..Config::default()
    };

    for id in [85, 86, 87, 88] {
        let row = row(&corpus, id);
        let moved = row.replay(&shipped);
        assert_eq!(moved.provider, Provider::Claude, "row {id} at 100");
        assert!(moved.gates.contains(&Gate::ProjectedOverdraw), "row {id}");

        let held = row.replay(&unreachable);
        assert_eq!(
            held.provider,
            Provider::Codex,
            "row {id} at an unreachable threshold"
        );
        assert!(
            !held.gates.contains(&Gate::ProjectedOverdraw),
            "row {id}: {:?}",
            held.gates
        );
    }
}

/// What the rule does to the 39 real dispatches whose resets were both read, pinned as a golden
/// number because it is the outcome the redesign was for: 17 on Claude and 22 on Codex, with the
/// override itself firing 14 times.
///
/// The retired rule produced 5 Claude and 34 Codex on these same rows with its own override firing
/// twice, so the 12 rows that changed hands are exactly rows 77 to 88, every one of which had a
/// Codex projecting between 393 and 452 percent of its allowance. It declined to act on those
/// because their run rate gap fell inside a dead zone widened to tolerate the plan size mismatch.
/// The override is no longer rare, and that is the correction rather than a regression: a rule
/// that fires twice where the provider is heading off a cliff fourteen times is not a conservative
/// version of this one, it was not measuring the right thing.
///
/// The two rows that overdraw and do NOT count as override fires are asserted by exclusion, since
/// "14 fires" alone would pass if the wrong 14 fired: row 8 reaches Claude on the orchestration pin
/// and row 28 on the ceiling, both of which run before the override and neither of which may also
/// record it.
///
/// The blind spot this corpus cannot cover: it contains no row where a computable Codex projection
/// lands INSIDE its allowance, so "does not fire when the provider is fine" is proven in
/// `pace_routing.rs` against constructed inputs and not here. The assertion below fails if a future
/// corpus gains such a row, which is the signal to bring that coverage back to real data.
#[test]
fn the_real_dispatches_replay_to_seventeen_claude_and_twenty_two_codex() {
    let config = Config::default();
    let corpus = corpus();
    let dispatched: Vec<&HistoricalRow> = corpus
        .iter()
        .filter(|row| row.dispatched() && row.resets_known())
        .collect();
    assert_eq!(dispatched.len(), 39);

    let (mut claude, mut codex) = (0, 0);
    let mut fired = Vec::new();
    let mut inside_allowance = Vec::new();
    for row in &dispatched {
        let decision = row.replay(&config);
        match decision.provider {
            Provider::Claude => claude += 1,
            Provider::Codex => codex += 1,
            Provider::Grok => panic!("row {} routed to grok", row.id),
            Provider::Opencode => panic!("row {} routed to opencode", row.id),
        }
        if decision.gates.contains(&Gate::ProjectedOverdraw) {
            fired.push(row.id);
        }
        if decision
            .codex_projected_draw
            .is_some_and(|draw| draw <= config.projection_overdraw_pct)
        {
            inside_allowance.push(row.id);
        }
    }

    assert_eq!((claude, codex), (17, 22));
    assert_eq!(
        fired,
        vec![6, 7, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88],
        "the override must fire on the overdrawing rows and only those"
    );
    assert!(
        !fired.contains(&8) && !fired.contains(&28),
        "the pin and the ceiling run before the override and cannot also record it"
    );
    assert!(
        inside_allowance.is_empty(),
        "the corpus gained a healthy Codex projection at rows {inside_allowance:?}: this file can \
         now cover the does-not-fire case that pace_routing.rs carries alone"
    );
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
    let (mut overdraws, mut unavailable, mut pinned) = (0, 0, 0);
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
            Provider::Grok => panic!("row {} routed to grok", row.id),
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
        if decision.gates.contains(&Gate::ProjectedOverdraw) {
            overdraws += 1;
        }
        if decision.gates.contains(&Gate::ProjectionUnavailable) {
            unavailable += 1;
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
    println!(
        "  projected overdraws {overdraws}, projection unavailable {unavailable}, pins {pinned}"
    );
}
