//! The decision engine: hard gates first, then headroom modulation. Pure given its inputs.

use crate::classify::{Classification, Complexity, Confidence};
use crate::config::{Config, DefaultProvider};
use crate::provider::Provider;
use crate::usage::UsageSnapshot;

/// The complexity an unscored task runs at: an explicitly named provider skips classification, so
/// there is no judgement to scale from.
const UNSCORED_COMPLEXITY: Complexity = Complexity::Standard;

/// Everything that fired on the way to a provider, in the order it fired. These are the tuning
/// signal in the decision log, so each one names a specific rule rather than a generic reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// The caller named a provider, so no classification ran.
    ExplicitProvider,
    /// The classifier could not answer, so the configured default remains in force.
    ClassifierFailed,
    /// A required connector is missing: an automatic Claude decision regardless of shape.
    MissingConnector,
    /// Two or more Claude signals held.
    ClaudeSignals,
    /// A confident verdict was flipped because its provider is exhausted and the other is not.
    FlippedOnExhaustion,
    /// A borderline verdict was flipped by the weekly-headroom gap.
    HeadroomTiebreak,
    /// Both providers are at or over the hard ceiling; the verdict provider was used anyway.
    OverCeiling,
    /// Weekly usage routing is disabled by policy.
    WeeklyRoutingDisabled,
    /// Weekly usage changed the provider while at least one output stayed with the prior choice.
    UsageFailoverPinned,
}

impl Gate {
    pub fn tag(self) -> &'static str {
        match self {
            Gate::ExplicitProvider => "explicit_provider",
            Gate::ClassifierFailed => "classifier_failed",
            Gate::MissingConnector => "missing_connector",
            Gate::ClaudeSignals => "claude_signals",
            Gate::FlippedOnExhaustion => "flipped_on_exhaustion",
            Gate::HeadroomTiebreak => "headroom_tiebreak",
            Gate::OverCeiling => "over_ceiling",
            Gate::WeeklyRoutingDisabled => "weekly_routing_disabled",
            Gate::UsageFailoverPinned => "usage_failover_pinned",
        }
    }
}

/// One routing decision: where the task goes, with what model and effort, and why.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Decision {
    pub provider: Provider,
    /// The model to spawn with. None means the backend resolves its own default.
    pub model: Option<String>,
    /// Codex reasoning effort. None for every other provider.
    pub effort: Option<String>,
    /// None when the caller named a provider and no classification ran.
    pub classification: Option<Classification>,
    pub gates: Vec<Gate>,
    pub usage: UsageSnapshot,
    pub rationale: String,
}

impl Decision {
    pub fn gate_tags(&self) -> Vec<&'static str> {
        self.gates.iter().map(|gate| gate.tag()).collect()
    }
}

/// PURE: the routing decision for a scored task.
pub fn decide(classification: Classification, usage: UsageSnapshot, config: &Config) -> Decision {
    let mut gates = Vec::new();
    let mut capability_pin = false;
    if classification.missing_connector {
        gates.push(Gate::MissingConnector);
        capability_pin = true;
    }
    if classification.claude_signal_count() >= 2 {
        gates.push(Gate::ClaudeSignals);
        capability_pin = true;
    }

    let mut provider = match config.policy.default_provider {
        DefaultProvider::Codex => Provider::Codex,
        DefaultProvider::Claude => Provider::Claude,
    };
    if capability_pin {
        provider = Provider::Claude;
    } else if classification.classifier_failed {
        gates.push(Gate::ClassifierFailed);
    } else {
        provider = classification.verdict.provider();
    }

    let pre_usage_provider = provider;
    let complexity = classification.complexity;
    let mut model = model_for(pre_usage_provider, complexity, config);
    let mut effort = effort_for(pre_usage_provider, complexity, config);
    let both_over_ceiling = usage.claude.weekly_pct >= config.hard_ceiling_pct
        && usage.codex.weekly_pct >= config.hard_ceiling_pct;

    if !capability_pin {
        if !config.policy.weekly_routing {
            gates.push(Gate::WeeklyRoutingDisabled);
        } else if !both_over_ceiling {
            let other = other_provider(provider);
            let used = weekly_used(&usage, provider);
            let other_used = weekly_used(&usage, other);
            let confidence = if classification.classifier_failed {
                Confidence::High
            } else {
                classification.confidence
            };
            match confidence {
                Confidence::High => {
                    if used >= config.hard_ceiling_pct && other_used < config.hard_ceiling_pct {
                        provider = other;
                        gates.push(Gate::FlippedOnExhaustion);
                    }
                }
                Confidence::Medium | Confidence::Low => {
                    if used - other_used > config.headroom_flip_gap {
                        provider = other;
                        gates.push(Gate::HeadroomTiebreak);
                    }
                }
            }
        }
    }

    if provider != pre_usage_provider {
        // The task did not get simpler by moving, so the new provider's tiers are read at the
        // same complexity rather than the old provider's model or effort being carried across.
        if config.policy.usage_failover_changes_model {
            model = model_for(provider, complexity, config);
        }
        if config.policy.usage_failover_changes_effort {
            effort = effort_for(provider, complexity, config);
        }
        if !config.policy.usage_failover_changes_model
            || !config.policy.usage_failover_changes_effort
        {
            gates.push(Gate::UsageFailoverPinned);
        }
    }

    if both_over_ceiling {
        // The router routes; refusing work over a ceiling is bonus drain's job, not this one.
        gates.push(Gate::OverCeiling);
    }

    let rationale = rationale(&classification, provider, &gates, &usage);
    Decision {
        provider,
        model,
        effort,
        classification: Some(classification),
        gates,
        usage,
        rationale,
    }
}

/// PURE: the decision for a caller-named provider. No classification runs, but the usage
/// snapshot is still recorded, because the log is the tuning data for the auto path.
pub fn decide_explicit(
    provider: Provider,
    model: Option<String>,
    usage: UsageSnapshot,
    config: &Config,
) -> Decision {
    Decision {
        provider,
        model: model.or_else(|| model_for(provider, UNSCORED_COMPLEXITY, config)),
        effort: effort_for(provider, UNSCORED_COMPLEXITY, config),
        classification: None,
        gates: vec![Gate::ExplicitProvider],
        usage,
        rationale: format!("{} requested explicitly", provider.name()),
    }
}

/// PURE: the model the job runs at, scaled by how much reasoning the task needs. Opencode has no
/// tiers in the MVP, so it resolves its own default.
fn model_for(provider: Provider, complexity: Complexity, config: &Config) -> Option<String> {
    match provider {
        Provider::Codex => Some(config.models.codex.pick(complexity).to_string()),
        Provider::Claude => Some(config.models.claude.pick(complexity).to_string()),
        Provider::Opencode => None,
    }
}

/// PURE: the reasoning effort, from the same complexity as the model.
fn effort_for(provider: Provider, complexity: Complexity, config: &Config) -> Option<String> {
    match provider {
        Provider::Codex => Some(config.effort.codex.pick(complexity).to_string()),
        Provider::Claude => Some(config.effort.claude.pick(complexity).to_string()),
        Provider::Opencode => None,
    }
}

/// PURE: the other member of the Codex/Claude pair. opencode is explicit-only, so it is never
/// the counterparty of a headroom comparison and maps to Claude's side of the pair.
fn other_provider(provider: Provider) -> Provider {
    match provider {
        Provider::Codex => Provider::Claude,
        Provider::Claude | Provider::Opencode => Provider::Codex,
    }
}

fn weekly_used(usage: &UsageSnapshot, provider: Provider) -> f64 {
    match provider {
        Provider::Codex => usage.codex.weekly_pct,
        // opencode has no usage source in the MVP, so it reads as the Claude side it rides on.
        Provider::Claude | Provider::Opencode => usage.claude.weekly_pct,
    }
}

/// PURE: the one-line reason, the string the CLI prints and the viewer will show.
fn rationale(
    classification: &Classification,
    provider: Provider,
    gates: &[Gate],
    usage: &UsageSnapshot,
) -> String {
    let tags = if gates.is_empty() {
        String::new()
    } else {
        format!(
            " [{}]",
            gates
                .iter()
                .map(|gate| gate.tag())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "{}: {}{tags} (codex_ready {}/6, claude_signals {}/6, {:?} confidence; codex weekly {:.0}%, claude weekly {:.0}%)",
        provider.name(),
        classification.rationale,
        classification.codex_ready_count(),
        classification.claude_signal_count(),
        classification.confidence,
        usage.codex.weekly_pct,
        usage.claude.weekly_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Verdict;
    use crate::usage::Headroom;

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
        for slot in signals.iter_mut().take(claude_signals) {
            *slot = true;
        }
        Classification {
            codex_ready: [true; 6],
            claude_signals: signals,
            missing_connector,
            verdict,
            confidence,
            complexity: Complexity::Standard,
            rationale: "fixture".to_string(),
            classifier_failed: false,
        }
    }

    /// One row of the decision matrix: what the classifier said, what both weekly numbers
    /// were, and where the engine must land.
    struct Case {
        label: &'static str,
        verdict: Verdict,
        confidence: Confidence,
        claude_signals: usize,
        missing_connector: bool,
        codex_weekly: f64,
        claude_weekly: f64,
        want_provider: Provider,
        want_gates: Vec<Gate>,
    }

    #[test]
    fn the_classification_matrix_over_headroom_combinations() {
        let config = Config::default();
        let cases = vec![
            Case {
                label: "a confident codex verdict with room on both sides is untouched",
                verdict: Verdict::Codex,
                confidence: Confidence::High,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 60.0,
                claude_weekly: 10.0,
                want_provider: Provider::Codex,
                want_gates: vec![],
            },
            Case {
                label: "even a huge headroom gap does not move a confident verdict",
                verdict: Verdict::Codex,
                confidence: Confidence::High,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 90.0,
                claude_weekly: 5.0,
                want_provider: Provider::Codex,
                want_gates: vec![],
            },
            Case {
                label: "exhausted verdict provider with room on the other flips",
                verdict: Verdict::Codex,
                confidence: Confidence::High,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 97.0,
                claude_weekly: 40.0,
                want_provider: Provider::Claude,
                want_gates: vec![Gate::FlippedOnExhaustion, Gate::UsageFailoverPinned],
            },
            Case {
                label: "the exhaustion flip works in the other direction too",
                verdict: Verdict::Claude,
                confidence: Confidence::High,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 40.0,
                claude_weekly: 99.0,
                want_provider: Provider::Codex,
                want_gates: vec![Gate::FlippedOnExhaustion, Gate::UsageFailoverPinned],
            },
            Case {
                label: "a borderline verdict wins a small gap",
                verdict: Verdict::Codex,
                confidence: Confidence::Medium,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 50.0,
                claude_weekly: 30.0,
                want_provider: Provider::Codex,
                want_gates: vec![],
            },
            Case {
                label: "a gap exactly at the flip threshold stays with the verdict",
                verdict: Verdict::Codex,
                confidence: Confidence::Medium,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 55.0,
                claude_weekly: 30.0,
                want_provider: Provider::Codex,
                want_gates: vec![],
            },
            Case {
                label: "a gap past the threshold flips a borderline verdict",
                verdict: Verdict::Codex,
                confidence: Confidence::Low,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 80.0,
                claude_weekly: 30.0,
                want_provider: Provider::Claude,
                want_gates: vec![Gate::HeadroomTiebreak, Gate::UsageFailoverPinned],
            },
            Case {
                label: "two claude signals force claude even with claude exhausted",
                verdict: Verdict::Codex,
                confidence: Confidence::High,
                claude_signals: 2,
                missing_connector: false,
                codex_weekly: 0.0,
                claude_weekly: 99.0,
                want_provider: Provider::Claude,
                want_gates: vec![Gate::ClaudeSignals],
            },
            Case {
                label: "a missing connector forces claude regardless of shape",
                verdict: Verdict::Codex,
                confidence: Confidence::High,
                claude_signals: 0,
                missing_connector: true,
                codex_weekly: 10.0,
                claude_weekly: 95.0,
                want_provider: Provider::Claude,
                want_gates: vec![Gate::MissingConnector],
            },
            Case {
                label: "both over the ceiling dispatches to the verdict provider anyway",
                verdict: Verdict::Codex,
                confidence: Confidence::High,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 98.0,
                claude_weekly: 99.0,
                want_provider: Provider::Codex,
                want_gates: vec![Gate::OverCeiling],
            },
            Case {
                label: "both over the ceiling on a borderline verdict with a small gap",
                verdict: Verdict::Claude,
                confidence: Confidence::Low,
                claude_signals: 0,
                missing_connector: false,
                codex_weekly: 98.0,
                claude_weekly: 99.0,
                want_provider: Provider::Claude,
                want_gates: vec![Gate::OverCeiling],
            },
        ];
        for case in cases {
            let classification = scored(
                case.verdict,
                case.confidence,
                case.claude_signals,
                case.missing_connector,
            );
            let decision = decide(
                classification,
                usage(case.codex_weekly, case.claude_weekly),
                &config,
            );
            assert_eq!(
                decision.provider, case.want_provider,
                "provider for {}",
                case.label
            );
            assert_eq!(decision.gates, case.want_gates, "gates for {}", case.label);
        }
    }

    /// The mutation target: the exhaustion flip compares weekly used against the ceiling with
    /// `>=`, and this case sits exactly ON the ceiling. Turning that `>=` into `>` (or dropping
    /// the `other_used < ceiling` guard, which this case's 40% satisfies) makes it fail.
    #[test]
    fn a_confident_verdict_flips_when_its_provider_sits_exactly_on_the_ceiling() {
        let config = Config::default();
        let decision = decide(
            scored(Verdict::Codex, Confidence::High, 0, false),
            usage(config.hard_ceiling_pct, 40.0),
            &config,
        );
        assert_eq!(decision.provider, Provider::Claude);
        assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
        assert!(decision.gates.contains(&Gate::UsageFailoverPinned));
    }

    #[test]
    fn an_exhausted_verdict_provider_does_not_flip_when_the_other_is_exhausted_too() {
        let config = Config::default();
        let decision = decide(
            scored(Verdict::Codex, Confidence::High, 0, false),
            usage(98.0, 97.5),
            &config,
        );
        assert_eq!(decision.provider, Provider::Codex);
        assert!(!decision.gates.contains(&Gate::FlippedOnExhaustion));
        assert!(decision.gates.contains(&Gate::OverCeiling));
    }

    #[test]
    fn a_failed_classifier_retains_the_configured_codex_default() {
        let config = Config::default();
        let decision = decide(
            Classification::fallback("timed out after 30s", DefaultProvider::Codex),
            // Claude exhaustion cannot move the configured Codex fallback.
            usage(0.0, 99.0),
            &config,
        );
        assert_eq!(decision.provider, Provider::Codex);
        assert_eq!(decision.gate_tags(), vec!["classifier_failed"]);
        // A fallback carries no complexity of its own, so it runs at the standard tier.
        assert_eq!(decision.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(decision.effort.as_deref(), Some("medium"));
    }

    /// The scoring fixture at a named complexity, with nothing else that could move the provider.
    fn at(verdict: Verdict, complexity: Complexity) -> Classification {
        Classification {
            complexity,
            ..scored(verdict, Confidence::High, 0, false)
        }
    }

    /// The whole point of the feature: both outputs come from the complexity, for both providers.
    /// Twelve cells, six per provider.
    #[test]
    fn the_model_and_effort_matrix_over_complexity_and_provider() {
        let config = Config::default();
        let cases = [
            (Verdict::Codex, Complexity::Trivial, "gpt-5.6-luna", "low"),
            (
                Verdict::Codex,
                Complexity::Standard,
                "gpt-5.6-terra",
                "medium",
            ),
            (Verdict::Codex, Complexity::Hard, "gpt-5.6-sol", "xhigh"),
            (Verdict::Claude, Complexity::Trivial, "sonnet", "low"),
            (Verdict::Claude, Complexity::Standard, "opus[1m]", "high"),
            (Verdict::Claude, Complexity::Hard, "opus[1m]", "xhigh"),
        ];
        for (verdict, complexity, model, effort) in cases {
            let decision = decide(at(verdict, complexity), usage(10.0, 10.0), &config);
            assert_eq!(decision.provider, verdict.provider());
            assert_eq!(
                decision.model.as_deref(),
                Some(model),
                "model for {verdict:?} at {complexity:?}"
            );
            assert_eq!(
                decision.effort.as_deref(),
                Some(effort),
                "effort for {verdict:?} at {complexity:?}"
            );
        }
    }

    /// A usage flip re-derives both outputs from the NEW provider's tiers at the SAME complexity,
    /// rather than carrying the old provider's model or effort across the flip.
    #[test]
    fn a_usage_failover_rederives_the_new_providers_tier_at_the_same_complexity() {
        let mut config = Config::default();
        config.policy.usage_failover_changes_model = true;
        config.policy.usage_failover_changes_effort = true;

        // Codex verdict, codex exhausted: lands on claude's trivial tier, not codex's.
        let to_claude = decide(
            at(Verdict::Codex, Complexity::Trivial),
            usage(99.0, 0.0),
            &config,
        );
        assert_eq!(to_claude.provider, Provider::Claude);
        assert_eq!(to_claude.model.as_deref(), Some("sonnet"));
        assert_eq!(to_claude.effort.as_deref(), Some("low"));
        assert_eq!(to_claude.gates, vec![Gate::FlippedOnExhaustion]);

        // The other direction at a different complexity: claude verdict, claude exhausted.
        let to_codex = decide(
            at(Verdict::Claude, Complexity::Hard),
            usage(0.0, 99.0),
            &config,
        );
        assert_eq!(to_codex.provider, Provider::Codex);
        assert_eq!(to_codex.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(to_codex.effort.as_deref(), Some("xhigh"));
    }

    /// With the failover flags off, both outputs stay with the pre-flip provider's tier, which is
    /// what `usage_failover_pinned` reports.
    #[test]
    fn a_pinned_usage_failover_keeps_the_prior_providers_tier() {
        let config = Config::default();
        let decision = decide(
            at(Verdict::Codex, Complexity::Trivial),
            usage(99.0, 0.0),
            &config,
        );
        assert_eq!(decision.provider, Provider::Claude);
        assert_eq!(decision.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(decision.effort.as_deref(), Some("low"));
        assert!(decision.gates.contains(&Gate::UsageFailoverPinned));
    }

    #[test]
    fn configured_tiers_override_the_built_in_defaults() {
        let mut config = Config::default();
        config.models.codex.trivial = "gpt-5.6-custom".to_string();
        config.effort.claude.hard = "max".to_string();

        let codex = decide(
            at(Verdict::Codex, Complexity::Trivial),
            usage(10.0, 10.0),
            &config,
        );
        assert_eq!(codex.model.as_deref(), Some("gpt-5.6-custom"));

        let claude = decide(
            at(Verdict::Claude, Complexity::Hard),
            usage(10.0, 10.0),
            &config,
        );
        assert_eq!(claude.effort.as_deref(), Some("max"));
    }

    /// An unscored task has no complexity to read, so it runs at the standard tier.
    #[test]
    fn an_explicit_provider_runs_at_the_standard_tier() {
        let config = Config::default();
        let codex = decide_explicit(Provider::Codex, None, usage(0.0, 0.0), &config);
        assert_eq!(codex.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(codex.effort.as_deref(), Some("medium"));

        let claude = decide_explicit(Provider::Claude, None, usage(0.0, 0.0), &config);
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
        assert_eq!(claude.effort.as_deref(), Some("high"));
    }

    #[test]
    fn an_explicit_provider_skips_classification_but_keeps_the_usage_snapshot() {
        let config = Config::default();
        let decision = decide_explicit(Provider::Opencode, None, usage(71.0, 50.0), &config);
        assert_eq!(decision.provider, Provider::Opencode);
        assert!(decision.classification.is_none());
        assert_eq!(decision.gate_tags(), vec!["explicit_provider"]);
        assert_eq!(decision.usage.codex.weekly_pct, 71.0);
        assert_eq!(decision.usage.claude.weekly_pct, 50.0);
        assert_eq!(decision.model, None);

        // An explicitly requested model overrides the per-provider default.
        let pinned = decide_explicit(
            Provider::Claude,
            Some("sonnet".to_string()),
            usage(0.0, 0.0),
            &config,
        );
        assert_eq!(pinned.model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn the_rationale_names_the_provider_the_gates_and_both_weekly_numbers() {
        let config = Config::default();
        let decision = decide(
            scored(Verdict::Codex, Confidence::High, 0, true),
            usage(71.0, 50.0),
            &config,
        );
        assert!(
            decision.rationale.starts_with("claude: "),
            "{}",
            decision.rationale
        );
        assert!(decision.rationale.contains("missing_connector"));
        assert!(decision.rationale.contains("codex weekly 71%"));
        assert!(decision.rationale.contains("claude weekly 50%"));
    }
}
