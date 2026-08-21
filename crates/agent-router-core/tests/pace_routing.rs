//! Projection based routing: the OVERRIDE that moves a task off a provider heading for an early
//! exhaustion of its weekly window, plus the rules around it.
//!
//! The engine under test is pure, so `decide` takes the instant it is deciding at rather than
//! reading the clock: every case below fixes `NOW` and states each provider's reset as a distance
//! from it, which is what makes the arithmetic assertable at all.
//!
//! Every usage number here is chosen so the projection is an exact decimal. Half a week remaining
//! is 50 percent elapsed, so a projected draw is simply twice the percent used: 5 percent used
//! projects to 10, 50 percent used projects to exactly 100, and 80 percent used projects to 160.
//!
//! Why the threshold is 100 and not a measured number: a projected draw is what a provider's weekly
//! spend extrapolates to at the instant its own window resets, as a percent of its own allowance,
//! so 100 is precisely "finishes the week having used exactly its plan" and anything above it is
//! the provider running out early. Nothing here depends on how large either plan is, which is the
//! property the retired `pace_flip_gap` lacked: that key had to be tuned above whatever chronic
//! band the two plan sizes produced, and when the Codex plan grew on 2026-08-01 its configured 70
//! became unreachable and the override silently stopped firing altogether.

use agent_router_core::classify::{
    Classification, Complexity, TaskContextHorizon, parse_classification,
};
use agent_router_core::config::{Config, DefaultProvider};
use agent_router_core::decide::{Gate, decide};
use agent_router_core::{Headroom, Provider, UsageSnapshot};

/// The instant every case decides at. Any epoch works; this one is in the same range as the
/// recorded decisions, so a number that looks like a reset in the log reads like one here.
const NOW: i64 = 1_785_400_000;
/// The weekly window, in seconds. 10080 minutes.
const WEEK: i64 = 604_800;
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

fn usage(claude: Headroom, codex: Headroom) -> UsageSnapshot {
    UsageSnapshot {
        claude,
        codex,
        grok: Headroom::closed(),
    }
}

/// A blowout: Codex 80 percent through its allowance with half its window gone, so it projects to
/// draw 160 percent of a plan it only has 100 of, against a Claude projecting 10. Every test that
/// needs the override to fire uses this, so the picture that trips it lives in one place.
fn blowout() -> UsageSnapshot {
    usage(window(5.0, HALF_WEEK, 0.0), window(80.0, HALF_WEEK, 0.0))
}

/// Hot, but inside its allowance: Codex 45 percent used at the half way point projects to 90, and
/// finishes the week with 10 points to spare. Claude projects 10. This is the picture the override
/// must ignore, and it is deliberately a WIDE separation (an 80 point projection gap, and a 40
/// point run rate gap under the retired rule) so that "does not fire" cannot be passing merely
/// because the two providers look alike.
fn within_allowance() -> UsageSnapshot {
    usage(window(5.0, HALF_WEEK, 0.0), window(45.0, HALF_WEEK, 0.0))
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
/// ceiling, its five hour window is exhausted, Codex is empty, and Codex's reset is unknown. Each
/// of those alone moves a plain task somewhere else, and none of them may move this one.
#[test]
fn an_orchestration_task_pins_to_claude_past_every_usage_rule() {
    let config = Config::default();
    let decision = decide(
        scored(true, false, Complexity::High),
        usage(window(99.0, HALF_WEEK, 100.0), unknown_window(0.0, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
    assert!(!decision.gates.contains(&Gate::ProjectionUnavailable));
    assert!(!decision.gates.contains(&Gate::FiveHourPacing));
    assert!(!decision.gates.contains(&Gate::FlippedOnExhaustion));
}

/// Rule 2, the other half of the pin. A task that cannot reach its connector on Codex is not a
/// cheaper job when paced there, it is a failed one.
#[test]
fn a_missing_connector_pins_to_claude_past_every_usage_rule() {
    let config = Config::default();
    let decision = decide(
        scored(false, true, Complexity::High),
        usage(window(99.0, HALF_WEEK, 100.0), unknown_window(0.0, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::MissingConnector));
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
    assert!(!decision.gates.contains(&Gate::ProjectionUnavailable));
    assert!(!decision.gates.contains(&Gate::FiveHourPacing));
}

// ------------------------------------------------------------------ rule 3: the hard ceiling

/// Rule 3, the case the whole backstop exists for. Claude sits exactly on the configured ceiling
/// with its window fully elapsed, while Codex projects to a heavier weekly draw. Eligibility is
/// evaluated before that projection, so the override cannot move the task onto Claude.
#[test]
fn the_override_can_never_flip_into_a_provider_at_the_hard_ceiling() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(
            window(config.hard_ceiling_pct, 0, 0.0),
            window(90.0, WEEK * 9 / 10, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
}

/// Rule 3. Exactly one provider ineligible routes to the other, whatever run rate says. Here run
/// rate argues for staying on Codex (Codex reads 43 points colder), and the ceiling overrides it,
/// because being out of weekly budget is a capacity fact rather than a preference.
#[test]
fn the_one_eligible_provider_takes_the_task_even_when_pace_prefers_the_other() {
    let config = Config::default();
    // Codex: exactly the configured ceiling used with the window fully elapsed.
    // Claude: 40 used with a full week left, a pace delta of +40.
    let decision = decide(
        plain(),
        usage(
            window(40.0, WEEK, 0.0),
            window(config.hard_ceiling_pct, 0, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
    // The job is dispatched to Claude, so its model comes from Claude's tiers.
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
}

/// Rule 3. Both ineligible keeps the default provider and says so. The router routes; refusing
/// work over a ceiling is bonus drain's job. A 99 point gap would clear the dead zone here, and
/// must not get the chance.
#[test]
fn both_providers_over_the_ceiling_keep_the_default_and_flag_it() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(99.0, 0, 0.0), window(98.0, WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::OverCeiling));
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
}

// ------------------------------------------------------------ rule 4: the projection override

/// Rule 4, the override firing. Codex has spent 80 percent of its allowance with half its window
/// gone, so at this rate it draws 160 percent of a plan that holds 100 and runs dry with days left.
/// Claude projects 10. The task moves and is dispatched with Claude's tier.
#[test]
fn a_provider_projecting_past_its_allowance_moves_the_task() {
    let config = Config::default();
    let decision = decide(plain(), blowout(), NOW, &config);

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.gates, vec![Gate::ProjectedOverdraw]);
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
    assert_eq!(decision.effort.as_deref(), Some("high"));
    assert_eq!(decision.codex_projected_draw, Some(160.0));
    assert_eq!(decision.claude_projected_draw, Some(10.0));
}

/// Rule 4, the threshold and its boundary. A provider that finishes its week inside its allowance
/// is not a problem to be routed around, however far ahead of the other one it is running: moving
/// work off it would strand allowance that was going to be spent. A projection of exactly 100 holds
/// too, because the comparison is strictly greater, and two tenths of a point more is the smallest
/// input that moves anything.
///
/// This is the assertion that fails if anyone reintroduces a threshold below a full allowance.
#[test]
fn a_projection_inside_the_allowance_or_exactly_on_it_does_not_move_the_task() {
    let config = Config::default();

    let inside = decide(plain(), within_allowance(), NOW, &config);
    assert_eq!(inside.provider, Provider::Codex);
    assert!(inside.gates.is_empty(), "{:?}", inside.gates);
    assert_eq!(inside.codex_projected_draw, Some(90.0));

    // Half the window gone and half the allowance spent projects to exactly 100.
    let on_the_line = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(50.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(on_the_line.provider, Provider::Codex);
    assert!(on_the_line.gates.is_empty(), "{:?}", on_the_line.gates);
    assert_eq!(on_the_line.codex_projected_draw, Some(100.0));

    let just_past = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(50.1, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(just_past.provider, Provider::Claude);
    assert_eq!(just_past.gates, vec![Gate::ProjectedOverdraw]);
}

/// Rule 4, the week that matters most. When BOTH providers are heading past their allowance there
/// is no safe destination, and the useful question is which one runs out later. The task goes to
/// the lighter projection so the two drain together, instead of one dying while the other still
/// has days of headroom, which is exactly the failure this rule was rebuilt to prevent.
///
/// A rule phrased as "move only when the other is under the threshold" passes every other test in
/// this file and fails here, holding all the work on the provider already furthest gone.
#[test]
fn both_providers_overdrawing_moves_the_task_to_whichever_runs_out_later() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(60.0, HALF_WEEK, 0.0), window(80.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.gates, vec![Gate::ProjectedOverdraw]);
    assert_eq!(decision.claude_projected_draw, Some(120.0));
    assert_eq!(decision.codex_projected_draw, Some(160.0));
}

/// Rule 4. The destination has to be an improvement. A provider overdrawing worse than the one the
/// task is already on is not somewhere to send it, so the task holds and no gate is recorded: the
/// override considered the move and declined it.
#[test]
fn an_overdrawing_provider_holds_when_the_other_projects_worse() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(80.0, HALF_WEEK, 0.0), window(60.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.is_empty(), "{:?}", decision.gates);
}

/// Rule 4. "Both running light defaults to Codex" is not a special case: neither provider is
/// overdrawing, so the first half of the condition is false and the default stands on its own. A
/// rule that named this case explicitly would be dead code.
#[test]
fn two_providers_running_equally_light_keep_the_default_with_no_gate() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(5.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.is_empty(), "{:?}", decision.gates);
    assert_eq!(decision.model.as_deref(), Some("gpt-5.6-sol"));
}

/// Rule 4, the reason a projection is per provider at all: the two weekly windows reset at
/// different times, so one provider's reset says nothing about how far through its window the
/// other is.
///
/// Claude has a full week left and 10 percent used, which is too early to project at all. Codex has
/// a tenth of a week left and 85 percent used, projecting to 94 and landing inside its allowance.
/// Nothing moves. Feed Codex's nearly spent window to Claude instead and Claude reads 11 percent
/// against a 90 percent elapsed window, and the pair would invert.
#[test]
fn a_projection_is_measured_against_each_providers_own_reset() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(10.0, WEEK, 0.0), window(85.0, WEEK / 10, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(!decision.gates.contains(&Gate::ProjectedOverdraw));
}

/// Rule 4, arithmetic edge: a window resetting at this exact instant is fully elapsed, so 20
/// percent used projects to 20 and not to something larger. Reading zero seconds remaining as a
/// fresh window instead makes the elapsed fraction zero, which is below the minimum to project at
/// all, and the override would decline to run rather than move this task.
#[test]
fn a_window_resetting_at_this_instant_counts_as_fully_elapsed() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(20.0, 0, 0.0), window(60.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.gates, vec![Gate::ProjectedOverdraw]);
    assert_eq!(decision.claude_projected_draw, Some(20.0));
    assert_eq!(decision.codex_projected_draw, Some(120.0));
}

/// Rule 4, symmetry. The projection is read from the provider the task is currently on, not from
/// Codex, and the override is not special cased by direction. With Claude configured as the
/// default, a Codex projecting 160 is no reason to leave Claude, and a Claude projecting 160 is
/// reason to leave it.
#[test]
fn the_override_is_symmetric_and_measured_from_the_current_provider() {
    let mut config = Config::default();
    config.policy.default_provider = DefaultProvider::Claude;

    let stays = decide(plain(), blowout(), NOW, &config);
    assert_eq!(stays.provider, Provider::Claude);
    assert!(stays.gates.is_empty(), "{:?}", stays.gates);

    let leaves = decide(
        plain(),
        usage(window(80.0, HALF_WEEK, 0.0), window(5.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(leaves.provider, Provider::Codex);
    assert_eq!(leaves.gates, vec![Gate::ProjectedOverdraw]);
    assert_eq!(leaves.model.as_deref(), Some("gpt-5.6-sol"));
}

// ------------------------------------------------- rule 5: unknown windows and unprojectable ones

/// Rule 5. A reset epoch of 0 means "not known", and a weekly percentage nobody read is not
/// headroom. The provider is ineligible, the decision records `weekly_unknown`, and the override is
/// never reached.
///
/// The mutation this catches is the tempting one: treat 0 as an epoch like any other. The window
/// then reads as fully elapsed, the unknown provider looks like a confidently measured one, and the
/// task routes on a number that was never read. The recorded projection stays None for exactly that
/// reason, and is asserted here rather than only in the log's own tests.
///
/// `projection_unavailable` cannot fire on this input, and that is asserted rather than left to be
/// noticed later: the override runs only with both providers eligible, eligibility requires a known
/// weekly window, and a known window is precisely what gives `projected_draw` something to divide
/// into. A decision that would once have carried it carries `weekly_unknown` instead, which names
/// the reason rather than the consequence.
#[test]
fn an_unknown_weekly_window_makes_a_provider_ineligible() {
    let config = Config::default();

    // Claude unknown against a Codex with room. The task is already on Codex and stays there, and
    // the only gate is the record that eligibility was decided against a missing number.
    let claude_unknown = decide(
        plain(),
        usage(unknown_window(10.0, 0.0), window(90.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(claude_unknown.provider, Provider::Codex);
    assert_eq!(claude_unknown.gates, vec![Gate::WeeklyUnknown]);
    assert_eq!(claude_unknown.claude_projected_draw, None);
    assert!(!claude_unknown.gates.contains(&Gate::ProjectionUnavailable));

    // Codex unknown, which is the exhausted Codex shape: its rollout carries no weekly window at
    // all, so it reported 0 percent used, live, and won every comparison in this block while it
    // was in fact hard limited. The default provider is now the ineligible one, so the task moves
    // to the Claude that did report a number.
    let codex_unknown = decide(
        plain(),
        usage(window(10.0, HALF_WEEK, 0.0), unknown_window(0.0, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(codex_unknown.provider, Provider::Claude);
    assert_eq!(
        codex_unknown.gates,
        vec![Gate::WeeklyUnknown, Gate::FlippedOnExhaustion]
    );
    assert_eq!(codex_unknown.codex_projected_draw, None);
    assert!(!codex_unknown.gates.contains(&Gate::ProjectionUnavailable));
}

/// Rule 5. Failing closed must never fail to route. With neither weekly window read there is no
/// provider with confirmed room, so the task keeps the configured default and says both why it had
/// no better destination and that the numbers behind that were missing.
///
/// This is the arm that keeps the fix bounded: closing on an unknown window redirects work, and
/// the worst case is the default provider rather than a refused dispatch.
#[test]
fn both_weekly_windows_unknown_still_route_to_the_default() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(unknown_window(0.0, 0.0), unknown_window(0.0, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert_eq!(decision.gates, vec![Gate::WeeklyUnknown, Gate::OverCeiling]);
}

/// Rule 5, the divide-by-almost-nothing guard. Early in a week a single job is a large fraction of
/// everything spent so far, so the projection is not merely noisy, it is confidently wrong in the
/// direction that moves traffic: 4 percent spent in the first twentieth of a window extrapolates to
/// 80 percent of the whole allowance on the strength of a couple of dispatches.
///
/// Just under the minimum the override declines to run at all. Just past it the same shaped picture
/// is allowed to move a task, which is what stops this guard from being a permanent off switch.
#[test]
fn too_little_of_the_window_elapsed_yields_no_projection() {
    let config = Config::default();

    // 4 percent elapsed: 96 percent of the window still to run.
    let too_early = decide(
        plain(),
        usage(
            window(1.0, WEEK * 96 / 100, 0.0),
            window(4.0, WEEK * 96 / 100, 0.0),
        ),
        NOW,
        &config,
    );
    assert_eq!(too_early.provider, Provider::Codex);
    assert!(too_early.gates.contains(&Gate::ProjectionUnavailable));
    assert_eq!(too_early.codex_projected_draw, None);
    assert_eq!(too_early.claude_projected_draw, None);

    // 10 percent elapsed, the same 4 percent spent: 40 percent projected, and now measurable.
    let measurable = decide(
        plain(),
        usage(
            window(1.0, WEEK * 90 / 100, 0.0),
            window(4.0, WEEK * 90 / 100, 0.0),
        ),
        NOW,
        &config,
    );
    assert!(!measurable.gates.contains(&Gate::ProjectionUnavailable));
    let projected = measurable.codex_projected_draw.expect("now projects");
    assert!(
        (projected - 40.0).abs() < 1e-9,
        "expected about 40, got {projected}"
    );
}

/// Rule 5. A reset more than a full window out cannot be a window this task is inside: the elapsed
/// fraction is negative, and dividing by it flips the projection's sign. An 11 percent draw would
/// read as MINUS 11 percent projected, which is colder than any real provider can be and would hold
/// a task on an exhausted one forever. It is refused rather than clamped up into a real looking
/// number.
#[test]
fn a_reset_beyond_a_full_window_out_yields_no_projection() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(45.0, HALF_WEEK, 0.0), window(11.0, WEEK * 2, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::ProjectionUnavailable));
    assert_eq!(decision.codex_projected_draw, None);
}

/// Rule 5, the other end. A reset already in the past is a stale reading of a window that has run
/// out, so the elapsed fraction is capped at one whole window and the projection is simply what has
/// been spent. Without the cap a week-stale reset halves the number, and the log would record a
/// provider as running half as hot as it measured.
///
/// This cannot change which provider a task lands on, because a percentage of an allowance never
/// exceeds 100 and a capped projection therefore never crosses the threshold on its own. It is
/// asserted on the recorded value, which is what a human reads back.
#[test]
fn a_reset_already_in_the_past_projects_at_what_was_spent() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(90.0, -WEEK, 0.0), window(30.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.claude_projected_draw, Some(90.0));
    assert_eq!(decision.provider, Provider::Codex);
}

// ------------------------------------------------------------------ rule 6: what does not change

/// Rule 6. Complexity picks the model tier and never the provider. The same eight tiers are
/// asserted against two opposite usage pictures, so a complexity that leaked into the provider
/// choice would have to leak identically in both, and effort stays undecided throughout.
#[test]
fn complexity_picks_the_tier_and_never_the_provider() {
    let config = Config::default();
    let cases = [
        (Complexity::Low, "gpt-5.6-luna", "sonnet", "low"),
        (Complexity::Medium, "gpt-5.6-terra", "opus[1m]", "medium"),
        (Complexity::High, "gpt-5.6-sol", "opus[1m]", "high"),
        (Complexity::Ultra, "gpt-5.6-sol", "fable", "high"),
    ];

    for (complexity, codex_model, claude_model, effort) in cases {
        let stays = decide(
            scored(false, false, complexity),
            within_allowance(),
            NOW,
            &config,
        );
        assert_eq!(stays.provider, Provider::Codex, "{complexity:?} on codex");
        assert_eq!(stays.model.as_deref(), Some(codex_model));
        assert_eq!(stays.effort.as_deref(), Some(effort));

        let flips = decide(scored(false, false, complexity), blowout(), NOW, &config);
        assert_eq!(flips.provider, Provider::Claude, "{complexity:?} on claude");
        assert_eq!(flips.model.as_deref(), Some(claude_model));
        assert_eq!(flips.effort.as_deref(), Some(effort));
    }
}

/// Rule 6. The five hour rule survives the rewrite and still runs last: an override onto Claude is
/// paced straight back when Claude's five hour window is at the threshold. The whole gate vector is
/// asserted rather than its members, because running the two rules in the other order leaves the
/// job on Claude and no single membership check would notice.
#[test]
fn an_override_to_claude_is_paced_straight_back_by_the_five_hour_window() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(
            window(5.0, HALF_WEEK, config.claude_five_hour_pacing_pct),
            window(80.0, HALF_WEEK, 0.0),
        ),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert_eq!(
        decision.gates,
        vec![Gate::ProjectedOverdraw, Gate::FiveHourPacing]
    );
    assert_eq!(decision.model.as_deref(), Some("gpt-5.6-sol"));
}

/// Rule 6. Pacing into an exhausted Codex would move the stall rather than avoid it, so a task the
/// ceiling has just moved onto Claude stays there even with Claude's five hour window full.
#[test]
fn five_hour_pacing_does_not_fire_when_codex_has_no_weekly_room() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(40.0, HALF_WEEK, 100.0), window(99.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
    assert!(!decision.gates.contains(&Gate::FiveHourPacing));
}

/// Rule 6. "Codex has room" means the same thing here as in the arms above, so a Claude job with an
/// exhausted five hour window is NOT paced onto a Codex whose weekly number nobody read. Pacing
/// into an unread window is the same mistake as pacing into an exhausted one, and this is the arm
/// where a second inline comparison would have kept the old fail open behaviour alive.
#[test]
fn five_hour_pacing_does_not_fire_when_codex_has_no_weekly_window() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(40.0, HALF_WEEK, 100.0), unknown_window(0.0, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::WeeklyUnknown));
    assert!(!decision.gates.contains(&Gate::FiveHourPacing));
}

/// Rule 6. Only Claude has a five hour window that constrains a stream of jobs on this box, so
/// Codex's own five hour number never influences routing, in either direction.
#[test]
fn a_codex_five_hour_window_never_moves_a_task() {
    let config = Config::default();

    // Codex projects 90, inside its allowance, so nothing but its five hour number could move
    // this task, and nothing does.
    let stays = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(45.0, HALF_WEEK, 100.0)),
        NOW,
        &config,
    );
    assert_eq!(stays.provider, Provider::Codex);
    assert!(stays.gates.is_empty(), "{:?}", stays.gates);

    let flips = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(80.0, HALF_WEEK, 100.0)),
        NOW,
        &config,
    );
    assert_eq!(flips.provider, Provider::Claude);
    assert_eq!(flips.gates, vec![Gate::ProjectedOverdraw]);
}

/// Rule 6. An operator who turned weekly routing off asked to route on task shape alone, and the
/// override is a usage rule like the others, so it sits under the same flag.
#[test]
fn disabled_weekly_routing_switches_the_override_off_too() {
    let mut config = Config::default();
    config.policy.weekly_routing = false;

    let decision = decide(plain(), blowout(), NOW, &config);

    assert_eq!(decision.provider, Provider::Codex);
    assert_eq!(decision.gates, vec![Gate::WeeklyRoutingDisabled]);
}

/// Rule 6. A classifier failure is not a capability pin: it keeps the configured default and stays
/// eligible for every usage rule, so a task nobody could score still lands on the provider with
/// room. The flip re-derives the model, since the fallback carried Codex's.
#[test]
fn a_failed_classifier_keeps_the_default_and_stays_eligible_for_the_override() {
    let config = Config::default();
    let decision = decide(
        Classification::fallback("timed out after 60s", DefaultProvider::Codex),
        blowout(),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(
        decision.gates,
        vec![Gate::ClassifierFailed, Gate::ProjectedOverdraw]
    );
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
}

// ------------------------------------------------------------------ rule 7: the config key

/// Rule 7. Both retired keys are gone, not aliased. A file still carrying `headroom_flip_gap` or
/// `pace_flip_gap` must route as though it said nothing at all, so the threshold stays at the
/// default 100 and a blowout still fires the override.
///
/// `pace_flip_gap = 200` is the case that matters. Under an alias it would set the overdraw
/// threshold to 200 percent of allowance and this task would hold, but the subtler damage is the
/// number every real file actually carries: honouring a `pace_flip_gap = 70` as a projection
/// threshold would let a provider run to 70 percent OVER its allowance before anything moved.
/// A number tuned for a difference of run rates means nothing as a ratio against an allowance.
///
/// The behavioural half is the point. Reading the field back as 100 would also pass against an
/// alias that only wrote through to a second field.
#[test]
fn neither_retired_flip_gap_key_is_an_alias_for_the_new_one() {
    let config = Config::default();
    let dir = tempfile::tempdir().expect("tempdir");

    for (name, body) in [
        ("headroom.toml", "headroom_flip_gap = 200.0\n"),
        ("pace.toml", "pace_flip_gap = 200.0\n"),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("write the stale config");
        let stale = Config::load_from(&path).expect("load the stale config");
        assert_eq!(
            stale.projection_overdraw_pct, config.projection_overdraw_pct,
            "{name} must not be read"
        );

        let ignored = decide(plain(), blowout(), NOW, &stale);
        assert_eq!(ignored.provider, Provider::Claude, "{name}");
        assert_eq!(ignored.gates, vec![Gate::ProjectedOverdraw], "{name}");
    }

    // The new key does take effect, on the same picture.
    let tuned_path = dir.path().join("tuned.toml");
    std::fs::write(&tuned_path, "projection_overdraw_pct = 200.0\n")
        .expect("write the tuned config");
    let tuned = Config::load_from(&tuned_path).expect("load the tuned config");
    assert_eq!(tuned.projection_overdraw_pct, 200.0);
    let honoured = decide(plain(), blowout(), NOW, &tuned);
    assert_eq!(honoured.provider, Provider::Codex);
    assert!(honoured.gates.is_empty(), "{:?}", honoured.gates);
}

/// Rule 7. The generated file states the new key at the new version and never mentions either
/// retired one: a file that still writes a key the router no longer reads is a config that lies
/// about what is running. An older file is migrated in place and loses the stale keys on the way
/// through, which is the half that reaches the boxes this tool has already written a config on.
#[test]
fn the_written_config_carries_the_overdraw_threshold_at_the_current_version() {
    let dir = tempfile::tempdir().expect("tempdir");

    let fresh_path = dir.path().join("fresh/config.toml");
    let created = Config::load_from(&fresh_path).expect("create the default config");
    assert_eq!(created.config_version, 4);
    assert_eq!(created.projection_overdraw_pct, 100.0);

    let document: toml::Value =
        toml::from_str(&std::fs::read_to_string(&fresh_path).expect("read the written config"))
            .expect("parse the written config");
    assert_eq!(document["config_version"].as_integer(), Some(4));
    assert_eq!(document["projection_overdraw_pct"].as_float(), Some(100.0));
    assert!(document.get("headroom_flip_gap").is_none());
    assert!(document.get("pace_flip_gap").is_none());

    let old_path = dir.path().join("v3.toml");
    std::fs::write(&old_path, "config_version = 3\npace_flip_gap = 70.0\n")
        .expect("write the v3 config");
    let migrated = Config::load_from(&old_path).expect("load the v3 config");
    assert_eq!(migrated.config_version, 4);
    assert_eq!(migrated.projection_overdraw_pct, 100.0);

    let rewritten: toml::Value =
        toml::from_str(&std::fs::read_to_string(&old_path).expect("re-read the migrated config"))
            .expect("parse the migrated config");
    assert_eq!(rewritten["config_version"].as_integer(), Some(4));
    assert!(rewritten.get("pace_flip_gap").is_none());
}

/// Rule 7. The reserve, stated as the number rather than as `config.hard_ceiling_pct`: every other
/// ceiling case in this file reads the threshold off the config, so all of them would follow the
/// default silently wherever it moved. This one is the assertion that a provider inside the last 2
/// points of its weekly limit is not a routing destination.
///
/// Both sides of the boundary, because "98 is refused" is also true of a rule that refuses
/// everything, and the comparison is at-or-above, so 97.9 must still route.
#[test]
fn a_provider_within_two_points_of_its_weekly_limit_takes_no_more_work() {
    let config = Config::default();
    assert_eq!(config.hard_ceiling_pct, 98.0);

    // Codex is the default provider, and at 98 percent used it is out. Its window is fully
    // elapsed, so it projects to exactly what it has spent and reads as comfortably inside its
    // allowance, which argues for staying; the ceiling wins. Claude's window is half elapsed
    // rather than untouched, so both providers are projectable and the override genuinely runs
    // here instead of declining for want of a number.
    let refused = decide(
        plain(),
        usage(window(40.0, HALF_WEEK, 0.0), window(98.0, 0, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(refused.provider, Provider::Claude);
    assert!(refused.gates.contains(&Gate::FlippedOnExhaustion));

    // A tenth of a point below it, the same task stays on Codex with no gate at all: the reserve
    // is a boundary and not a general aversion to a busy provider.
    let allowed = decide(
        plain(),
        usage(window(40.0, HALF_WEEK, 0.0), window(97.9, 0, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(allowed.provider, Provider::Codex);
    assert!(allowed.gates.is_empty(), "{:?}", allowed.gates);
}

// ------------------------------------------- the implement context window pin (capability pin 3)

/// An implement run scored `high` or `ultra` is the build tier, and the build tier does not fit
/// Codex's 258,400 token window. Like the other two capability pins, it bypasses every usage rule
/// below it: routing a job into a window it cannot fit is not a cheaper job, it is a failed one.
///
/// The blowout picture is deliberate. Codex projects to draw 160 percent of its allowance and
/// Claude 10, so every usage rule in the engine argues for moving work ONTO Claude anyway; the
/// case that proves a pin is the reverse, below.
#[test]
fn a_build_tier_implement_run_pins_to_claude_over_every_usage_rule() {
    let config = Config::default();
    for complexity in [Complexity::High, Complexity::Ultra] {
        // Codex idle, Claude nearly out: usage says Codex, loudly. The pin still wins.
        let decision = decide(
            implement(complexity),
            usage(window(97.0, HALF_WEEK, 0.0), window(1.0, HALF_WEEK, 0.0)),
            NOW,
            &config,
        );
        assert_eq!(
            decision.provider,
            Provider::Claude,
            "{complexity:?} implement run must pin to claude"
        );
        assert!(decision.gates.contains(&Gate::ImplementContextWindow));
        // A capability pin runs no usage rule, so no usage gate may appear beside it.
        assert!(
            !decision.gates.contains(&Gate::FlippedOnExhaustion)
                && !decision.gates.contains(&Gate::ProjectedOverdraw)
                && !decision.gates.contains(&Gate::FiveHourPacing),
            "{:?}",
            decision.gates
        );
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
            within_allowance(),
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
        let decision = decide(implement(complexity), within_allowance(), NOW, &config);
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
    let decision = decide(unscored, within_allowance(), NOW, &config);
    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::ImplementContextWindow));
}
