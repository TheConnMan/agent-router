//! The decision engine: hard gates first, then headroom modulation. Pure given its inputs.

use crate::classify::{Classification, Confidence};
use crate::config::Config;
use crate::usage::UsageSnapshot;
use agent_viewer_core::BackendKind;

/// Claude bg jobs are pinned to opus[1m] by house policy, never chosen per task.
pub const CLAUDE_MODEL: &str = "opus[1m]";
/// Codex jobs leave the model to codex and run at the highest reasoning effort.
pub const CODEX_EFFORT: &str = "xhigh";

/// Everything that fired on the way to a provider, in the order it fired. These are the tuning
/// signal in the decision log, so each one names a specific rule rather than a generic reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// The caller named a provider, so no classification ran.
    ExplicitProvider,
    /// The classifier could not answer; the fallback verdict is in force.
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
        }
    }
}

/// One routing decision: where the task goes, with what model and effort, and why.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Decision {
    pub provider: BackendKind,
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
    let mut hard_gate = false;
    if classification.classifier_failed {
        gates.push(Gate::ClassifierFailed);
        hard_gate = true;
    }
    if classification.missing_connector {
        gates.push(Gate::MissingConnector);
        hard_gate = true;
    }
    if classification.claude_signal_count() >= 2 {
        gates.push(Gate::ClaudeSignals);
        hard_gate = true;
    }

    // A hard gate is Claude, full stop. Headroom modulates a verdict; it never overrides a gate.
    let mut provider = if hard_gate {
        BackendKind::Claude
    } else {
        classification.verdict.backend()
    };

    if !hard_gate {
        let other = other_provider(provider);
        let used = weekly_used(&usage, provider);
        let other_used = weekly_used(&usage, other);
        match classification.confidence {
            // A confident verdict survives any headroom difference except its own provider
            // being effectively exhausted while the other still has room.
            Confidence::High => {
                if used >= config.hard_ceiling_pct && other_used < config.hard_ceiling_pct {
                    provider = other;
                    gates.push(Gate::FlippedOnExhaustion);
                }
            }
            // A borderline verdict wins ties and small gaps; a large headroom gap flips it.
            Confidence::Medium | Confidence::Low => {
                if used - other_used > config.headroom_flip_gap {
                    provider = other;
                    gates.push(Gate::HeadroomTiebreak);
                }
            }
        }
    }

    if usage.claude.weekly_pct >= config.hard_ceiling_pct
        && usage.codex.weekly_pct >= config.hard_ceiling_pct
    {
        // The router routes; refusing work over a ceiling is bonus-drain's job, not this one.
        gates.push(Gate::OverCeiling);
    }

    let rationale = rationale(&classification, provider, &gates, &usage);
    Decision {
        provider,
        model: model_for(provider),
        effort: effort_for(provider),
        classification: Some(classification),
        gates,
        usage,
        rationale,
    }
}

/// PURE: the decision for a caller-named provider. No classification runs, but the usage
/// snapshot is still recorded, because the log is the tuning data for the auto path.
pub fn decide_explicit(
    provider: BackendKind,
    model: Option<String>,
    usage: UsageSnapshot,
) -> Decision {
    Decision {
        provider,
        model: model.or_else(|| model_for(provider)),
        effort: effort_for(provider),
        classification: None,
        gates: vec![Gate::ExplicitProvider],
        usage,
        rationale: format!("{} requested explicitly", provider.name()),
    }
}

/// PURE: the model policy. Claude bg jobs are always opus[1m]; codex and opencode resolve their
/// own defaults.
fn model_for(provider: BackendKind) -> Option<String> {
    match provider {
        BackendKind::Claude => Some(CLAUDE_MODEL.to_string()),
        BackendKind::Codex | BackendKind::Opencode => None,
    }
}

fn effort_for(provider: BackendKind) -> Option<String> {
    match provider {
        BackendKind::Codex => Some(CODEX_EFFORT.to_string()),
        BackendKind::Claude | BackendKind::Opencode => None,
    }
}

/// PURE: the other member of the Codex/Claude pair. opencode is explicit-only, so it is never
/// the counterparty of a headroom comparison and maps to Claude's side of the pair.
fn other_provider(provider: BackendKind) -> BackendKind {
    match provider {
        BackendKind::Codex => BackendKind::Claude,
        BackendKind::Claude | BackendKind::Opencode => BackendKind::Codex,
    }
}

fn weekly_used(usage: &UsageSnapshot, provider: BackendKind) -> f64 {
    match provider {
        BackendKind::Codex => usage.codex.weekly_pct,
        // opencode has no usage source in the MVP, so it reads as the Claude side it rides on.
        BackendKind::Claude | BackendKind::Opencode => usage.claude.weekly_pct,
    }
}

/// PURE: the one-line reason, the string the CLI prints and the viewer will show.
fn rationale(
    classification: &Classification,
    provider: BackendKind,
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
        want_provider: BackendKind,
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
                want_provider: BackendKind::Codex,
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
                want_provider: BackendKind::Codex,
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
                want_provider: BackendKind::Claude,
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
                want_provider: BackendKind::Codex,
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
                want_provider: BackendKind::Codex,
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
                want_provider: BackendKind::Codex,
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
                want_provider: BackendKind::Claude,
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
                want_provider: BackendKind::Claude,
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
                want_provider: BackendKind::Claude,
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
                want_provider: BackendKind::Codex,
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
                want_provider: BackendKind::Claude,
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
        assert_eq!(decision.provider, BackendKind::Claude);
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
        assert_eq!(decision.provider, BackendKind::Codex);
        assert!(!decision.gates.contains(&Gate::FlippedOnExhaustion));
        assert!(decision.gates.contains(&Gate::OverCeiling));
    }

    #[test]
    fn a_failed_classifier_routes_to_claude_and_records_the_failure() {
        let config = Config::default();
        let decision = decide(
            Classification::fallback("timed out after 30s"),
            // Claude exhausted, Codex empty: the fallback still goes to Claude, because a hard
            // gate is not a preference that headroom gets to overrule.
            usage(0.0, 99.0),
            &config,
        );
        assert_eq!(decision.provider, BackendKind::Claude);
        assert_eq!(decision.gate_tags(), vec!["classifier_failed"]);
        assert_eq!(decision.model.as_deref(), Some(CLAUDE_MODEL));
    }

    #[test]
    fn model_and_effort_follow_the_provider_not_the_task() {
        let config = Config::default();
        let codex = decide(
            scored(Verdict::Codex, Confidence::High, 0, false),
            usage(10.0, 10.0),
            &config,
        );
        assert_eq!(codex.model, None, "codex resolves its own model");
        assert_eq!(codex.effort.as_deref(), Some("xhigh"));

        let claude = decide(
            scored(Verdict::Claude, Confidence::High, 0, false),
            usage(10.0, 10.0),
            &config,
        );
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
        assert_eq!(claude.effort, None);
    }

    #[test]
    fn an_explicit_provider_skips_classification_but_keeps_the_usage_snapshot() {
        let decision = decide_explicit(BackendKind::Opencode, None, usage(71.0, 50.0));
        assert_eq!(decision.provider, BackendKind::Opencode);
        assert!(decision.classification.is_none());
        assert_eq!(decision.gate_tags(), vec!["explicit_provider"]);
        assert_eq!(decision.usage.codex.weekly_pct, 71.0);
        assert_eq!(decision.usage.claude.weekly_pct, 50.0);
        assert_eq!(decision.model, None);

        // An explicitly requested model overrides the per-provider default.
        let pinned = decide_explicit(
            BackendKind::Claude,
            Some("sonnet".to_string()),
            usage(0.0, 0.0),
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
