//! The decision engine: hard gates first, then headroom modulation. Pure given its inputs.

use crate::classify::{Classification, Complexity, Confidence};
use crate::config::{Config, DefaultProvider};
use crate::provider::Provider;
use crate::usage::UsageSnapshot;

/// The complexity an unscored task runs at: an explicitly named provider skips classification, so
/// there is no judgement to scale from, and unscored work errs toward capability.
const UNSCORED_COMPLEXITY: Complexity = Complexity::High;

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
        }
    }
}

/// One routing decision: where the task goes, with what model and effort, and why.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Decision {
    pub provider: Provider,
    /// The model to spawn with. None means the backend resolves its own default.
    pub model: Option<String>,
    /// The reasoning effort to force. Always None: the model tier is the toggle, so the router
    /// decides no effort at all and each backend resolves its own. That resolution is not the
    /// same on both. Claude runs at the model's own default, because nothing else sets one. Codex
    /// runs at whatever `~/.codex/config.toml` resolves, because dispatch goes through
    /// `codex app-server daemon`, which loads user config; only when that file names no
    /// `model_reasoning_effort` does a codex job fall through to the model's catalogue default.
    /// So the effort a codex job runs at is neither decided nor recorded here.
    ///
    /// Dispatch still honours a value if one is set, on codex (`turn/start` `effort`) and on
    /// claude (`--effort`). Opencode discards it.
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
        // The job is dispatched to the provider it landed on, so its model is read from that
        // provider's tiers. The task did not get simpler by moving, so the complexity is the
        // same one; only the provider changed. Carrying the prior provider's model across would
        // hand a backend a name it cannot resolve, since no model exists on both.
        model = model_for(provider, complexity, config);
    }

    if both_over_ceiling {
        // The router routes; refusing work over a ceiling is bonus drain's job, not this one.
        gates.push(Gate::OverCeiling);
    }

    let rationale = rationale(&classification, provider, &gates, &usage);
    Decision {
        provider,
        model,
        effort: None,
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
        effort: None,
        classification: None,
        gates: vec![Gate::ExplicitProvider],
        usage,
        rationale: format!("{} requested explicitly", provider.name()),
    }
}

/// PURE: the model the job runs on, scaled by how much reasoning the task needs. This is the only
/// tier lever: no reasoning effort is decided at all, so each backend resolves its own. See the
/// `effort` field on `Decision` for what each one resolves it to, which is not the model default
/// on codex. Opencode has no tiers in the MVP, so it resolves its own default.
fn model_for(provider: Provider, complexity: Complexity, config: &Config) -> Option<String> {
    match provider {
        Provider::Codex => Some(config.models.codex.pick(complexity).to_string()),
        Provider::Claude => Some(config.models.claude.pick(complexity).to_string()),
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
            complexity: Complexity::High,
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
                want_gates: vec![Gate::FlippedOnExhaustion],
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
                want_gates: vec![Gate::FlippedOnExhaustion],
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
                want_gates: vec![Gate::HeadroomTiebreak],
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
        // A fallback carries no complexity of its own, so it runs at the high tier.
        assert_eq!(decision.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(decision.effort, None);
    }

    /// The scoring fixture at a named complexity, with nothing else that could move the provider.
    fn at(verdict: Verdict, complexity: Complexity) -> Classification {
        Classification {
            complexity,
            ..scored(verdict, Confidence::High, 0, false)
        }
    }

    /// The whole point of the feature: the model comes from the complexity, for both providers.
    /// Eight cells, four per provider. Effort is never decided, so every cell leaves it None and
    /// the backend resolves its own.
    #[test]
    fn the_model_matrix_over_complexity_and_provider() {
        let config = Config::default();
        let cases = [
            (Verdict::Codex, Complexity::Low, "gpt-5.6-luna"),
            (Verdict::Codex, Complexity::Medium, "gpt-5.6-terra"),
            (Verdict::Codex, Complexity::High, "gpt-5.6-sol"),
            (Verdict::Codex, Complexity::Ultra, "gpt-5.6-sol"),
            (Verdict::Claude, Complexity::Low, "sonnet"),
            (Verdict::Claude, Complexity::Medium, "opus[1m]"),
            (Verdict::Claude, Complexity::High, "opus[1m]"),
            (Verdict::Claude, Complexity::Ultra, "fable"),
        ];
        for (verdict, complexity, model) in cases {
            let decision = decide(at(verdict, complexity), usage(10.0, 10.0), &config);
            assert_eq!(decision.provider, verdict.provider());
            assert_eq!(
                decision.model.as_deref(),
                Some(model),
                "model for {verdict:?} at {complexity:?}"
            );
            assert_eq!(
                decision.effort, None,
                "effort must stay with the model default for {verdict:?} at {complexity:?}"
            );
        }
    }

    /// A usage flip re-derives the model from the NEW provider's tiers at the SAME complexity,
    /// rather than carrying the old provider's model across the flip.
    #[test]
    fn a_usage_failover_rederives_the_new_providers_tier_at_the_same_complexity() {
        let config = Config::default();

        // Codex verdict, codex exhausted: lands on claude's low tier, not codex's.
        let to_claude = decide(
            at(Verdict::Codex, Complexity::Low),
            usage(99.0, 0.0),
            &config,
        );
        assert_eq!(to_claude.provider, Provider::Claude);
        assert_eq!(to_claude.model.as_deref(), Some("sonnet"));
        assert_eq!(to_claude.gates, vec![Gate::FlippedOnExhaustion]);

        // The other direction at the top tier: claude ultra is fable, codex ultra is sol.
        let to_codex = decide(
            at(Verdict::Claude, Complexity::Ultra),
            usage(0.0, 99.0),
            &config,
        );
        assert_eq!(to_codex.provider, Provider::Codex);
        assert_eq!(to_codex.model.as_deref(), Some("gpt-5.6-sol"));
    }

    /// The regression this fix exists for: a flipped job must never be dispatched with the model
    /// of the provider it just left, because no model name is valid on both backends.
    #[test]
    fn a_flipped_job_never_carries_the_other_providers_model() {
        let config = Config::default();
        let to_claude = decide(
            at(Verdict::Codex, Complexity::Low),
            usage(99.0, 0.0),
            &config,
        );
        assert_eq!(to_claude.provider, Provider::Claude);
        assert_eq!(to_claude.model.as_deref(), Some("sonnet"));
        assert_eq!(to_claude.gates, vec![Gate::FlippedOnExhaustion]);

        let to_codex = decide(
            at(Verdict::Claude, Complexity::Low),
            usage(0.0, 99.0),
            &config,
        );
        assert_eq!(to_codex.provider, Provider::Codex);
        assert_eq!(to_codex.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(to_codex.gates, vec![Gate::FlippedOnExhaustion]);
    }

    #[test]
    fn configured_tiers_override_the_built_in_defaults() {
        let mut config = Config::default();
        config.models.codex.low = "gpt-5.6-custom".to_string();
        config.models.claude.ultra = "opus[1m]".to_string();

        let codex = decide(
            at(Verdict::Codex, Complexity::Low),
            usage(10.0, 10.0),
            &config,
        );
        assert_eq!(codex.model.as_deref(), Some("gpt-5.6-custom"));

        let claude = decide(
            at(Verdict::Claude, Complexity::Ultra),
            usage(10.0, 10.0),
            &config,
        );
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
    }

    /// An unscored task has no complexity to read, so it runs at the high tier.
    #[test]
    fn an_explicit_provider_runs_at_the_high_tier() {
        let config = Config::default();
        let codex = decide_explicit(Provider::Codex, None, usage(0.0, 0.0), &config);
        assert_eq!(codex.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(codex.effort, None);

        let claude = decide_explicit(Provider::Claude, None, usage(0.0, 0.0), &config);
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
        assert_eq!(claude.effort, None);
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
