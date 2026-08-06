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

/// How much of a weekly window must have elapsed before a projection across it means anything.
/// A twentieth of a week is about 8.4 hours. Below this the divisor is small enough that a single
/// job projects to a blowout, and the override would move traffic on the strength of one dispatch.
const MIN_PROJECTION_ELAPSED: f64 = 0.05;

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
    /// The provider the task was on is ineligible and the other is not, so it moved. Ineligible is
    /// either at or over the hard ceiling, or carrying a weekly number nobody read: see
    /// `WeeklyUnknown`, which is recorded alongside this one to tell the two apart.
    FlippedOnExhaustion,
    /// Both providers are ineligible; the default provider was used anyway.
    OverCeiling,
    /// At least one provider's weekly window was never read, so its percentage is a default rather
    /// than a reading. Such a provider is ineligible: an unread window reports 0 percent used, and
    /// treating that as headroom routes work into a provider that may be out of budget, which is
    /// what an exhausted Codex looked like before this gate existed. Recorded whenever eligibility
    /// was decided against a missing number, whether or not it changed the destination.
    WeeklyUnknown,
    /// Weekly usage routing is disabled by policy.
    WeeklyRoutingDisabled,
    /// The provider the task was on projects to overdraw its weekly window before the window
    /// resets, and the other provider projects a lighter draw, so the task moved.
    ProjectedOverdraw,
    /// No projection could be computed for at least one provider, so the override was skipped.
    /// In practice this means one thing: too little of a window has elapsed for a projection across
    /// it to mean anything. A projection is also uncomputable when a reset was never read, but the
    /// override runs only with both providers eligible and eligibility already requires a known
    /// weekly window, so that decision carries `WeeklyUnknown` instead. Recorded rather than
    /// inferred, because declining to measure and measuring a healthy provider are not the same
    /// input and must not read the same in the log.
    ProjectionUnavailable,
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
            Gate::WeeklyUnknown => "weekly_unknown",
            Gate::WeeklyRoutingDisabled => "weekly_routing_disabled",
            Gate::ProjectedOverdraw => "projected_overdraw",
            Gate::ProjectionUnavailable => "projection_unavailable",
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
    /// What each provider's weekly draw projects to at the moment its window resets, as a percent
    /// of that provider's own weekly allowance, at the deciding instant. Over 100 means the
    /// provider runs out before the window does. None when the projection could not be computed,
    /// and therefore when the override could not run: the log records the number the rule saw, so
    /// the next tuning pass reads why a task moved rather than only that it did. Always None on
    /// the explicit path, which ran no usage rule to measure.
    pub claude_projected_draw: Option<f64>,
    pub codex_projected_draw: Option<f64>,
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
/// 2. Eligibility, before the override: a provider at or over the hard ceiling, or carrying a
///    weekly number nobody read, is not a destination. Being out of weekly budget is a capacity
///    fact, and a provider down to its reserve projects to finish INSIDE its allowance (95 percent
///    used against 99 percent elapsed projects to 96), so an override allowed to run first would
///    see nothing wrong and route into an exhausted provider.
/// 3. The projection override. See `projected_draw`.
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
    let claude_projected_draw = projected_draw(&usage.claude, now_epoch_secs);
    let codex_projected_draw = projected_draw(&usage.codex, now_epoch_secs);

    if !capability_pin {
        if !config.policy.weekly_routing {
            gates.push(Gate::WeeklyRoutingDisabled);
        } else {
            let other = other_provider(provider);
            // Fail closed on a weekly number nobody read. The percentage of an unread window is 0,
            // which is the same reading as a genuinely idle provider, so trusting it hands every
            // job to whichever provider failed to report. An exhausted Codex is exactly that
            // shape: its rollout carried no weekly window, so it read as 0 percent used, live, and
            // won every comparison in this block while it was actually hard limited.
            //
            // Closing here rather than in the reader keeps the reader's fail open contract intact.
            // It also cannot block a dispatch: both providers unknown falls through to the
            // `over_ceiling` arm, which still routes.
            let eligible = |candidate| {
                headroom(&usage, candidate).weekly_known()
                    && weekly_used(&usage, candidate) < config.hard_ceiling_pct
            };
            if !headroom(&usage, provider).weekly_known() || !headroom(&usage, other).weekly_known()
            {
                gates.push(Gate::WeeklyUnknown);
            }
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
                    projection_override(
                        &mut provider,
                        &mut gates,
                        other,
                        claude_projected_draw,
                        codex_projected_draw,
                        config,
                    );
                }
            }

            // Pacing runs after the rules above, on the provider they landed on, and reads only
            // Claude's five hour window: Codex's own is never a routing input. It applies however
            // the task got to Claude, because an exhausted five hour window is a capacity fact
            // rather than a preference, and a Claude dispatch into one stalls.
            //
            // "Codex has room" is the same `eligible` test the arms above used, rather than a
            // second inline comparison that could drift away from it. That is what stops a paced
            // job relocating a Claude stall onto a Codex whose weekly number nobody read.
            if provider == Provider::Claude
                && usage.claude.five_hour_pct >= config.claude_five_hour_pacing_pct
                && eligible(Provider::Codex)
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
        claude_projected_draw,
        codex_projected_draw,
        rationale,
    }
}

/// PURE: the projection override. Moves the task to `other` when the provider it is on projects to
/// run out of weekly allowance before its window resets and `other` projects a lighter draw.
///
/// Both conditions matter. The first is what makes the rule fire only on a real problem: a provider
/// projecting under the threshold finishes its week with allowance to spare, and moving work off it
/// would strand that allowance. The second is what makes the destination an improvement rather than
/// merely a different provider; without it, two equally doomed providers would trade jobs back and
/// forth. Comparing the two projections rather than testing `other` against the threshold keeps the
/// rule useful when BOTH overdraw, which is the week where routing matters most: the task goes to
/// whichever provider runs out later, and the two drain together instead of one dying first.
///
/// This replaced a rule that compared how far ahead of pace each provider was in POINTS, against a
/// configured gap. Two things were wrong with it. The gap was a tuning constant calibrated against
/// one particular pair of plan sizes, so it silently went stale when either plan changed: it was
/// set to 70 points to clear the chronic band that a 5x Codex plan against 20x Claude plans
/// produced, the Codex plan grew on 2026-08-01, the chronic band collapsed to under 38 points, and
/// the rule then could not fire at all. It never fired once, across every decision ever logged.
/// The deeper fault is that a points difference is the wrong shape: `spent - elapsed` under-reacts
/// early in a window, exactly when there is still time to correct. Twenty percent spent in the
/// first tenth of a week is 10 points hot and a 200 percent projected draw. A ratio against each
/// provider's own allowance and its own window needs no plan sizes and no calibration, which is why
/// the threshold below is 100 rather than a number somebody measured.
///
/// Symmetric, because the rule is about the provider the task is on and not about Codex; that it
/// only ever fires toward Claude on this box is a property of how the box is provisioned today.
fn projection_override(
    provider: &mut Provider,
    gates: &mut Vec<Gate>,
    other: Provider,
    claude_projected_draw: Option<f64>,
    codex_projected_draw: Option<f64>,
    config: &Config,
) {
    let (Some(claude), Some(codex)) = (claude_projected_draw, codex_projected_draw) else {
        // Routing on a projection nobody could compute is worse than not routing on one. Reaching
        // here means a window barely started, which projects wildly off a handful of jobs: the
        // unread-reset half of `projected_draw` cannot reach this point, because eligibility
        // refused an unknown window before the override was consulted.
        gates.push(Gate::ProjectionUnavailable);
        return;
    };
    let draw = |candidate| match candidate {
        Provider::Codex => codex,
        Provider::Claude | Provider::Opencode => claude,
    };
    let current = draw(*provider);
    if current > config.projection_overdraw_pct && draw(other) < current {
        *provider = other;
        gates.push(Gate::ProjectedOverdraw);
    }
}

/// PURE: what a provider's weekly draw projects to by the time its window resets, as a percent of
/// that provider's own weekly allowance. Spending 20 percent in the first tenth of the week
/// projects to 200: at this rate the allowance runs out with most of the week still to go.
///
/// Each provider is measured against its OWN reset and its OWN allowance, which is what makes the
/// two numbers comparable across providers on different plans. The percent is already normalized by
/// allowance, so no plan sizes appear here and none need maintaining.
///
/// None in two cases:
///
/// - The reset epoch is 0, which `usage.rs` documents as "not known" rather than as a window
///   resetting at the epoch. This one never reaches the override, which runs only once eligibility
///   has established that both windows are known; it is here so the value RECORDED on such a
///   decision is an honest absence rather than a number derived from a zero epoch.
/// - Less than `MIN_PROJECTION_ELAPSED` of the window has gone. Dividing by a small elapsed
///   fraction turns one job into a four-figure projection, so early in a window the projection is
///   not merely noisy, it is confidently wrong in the direction that moves traffic. This is the
///   case that actually stops the override.
///
/// The elapsed fraction is clamped at the top, because a reset outside the window (a stale rollout,
/// a clock skew) would otherwise read as more than a full window elapsed.
fn projected_draw(headroom: &Headroom, now_epoch_secs: i64) -> Option<f64> {
    if headroom.weekly_reset_epoch == 0 {
        return None;
    }
    let remaining = (headroom.weekly_reset_epoch - now_epoch_secs) as f64;
    let elapsed = (1.0 - remaining / WEEKLY_WINDOW_SECS).min(1.0);
    if elapsed < MIN_PROJECTION_ELAPSED {
        return None;
    }
    Some(headroom.weekly_pct / elapsed)
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
        // No usage rule ran, so no projection was measured. Recording one anyway would put a number
        // in the log that nothing consulted, which the next backtest would read as a rule firing.
        claude_projected_draw: None,
        codex_projected_draw: None,
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

/// PURE: the snapshot half a provider is judged on. opencode has no usage source in the MVP, so it
/// reads as the Claude side it rides on.
fn headroom(usage: &UsageSnapshot, provider: Provider) -> &Headroom {
    match provider {
        Provider::Codex => &usage.codex,
        Provider::Claude | Provider::Opencode => &usage.claude,
    }
}

fn weekly_used(usage: &UsageSnapshot, provider: Provider) -> f64 {
    headroom(usage, provider).weekly_pct
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
        assert_eq!(decision.claude_projected_draw, None);
        assert_eq!(decision.codex_projected_draw, None);

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
