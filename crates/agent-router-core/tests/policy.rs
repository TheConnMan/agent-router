use agent_router_core::classify::{Classification, Confidence, Verdict};
use agent_router_core::config::{Config, DefaultProvider};
use agent_router_core::decide::{CLAUDE_MODEL, CODEX_EFFORT, Gate, decide, decide_explicit};
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
    assert!(!created.policy.usage_failover_changes_model);
    assert!(!created.policy.usage_failover_changes_effort);
    assert!(created.parity.roots.is_empty());
    assert!(created.parity.exceptions.is_empty());
    assert_eq!(
        document["policy"]["default_provider"].as_str(),
        Some("codex")
    );
    assert_eq!(document["policy"]["weekly_routing"].as_bool(), Some(true));
    assert_eq!(
        document["policy"]["usage_failover_changes_model"].as_bool(),
        Some(false)
    );
    assert_eq!(
        document["policy"]["usage_failover_changes_effort"].as_bool(),
        Some(false)
    );
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
    assert!(!config.policy.usage_failover_changes_model);
    assert!(!config.policy.usage_failover_changes_effort);
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
    assert_eq!(decision.model.as_deref(), Some(CLAUDE_MODEL));
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
    assert_eq!(decision.model, None);
    assert_eq!(decision.effort.as_deref(), Some(CODEX_EFFORT));
    assert_eq!(
        decision.gates,
        vec![
            Gate::ClassifierFailed,
            Gate::FlippedOnExhaustion,
            Gate::UsageFailoverPinned,
        ]
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
    assert_eq!(missing_connector.model.as_deref(), Some(CLAUDE_MODEL));
    assert_eq!(missing_connector.effort, None);

    let claude_signals = decide(
        scored(Verdict::Codex, Confidence::High, 2, false),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(claude_signals.provider, Provider::Claude);
    assert_eq!(claude_signals.gates, vec![Gate::ClaudeSignals]);
    assert_eq!(claude_signals.model.as_deref(), Some(CLAUDE_MODEL));
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
    assert_eq!(codex.model, None);
    assert_eq!(codex.effort.as_deref(), Some(CODEX_EFFORT));

    let claude = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(claude.provider, Provider::Claude);
    assert_eq!(claude.gates, vec![Gate::WeeklyRoutingDisabled]);
    assert_eq!(claude.model.as_deref(), Some(CLAUDE_MODEL));
    assert_eq!(claude.effort, None);
}

#[test]
fn weekly_failover_pins_both_outputs_in_both_directions_by_default() {
    let config = Config::default();

    let to_claude = decide(
        scored(Verdict::Codex, Confidence::High, 0, false),
        usage(99.0, 0.0),
        &config,
    );
    assert_eq!(to_claude.provider, Provider::Claude);
    assert_eq!(to_claude.model, None);
    assert_eq!(to_claude.effort.as_deref(), Some(CODEX_EFFORT));
    assert_eq!(
        to_claude.gates,
        vec![Gate::FlippedOnExhaustion, Gate::UsageFailoverPinned]
    );

    let to_codex = decide(
        scored(Verdict::Claude, Confidence::High, 0, false),
        usage(0.0, 99.0),
        &config,
    );
    assert_eq!(to_codex.provider, Provider::Codex);
    assert_eq!(to_codex.model.as_deref(), Some(CLAUDE_MODEL));
    assert_eq!(to_codex.effort, None);
    assert_eq!(
        to_codex.gates,
        vec![Gate::FlippedOnExhaustion, Gate::UsageFailoverPinned]
    );
}

#[test]
fn model_and_effort_failover_flags_are_independent() {
    struct Case {
        model_changes: bool,
        effort_changes: bool,
        expected_model: Option<&'static str>,
        expected_effort: Option<&'static str>,
        pinned: bool,
    }

    let cases = [
        Case {
            model_changes: false,
            effort_changes: false,
            expected_model: None,
            expected_effort: Some(CODEX_EFFORT),
            pinned: true,
        },
        Case {
            model_changes: true,
            effort_changes: false,
            expected_model: Some(CLAUDE_MODEL),
            expected_effort: Some(CODEX_EFFORT),
            pinned: true,
        },
        Case {
            model_changes: false,
            effort_changes: true,
            expected_model: None,
            expected_effort: None,
            pinned: true,
        },
        Case {
            model_changes: true,
            effort_changes: true,
            expected_model: Some(CLAUDE_MODEL),
            expected_effort: None,
            pinned: false,
        },
    ];

    for case in cases {
        let mut config = Config::default();
        config.policy.usage_failover_changes_model = case.model_changes;
        config.policy.usage_failover_changes_effort = case.effort_changes;

        let decision = decide(
            scored(Verdict::Codex, Confidence::High, 0, false),
            usage(99.0, 0.0),
            &config,
        );

        assert_eq!(decision.provider, Provider::Claude);
        assert_eq!(decision.model.as_deref(), case.expected_model);
        assert_eq!(decision.effort.as_deref(), case.expected_effort);
        assert_eq!(
            decision.gates,
            if case.pinned {
                vec![Gate::FlippedOnExhaustion, Gate::UsageFailoverPinned]
            } else {
                vec![Gate::FlippedOnExhaustion]
            }
        );
    }
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

    let codex = decide_explicit(Provider::Codex, None, snapshot);
    assert_eq!(codex.provider, Provider::Codex);
    assert_eq!(codex.model, None);
    assert_eq!(codex.effort.as_deref(), Some(CODEX_EFFORT));
    assert_eq!(codex.gates, vec![Gate::ExplicitProvider]);
    assert!(codex.classification.is_none());

    let claude = decide_explicit(Provider::Claude, Some("sonnet".to_string()), snapshot);
    assert_eq!(claude.provider, Provider::Claude);
    assert_eq!(claude.model.as_deref(), Some("sonnet"));
    assert_eq!(claude.effort, None);
    assert_eq!(claude.gates, vec![Gate::ExplicitProvider]);
    assert!(claude.classification.is_none());

    let opencode = decide_explicit(Provider::Opencode, None, snapshot);
    assert_eq!(opencode.provider, Provider::Opencode);
    assert_eq!(opencode.model, None);
    assert_eq!(opencode.effort, None);
    assert_eq!(opencode.gates, vec![Gate::ExplicitProvider]);
    assert!(opencode.classification.is_none());
}
