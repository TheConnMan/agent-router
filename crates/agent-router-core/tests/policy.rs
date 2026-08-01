use agent_router_core::classify::Complexity;
use agent_router_core::classify::{Classification, Confidence, Verdict};
use agent_router_core::config::{Config, DefaultProvider};
use agent_router_core::decide::{Gate, decide, decide_explicit};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use std::path::Path;

fn usage(codex_weekly: f64, claude_weekly: f64) -> UsageSnapshot {
    UsageSnapshot {
        codex: Headroom {
            weekly_pct: codex_weekly,
            ..Headroom::full()
        },
        claude: Headroom {
            weekly_pct: claude_weekly,
            ..Headroom::full()
        },
    }
}

fn scored(
    verdict: Verdict,
    confidence: Confidence,
    claude_signals: usize,
    missing_connector: bool,
) -> Classification {
    let mut signals = [false; 6];
    for signal in signals.iter_mut().take(claude_signals) {
        *signal = true;
    }
    Classification {
        codex_ready: [true; 6],
        claude_signals: signals,
        missing_connector,
        verdict,
        confidence,
        complexity: Complexity::High,
        rationale: "fixture".to_string(),
        classifier_failed: false,
    }
}

fn load_config(text: &str, path: &Path) -> agent_router_core::Result<Config> {
    std::fs::write(path, text).expect("write config fixture");
    Config::load_from(path)
}

#[test]
fn written_defaults_round_trip_with_codex_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/config.toml");

    let created = Config::load_from(&path).expect("create default config");
    let written = std::fs::read_to_string(&path).expect("read written config");
    let document: toml::Value = toml::from_str(&written).expect("parse written config");

    assert_eq!(created.policy.default_provider, DefaultProvider::Codex);
    assert!(created.policy.weekly_routing);
    assert!(created.parity.roots.is_empty());
    assert!(created.parity.exceptions.is_empty());
    assert_eq!(
        document["policy"]["default_provider"].as_str(),
        Some("codex")
    );
    assert_eq!(document["policy"]["weekly_routing"].as_bool(), Some(true));
    assert_eq!(
        Config::load_from(&path).expect("reload written config"),
        created
    );
}

#[test]
fn partial_policy_uses_defaults_for_omitted_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let config = load_config(
        r#"
[policy]
default_provider = "claude"
"#,
        &path,
    )
    .expect("load partial policy");

    assert_eq!(config.policy.default_provider, DefaultProvider::Claude);
    assert!(config.policy.weekly_routing);
    assert_eq!(config.connectors, Config::default().connectors);
    assert_eq!(config.hard_ceiling_pct, Config::default().hard_ceiling_pct);
}

#[test]
fn invalid_policy_provider_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    assert!(
        load_config(
            r#"
[policy]
default_provider = "opencode"
"#,
            &path,
        )
        .is_err()
    );
}

#[test]
fn incomplete_or_invalid_parity_exceptions_are_rejected() {
    let fixtures = [
        (
            "missing path",
            r#"
[[parity.exceptions]]
reason = "intentional"
"#,
        ),
        (
            "empty path",
            r#"
[[parity.exceptions]]
path = ""
reason = "intentional"
"#,
        ),
        (
            "missing reason",
            r#"
[[parity.exceptions]]
path = "project"
"#,
        ),
        (
            "blank reason",
            r#"
[[parity.exceptions]]
path = "project"
reason = "   "
"#,
        ),
        (
            "unknown kind",
            r#"
[[parity.exceptions]]
path = "project"
reason = "intentional"
kind = "different"
"#,
        ),
    ];

    for (label, text) in fixtures {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        assert!(
            load_config(text, &path).is_err(),
            "{label} must be rejected"
        );
    }
}

#[test]
fn classifier_fallback_uses_and_names_each_configured_default() {
    let codex = Classification::fallback("timeout", DefaultProvider::Codex);
    assert_eq!(codex.verdict, Verdict::Codex);
    assert!(codex.classifier_failed);
    assert!(codex.rationale.contains("defaulting to codex"));

    let claude = Classification::fallback("timeout", DefaultProvider::Claude);
    assert_eq!(claude.verdict, Verdict::Claude);
    assert!(claude.classifier_failed);
    assert!(claude.rationale.contains("defaulting to claude"));
}

#[test]
fn configured_claude_default_controls_a_failed_classifier_decision() {
    let mut config = Config::default();
    config.policy.default_provider = DefaultProvider::Claude;

    let decision = decide(
        Classification::fallback("timeout", DefaultProvider::Claude),
        usage(10.0, 10.0),
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
    assert_eq!(decision.effort, None);
    assert_eq!(decision.gates, vec![Gate::ClassifierFailed]);
}

#[test]
fn successful_classifier_verdict_overrides_each_configured_default() {
    let mut codex_default = Config::default();
    codex_default.policy.default_provider = DefaultProvider::Codex;
    let claude = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage(10.0, 10.0),
        &codex_default,
    );
    assert_eq!(claude.provider, Provider::Claude);
    assert!(claude.gates.is_empty());

    let mut claude_default = Config::default();
    claude_default.policy.default_provider = DefaultProvider::Claude;
    let codex = decide(
        scored(Verdict::Codex, Confidence::High, 0, false),
        usage(10.0, 10.0),
        &claude_default,
    );
    assert_eq!(codex.provider, Provider::Codex);
    assert!(codex.gates.is_empty());
}

#[test]
fn failed_classifier_remains_eligible_for_weekly_routing() {
    let config = Config::default();
    let decision = decide(
        Classification::fallback("timeout", DefaultProvider::Codex),
        usage(99.0, 0.0),
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    // The flip re-derives for claude: the codex fallback model must not survive the move.
    assert_eq!(decision.model.as_deref(), Some("opus[1m]"));
    assert_eq!(decision.effort, None);
    assert_eq!(
        decision.gates,
        vec![Gate::ClassifierFailed, Gate::FlippedOnExhaustion]
    );
}

#[test]
fn capability_pins_force_claude_and_skip_weekly_routing() {
    let config = Config::default();
    let missing_connector = decide(
        scored(Verdict::Codex, Confidence::High, 0, true),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(missing_connector.provider, Provider::Claude);
    assert_eq!(missing_connector.gates, vec![Gate::MissingConnector]);
    assert_eq!(missing_connector.model.as_deref(), Some("opus[1m]"));
    assert_eq!(missing_connector.effort, None);

    let claude_signals = decide(
        scored(Verdict::Codex, Confidence::High, 2, false),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(claude_signals.provider, Provider::Claude);
    assert_eq!(claude_signals.gates, vec![Gate::ClaudeSignals]);
    assert_eq!(claude_signals.model.as_deref(), Some("opus[1m]"));
    assert_eq!(claude_signals.effort, None);
}

#[test]
fn disabled_weekly_routing_blocks_both_flip_directions() {
    let mut config = Config::default();
    config.policy.weekly_routing = false;

    let codex = decide(
        scored(Verdict::Codex, Confidence::High, 0, false),
        usage(99.0, 0.0),
        &config,
    );
    assert_eq!(codex.provider, Provider::Codex);
    assert_eq!(codex.gates, vec![Gate::WeeklyRoutingDisabled]);
    assert_eq!(codex.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(codex.effort, None);

    let claude = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(claude.provider, Provider::Claude);
    assert_eq!(claude.gates, vec![Gate::WeeklyRoutingDisabled]);
    assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
    assert_eq!(claude.effort, None);
}

/// A weekly failover dispatches to the other provider, so the model must be re-derived for the
/// provider actually receiving the job. Carrying the prior provider's model across a flip sends a
/// name the target cannot resolve, in either direction.
#[test]
fn a_weekly_failover_rederives_the_model_for_the_provider_it_lands_on() {
    let config = Config::default();

    let to_claude = decide(
        scored(Verdict::Codex, Confidence::High, 0, false),
        usage(99.0, 0.0),
        &config,
    );
    assert_eq!(to_claude.provider, Provider::Claude);
    assert_eq!(to_claude.model.as_deref(), Some("opus[1m]"));
    assert_eq!(to_claude.effort, None);
    assert_eq!(to_claude.gates, vec![Gate::FlippedOnExhaustion]);

    let to_codex = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(to_codex.provider, Provider::Codex);
    assert_eq!(to_codex.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(to_codex.effort, None);
    assert_eq!(to_codex.gates, vec![Gate::FlippedOnExhaustion]);
}

#[test]
fn both_providers_over_ceiling_keep_the_selected_provider() {
    let config = Config::default();

    for (verdict, provider) in [
        (Verdict::Codex, Provider::Codex),
        (Verdict::Claude, Provider::Claude),
    ] {
        let decision = decide(
            scored(verdict, Confidence::High, 0, false),
            usage(98.0, 99.0),
            &config,
        );
        assert_eq!(decision.provider, provider);
        assert_eq!(decision.gates, vec![Gate::OverCeiling]);
    }
}

#[test]
fn explicit_provider_decisions_remain_outside_policy_routing() {
    let snapshot = usage(99.0, 99.0);

    let config = Config::default();
    let codex = decide_explicit(Provider::Codex, None, snapshot, &config);
    assert_eq!(codex.provider, Provider::Codex);
    assert_eq!(codex.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(codex.effort, None);
    assert_eq!(codex.gates, vec![Gate::ExplicitProvider]);
    assert!(codex.classification.is_none());

    let claude = decide_explicit(
        Provider::Claude,
        Some("sonnet".to_string()),
        snapshot,
        &config,
    );
    assert_eq!(claude.provider, Provider::Claude);
    assert_eq!(claude.model.as_deref(), Some("sonnet"));
    assert_eq!(claude.effort, None);
    assert_eq!(claude.gates, vec![Gate::ExplicitProvider]);
    assert!(claude.classification.is_none());

    let opencode = decide_explicit(Provider::Opencode, None, snapshot, &config);
    assert_eq!(opencode.provider, Provider::Opencode);
    assert_eq!(opencode.model, None);
    assert_eq!(opencode.effort, None);
    assert_eq!(opencode.gates, vec![Gate::ExplicitProvider]);
    assert!(opencode.classification.is_none());
}

/// The weekly-only fixture above cannot express a five hour number, and widening it would touch
/// every existing case for no gain. This sibling carries both windows for both providers, so a
/// pacing test can state Codex's five hour number as well as Claude's and prove it is ignored.
fn usage_with_five_hour(
    codex_weekly: f64,
    claude_weekly: f64,
    codex_five_hour: f64,
    claude_five_hour: f64,
) -> UsageSnapshot {
    UsageSnapshot {
        codex: Headroom {
            weekly_pct: codex_weekly,
            five_hour_pct: codex_five_hour,
            ..Headroom::full()
        },
        claude: Headroom {
            weekly_pct: claude_weekly,
            five_hour_pct: claude_five_hour,
            ..Headroom::full()
        },
    }
}

/// The mutation target: the pacing rule compares Claude's five hour percent against
/// `claude_five_hour_pacing_pct` with `>=`, and this case sits exactly ON the threshold. Turning
/// that `>=` into a `>` makes it fail, and so does restricting the rule to borderline verdicts,
/// since this verdict is a confident one.
#[test]
fn a_claude_route_paces_away_when_claude_five_hour_sits_exactly_on_the_threshold() {
    let config = Config::default();
    let decision = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage_with_five_hour(10.0, 10.0, 0.0, config.claude_five_hour_pacing_pct),
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert!(decision.gates.contains(&Gate::FiveHourPacing));
}

/// A capability pin is a statement that the task cannot run on Codex at all, so an exhausted five
/// hour window cannot move it: a paced job that cannot reach its connector is a failed job, not a
/// cheaper one. Both pins are covered, because they set `capability_pin` from different branches.
#[test]
fn a_capability_pin_survives_an_exhausted_claude_five_hour_window() {
    let config = Config::default();

    let missing_connector = decide(
        scored(Verdict::Codex, Confidence::High, 0, true),
        usage_with_five_hour(0.0, 0.0, 0.0, 100.0),
        &config,
    );
    assert_eq!(missing_connector.provider, Provider::Claude);
    assert_eq!(missing_connector.gates, vec![Gate::MissingConnector]);
    assert!(!missing_connector.gates.contains(&Gate::FiveHourPacing));

    let claude_signals = decide(
        scored(Verdict::Codex, Confidence::High, 2, false),
        usage_with_five_hour(0.0, 0.0, 0.0, 100.0),
        &config,
    );
    assert_eq!(claude_signals.provider, Provider::Claude);
    assert_eq!(claude_signals.gates, vec![Gate::ClaudeSignals]);
    assert!(!claude_signals.gates.contains(&Gate::FiveHourPacing));
}

/// Codex having room is judged by the same `hard_ceiling_pct` the exhaustion flip uses, and the
/// comparison is a strict `<`, so a Codex sitting exactly on the ceiling has no room. Pacing into
/// an exhausted Codex would move the stall rather than avoid it.
#[test]
fn pacing_does_not_fire_when_codex_has_no_weekly_room() {
    let config = Config::default();

    let on_the_ceiling = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage_with_five_hour(config.hard_ceiling_pct, 10.0, 0.0, 100.0),
        &config,
    );
    assert_eq!(on_the_ceiling.provider, Provider::Claude);
    assert!(on_the_ceiling.gates.is_empty());

    let over_the_ceiling = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage_with_five_hour(99.0, 10.0, 0.0, 100.0),
        &config,
    );
    assert_eq!(over_the_ceiling.provider, Provider::Claude);
    assert!(!over_the_ceiling.gates.contains(&Gate::FiveHourPacing));
}

/// An operator who set `weekly_routing = false` asked to route purely on task shape. A usage
/// driven pacing flip contradicts that, so it sits under the same flag rather than behind a second
/// one that could be left on by accident.
#[test]
fn pacing_is_off_when_weekly_routing_is_disabled() {
    let mut config = Config::default();
    config.policy.weekly_routing = false;

    let decision = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage_with_five_hour(0.0, 0.0, 0.0, 100.0),
        &config,
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert_eq!(decision.gates, vec![Gate::WeeklyRoutingDisabled]);
    assert!(!decision.gates.contains(&Gate::FiveHourPacing));
}

/// The hard constraint: only Claude has a five hour window that constrains a stream of jobs on this
/// box, so Codex's own five hour number never influences routing. This is the test that fails if
/// the rule is later generalized to both providers, in either direction.
#[test]
fn a_codex_five_hour_window_never_moves_a_task() {
    let config = Config::default();

    let codex_verdict = decide(
        scored(Verdict::Codex, Confidence::High, 0, false),
        usage_with_five_hour(10.0, 10.0, 100.0, 0.0),
        &config,
    );
    assert_eq!(codex_verdict.provider, Provider::Codex);
    assert!(codex_verdict.gates.is_empty());

    let claude_verdict = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage_with_five_hour(10.0, 10.0, 100.0, 0.0),
        &config,
    );
    assert_eq!(claude_verdict.provider, Provider::Claude);
    assert!(claude_verdict.gates.is_empty());
}

/// A paced job is dispatched to Codex, so its model must be read from Codex's tiers at the same
/// complexity. The ultra case is the one that proves the re-derivation rather than a coincidence:
/// Claude's ultra tier is `fable`, which no Codex backend can resolve.
#[test]
fn a_paced_job_carries_the_codex_tier_for_its_complexity() {
    let config = Config::default();

    let low = decide(
        Classification {
            complexity: Complexity::Low,
            ..scored(Verdict::Claude, Confidence::High, 0, false)
        },
        usage_with_five_hour(10.0, 10.0, 0.0, 100.0),
        &config,
    );
    assert_eq!(low.provider, Provider::Codex);
    assert_eq!(low.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(low.gates, vec![Gate::FiveHourPacing]);

    let ultra = decide(
        Classification {
            complexity: Complexity::Ultra,
            ..scored(Verdict::Claude, Confidence::High, 0, false)
        },
        usage_with_five_hour(10.0, 10.0, 0.0, 100.0),
        &config,
    );
    assert_eq!(ultra.provider, Provider::Codex);
    assert_eq!(ultra.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(ultra.gates, vec![Gate::FiveHourPacing]);
}

/// The one reachable double fire, pinned in order. A borderline Codex verdict with codex weekly 50
/// and claude weekly 20 is a gap past `headroom_flip_gap`, so the tiebreak moves it to Claude;
/// Claude's five hour window then moves it straight back. The whole vector is asserted rather than
/// its members, because reordering the two rules changes the final provider silently: run the
/// pacing rule first and the tiebreak has the last word, leaving the job on Claude.
#[test]
fn a_headroom_tiebreak_to_claude_is_paced_straight_back_to_codex_carrying_both_tags() {
    let config = Config::default();
    let decision = decide(
        scored(Verdict::Codex, Confidence::Medium, 0, false),
        usage_with_five_hour(50.0, 20.0, 0.0, config.claude_five_hour_pacing_pct),
        &config,
    );

    assert_eq!(decision.provider, Provider::Codex);
    assert_eq!(
        decision.gates,
        vec![Gate::HeadroomTiebreak, Gate::FiveHourPacing]
    );
    assert_eq!(decision.model.as_deref(), Some("gpt-5.6-sol"));
}

/// The number that produced the flip travels with the tag. The five hour figure is in the rationale
/// unconditionally rather than only when pacing fires, because a conditional field is a second code
/// path that can disagree with the gate it is supposed to explain.
#[test]
fn five_hour_pacing_is_visible_in_the_rationale_and_the_gate_tags() {
    let config = Config::default();

    let paced = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage_with_five_hour(10.0, 10.0, 0.0, 90.0),
        &config,
    );
    assert!(paced.gate_tags().contains(&"five_hour_pacing"));
    assert!(
        paced.rationale.contains("five_hour_pacing"),
        "{}",
        paced.rationale
    );
    assert!(
        paced.rationale.contains("claude 5h 90%"),
        "{}",
        paced.rationale
    );

    let untouched = decide(
        scored(Verdict::Codex, Confidence::High, 0, false),
        usage_with_five_hour(10.0, 10.0, 100.0, 12.0),
        &config,
    );
    assert!(!untouched.gate_tags().contains(&"five_hour_pacing"));
    assert!(
        untouched.rationale.contains("claude 5h 12%"),
        "{}",
        untouched.rationale
    );
}

/// The threshold is one scalar key with its own default, so an operator can lower it without
/// restating every other ceiling. Writing the defaults and reading them back also proves the key is
/// declared above `policy`: a scalar after a table makes the TOML serializer fail on first run.
#[test]
fn the_pacing_threshold_defaults_when_absent_and_overrides_on_its_own() {
    assert_eq!(Config::default().claude_five_hour_pacing_pct, 90.0);

    let dir = tempfile::tempdir().expect("tempdir");
    let written_path = dir.path().join("written/config.toml");
    let created = Config::load_from(&written_path).expect("create the default config");
    assert_eq!(created.claude_five_hour_pacing_pct, 90.0);
    let document: toml::Value =
        toml::from_str(&std::fs::read_to_string(&written_path).expect("read the written config"))
            .expect("parse the written config");
    assert_eq!(
        document["claude_five_hour_pacing_pct"].as_float(),
        Some(90.0)
    );

    let path = dir.path().join("partial.toml");
    let config = load_config("claude_five_hour_pacing_pct = 55.0\n", &path)
        .expect("load the partial config");
    assert_eq!(config.claude_five_hour_pacing_pct, 55.0);
    assert_eq!(config.hard_ceiling_pct, Config::default().hard_ceiling_pct);
    assert_eq!(
        config.headroom_flip_gap,
        Config::default().headroom_flip_gap
    );
    assert!(config.policy.weekly_routing);
}
