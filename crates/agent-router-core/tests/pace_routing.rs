//! Pace based routing: a rare run rate OVERRIDE with a wide dead zone, plus the rules around it.
//!
//! The engine under test is pure, so `decide` takes the instant it is deciding at rather than
//! reading the clock: every case below fixes `NOW` and states each provider's reset as a distance
//! from it, which is what makes the arithmetic assertable at all.
//!
//! Every usage number here is chosen so the expected burn is an exact decimal: half a week
//! remaining is 50 percent elapsed, a full week remaining is 0, and a reset at `NOW` is 100.
//!
//! Why the flip gap is 70 and not a tighter number: this box runs two Claude 20x Max plans against
//! a single Codex 5x plan, so Codex's allowance is about four times smaller and identical work
//! shows up as about four times the percentage. Measured over the real dispatches, Codex sits 43 to
//! 58 points over pace chronically, which is a plan size artifact and not a signal. The dead zone
//! has to clear that band, or the override stops being an override and becomes the routing rule.

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
    UsageSnapshot { claude, codex }
}

/// A blowout: Claude 45 points under pace against a Codex 30 points over it, a 75 point gap that
/// clears the 70 point dead zone. This is the only usage picture in this file that moves a task on
/// run rate alone, and every test that needs a flip uses it, so the threshold lives in one place.
fn blowout() -> UsageSnapshot {
    usage(window(5.0, HALF_WEEK, 0.0), window(80.0, HALF_WEEK, 0.0))
}

/// The chronic band: a 50 point gap, which is what this box's plan sizes produce all week. It is
/// the picture the dead zone exists to ignore.
fn chronic() -> UsageSnapshot {
    usage(window(5.0, HALF_WEEK, 0.0), window(55.0, HALF_WEEK, 0.0))
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
    }
}

/// A plain task: nothing pinned, so it is decided entirely by usage.
fn plain() -> Classification {
    scored(false, false, Complexity::High)
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
    assert!(!decision.gates.contains(&Gate::PaceFlip));
    assert!(!decision.gates.contains(&Gate::PaceUnavailable));
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
    assert!(!decision.gates.contains(&Gate::PaceFlip));
    assert!(!decision.gates.contains(&Gate::PaceUnavailable));
    assert!(!decision.gates.contains(&Gate::FiveHourPacing));
}

// ------------------------------------------------------------------ rule 3: the hard ceiling

/// Rule 3, the case the whole backstop exists for. Claude sits exactly ON the ceiling with its
/// window fully elapsed, so run rate reads it as 3 points COLD while 3 points of real allowance
/// remain, and Codex is 80 points over pace. That is an 83 point gap, past the dead zone, so the
/// override alone would move the task onto a provider that is out of budget. Eligibility is
/// evaluated first, so it does not.
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
    assert!(!decision.gates.contains(&Gate::PaceFlip));
}

/// Rule 3. Exactly one provider ineligible routes to the other, whatever run rate says. Here run
/// rate argues for staying on Codex (Codex reads 43 points colder), and the ceiling overrides it,
/// because being out of weekly budget is a capacity fact rather than a preference.
#[test]
fn the_one_eligible_provider_takes_the_task_even_when_pace_prefers_the_other() {
    let config = Config::default();
    // Codex: 97 used with the window fully elapsed, a pace delta of -3.
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
    assert!(!decision.gates.contains(&Gate::PaceFlip));
}

// ------------------------------------------------------------------ rule 4: the run rate override

/// Rule 4, the override firing. Claude has burned 5 percent of its week with half the window gone
/// (45 points under pace) while Codex has burned 80 percent of the same fraction (30 points over
/// it). The 75 point gap clears the dead zone, so the task moves and is dispatched with Claude's
/// tier.
#[test]
fn a_run_rate_gap_past_the_dead_zone_moves_the_task() {
    let config = Config::default();
    let decision = decide(plain(), blowout(), NOW, &config);

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.gates, vec![Gate::PaceFlip]);
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
    assert_eq!(decision.effort, None);
}

/// Rule 4, the dead zone and its boundary. A 50 point gap is the chronic reading this box produces
/// all week from its plan sizes, and it must not move anything. A gap of exactly `pace_flip_gap`
/// must not move anything either, because the comparison is strictly greater; one tenth of a point
/// more is the smallest input that does.
///
/// This is the assertion that fails if anyone tunes the threshold back toward the chronic band.
#[test]
fn a_gap_inside_the_dead_zone_or_exactly_on_it_does_not_flip() {
    let config = Config::default();

    let inside = decide(plain(), chronic(), NOW, &config);
    assert_eq!(inside.provider, Provider::Codex);
    assert!(inside.gates.is_empty(), "{:?}", inside.gates);

    // Claude at -45 against Codex at +25 is a gap of exactly 70.
    let on_the_line = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(75.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(on_the_line.provider, Provider::Codex);
    assert!(on_the_line.gates.is_empty(), "{:?}", on_the_line.gates);

    let just_past = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(75.1, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(just_past.provider, Provider::Claude);
    assert_eq!(just_past.gates, vec![Gate::PaceFlip]);
}

/// Rule 4. "Both running light defaults to Codex" is not a special case: two providers 45 points
/// under pace have a gap of zero, which is not past the dead zone, so the default stands on its
/// own. A rule that named this case explicitly would be dead code.
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

/// Rule 4, the reason run rate is per provider at all: the two weekly windows reset at different
/// times, so one provider's reset says nothing about the other's progress.
///
/// Claude has a full week left (0 percent elapsed, 10 used, delta +10). Codex has a tenth of a
/// week left (90 percent elapsed, 85 used, delta -5). Codex is 15 points COLDER, so the task
/// stays. Feed either provider's reset to both and the answer inverts: with Claude's reset on
/// Codex the delta reads +85 and with Codex's reset on Claude it reads -80, and both mistakes
/// produce a 75 point gap that clears the dead zone.
#[test]
fn pace_is_measured_against_each_providers_own_reset() {
    let config = Config::default();
    let decision = decide(
        plain(),
        usage(window(10.0, WEEK, 0.0), window(85.0, WEEK / 10, 0.0)),
        NOW,
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(!decision.gates.contains(&Gate::PaceFlip));
}

/// Rule 4, arithmetic edge: a window resetting at this exact instant is fully elapsed, so 20
/// percent used against 100 percent elapsed is 80 points under pace, and against a Codex 10 points
/// over it that is a 90 point gap. Reading zero seconds remaining as a fresh window instead would
/// put Claude 20 points OVER pace and lose the flip entirely.
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
    assert_eq!(decision.gates, vec![Gate::PaceFlip]);
}

/// Rule 4, arithmetic edge at both ends. Expected burn is a percentage of a window, so it is
/// clamped to 0..=100 and each unclamped reading changes the answer:
///
/// - a reset two weeks out is -100 percent elapsed unclamped, which would read Codex as 110 points
///   hot instead of 10 and produce a 115 point gap where the true one is 15;
/// - a reset a week in the past is 200 percent elapsed unclamped, which would read Claude as 110
///   points cold instead of 10 and produce a 120 point gap where the true one is 20.
#[test]
fn expected_burn_is_clamped_at_both_ends_of_the_window() {
    let config = Config::default();

    let far_future_reset = decide(
        plain(),
        usage(window(45.0, HALF_WEEK, 0.0), window(10.0, WEEK * 2, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(far_future_reset.provider, Provider::Codex);
    assert!(!far_future_reset.gates.contains(&Gate::PaceFlip));

    let stale_past_reset = decide(
        plain(),
        usage(window(90.0, -WEEK, 0.0), window(60.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(stale_past_reset.provider, Provider::Codex);
    assert!(!stale_past_reset.gates.contains(&Gate::PaceFlip));
}

/// Rule 4, symmetry. The gap is measured from the provider the task is currently on, not from
/// Codex, and the override is not special cased by direction. With Claude configured as the
/// default, a Codex 75 points hotter is no reason to leave Claude, and a Claude 75 points hotter
/// is reason to leave it.
///
/// This direction never fires on the recorded corpus: Claude's pace delta there tops out at +3.4
/// while Codex's floor is 0.0, so the gap is never negative enough. That is a property of how this
/// box happens to be provisioned today, not of the rule, so it is not encoded as one.
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
    assert_eq!(leaves.gates, vec![Gate::PaceFlip]);
    assert_eq!(leaves.model.as_deref(), Some("gpt-5.6-sol"));
}

// ------------------------------------------------------------------ rule 5: unknown reset

/// Rule 5. A reset epoch of 0 means "not known", so there is no elapsed fraction to compare
/// against and the override is skipped in favour of the default, with the reason recorded.
///
/// The mutation this catches is the tempting one: treat 0 as an epoch like any other and the
/// window reads as 100 percent elapsed, which turns an unknown Claude window into a 130 point gap
/// and routes on a number that was never measured.
#[test]
fn an_unknown_reset_skips_pace_and_keeps_the_default() {
    let config = Config::default();

    let claude_unknown = decide(
        plain(),
        usage(unknown_window(10.0, 0.0), window(90.0, HALF_WEEK, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(claude_unknown.provider, Provider::Codex);
    assert!(claude_unknown.gates.contains(&Gate::PaceUnavailable));
    assert!(!claude_unknown.gates.contains(&Gate::PaceFlip));

    let codex_unknown = decide(
        plain(),
        usage(window(10.0, HALF_WEEK, 0.0), unknown_window(90.0, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(codex_unknown.provider, Provider::Codex);
    assert!(codex_unknown.gates.contains(&Gate::PaceUnavailable));
    assert!(!codex_unknown.gates.contains(&Gate::PaceFlip));
}

// ------------------------------------------------------------------ rule 6: what does not change

/// Rule 6. Complexity picks the model tier and never the provider. The same eight tiers are
/// asserted against two opposite usage pictures, so a complexity that leaked into the provider
/// choice would have to leak identically in both, and effort stays undecided throughout.
#[test]
fn complexity_picks_the_tier_and_never_the_provider() {
    let config = Config::default();
    let cases = [
        (Complexity::Low, "gpt-5.6-luna", "sonnet"),
        (Complexity::Medium, "gpt-5.6-terra", "opus[1m]"),
        (Complexity::High, "gpt-5.6-sol", "opus[1m]"),
        (Complexity::Ultra, "gpt-5.6-sol", "fable"),
    ];

    for (complexity, codex_model, claude_model) in cases {
        let stays = decide(scored(false, false, complexity), chronic(), NOW, &config);
        assert_eq!(stays.provider, Provider::Codex, "{complexity:?} on codex");
        assert_eq!(stays.model.as_deref(), Some(codex_model));
        assert_eq!(stays.effort, None);

        let flips = decide(scored(false, false, complexity), blowout(), NOW, &config);
        assert_eq!(flips.provider, Provider::Claude, "{complexity:?} on claude");
        assert_eq!(flips.model.as_deref(), Some(claude_model));
        assert_eq!(flips.effort, None);
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
    assert_eq!(decision.gates, vec![Gate::PaceFlip, Gate::FiveHourPacing]);
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

/// Rule 6. Only Claude has a five hour window that constrains a stream of jobs on this box, so
/// Codex's own five hour number never influences routing, in either direction.
#[test]
fn a_codex_five_hour_window_never_moves_a_task() {
    let config = Config::default();

    let stays = decide(
        plain(),
        usage(window(5.0, HALF_WEEK, 0.0), window(55.0, HALF_WEEK, 100.0)),
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
    assert_eq!(flips.gates, vec![Gate::PaceFlip]);
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
    assert_eq!(decision.gates, vec![Gate::ClassifierFailed, Gate::PaceFlip]);
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
}

// ------------------------------------------------------------------ rule 7: the config key

/// Rule 7. The old key is gone, not aliased. A file still carrying `headroom_flip_gap = 200` must
/// route as though it said nothing at all: the gap stays at the 70 point default, so a 75 point
/// blowout still fires the override. Under an alias the 200 would be honoured and the same task
/// would hold.
///
/// The behavioural half is the point. Reading `pace_flip_gap` back as 70 would also pass against
/// an alias that only wrote through to a second field.
#[test]
fn the_old_flip_gap_key_is_not_an_alias_for_the_new_one() {
    let config = Config::default();
    let dir = tempfile::tempdir().expect("tempdir");

    let stale_path = dir.path().join("stale.toml");
    std::fs::write(&stale_path, "headroom_flip_gap = 200.0\n").expect("write the stale config");
    let stale = Config::load_from(&stale_path).expect("load the stale config");
    assert_eq!(stale.pace_flip_gap, config.pace_flip_gap);

    let ignored = decide(plain(), blowout(), NOW, &stale);
    assert_eq!(ignored.provider, Provider::Claude);
    assert_eq!(ignored.gates, vec![Gate::PaceFlip]);

    // The new key does take effect, on the same 75 point gap.
    let tuned_path = dir.path().join("tuned.toml");
    std::fs::write(&tuned_path, "pace_flip_gap = 200.0\n").expect("write the tuned config");
    let tuned = Config::load_from(&tuned_path).expect("load the tuned config");
    assert_eq!(tuned.pace_flip_gap, 200.0);
    let honoured = decide(plain(), blowout(), NOW, &tuned);
    assert_eq!(honoured.provider, Provider::Codex);
    assert!(honoured.gates.is_empty(), "{:?}", honoured.gates);
}

/// Rule 7. The generated file states the new key at the new version and never mentions the old
/// one: a file that still writes a key the router no longer reads is a config that lies about what
/// is running. A v1 file is migrated in place and loses the stale key on the way through.
///
/// The 70 point default is a measured property of this box's plan mix rather than a round number,
/// so it is pinned here as well as behaviourally: moving it silently re-tunes every route.
#[test]
fn the_written_config_carries_pace_flip_gap_at_the_current_version() {
    let dir = tempfile::tempdir().expect("tempdir");

    let fresh_path = dir.path().join("fresh/config.toml");
    let created = Config::load_from(&fresh_path).expect("create the default config");
    assert_eq!(created.config_version, 3);
    assert_eq!(created.pace_flip_gap, 70.0);

    let document: toml::Value =
        toml::from_str(&std::fs::read_to_string(&fresh_path).expect("read the written config"))
            .expect("parse the written config");
    assert_eq!(document["config_version"].as_integer(), Some(3));
    assert_eq!(document["pace_flip_gap"].as_float(), Some(70.0));
    assert!(document.get("headroom_flip_gap").is_none());

    let old_path = dir.path().join("v1.toml");
    std::fs::write(&old_path, "config_version = 1\nheadroom_flip_gap = 25.0\n")
        .expect("write the v1 config");
    let migrated = Config::load_from(&old_path).expect("load the v1 config");
    assert_eq!(migrated.config_version, 3);
    assert_eq!(migrated.pace_flip_gap, 70.0);

    let rewritten: toml::Value =
        toml::from_str(&std::fs::read_to_string(&old_path).expect("re-read the migrated config"))
            .expect("parse the migrated config");
    assert_eq!(rewritten["config_version"].as_integer(), Some(3));
    assert!(rewritten.get("headroom_flip_gap").is_none());
}

/// Rule 7. The reserve, stated as the number rather than as `config.hard_ceiling_pct`: every other
/// ceiling case in this file reads the threshold off the config, so all of them would follow the
/// default silently wherever it moved. This one is the assertion that a provider inside the last 5
/// points of its weekly limit is not a routing destination.
///
/// Both sides of the boundary, because "95 is refused" is also true of a rule that refuses
/// everything, and the comparison is at-or-above, so 94.9 must still route.
#[test]
fn a_provider_within_five_points_of_its_weekly_limit_takes_no_more_work() {
    let config = Config::default();
    assert_eq!(config.hard_ceiling_pct, 95.0);

    // Codex is the default provider, and at 95 percent used it is out. Its window is fully
    // elapsed, so run rate reads it as 5 points COLD and argues for staying; the ceiling wins.
    let refused = decide(
        plain(),
        usage(window(40.0, WEEK, 0.0), window(95.0, 0, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(refused.provider, Provider::Claude);
    assert!(refused.gates.contains(&Gate::FlippedOnExhaustion));

    // A tenth of a point below it, the same task stays on Codex with no gate at all: the reserve
    // is a boundary and not a general aversion to a busy provider.
    let allowed = decide(
        plain(),
        usage(window(40.0, WEEK, 0.0), window(94.9, 0, 0.0)),
        NOW,
        &config,
    );
    assert_eq!(allowed.provider, Provider::Codex);
    assert!(allowed.gates.is_empty(), "{:?}", allowed.gates);
}
