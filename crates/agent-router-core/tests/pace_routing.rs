//! Workhorse routing: automatic tasks balance between Codex and Grok from projected weekly
//! draw (pace), while Claude remains reserved for capability and context pins.
//!
//! The engine under test is pure, so `decide` takes the instant it is deciding at rather than
//! reading the clock: every case below fixes `NOW` and states each provider's reset as a distance
//! from it, which is what makes the arithmetic assertable at all.
//!
//! A smaller projected draw means the provider is further below its own week's pace. When both
//! windows are equally elapsed that is the same as a smaller weekly percentage; when they are
//! not, the under-pacing provider wins even at a higher current percent. Unknown readings and
//! the final reserve below the hard ceiling are not eligible capacity, so neither can win a
//! balance.

use agent_router_core::classify::{
    Classification, Complexity, TaskContextHorizon, parse_classification,
};
use agent_router_core::config::Config;
use agent_router_core::decide::{Gate, decide, decide_explicit, decide_with_task};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use std::collections::BTreeMap;

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
        unlaunchable: None,
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

/// Rule 2. A connector absent from the authoritative inventory is not evidence that Claude has
/// it, so routing must retain the miss but block dispatch instead of assigning a provider halo.
#[test]
fn a_missing_connector_is_capability_blocked_instead_of_pinning_claude() {
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

    assert!(decision.gates.contains(&Gate::MissingConnector));
    assert!(decision.gates.contains(&Gate::CapabilityBlocked));
    assert!(decision.capability_blocked);
    assert_ne!(decision.provider, Provider::Claude);
}

/// A classifier can correctly recognize a named external system while its old global inventory
/// cannot. A live provider inventory must narrow the policy pool, not turn that observation into
/// a global stop: reverting the provider eligibility filter makes this block again.
#[test]
fn a_provider_scoped_capability_keeps_auto_routing_inside_the_eligible_pool() {
    let config = Config {
        provider_capabilities: BTreeMap::from([
            ("claude".to_string(), vec!["Granola".to_string()]),
            ("codex".to_string(), vec!["Granola".to_string()]),
        ]),
        ..Config::default()
    };
    let decision = decide(
        Classification {
            rationale: "requires Granola meeting notes".to_string(),
            ..scored(false, true, Complexity::Medium)
        },
        usage_with_grok(
            window(99.0, HALF_WEEK, 100.0),
            window(70.0, HALF_WEEK, 0.0),
            window(1.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::MissingConnector));
    assert!(!decision.gates.contains(&Gate::CapabilityBlocked));
    assert!(!decision.capability_blocked);
}

/// The same Slack inventory recovers when the product name is in the task and the one-sentence
/// rationale omits it. Grok stays ineligible because it is not in the inventory.
#[test]
fn a_task_named_capability_recovers_when_the_rationale_omits_the_product() {
    let config = Config {
        provider_capabilities: BTreeMap::from([
            ("claude".to_string(), vec!["Slack".to_string()]),
            ("codex".to_string(), vec!["Slack".to_string()]),
        ]),
        ..Config::default()
    };
    let classification = Classification {
        rationale: "cross-system triage and judgment".to_string(),
        ..scored(false, true, Complexity::High)
    };
    let usage = usage_with_grok(
        window(8.0, HALF_WEEK, 1.0),
        window(25.0, HALF_WEEK, 0.0),
        window(76.0, HALF_WEEK, 0.0),
    );

    let blocked = decide(classification.clone(), usage, NOW, &config);
    assert!(blocked.capability_blocked);
    assert!(blocked.gates.contains(&Gate::CapabilityBlocked));

    let recovered = decide_with_task(
        "Use the client specific Slack MCP connection for each linked Slack task.",
        classification,
        usage,
        NOW,
        &config,
    );
    assert!(!recovered.capability_blocked);
    assert_eq!(recovered.provider, Provider::Codex);
    assert!(recovered.gates.contains(&Gate::MissingConnector));
    assert!(!recovered.gates.contains(&Gate::CapabilityBlocked));
    assert_eq!(
        recovered.matched_capabilities,
        vec![agent_router_core::config::MatchedCapability {
            name: "Slack".to_string(),
            in_task: true,
            in_rationale: false,
        }]
    );
    assert_eq!(recovered.requested_model, None);
}

/// English "notion" in a task must not recover a Notion-capable provider.
#[test]
fn english_notion_does_not_unblock_a_missing_connector() {
    let config = Config {
        provider_capabilities: BTreeMap::from([
            ("claude".to_string(), vec!["Notion".to_string()]),
            ("codex".to_string(), vec!["Notion".to_string()]),
        ]),
        ..Config::default()
    };
    let decision = decide_with_task(
        "the notion that we should wait and report only",
        Classification {
            rationale: "cross-system triage and judgment".to_string(),
            ..scored(false, true, Complexity::High)
        },
        usage_with_grok(
            window(8.0, HALF_WEEK, 1.0),
            window(25.0, HALF_WEEK, 0.0),
            window(76.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );
    assert!(decision.capability_blocked);
    assert!(decision.matched_capabilities.is_empty());
}

/// Capability eligibility is an Auto-only preflight. An explicit caller continues to own the
/// provider choice even when another provider is the only one that advertises the capability.
#[test]
fn explicit_provider_bypasses_automatic_capability_eligibility() {
    let config = Config {
        provider_capabilities: BTreeMap::from([("codex".to_string(), vec!["Granola".to_string()])]),
        ..Config::default()
    };
    let decision = decide_explicit(
        Provider::Grok,
        None,
        None,
        Some(Classification {
            rationale: "requires Granola notes".to_string(),
            ..scored(false, true, Complexity::Medium)
        }),
        usage_with_grok(
            window(1.0, HALF_WEEK, 0.0),
            window(1.0, HALF_WEEK, 0.0),
            window(1.0, HALF_WEEK, 0.0),
        ),
        &config,
    );

    assert_eq!(decision.provider, Provider::Grok);
    assert_eq!(decision.gates, vec![Gate::ExplicitProvider]);
    assert!(!decision.capability_blocked);
}

// ------------------------------------------------- rule 3: the workhorse headroom comparison

/// Automatic work stays in the Codex/Grok workhorse pool. Once both report a usable weekly
/// window, and both windows are equally elapsed, the provider with more weekly headroom wins;
/// equality deliberately preserves Codex as the deterministic default. Claude is nearly exhausted
/// in every case so these assertions fail if its premium lane is accidentally reintroduced as a
/// capacity competitor.
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

/// Pace, not current percent. Codex is further into its week at 11 percent used (projects to 82);
/// Grok is earlier in its week at 9 percent used (projects to 86). Current percent would pick
/// Grok; projected draw picks the under-pacing Codex.
#[test]
fn a_higher_current_percent_still_wins_when_it_is_further_below_pace() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            // 145.5h remaining of 168h: 13.4 percent elapsed, 11 / 0.134 = 82 projected.
            window(11.0, 523_800, 0.0),
            // 150.5h remaining of 168h: 10.4 percent elapsed, 9 / 0.104 = 86 projected.
            window(9.0, 541_800, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(!decision.gates.contains(&Gate::ProjectionUnavailable));
    let grok_draw = decision.grok_projected_draw.expect("grok projects");
    let codex_draw = decision.codex_projected_draw.expect("codex projects");
    assert!(
        codex_draw < grok_draw,
        "codex {codex_draw} should be further below pace than grok {grok_draw}"
    );
}

/// The inverse: Grok is further into its week at a higher current percent, but still under-pacing
/// Codex, so Grok takes the work.
#[test]
fn grok_wins_on_pace_even_with_a_higher_current_percent() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            window(9.0, 541_800, 0.0),
            window(11.0, 523_800, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Grok);
}

/// Early in both windows a projection is noise, so the comparison falls back to current weekly
/// percent and says so. Four percent elapsed is under the twentieth-of-a-week floor.
#[test]
fn an_uncomputable_projection_falls_back_to_weekly_percent() {
    let config = Config::default();
    // 96 percent of the window still remaining: 4 percent elapsed.
    let early = 580_608;
    let decision = decide(
        plain(),
        usage_with_grok(
            window(99.0, HALF_WEEK, 0.0),
            window(20.0, early, 0.0),
            window(10.0, early, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Grok);
    assert!(decision.gates.contains(&Gate::ProjectionUnavailable));
    assert_eq!(decision.codex_projected_draw, None);
    assert_eq!(decision.grok_projected_draw, None);
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
        Classification::fallback("timed out after 60s"),
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
    let mut unscored = Classification::fallback("timeout");
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
