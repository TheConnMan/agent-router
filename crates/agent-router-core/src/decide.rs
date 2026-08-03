//! The decision engine: the capability pin first, then the hard ceiling, then a rare run rate
//! override, then Claude's five hour window pacing a stream of jobs away from Claude. Pure given
//! its inputs, the deciding instant included.

use crate::classify::{Classification, Complexity};
use crate::config::{Config, DefaultProvider};
use crate::provider::Provider;
use crate::usage::{Headroom, UsageSnapshot};

/// The complexity an unscored task runs at: an explicitly named provider skips classification, so
/// there is no judgement to scale from, and unscored work errs toward capability.
const UNSCORED_COMPLEXITY: Complexity = Complexity::High;

/// The weekly window, in seconds. 10080 minutes, the same window `usage.rs` identifies a weekly
/// rate limit by. Reset epochs are recorded in SECONDS on both providers, so a distance to a reset
/// is directly comparable to this.
const WEEKLY_WINDOW_SECS: f64 = 604_800.0;

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
    /// The task needs several agents exchanging findings mid-run, which Codex cannot do: an
    /// automatic Claude decision regardless of usage.
    Orchestration,
    /// The provider the task was on is at the hard ceiling and the other is not, so it moved.
    FlippedOnExhaustion,
    /// Both providers are at or over the hard ceiling; the default provider was used anyway.
    OverCeiling,
    /// Weekly usage routing is disabled by policy.
    WeeklyRoutingDisabled,
    /// The provider the task was on is burning through its weekly window far enough ahead of the
    /// other that the task moved. Rare by construction: see `pace_flip_gap`.
    PaceFlip,
    /// A weekly reset epoch was never read, so there is no elapsed fraction to measure a run rate
    /// against and the override was skipped. Recorded rather than inferred, because a missing epoch
    /// and a window that resets at this instant are not the same input.
    PaceUnavailable,
    /// Claude's five hour window is near exhausted and Codex has weekly room, so the task was paced
    /// away from Claude. Never fires for Codex: only Claude has a five hour window that constrains a
    /// stream of jobs on this box.
    FiveHourPacing,
}

impl Gate {
    pub fn tag(self) -> &'static str {
        match self {
            Gate::ExplicitProvider => "explicit_provider",
            Gate::ClassifierFailed => "classifier_failed",
            Gate::MissingConnector => "missing_connector",
            Gate::Orchestration => "orchestration",
            Gate::FlippedOnExhaustion => "flipped_on_exhaustion",
            Gate::OverCeiling => "over_ceiling",
            Gate::WeeklyRoutingDisabled => "weekly_routing_disabled",
            Gate::PaceFlip => "pace_flip",
            Gate::PaceUnavailable => "pace_unavailable",
            Gate::FiveHourPacing => "five_hour_pacing",
        }
    }
}

/// One routing decision: where the task goes, with what model and effort, and why.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Decision {
    pub provider: Provider,
    /// The model to spawn with. None means the backend resolves its own default.
    pub model: Option<String>,
    /// The reasoning effort the router asks the backend to run at. Dispatch honours it on codex
    /// (`turn/start` `effort`) and on claude (`--effort`), and opencode discards it. Nothing
    /// currently sets it: the model tier is the toggle, so the router decides no effort at all and
    /// each backend resolves its own. The field is kept because that dispatch path is live and
    /// proven by `claude_argv_carries_the_decided_effort_and_omits_the_flag_without_one` and
    /// `codex_requests_pin_security_posture_and_put_effort_on_the_turn`, which pin backend argv and
    /// param shapes that drift on the backends' schedule rather than ours.
    ///
    /// What the backends then resolve is not the same on both. Claude runs at the model's own
    /// default, because nothing else sets one, and it reports that value nowhere. Codex runs at
    /// whatever `~/.codex/config.toml` resolves, because dispatch goes through
    /// `codex app-server daemon`, which loads user config; only when that file names no
    /// `model_reasoning_effort` does a codex job fall through to the model's catalogue default.
    ///
    /// So this is the effort the router decided, and it is not the effort a job ran at. The codex
    /// daemon reports the resolved value on the `thread/start` reply, and that reading is recorded
    /// separately in the log's `effective_effort` column. Claude and opencode record nothing there,
    /// because neither exposes one to read.
    pub effort: Option<String>,
    /// None when the caller named a provider and no classification ran.
    pub classification: Option<Classification>,
    pub gates: Vec<Gate>,
    pub usage: UsageSnapshot,
    /// How far ahead of its own weekly window each provider was burning at the deciding instant,
    /// in points. Positive is hot. None when that provider's reset was never read, and therefore
    /// when the override could not run: the log records the number the rule saw, so the next
    /// tuning pass reads why a task moved rather than only that it did. Always None on the
    /// explicit path, which ran no usage rule to measure.
    pub claude_pace_delta: Option<f64>,
    pub codex_pace_delta: Option<f64>,
    pub rationale: String,
}

impl Decision {
    pub fn gate_tags(&self) -> Vec<&'static str> {
        self.gates.iter().map(|gate| gate.tag()).collect()
    }
}

/// PURE: the routing decision for a scored task, at the instant `now_epoch_secs`.
///
/// The instant is a parameter rather than a clock read, because the run rate rules below are a
/// function of how much of each weekly window has elapsed. Passing it in is what makes a decision
/// replayable: the backtest replays each recorded row at the instant it was actually decided.
///
/// The rules, in the order they run, and each in the order it must run:
///
/// 1. The capability pin. A task Codex cannot do is not a cheaper job when routed there, it is a
///    failed one, so a pin bypasses every usage rule below including the ceiling.
/// 2. The hard ceiling, before the override. Being out of weekly budget is a capacity fact, and a
///    run rate can read a provider with two points left as running cold (95 percent used against
///    99 percent elapsed is a negative delta), so an override allowed to run first would route
///    into an exhausted provider.
/// 3. The run rate override, which is deliberately rare. See `pace_flip_gap`.
/// 4. Claude's five hour pacing, last, on whichever provider the task landed on.
pub fn decide(
    classification: Classification,
    usage: UsageSnapshot,
    now_epoch_secs: i64,
    config: &Config,
) -> Decision {
    let mut gates = Vec::new();
    let mut capability_pin = false;
    if classification.missing_connector {
        gates.push(Gate::MissingConnector);
        capability_pin = true;
    }
    if classification.orchestration {
        gates.push(Gate::Orchestration);
        capability_pin = true;
    }

    let mut provider = match config.policy.default_provider {
        DefaultProvider::Codex => Provider::Codex,
        DefaultProvider::Claude => Provider::Claude,
    };
    if capability_pin {
        provider = Provider::Claude;
    } else if classification.classifier_failed {
        // Not a pin: a task nobody could score keeps the configured default and stays eligible for
        // every usage rule, so it still lands on the provider with room.
        gates.push(Gate::ClassifierFailed);
    }

    let pre_usage_provider = provider;
    let complexity = classification.complexity;
    let mut model = model_for(pre_usage_provider, complexity, config);
    let claude_pace_delta = pace_delta(&usage.claude, now_epoch_secs);
    let codex_pace_delta = pace_delta(&usage.codex, now_epoch_secs);

    if !capability_pin {
        if !config.policy.weekly_routing {
            gates.push(Gate::WeeklyRoutingDisabled);
        } else {
            let other = other_provider(provider);
            let eligible = |candidate| weekly_used(&usage, candidate) < config.hard_ceiling_pct;
            match (eligible(provider), eligible(other)) {
                (false, false) => {
                    // The router routes; refusing work over a ceiling is bonus drain's job. The
                    // override is not consulted, because there is no provider to move to.
                    gates.push(Gate::OverCeiling);
                }
                (false, true) => {
                    provider = other;
                    gates.push(Gate::FlippedOnExhaustion);
                }
                // Exactly the current provider eligible: the task stays whatever run rate says,
                // since the only provider it could move to is out of weekly budget.
                (true, false) => {}
                (true, true) => {
                    pace_override(
                        &mut provider,
                        &mut gates,
                        other,
                        claude_pace_delta,
                        codex_pace_delta,
                        config,
                    );
                }
            }

            // Pacing runs after the rules above, on the provider they landed on, and reads only
            // Claude's five hour window: Codex's own is never a routing input. It applies however
            // the task got to Claude, because an exhausted five hour window is a capacity fact
            // rather than a preference, and a Claude dispatch into one stalls.
            if provider == Provider::Claude
                && usage.claude.five_hour_pct >= config.claude_five_hour_pacing_pct
                && usage.codex.weekly_pct < config.hard_ceiling_pct
            {
                provider = Provider::Codex;
                gates.push(Gate::FiveHourPacing);
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

    let rationale = rationale(&classification, provider, &gates, &usage);
    Decision {
        provider,
        model,
        effort: None,
        classification: Some(classification),
        gates,
        usage,
        claude_pace_delta,
        codex_pace_delta,
        rationale,
    }
}

/// PURE: the run rate override. Moves the task to `other` when the provider it is on is burning
/// far enough further ahead of its own weekly window, and does nothing otherwise.
///
/// The dead zone is wide because this box runs two Claude 20x Max plans against one Codex 5x plan,
/// an allowance mismatch of about 8x. Identical work therefore shows up as several times the
/// percentage on Codex, and over the recorded decisions Codex sat 43 to 58 points over pace all
/// week purely from that. A threshold inside that band is not an override at all: at 25 points it
/// moved 27 of 39 real dispatches, 25 of them onto the provider with LESS absolute allowance left.
/// So the gap has to clear the chronic band, and what is left is the genuine blowout.
///
/// Strictly greater, so a gap exactly on the threshold holds. Symmetric, because the rule is about
/// the provider the task is on and not about Codex; that it only ever fires toward Claude on this
/// box is a property of how the box is provisioned today, not of the rule.
fn pace_override(
    provider: &mut Provider,
    gates: &mut Vec<Gate>,
    other: Provider,
    claude_pace_delta: Option<f64>,
    codex_pace_delta: Option<f64>,
    config: &Config,
) {
    let (Some(claude), Some(codex)) = (claude_pace_delta, codex_pace_delta) else {
        // An unread reset has no elapsed fraction to measure against. Treating the missing epoch
        // as an ordinary one reads the window as fully elapsed, which turns an unknown provider
        // into an apparently idle one and routes on a number nobody measured.
        gates.push(Gate::PaceUnavailable);
        return;
    };
    let delta = |candidate| match candidate {
        Provider::Codex => codex,
        Provider::Claude | Provider::Opencode => claude,
    };
    if delta(*provider) - delta(other) > config.pace_flip_gap {
        *provider = other;
        gates.push(Gate::PaceFlip);
    }
}

/// PURE: how far ahead of its own weekly window a provider is burning, in points. Positive is hot:
/// 80 percent spent with half the window gone is 30 points over pace.
///
/// Each provider is measured against its OWN reset, because the two weekly windows start and end
/// at different instants and one provider's reset says nothing about the other's progress.
///
/// None when the reset epoch is 0, which `usage.rs` documents as "not known" rather than as a
/// window resetting at the epoch. The expected burn is clamped, because a reset outside the window
/// (a stale rollout, a clock skew) would otherwise read as more than a full window elapsed or less
/// than none, and either produces a delta larger than the scale it is measured on.
fn pace_delta(headroom: &Headroom, now_epoch_secs: i64) -> Option<f64> {
    if headroom.weekly_reset_epoch == 0 {
        return None;
    }
    let remaining = (headroom.weekly_reset_epoch - now_epoch_secs) as f64;
    let expected_pct = (100.0 * (1.0 - remaining / WEEKLY_WINDOW_SECS)).clamp(0.0, 100.0);
    Some(headroom.weekly_pct - expected_pct)
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
        // No usage rule ran, so no run rate was measured. Recording one anyway would put a number
        // in the log that nothing consulted, which the next backtest would read as a rule firing.
        claude_pace_delta: None,
        codex_pace_delta: None,
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
        "{}: {}{tags} (orchestration {}; codex weekly {:.0}%, claude weekly {:.0}%, claude 5h {:.0}%)",
        provider.name(),
        classification.rationale,
        if classification.orchestration {
            "yes"
        } else {
            "no"
        },
        usage.codex.weekly_pct,
        usage.claude.weekly_pct,
        usage.claude.five_hour_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::TaskContextHorizon;
    use crate::usage::Headroom;

    /// The rules the engine routes by live in `tests/pace_routing.rs`, against the public API.
    /// What is left here is the explicit path, which no rule touches, and the rationale string.
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
        // No usage rule ran on this path, so there is no run rate to record.
        assert_eq!(decision.claude_pace_delta, None);
        assert_eq!(decision.codex_pace_delta, None);

        // An explicitly requested model overrides the per-provider default.
        let pinned = decide_explicit(
            Provider::Claude,
            Some("sonnet".to_string()),
            usage(0.0, 0.0),
            &config,
        );
        assert_eq!(pinned.model.as_deref(), Some("sonnet"));
    }

    /// The rationale is the one line the CLI prints and the viewer shows, so it names the provider,
    /// every gate that fired, and the numbers those gates were decided on.
    #[test]
    fn the_rationale_names_the_provider_the_gates_and_both_weekly_numbers() {
        let config = Config::default();
        let decision = decide(
            Classification {
                orchestration: false,
                missing_connector: true,
                complexity: Complexity::High,
                task_context_horizon: TaskContextHorizon::Ordinary,
                rationale: "fixture".to_string(),
                classifier_failed: false,
            },
            usage(71.0, 50.0),
            1_785_400_000,
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
