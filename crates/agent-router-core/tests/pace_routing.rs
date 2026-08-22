//! Workhorse routing: automatic tasks balance between Codex and Grok from measured weekly
//! headroom, while Claude remains reserved for capability and context pins.
//!
//! The engine under test is pure, so `decide` takes the instant it is deciding at rather than
//! reading the clock: every case below fixes `NOW` and states each provider's reset as a distance
//! from it, which is what makes the arithmetic assertable at all.
//!
//! A smaller weekly percentage means more remaining weekly headroom. Unknown readings and the
//! final reserve below the hard ceiling are not eligible capacity, so neither can win a balance.

use agent_router_core::classify::{
    Classification, Complexity, TaskContextHorizon, parse_classification,
};
use agent_router_core::config::{Config, DefaultProvider};
use agent_router_core::decide::{Gate, decide};
use agent_router_core::{Headroom, Provider, UsageSnapshot};

/// The instant every case decides at. Any epoch works; this one is in the same range as the
/// recorded decisions, so a number that looks like a reset in the log reads like one here.
const NOW: i64 = 1_785_400_000;
/// Half the weekly window: a reset this far out means the window is exactly 50 percent elapsed.
const HALF_WEEK: i64 = 302_400;

/// One provider's window, stated as how long is left of its weekly window rather than as an
/// absolute reset, because the expected burn is a function of exactly that distance.
fn window(weekly_pct: f64, weekly_remaining_secs: i64, five_hour_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + weekly_remaining_secs,
        weekly_capacity_known: true,
        five_hour_pct,
        ..Headroom::full()
    }
}

/// A provider whose reset is not known. A reset epoch of 0 is the documented "not known" value,
/// and it is NOT the same input as a window that resets at this instant.
fn unknown_window(weekly_pct: f64, five_hour_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: 0,
        five_hour_pct,
        ..Headroom::full()
    }
}

/// Supply all three provider windows when a rule must distinguish the two workhorse providers.
fn usage_with_grok(claude: Headroom, codex: Headroom, grok: Headroom) -> UsageSnapshot {
    UsageSnapshot {
        claude,
        codex,
        grok,
    }
}

/// The whole classifier answer. The field list is exhaustive on purpose: a classifier that grows
/// a fifth scored field stops compiling here rather than quietly influencing a route.
fn scored(orchestration: bool, missing_connector: bool, complexity: Complexity) -> Classification {
    Classification {
        orchestration,
        missing_connector,
        complexity,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
        invokes_implement: false,
    }
}

/// A plain task: nothing pinned, so it is decided entirely by usage.
fn plain() -> Classification {
    scored(false, false, Complexity::High)
}

/// An `/implement` dispatch at the given tier. `invokes_implement` is read from the task text by
/// `classify`, never scored, so a routing test sets it directly.
fn implement(complexity: Complexity) -> Classification {
    Classification {
        invokes_implement: true,
        ..scored(false, false, complexity)
    }
}

// ------------------------------------------------------------------ rule 1: the scored fields

/// Rule 1. The classifier answers with exactly four scored fields, and an answer in the old
/// multi signal shape is a failed score rather than a partially understood one. This is the
/// parse level half of "no compatibility path": if `orchestration` were given a serde default, an
/// old shaped answer would parse as "no orchestration" and route silently on a field the model
/// never scored.
#[test]
fn the_classifier_answer_carries_the_four_scored_fields_and_nothing_else() {
    let scored = parse_classification(
        r#"{"orchestration":true,"missing_connector":false,"complexity":"ultra","task_context_horizon":"ordinary","rationale":"needs a council"}"#,
    )
    .expect("the new answer shape parses");
    assert!(scored.orchestration);
    assert!(!scored.missing_connector);
    assert_eq!(scored.complexity, Complexity::Ultra);
    assert_eq!(scored.task_context_horizon, TaskContextHorizon::Ordinary);
    assert!(!scored.classifier_failed);

    let old_shape = parse_classification(
        r#"{"codex_ready":[true,true,true,true,true,true],"claude_signals":[false,true,false,false,false,false],"missing_connector":false,"verdict":"claude","confidence":"high","complexity":"high","rationale":"stale shape"}"#,
    );
    assert!(
        old_shape.is_none(),
        "an answer without every required score must fail, not default"
    );
}

// ------------------------------------------------------------------ rule 2: the capability pin

/// Rule 2. Orchestration pins to Claude and bypasses every usage rule: Claude is over the hard
/// ceiling, its five hour window is exhausted, Codex is empty, Codex's reset is unknown, and Grok
/// has the most workhorse headroom. Each of those alone moves a plain task somewhere else, and
/// none of them may move this one.
#[test]
fn an_orchestration_task_pins_to_claude_past_every_usage_rule() {
    let config = Config::default();
    let decision = decide(
        scored(true, false, Complexity::High),
        usage_with_grok(
            window(99.0, HALF_WEEK, 100.0),
            unknown_window(0.0, 0.0),
            window(1.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
    assert!(decision.gates.contains(&Gate::Orchestration));
}

/// Rule 2, the other half of the pin. A task that cannot reach its connector on Codex is not a
/// cheaper job when paced there, it is a failed one.
#[test]
fn a_missing_connector_pins_to_claude_past_every_usage_rule() {
    let config = Config::default();
    let decision = decide(
        scored(false, true, Complexity::High),
        usage_with_grok(
            window(99.0, HALF_WEEK, 100.0),
            unknown_window(0.0, 0.0),
            window(1.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::MissingConnector));
}

// ------------------------------------------------- rule 3: the workhorse headroom comparison

/// Automatic work stays in the Codex/Grok workhorse pool. Once both report a usable weekly
/// window, the provider with more weekly headroom wins; equality deliberately preserves Codex as
/// the deterministic default. Claude is nearly exhausted in every case so these assertions fail
/// if its premium lane is accidentally reintroduced as a capacity competitor.
#[test]
fn workhorse_routing_uses_known_weekly_headroom_and_breaks_ties_to_codex() {
    let config = Config::default();
    let scenarios = [
        (60.0, 10.0, Provider::Grok, "Grok has more weekly headroom"),
        (
            10.0,
            60.0,
            Provider::Codex,
            "Codex has more weekly headroom",
        ),
        (
            10.0,
            10.0,
            Provider::Codex,
            "a tie stays deterministically on Codex",
        ),
    ];

    for (codex_used, grok_used, expected, reason) in scenarios {
        let decision = decide(
            plain(),
            usage_with_grok(
                window(99.0, HALF_WEEK, 0.0),
                window(codex_used, HALF_WEEK, 0.0),
                window(grok_used, HALF_WEEK, 0.0),
            ),
            NOW,
            &config,
        );

        assert_eq!(decision.provider, expected, "{reason}");
    }
}

/// A zero-looking value without a known weekly window is not free capacity, and the same ceiling
/// reserve that protects Codex also protects Grok. Either input must leave the healthy workhorse
/// as the only automatic destination.
#[test]
fn unavailable_or_exhausted_grok_cannot_win_the_workhorse_comparison() {
    let config = Config::default();
    let scenarios = [
        (Headroom::closed(), "no Grok telemetry"),
        (unknown_window(0.0, 0.0), "unknown Grok weekly capacity"),
        (
            window(config.hard_ceiling_pct, HALF_WEEK, 0.0),
            "Grok at the weekly ceiling",
        ),
    ];

    for (grok, reason) in scenarios {
        let decision = decide(
            plain(),
            usage_with_grok(
                window(99.0, HALF_WEEK, 0.0),
                window(70.0, HALF_WEEK, 0.0),
                grok,
            ),
            NOW,
            &config,
        );

        assert_eq!(decision.provider, Provider::Codex, "{reason}");
    }
}

// ------------------------------------------------------------------ rule 4: the hard ceiling

/// The reserve is a capacity boundary for each workhorse. When one provider reaches it, automatic
/// work goes to the other healthy workhorse, never to Claude's premium lane.
#[test]
fn a_workhorse_at_the_hard_ceiling_yields_to_the_other_workhorse() {
    let config = Config::default();
    let scenarios = [
        (
            window(config.hard_ceiling_pct, HALF_WEEK, 0.0),
            window(70.0, HALF_WEEK, 0.0),
            Provider::Grok,
        ),
        (
            window(70.0, HALF_WEEK, 0.0),
            window(config.hard_ceiling_pct, HALF_WEEK, 0.0),
            Provider::Codex,
        ),
    ];

    for (codex, grok, expected) in scenarios {
        let decision = decide(
            plain(),
            usage_with_grok(window(99.0, HALF_WEEK, 0.0), codex, grok),
            NOW,
            &config,
        );
        assert_eq!(decision.provider, expected);
    }
}

/// Failing closed cannot refuse ordinary work. With both workhorses at their reserve, the router
/// retains its deterministic Codex fallback and records the exhausted-capacity condition.
#[test]
fn both_workhorses_over_the_ceiling_keep_the_codex_fallback_and_flag_it() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage_with_grok(
            window(10.0, HALF_WEEK, 0.0),
            window(config.hard_ceiling_pct, HALF_WEEK, 0.0),
            window(config.hard_ceiling_pct, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::OverCeiling));
}

// ------------------------------------------------------------ rule 5: unknown workhorse windows

/// An unread weekly percentage is not headroom. The healthy peer remains the only automatic
/// destination whether the unread provider is Codex or Grok; Claude's capacity is irrelevant.
#[test]
fn an_unknown_workhorse_weekly_window_is_ineligible() {
    let config = Config::default();
    let scenarios = [
        (
            unknown_window(0.0, 0.0),
            window(70.0, HALF_WEEK, 0.0),
            Provider::Grok,
        ),
        (
            window(70.0, HALF_WEEK, 0.0),
            unknown_window(0.0, 0.0),
            Provider::Codex,
        ),
    ];

    for (codex, grok, expected) in scenarios {
        let decision = decide(
            plain(),
            usage_with_grok(window(99.0, HALF_WEEK, 0.0), codex, grok),
            NOW,
            &config,
        );
        assert_eq!(decision.provider, expected);
        assert!(decision.gates.contains(&Gate::WeeklyUnknown));
    }
}

/// Failing closed must never refuse ordinary work. When neither workhorse reports usable weekly
/// capacity, the router retains its deterministic Codex fallback and says why.
#[test]
fn both_workhorse_weekly_windows_unknown_still_route_to_codex() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            unknown_window(0.0, 0.0),
            unknown_window(0.0, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::WeeklyUnknown));
    assert!(decision.gates.contains(&Gate::OverCeiling));
}

// ------------------------------------------------------------------ rule 6: policy boundaries

/// Complexity controls the selected Codex tier, but never overrules measured workhorse headroom.
#[test]
fn complexity_picks_codex_tiers_without_overruling_workhorse_headroom() {
    let config = Config::default();
    let cases = [
        (Complexity::Low, "gpt-5.6-luna", "low"),
        (Complexity::Medium, "gpt-5.6-terra", "medium"),
        (Complexity::High, "gpt-5.6-sol", "high"),
        (Complexity::Ultra, "gpt-5.6-sol", "high"),
    ];

    for (complexity, codex_model, effort) in cases {
        let decision = decide(
            scored(false, false, complexity),
            usage_with_grok(
                window(99.0, HALF_WEEK, 0.0),
                window(10.0, HALF_WEEK, 0.0),
                window(60.0, HALF_WEEK, 0.0),
            ),
            NOW,
            &config,
        );
        assert_eq!(decision.provider, Provider::Codex, "{complexity:?}");
        assert_eq!(decision.model.as_deref(), Some(codex_model));
        assert_eq!(decision.effort.as_deref(), Some(effort));
    }
}

/// Disabling weekly routing intentionally restores the configured Codex default even when Grok
/// reports more headroom.
#[test]
fn disabled_weekly_routing_switches_workhorse_balancing_off() {
    let mut config = Config::default();
    config.policy.weekly_routing = false;

    let decision = decide(
        plain(),
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            window(80.0, HALF_WEEK, 0.0),
            window(10.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::WeeklyRoutingDisabled));
}

/// A failed classifier is not a capability pin: known weekly headroom still balances the task
/// between the workhorses.
#[test]
fn a_failed_classifier_stays_eligible_for_workhorse_balancing() {
    let config = Config::default();
    let decision = decide(
        Classification::fallback("timed out after 60s", DefaultProvider::Codex),
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            window(80.0, HALF_WEEK, 0.0),
            window(10.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Grok);
    assert!(decision.gates.contains(&Gate::ClassifierFailed));
}

// ------------------------------------------- the implement context window pin (capability pin 3)

/// An implement run scored `high` or `ultra` is the build tier, and the build tier does not fit
/// Codex's 258,400 token window. Like the other two capability pins, it bypasses every usage rule
/// below it: routing a job into a window it cannot fit is not a cheaper job, it is a failed one.
///
/// Grok has more weekly headroom than Codex in the fixture below, so the assertion proves the
/// capability pin outranks ordinary workhorse balancing.
#[test]
fn a_build_tier_implement_run_pins_to_claude_over_every_usage_rule() {
    let config = Config::default();
    for complexity in [Complexity::High, Complexity::Ultra] {
        // Codex is nearly idle and Grok has more weekly headroom; the capability pin still wins.
        let decision = decide(
            implement(complexity),
            usage_with_grok(
                window(97.0, HALF_WEEK, 0.0),
                window(1.0, HALF_WEEK, 0.0),
                window(0.0, HALF_WEEK, 0.0),
            ),
            NOW,
            &config,
        );
        assert_eq!(
            decision.provider,
            Provider::Claude,
            "{complexity:?} implement run must pin to claude"
        );
        assert!(decision.gates.contains(&Gate::ImplementContextWindow));
    }
}

/// The pin is narrow on both of its conditions, and each half is checked against the other's
/// opposite so neither can be passing on the wrong one.
#[test]
fn the_implement_pin_needs_both_the_invocation_and_the_build_tier() {
    let config = Config::default();

    // Right tier, not an implement run: no pin. This is the guard against pinning every
    // high-complexity task on the box to Claude.
    for complexity in [Complexity::High, Complexity::Ultra] {
        let decision = decide(
            scored(false, false, complexity),
            usage_with_grok(
                window(99.0, HALF_WEEK, 0.0),
                window(10.0, HALF_WEEK, 0.0),
                window(60.0, HALF_WEEK, 0.0),
            ),
            NOW,
            &config,
        );
        assert!(
            !decision.gates.contains(&Gate::ImplementContextWindow),
            "non-implement {complexity:?} must not pin: {:?}",
            decision.gates
        );
    }

    // An implement run, but the direct and quick tiers, which fit the window comfortably and are
    // the share of this workload Codex handles well. Codex is the default provider and this usage
    // picture trips no rule, so they stay there.
    for complexity in [Complexity::Low, Complexity::Medium] {
        let decision = decide(
            implement(complexity),
            usage_with_grok(
                window(99.0, HALF_WEEK, 0.0),
                window(10.0, HALF_WEEK, 0.0),
                window(60.0, HALF_WEEK, 0.0),
            ),
            NOW,
            &config,
        );
        assert!(
            !decision.gates.contains(&Gate::ImplementContextWindow),
            "{complexity:?} implement run must not pin: {:?}",
            decision.gates
        );
        assert_eq!(decision.provider, Provider::Codex);
    }
}

/// A classifier failure on an implement run pins rather than gambles: `Complexity`'s default is
/// `High`, so an unscored implement run reads as the build tier and lands where it fits. This is
/// the one place the fallback is deliberately NOT neutral, and it is the shape that costs most
/// when it goes wrong.
#[test]
fn an_unscored_implement_run_pins_to_claude() {
    let config = Config::default();
    let mut unscored = Classification::fallback("timeout", DefaultProvider::Codex);
    unscored.invokes_implement = true;
    let decision = decide(
        unscored,
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            window(10.0, HALF_WEEK, 0.0),
            window(60.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );
    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::ImplementContextWindow));
}
