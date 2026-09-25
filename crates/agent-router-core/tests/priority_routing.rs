//! Priority routing: `[routing] priority` lists the providers ordinary automatic work may use, in
//! the operator's order, and `priority_margin_pct` says how much worse (in projected draw points,
//! or weekly percent points when a projection is missing) an earlier provider may be and still
//! keep the work.
//!
//! The rule under test: candidates are the listed providers that pass the capability filter. The
//! first candidate is where the route starts. Eligible candidates (known weekly reading, under the
//! hard ceiling, launchable) are scored by projected draw when every one has a projection, and by
//! current weekly percent otherwise. The winner is the first eligible candidate, in priority order,
//! whose score is within the margin of the best score. A move off the first candidate records
//! exactly one gate: `flipped_on_exhaustion` when the first candidate was ineligible, otherwise
//! `priority_overridden_by_usage`.
//!
//! Every window below is exactly half elapsed (`HALF_WEEK`), so a projected draw is exactly twice
//! the weekly percent, which keeps the inclusive margin boundary exact in f64.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::{Config, Routing};
use agent_router_core::decide::{Decision, Gate, decide, decide_with_task};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use serde_json::json;
use std::collections::BTreeMap;

const NOW: i64 = 1_785_400_000;
/// Half the weekly window: elapsed is exactly 0.5, so draw = 2 x percent with no rounding.
const HALF_WEEK: i64 = 302_400;

/// A known weekly reading whose window is `weekly_remaining_secs` from its reset.
fn window(weekly_pct: f64, weekly_remaining_secs: i64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + weekly_remaining_secs,
        weekly_capacity_known: true,
        ..Headroom::full()
    }
}

/// A weekly reading nobody authoritatively read: ineligible regardless of its number.
fn unknown_window(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: 0,
        ..Headroom::full()
    }
}

/// Known weekly capacity with no reset timestamp: eligible, but no projected draw.
fn projectionless_window(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: 0,
        weekly_capacity_known: true,
        ..Headroom::full()
    }
}

fn usage(claude: Headroom, codex: Headroom, grok: Headroom) -> UsageSnapshot {
    UsageSnapshot {
        claude,
        codex,
        grok,
    }
}

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

/// A plain task at complexity High: nothing pinned, decided entirely by priority and usage.
fn plain() -> Classification {
    scored(false, false, Complexity::High)
}

fn prioritized(priority: Vec<Provider>, priority_margin_pct: f64) -> Config {
    Config {
        routing: Routing {
            priority,
            priority_margin_pct,
        },
        ..Config::default()
    }
}

/// Slack is established on Claude and Codex only; Grok does not have it.
fn slack_on_claude_and_codex(config: Config) -> Config {
    Config {
        provider_capabilities: BTreeMap::from([
            ("claude".to_string(), vec!["Slack".to_string()]),
            ("codex".to_string(), vec!["Slack".to_string()]),
        ]),
        ..config
    }
}

const SLACK_TASK: &str = "Use the client specific Slack MCP connection for each linked Slack task.";

/// A classifier miss whose rationale omits the product; the task text names Slack.
fn slack_classification() -> Classification {
    Classification {
        rationale: "cross-system triage and judgment".to_string(),
        ..scored(false, true, Complexity::High)
    }
}

fn has(decision: &Decision, gate: Gate) -> bool {
    decision.gates.contains(&gate)
}

/// The two provider-moving gates, at most one of which may fire on a decision.
fn assert_no_move_gate(decision: &Decision, why: &str) {
    assert!(
        !has(decision, Gate::PriorityOverriddenByUsage),
        "{why}: {:?}",
        decision.gates
    );
    assert!(
        !has(decision, Gate::FlippedOnExhaustion),
        "{why}: {:?}",
        decision.gates
    );
}

// ------------------------------------------------------------------ (a) a listed Claude leads

/// (a) Claude first in the list with every candidate equally paced keeps the work on Claude, reads
/// the model from Claude's tiers, and records no move because nothing moved.
#[test]
fn a_first_listed_claude_keeps_ordinary_work_on_an_even_week() {
    let config = prioritized(vec![Provider::Claude, Provider::Grok, Provider::Codex], 0.0);
    let decision = decide(
        plain(),
        usage(
            window(20.0, HALF_WEEK),
            window(20.0, HALF_WEEK),
            window(20.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_no_move_gate(&decision, "Claude led and stayed");
    assert_eq!(decision.model.as_deref(), Some("claude-opus-5-5[1m]"));
    assert_eq!(decision.effort.as_deref(), Some("low"));
    assert!(!decision.capability_blocked);
}

// ------------------------------------------------------------------ (b) the margin

/// (b) With a 10 point margin, an earlier provider keeps the work unless a later one beats it by
/// more than the margin. The boundary is inclusive: exactly `best + margin` still wins.
#[test]
fn the_margin_keeps_an_earlier_provider_unless_a_later_one_is_clearly_better() {
    let config = prioritized(
        vec![Provider::Claude, Provider::Grok, Provider::Codex],
        10.0,
    );
    // (claude pct, grok pct, codex pct, expected, moved, why). Draws are twice the percent.
    let cases = [
        (
            35.0,
            45.0,
            27.5,
            Provider::Codex,
            true,
            "Claude 70 and Grok 90 are both past Codex 55 plus 10",
        ),
        (
            32.5,
            45.0,
            27.5,
            Provider::Claude,
            false,
            "Claude 65 is exactly Codex 55 plus 10, and the boundary is inclusive",
        ),
        (
            30.0,
            45.0,
            27.5,
            Provider::Claude,
            false,
            "Claude 60 is inside the margin",
        ),
        (
            35.0,
            30.0,
            27.5,
            Provider::Grok,
            true,
            "Grok 60 is the first within Codex 55 plus 10, even though Codex is the best",
        ),
    ];

    for (claude, grok, codex, expected, moved, why) in cases {
        let decision = decide(
            plain(),
            usage(
                window(claude, HALF_WEEK),
                window(codex, HALF_WEEK),
                window(grok, HALF_WEEK),
            ),
            NOW,
            &config,
        );

        assert_eq!(decision.provider, expected, "{why}");
        assert_eq!(
            has(&decision, Gate::PriorityOverriddenByUsage),
            moved,
            "{why}: {:?}",
            decision.gates
        );
        assert!(!has(&decision, Gate::FlippedOnExhaustion), "{why}");
        assert!(!has(&decision, Gate::ProjectionUnavailable), "{why}");
        match expected {
            Provider::Codex => assert_eq!(decision.model.as_deref(), Some("gpt-6-astra"), "{why}"),
            Provider::Claude => assert_eq!(
                decision.model.as_deref(),
                Some("claude-opus-5-5[1m]"),
                "{why}"
            ),
            Provider::Grok => assert_eq!(decision.model, None, "{why}"),
        }
    }
}

// ------------------------------------------------------------------ (c) exact ties

/// (c) With no margin, an exact tie goes to whichever provider is listed first, in either order.
#[test]
fn an_exact_tie_goes_to_the_first_listed_provider() {
    let even = usage(
        window(90.0, HALF_WEEK),
        window(20.0, HALF_WEEK),
        window(20.0, HALF_WEEK),
    );

    let grok_first = decide(
        plain(),
        even,
        NOW,
        &prioritized(vec![Provider::Grok, Provider::Codex], 0.0),
    );
    assert_eq!(grok_first.provider, Provider::Grok);
    assert_no_move_gate(&grok_first, "Grok listed first on a tie");

    let codex_first = decide(
        plain(),
        even,
        NOW,
        &prioritized(vec![Provider::Codex, Provider::Grok], 0.0),
    );
    assert_eq!(codex_first.provider, Provider::Codex);
    assert_no_move_gate(&codex_first, "Codex listed first on a tie");
}

// ------------------------------------------------------------------ (d) an ineligible first choice

/// (d) When the first listed provider is ineligible, the move is `flipped_on_exhaustion` and never
/// also `priority_overridden_by_usage`, whatever made it ineligible. The winner is still chosen on
/// usage among the rest: Grok at 10 beats Codex at 30.
#[test]
fn an_ineligible_first_choice_flips_on_exhaustion_not_on_priority() {
    let config = prioritized(vec![Provider::Claude, Provider::Grok, Provider::Codex], 0.0);

    let unknown = decide(
        plain(),
        usage(
            unknown_window(0.0),
            window(30.0, HALF_WEEK),
            window(10.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(unknown.provider, Provider::Grok);
    assert!(has(&unknown, Gate::WeeklyUnknown));
    assert!(has(&unknown, Gate::FlippedOnExhaustion));
    assert!(!has(&unknown, Gate::PriorityOverriddenByUsage));
    assert_eq!(unknown.model, None, "Grok takes no model");

    let ceiling = decide(
        plain(),
        usage(
            window(config.hard_ceiling_pct, HALF_WEEK),
            window(30.0, HALF_WEEK),
            window(10.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(ceiling.provider, Provider::Grok);
    assert!(has(&ceiling, Gate::FlippedOnExhaustion));
    assert!(!has(&ceiling, Gate::PriorityOverriddenByUsage));
    assert!(!has(&ceiling, Gate::WeeklyUnknown));

    let unlaunchable = decide(
        Classification {
            unlaunchable: Some(Provider::Claude),
            ..plain()
        },
        usage(
            window(5.0, HALF_WEEK),
            window(30.0, HALF_WEEK),
            window(10.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(unlaunchable.provider, Provider::Grok);
    assert!(has(&unlaunchable, Gate::ClassifierUnlaunchable));
    assert!(has(&unlaunchable, Gate::FlippedOnExhaustion));
    assert!(!has(&unlaunchable, Gate::PriorityOverriddenByUsage));
}

// ------------------------------------------------------------------ (e) defaults leave Claude out

/// (e) The default priority does not list Claude, so an almost idle Claude never takes ordinary
/// work, and a capability shared by Claude and Codex stays on Codex with no move recorded.
#[test]
fn the_default_priority_never_routes_ordinary_or_shared_capability_work_to_claude() {
    let config = Config::default();

    let ordinary = decide(
        plain(),
        usage(
            window(1.0, HALF_WEEK),
            window(60.0, HALF_WEEK),
            window(70.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(ordinary.provider, Provider::Codex);
    assert_no_move_gate(&ordinary, "Codex led and stayed");

    let shared = decide_with_task(
        SLACK_TASK,
        slack_classification(),
        usage(
            window(8.0, HALF_WEEK),
            window(25.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
        ),
        NOW,
        &slack_on_claude_and_codex(config),
    );
    assert_eq!(shared.provider, Provider::Codex);
    assert!(!shared.capability_blocked);
    assert!(has(&shared, Gate::MissingConnector));
    assert!(!has(&shared, Gate::CapabilityProjectedDraw));
    assert_no_move_gate(&shared, "Codex is the only capable listed provider");
}

// ------------------------------------------------------------------ (f) capability filtering

/// (f) The capability filter runs before priority: Grok is listed first and has the lowest draw,
/// but it lacks Slack, so the first capable listed provider (Claude) starts and keeps the work.
/// Grok is not a candidate at all, so an unread Grok is not reported either.
#[test]
fn capability_filters_candidates_before_priority_applies() {
    let config = slack_on_claude_and_codex(prioritized(
        vec![Provider::Grok, Provider::Claude, Provider::Codex],
        0.0,
    ));

    let decision = decide_with_task(
        SLACK_TASK,
        slack_classification(),
        usage(
            window(20.0, HALF_WEEK),
            window(20.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(decision.provider, Provider::Claude);
    assert!(!decision.capability_blocked);
    assert_no_move_gate(&decision, "Claude is the first capable candidate");

    let grok_unread = decide_with_task(
        SLACK_TASK,
        slack_classification(),
        usage(
            window(20.0, HALF_WEEK),
            window(20.0, HALF_WEEK),
            Headroom::closed(),
        ),
        NOW,
        &config,
    );
    assert_eq!(grok_unread.provider, Provider::Claude);
    assert!(!has(&grok_unread, Gate::GrokUnavailable));
    assert!(!has(&grok_unread, Gate::WeeklyUnknown));
    assert_no_move_gate(&grok_unread, "Grok is not a candidate");
}

/// (f) Capable providers exist, but none is listed: that is a capability block, not a capacity
/// verdict, with or without weekly routing.
#[test]
fn a_capability_no_listed_provider_has_is_blocked_not_over_ceiling() {
    let config = slack_on_claude_and_codex(prioritized(vec![Provider::Grok], 0.0));
    let even = usage(
        window(20.0, HALF_WEEK),
        window(20.0, HALF_WEEK),
        window(20.0, HALF_WEEK),
    );

    let routed = decide_with_task(SLACK_TASK, slack_classification(), even, NOW, &config);
    assert!(routed.capability_blocked);
    assert!(has(&routed, Gate::CapabilityBlocked));
    assert!(!has(&routed, Gate::OverCeiling));

    let mut disabled = config;
    disabled.policy.weekly_routing = false;
    let unrouted = decide_with_task(SLACK_TASK, slack_classification(), even, NOW, &disabled);
    assert!(unrouted.capability_blocked);
    assert!(has(&unrouted, Gate::CapabilityBlocked));
    assert!(has(&unrouted, Gate::WeeklyRoutingDisabled));
    assert!(!has(&unrouted, Gate::OverCeiling));
}

/// (f) A capability only Claude has is still a pin, even when the priority does not list Claude.
#[test]
fn a_claude_only_capability_still_pins_claude_under_the_default_priority() {
    let config = Config {
        provider_capabilities: BTreeMap::from([("claude".to_string(), vec!["Slack".to_string()])]),
        ..Config::default()
    };
    let decision = decide_with_task(
        SLACK_TASK,
        slack_classification(),
        usage(
            window(90.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(decision.provider, Provider::Claude);
    assert!(!decision.capability_blocked);
    assert_no_move_gate(&decision, "a pin is not a move");
}

// ------------------------------------------------------------------ (g) projection fallback

/// (g) When any eligible candidate lacks a projection, every eligible candidate is scored on
/// current weekly percent. Claude at 35 percent is past Codex's 20 plus 10, so Codex takes it;
/// Claude at 25 percent is inside the margin and keeps it. Codex's draw would be 40, so the second
/// case only holds if percent, not draw, is compared.
#[test]
fn a_missing_projection_scores_every_eligible_candidate_on_percent() {
    let config = prioritized(vec![Provider::Claude, Provider::Codex], 10.0);

    let moved = decide(
        plain(),
        usage(
            projectionless_window(35.0),
            window(20.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(moved.provider, Provider::Codex);
    assert!(has(&moved, Gate::ProjectionUnavailable));
    assert!(has(&moved, Gate::PriorityOverriddenByUsage));
    assert!(!has(&moved, Gate::FlippedOnExhaustion));

    let kept = decide(
        plain(),
        usage(
            projectionless_window(25.0),
            window(20.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
        ),
        NOW,
        &config,
    );
    assert_eq!(kept.provider, Provider::Claude);
    assert!(has(&kept, Gate::ProjectionUnavailable));
    assert_no_move_gate(&kept, "Claude within the percent margin");
}

/// (g) A single eligible candidate wins outright; there was no comparison, so no fallback is
/// recorded even though it has no projection.
#[test]
fn a_single_eligible_candidate_records_no_projection_fallback() {
    let decision = decide(
        plain(),
        usage(
            window(1.0, HALF_WEEK),
            projectionless_window(20.0),
            Headroom::closed(),
        ),
        NOW,
        &Config::default(),
    );
    assert_eq!(decision.provider, Provider::Codex);
    assert!(!has(&decision, Gate::ProjectionUnavailable));
    assert_no_move_gate(&decision, "Codex led and stayed");
}

// ------------------------------------------------------------------ (h) the new gate

/// (h) Under the default priority, a Grok pick on pace is a move off Codex chosen by usage, not an
/// exhaustion flip.
#[test]
fn a_default_pace_pick_of_grok_is_a_priority_override() {
    let decision = decide(
        plain(),
        usage(
            window(99.0, HALF_WEEK),
            window(60.0, HALF_WEEK),
            window(10.0, HALF_WEEK),
        ),
        NOW,
        &Config::default(),
    );
    assert_eq!(decision.provider, Provider::Grok);
    assert!(has(&decision, Gate::PriorityOverriddenByUsage));
    assert!(!has(&decision, Gate::FlippedOnExhaustion));
    assert!(
        decision
            .gate_tags()
            .contains(&"priority_overridden_by_usage")
    );
}

/// (h) The new gate's log tag and JSON spelling agree, and the retired capability gate still
/// decodes so old log rows and old JSON keep reading.
#[test]
fn the_new_gate_tag_and_the_retired_gate_both_round_trip() {
    assert_eq!(
        Gate::PriorityOverriddenByUsage.tag(),
        "priority_overridden_by_usage"
    );
    assert_eq!(
        serde_json::to_value(Gate::PriorityOverriddenByUsage).expect("serializes"),
        json!("priority_overridden_by_usage")
    );
    assert_eq!(
        serde_json::from_value::<Gate>(json!("priority_overridden_by_usage")).expect("decodes"),
        Gate::PriorityOverriddenByUsage
    );

    assert_eq!(
        Gate::CapabilityProjectedDraw.tag(),
        "capability_projected_draw"
    );
    assert_eq!(
        serde_json::from_value::<Gate>(json!("capability_projected_draw")).expect("decodes"),
        Gate::CapabilityProjectedDraw
    );
}

// ------------------------------------------------------------------ policy and pins

/// With weekly routing off, the first listed provider takes the work and no usage gate fires.
#[test]
fn disabled_weekly_routing_takes_the_first_listed_provider_without_usage_gates() {
    let mut config = prioritized(vec![Provider::Claude, Provider::Codex], 0.0);
    config.policy.weekly_routing = false;
    let decision = decide(
        plain(),
        usage(
            window(90.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
            Headroom::closed(),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.model.as_deref(), Some("claude-opus-5-5[1m]"));
    assert!(has(&decision, Gate::WeeklyRoutingDisabled));
    for gate in [
        Gate::PriorityOverriddenByUsage,
        Gate::FlippedOnExhaustion,
        Gate::OverCeiling,
        Gate::WeeklyUnknown,
        Gate::GrokUnavailable,
        Gate::ProjectionUnavailable,
        Gate::ClassifierUnlaunchable,
    ] {
        assert!(!has(&decision, gate), "{gate:?} in {:?}", decision.gates);
    }
}

/// Orchestration pins Claude even when the priority does not list Claude.
#[test]
fn orchestration_pins_claude_whatever_the_priority() {
    let decision = decide(
        scored(true, false, Complexity::High),
        usage(
            window(90.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
            window(1.0, HALF_WEEK),
        ),
        NOW,
        &prioritized(vec![Provider::Grok], 0.0),
    );
    assert_eq!(decision.provider, Provider::Claude);
    assert!(has(&decision, Gate::Orchestration));
    assert_no_move_gate(&decision, "a pin is not a move");
}
