//! The decision engine: hard pins select Claude; ordinary work walks `[routing] priority`, narrowed
//! to capable providers, and takes the first eligible candidate whose projected weekly draw is
//! within `priority_margin_pct` of the lowest. Pure given its inputs. See
//! docs/decisions/0006-projected-draw-replaces-pace-flip-gap.md,
//! docs/decisions/0007-claude-capability-only.md, and
//! docs/decisions/0012-configurable-provider-priority.md.

use crate::classify::{Classification, Complexity};
use crate::config::{Config, MatchedCapability};
use crate::provider::Provider;
use crate::usage::{Headroom, UsageSnapshot};

/// The weekly window, in seconds. 10080 minutes, the same window `usage.rs` identifies a weekly
/// rate limit by. Reset epochs are recorded in SECONDS on both providers, so a distance to a reset
/// is directly comparable to this.
const WEEKLY_WINDOW_SECS: f64 = 604_800.0;

/// How much of a weekly window must have elapsed before a projected-draw diagnostic is useful.
/// A twentieth of a week is about 8.4 hours; below this, one dispatch gives a misleading rate.
const MIN_PROJECTION_ELAPSED: f64 = 0.05;

/// Everything that fired on the way to a provider, in the order it fired. These are the tuning
/// signal in the decision log, so each one names a specific rule rather than a generic reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// The caller pinned a provider, so provider routing was bypassed.
    ExplicitProvider,
    /// The classifier could not answer; automatic capacity routing still applies.
    ClassifierFailed,
    /// A required connector is absent from the configured inventory.
    MissingConnector,
    /// A matched inventory name has no dispatcher. Unmatched classifier misses are not this gate:
    /// they stay on ordinary workhorse routing. This follows `MissingConnector` so the log keeps
    /// the classifier observation. See docs/decisions/0010-unmatched-connector-is-not-a-block.md.
    CapabilityBlocked,
    /// The task needs several agents exchanging findings mid-run, which Codex cannot do: an
    /// automatic Claude decision regardless of usage.
    Orchestration,
    /// A build-tier `/implement` run, which does not fit Codex's context window: an automatic
    /// Claude decision regardless of usage. See `implement_exceeds_codex_window`.
    ImplementContextWindow,
    /// Retired: no longer emitted since 0.30.0. It recorded the removed Claude and Codex shared
    /// capability comparison. Kept so rows and JSON written before 0.30.0 still decode and still
    /// count toward the flip rate. The general priority engine records the same move as
    /// `PriorityOverriddenByUsage`. See docs/decisions/0012-configurable-provider-priority.md.
    CapabilityProjectedDraw,
    /// The highest-priority capable candidate is ineligible and another eligible candidate took
    /// the task. Ineligible is either at or over the hard ceiling, carrying a weekly number nobody
    /// read, or unable to launch. See `WeeklyUnknown` and `ClassifierUnlaunchable` for the diagnostic cause.
    FlippedOnExhaustion,
    /// Every capable priority candidate is ineligible on CAPACITY, so the first capable candidate
    /// was used anyway. Recorded only when at least one candidate cleared the capability filter: a
    /// field emptied by a matched inventory name with no dispatcher is `CapabilityBlocked` and
    /// nothing else, because naming it `over_ceiling` alongside a weekly reading of 14 percent tells a reviewer the
    /// opposite of what happened.
    OverCeiling,
    /// At least one provider consulted by the active comparison has no authoritative weekly
    /// reading, so its percentage is a default. Such a provider is ineligible. See
    /// docs/decisions/0004-fail-closed-weekly-unknown.md.
    WeeklyUnknown,
    /// Grok is a priority candidate but is excluded because its official weekly telemetry is
    /// unknown. Not recorded when Grok is not a candidate.
    GrokUnavailable,
    /// Weekly usage routing is disabled by policy.
    WeeklyRoutingDisabled,
    /// The classifier could not LAUNCH a provider's CLI — it resolved nowhere, or the exec failed
    /// — as distinct from launching it and getting a bad answer back. That provider was therefore
    /// excluded from the eligibility test, in the same sense as one over its hard ceiling.
    ///
    /// This is a DIAGNOSTIC gate, not a provider-moving one. It records only that unlaunchability
    /// was applied, and only when the unlaunchable provider is a priority candidate.
    /// `FlippedOnExhaustion` records a move off the first candidate. An unlaunchable
    /// lower-priority candidate can be excluded without any move gate.
    ///
    /// It deliberately does not belong in `stats.rs`'s `FLIP_GATES`. A row moved off its first candidate already
    /// has `flipped_on_exhaustion`, and `any()` counts that row once. A row that did not move must
    /// not count as a flip.
    ClassifierUnlaunchable,
    /// Two or more priority candidates are eligible but at least one projected weekly draw could
    /// not be computed, so every eligible candidate was compared on raw weekly percent used.
    /// Typically this is a window with less than a twentieth elapsed, where dividing by that
    /// fraction would turn a couple of jobs into a four-figure projection.
    ProjectionUnavailable,
    /// The highest-priority capable candidate was eligible, but usage selected a lower-priority one
    /// whose score beat it by more than `priority_margin_pct`. A provider-moving gate: it is in
    /// `stats.rs` `FLIP_GATES`.
    PriorityOverriddenByUsage,
}

impl Gate {
    pub fn tag(self) -> &'static str {
        match self {
            Gate::ExplicitProvider => "explicit_provider",
            Gate::ClassifierFailed => "classifier_failed",
            Gate::MissingConnector => "missing_connector",
            Gate::CapabilityBlocked => "capability_blocked",
            Gate::Orchestration => "orchestration",
            Gate::ImplementContextWindow => "implement_context_window",
            Gate::CapabilityProjectedDraw => "capability_projected_draw",
            Gate::FlippedOnExhaustion => "flipped_on_exhaustion",
            Gate::OverCeiling => "over_ceiling",
            Gate::WeeklyUnknown => "weekly_unknown",
            Gate::GrokUnavailable => "grok_unavailable",
            Gate::WeeklyRoutingDisabled => "weekly_routing_disabled",
            Gate::ClassifierUnlaunchable => "classifier_unlaunchable",
            Gate::ProjectionUnavailable => "projection_unavailable",
            Gate::PriorityOverriddenByUsage => "priority_overridden_by_usage",
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
    /// (`turn/start` `effort`) and on claude (`--effort`).
    ///
    /// What the backends then expose is not the same on both. Codex accepts this as a turn override;
    /// after `turn/start` accepts it, the log records it separately as `effective_effort`. Without
    /// an override, Codex reports the thread default resolved from user config. Claude receives the
    /// requested value but reports no resolved value, and Grok accepts no effort value.
    pub effort: Option<String>,
    /// None when provider, model, and effort were all pinned.
    pub classification: Option<Classification>,
    pub gates: Vec<Gate>,
    /// The configured capability inventory could not satisfy the request. `provider` remains the
    /// ordinary route for log compatibility, but `run` must not dispatch it.
    pub capability_blocked: bool,
    pub usage: UsageSnapshot,
    /// What each provider's weekly draw projects to at the moment its window resets, as a percent
    /// of that provider's own weekly allowance, at the deciding instant. Over 100 means the
    /// provider runs out before the window does. None when the projection could not be computed,
    /// and therefore when the pace comparison could not use that provider's number: the log
    /// records the reading the rule saw, so the next tuning pass reads why a task moved rather
    /// than only that it did. Always None on the explicit path, which ran no usage rule to measure.
    pub claude_projected_draw: Option<f64>,
    pub codex_projected_draw: Option<f64>,
    pub grok_projected_draw: Option<f64>,
    pub rationale: String,
    /// Inventory names that recovered a missing-connector route, and whether each came from the
    /// task, the rationale, or both. Empty when the classifier did not report a miss, and on the
    /// explicit path which does not run this filter.
    pub matched_capabilities: Vec<MatchedCapability>,
    /// The caller's `--model` pin. None on auto and on an explicit provider that omitted `--model`,
    /// which is not the same as `model` above: that field is the assigned spawn model after
    /// complexity scaling fills an omitted pin.
    pub requested_model: Option<String>,
}

impl Decision {
    pub fn gate_tags(&self) -> Vec<&'static str> {
        self.gates.iter().map(|gate| gate.tag()).collect()
    }
}

/// PURE: is this a `/implement` run whose working set does not fit Codex's context window?
///
/// A capability pin, in the same sense as `orchestration`: the task is not cheaper on Codex, it
/// is failed there. Codex's window is 258,400 tokens. Both conditions are required: the task
/// must dispatch `/implement` (read from the text), and complexity must be `high` or `ultra`
/// (the build-tier proxy). An unscored task reads as `high`, so a classifier failure pins rather
/// than gambles. `low` and `medium` stay on automatic routing. See
/// docs/decisions/0003-implement-context-window.md.
fn implement_exceeds_codex_window(classification: &Classification) -> bool {
    classification.invokes_implement
        && matches!(
            classification.complexity,
            Complexity::High | Complexity::Ultra
        )
}

/// PURE: the routing decision for a scored task, at the instant `now_epoch_secs`.
///
/// The instant is a parameter rather than a clock read because compatible projected-draw log
/// fields depend on how much of each weekly window has elapsed.
///
/// The rules, in the order they run, and each in the order it must run:
///
/// 1. Capability pins (orchestration, the implement context window, a Claude-only capability)
///    select Claude, bypassing automatic capacity routing. A matched inventory name with no
///    capable listed provider is blocked. An unmatched miss is not a constraint. See
///    docs/decisions/0010-unmatched-connector-is-not-a-block.md.
/// 2. Ordinary work walks `[routing] priority`, narrowed to capable providers. Eligibility is
///    current weekly percent: unknown, at/over the hard ceiling, or unlaunchable is out. Among
///    eligible candidates the score is projected draw (or current weekly percent for all when any
///    projection is missing, recorded as `projection_unavailable`), and the first candidate in
///    priority order within `priority_margin_pct` of the lowest score wins, so an exact tie stays
///    with the higher priority. See docs/decisions/0006-projected-draw-replaces-pace-flip-gap.md
///    and docs/decisions/0012-configurable-provider-priority.md.
pub fn decide(
    classification: Classification,
    usage: UsageSnapshot,
    now_epoch_secs: i64,
    config: &Config,
) -> Decision {
    decide_with_task("", classification, usage, now_epoch_secs, config)
}

/// Automatic routing for a scored task. Inventory names in the task or classifier
/// rationale recover against `provider_capabilities` even when the classifier left
/// `missing_connector` false, so a paraphrased rationale cannot send an Airtable job
/// to a provider without Airtable. An unmatched miss is not a constraint: it does not pin
/// Claude and it does not refuse dispatch. A matched capability narrows the priority
/// candidates, and a matched name with no listed capable provider blocks
/// (docs/decisions/0007-claude-capability-only.md,
/// docs/decisions/0010-unmatched-connector-is-not-a-block.md,
/// docs/decisions/0012-configurable-provider-priority.md).
pub fn decide_with_task(
    task: &str,
    classification: Classification,
    usage: UsageSnapshot,
    now_epoch_secs: i64,
    config: &Config,
) -> Decision {
    let mut gates = Vec::new();
    let matched_capabilities = config.matched_capabilities(task, &classification.rationale);
    let capability_providers = config.capability_providers(&matched_capabilities);
    // A named inventory connector constrains even when the classifier omitted the flag.
    // An unmatched classifier miss is not a constraint.
    let capability_constraint = !matched_capabilities.is_empty();
    let mut capability_blocked = capability_constraint && capability_providers.is_empty();
    let mut capability_pin = false;
    if classification.missing_connector || capability_constraint {
        gates.push(Gate::MissingConnector);
        if capability_blocked {
            gates.push(Gate::CapabilityBlocked);
        }
    }
    if classification.orchestration {
        gates.push(Gate::Orchestration);
        capability_pin = true;
    }
    if implement_exceeds_codex_window(&classification) {
        gates.push(Gate::ImplementContextWindow);
        capability_pin = true;
    }

    // Ordinary work starts on the first capable priority candidate. Claude is a candidate only
    // when listed; the hard pins below reach it regardless. See
    // docs/decisions/0012-configurable-provider-priority.md.
    let capability_eligible =
        |candidate: Provider| !capability_constraint || capability_providers.contains(&candidate);
    let candidates: Vec<Provider> = config
        .routing
        .priority
        .iter()
        .copied()
        .filter(|candidate| capability_eligible(*candidate))
        .collect();
    // With no capable candidate the ordinary route is kept for log compatibility and never
    // dispatched. Load rejects an empty priority, so the Codex literal only keeps `decide` total
    // for a `Config` built in code.
    let mut provider = candidates
        .first()
        .or(config.routing.priority.first())
        .copied()
        .unwrap_or(Provider::Codex);
    if capability_providers == [Provider::Claude] {
        // Claude remains a capability pin when it is the only established provider.
        capability_pin = true;
        provider = Provider::Claude;
    } else if capability_pin {
        provider = Provider::Claude;
    } else if classification.classifier_failed {
        // Not a pin: a task nobody could score still selects by known candidate capacity.
        gates.push(Gate::ClassifierFailed);
    }

    let pre_usage_provider = provider;
    let complexity = classification.complexity;
    let mut model = model_for(pre_usage_provider, complexity, config);
    let claude_projected_draw = projected_draw(&usage.claude, now_epoch_secs);
    let codex_projected_draw = projected_draw(&usage.codex, now_epoch_secs);
    let grok_projected_draw = projected_draw(&usage.grok, now_epoch_secs);

    if !capability_pin && !config.policy.weekly_routing {
        gates.push(Gate::WeeklyRoutingDisabled);
        if capability_constraint && candidates.is_empty() && !capability_blocked {
            capability_blocked = true;
            gates.push(Gate::CapabilityBlocked);
        }
    } else if !capability_pin {
        // Fail closed on a weekly number nobody read: an unread window reports 0 percent, the
        // same as idle. Closing here keeps the reader's fail-open contract intact; every
        // candidate unknown falls through to `over_ceiling`. A launch failure is ineligible the
        // same way and is not a pin to Claude. See docs/decisions/0004-fail-closed-weekly-unknown.md
        // and docs/decisions/0007-claude-capability-only.md.
        let usage_eligible = |candidate: Provider| {
            headroom(&usage, candidate).weekly_known()
                && weekly_used(&usage, candidate) < config.hard_ceiling_for(candidate)
                && classification.unlaunchable != Some(candidate)
        };
        if classification
            .unlaunchable
            .is_some_and(|unlaunchable| candidates.contains(&unlaunchable))
        {
            gates.push(Gate::ClassifierUnlaunchable);
        }
        let weekly_unknown: Vec<Provider> = candidates
            .iter()
            .copied()
            .filter(|candidate| !headroom(&usage, *candidate).weekly_known())
            .collect();
        if !weekly_unknown.is_empty() {
            gates.push(Gate::WeeklyUnknown);
        }
        if weekly_unknown.contains(&Provider::Grok) {
            gates.push(Gate::GrokUnavailable);
        }
        let eligible: Vec<Provider> = candidates
            .iter()
            .copied()
            .filter(|candidate| usage_eligible(*candidate))
            .collect();
        if eligible.is_empty() {
            // The router routes; refusing work over a ceiling is bonus drain's job. The fallback
            // stays on the first capable candidate when no authoritative weekly reading is usable.
            if capability_constraint && candidates.is_empty() && !capability_blocked {
                capability_blocked = true;
                gates.push(Gate::CapabilityBlocked);
            } else if !candidates.is_empty() {
                // Some candidate had the capability and still had nowhere to go, so this really
                // is a capacity verdict. When none did, `CapabilityBlocked` is already recorded
                // and adding `over_ceiling` would misattribute the block.
                gates.push(Gate::OverCeiling);
            }
        } else {
            // Pace, not current percent, and never a mix of the two: when any eligible candidate
            // lacks a projection, every eligible candidate is scored on weekly percent. See
            // docs/decisions/0006-projected-draw-replaces-pace-flip-gap.md and
            // docs/decisions/0012-configurable-provider-priority.md.
            let winner = if eligible.len() == 1 {
                eligible[0]
            } else {
                // Score on the same projections the decision records, so the logged pace always
                // explains the route.
                let draw_for = |candidate: Provider| match candidate {
                    Provider::Claude => claude_projected_draw,
                    Provider::Codex => codex_projected_draw,
                    Provider::Grok => grok_projected_draw,
                };
                let draws: Option<Vec<f64>> = eligible
                    .iter()
                    .map(|candidate| draw_for(*candidate))
                    .collect();
                let scores = draws.unwrap_or_else(|| {
                    gates.push(Gate::ProjectionUnavailable);
                    eligible
                        .iter()
                        .map(|candidate| weekly_used(&usage, *candidate))
                        .collect()
                });
                first_within_margin(&eligible, &scores, config.routing.priority_margin_pct)
            };
            if winner != candidates[0] {
                gates.push(if eligible.contains(&candidates[0]) {
                    Gate::PriorityOverriddenByUsage
                } else {
                    Gate::FlippedOnExhaustion
                });
            }
            provider = winner;
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
        effort: effort_for(provider, complexity),
        classification: Some(classification),
        gates,
        capability_blocked,
        usage,
        claude_projected_draw,
        codex_projected_draw,
        grok_projected_draw,
        rationale,
        matched_capabilities,
        requested_model: None,
    }
}

/// PURE: what a provider's weekly draw projects to by the time its window resets, as a percent of
/// that provider's own weekly allowance. Each provider is measured against its own reset and
/// allowance; no plan sizes appear here. None when the reset epoch is 0 or less than
/// `MIN_PROJECTION_ELAPSED` of the window has gone. The elapsed fraction is clamped at the top
/// so a stale reset cannot read as more than a full window. See
/// docs/decisions/0006-projected-draw-replaces-pace-flip-gap.md.
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

/// PURE: the first candidate, in priority order, whose score is within `margin` points of the
/// lowest score. `candidates` and `scores` are parallel and non-empty; the lowest-scoring candidate
/// always qualifies, so the fallback to the first candidate is never reached with finite scores.
fn first_within_margin(candidates: &[Provider], scores: &[f64], margin: f64) -> Provider {
    let best = scores.iter().copied().fold(f64::INFINITY, f64::min);
    candidates
        .iter()
        .zip(scores)
        .find(|(_, score)| **score <= best + margin)
        .map(|(candidate, _)| *candidate)
        .unwrap_or(candidates[0])
}

/// PURE: the decision for a caller pinned provider. The provider stays exact while classification
/// supplies any omitted downstream values.
pub fn decide_explicit(
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
    classification: Option<Classification>,
    usage: UsageSnapshot,
    config: &Config,
) -> Decision {
    let complexity = classification
        .as_ref()
        .map(|classification| classification.complexity);
    let requested_model = model.clone();
    let model = model.or_else(|| complexity.and_then(|value| model_for(provider, value, config)));
    let effort = effort.or_else(|| {
        complexity.and_then(|value| {
            if requested_model.is_some() && provider != Provider::Grok {
                Some(complexity_effort(value).to_string())
            } else {
                effort_for(provider, value)
            }
        })
    });
    let rationale = classification
        .as_ref()
        .map(|classification| {
            format!(
                "{} requested explicitly: {}",
                provider.name(),
                classification.rationale
            )
        })
        .unwrap_or_else(|| format!("{} requested explicitly", provider.name()));
    let mut gates = vec![Gate::ExplicitProvider];
    if provider == Provider::Grok
        && (!usage.grok.weekly_known() || usage.grok.weekly_reset_epoch == 0)
    {
        gates.push(Gate::GrokUnavailable);
    }
    Decision {
        provider,
        model,
        effort,
        classification,
        gates,
        capability_blocked: false,
        usage,
        // No usage rule ran, so no projection was measured. Recording one anyway would put a number
        // in the log that nothing consulted, which the next backtest would read as a rule firing.
        claude_projected_draw: None,
        codex_projected_draw: None,
        grok_projected_draw: None,
        rationale,
        matched_capabilities: Vec::new(),
        requested_model,
    }
}

/// PURE: the model the job runs on, scaled by how much reasoning the task needs. Grok has no
/// tiers in the MVP, so it resolves its own default.
fn model_for(provider: Provider, complexity: Complexity, config: &Config) -> Option<String> {
    match provider {
        Provider::Codex => Some(config.models.codex.pick(complexity).to_string()),
        Provider::Claude => Some(config.models.claude.pick(complexity).to_string()),
        Provider::Grok => None,
    }
}

/// PURE: the shared effort policy for providers that accept an effort value.
///
/// Each stronger model enters at a lower reasoning setting: low-complexity work uses the workhorse
/// at high effort, medium uses the stronger model at medium, and high uses the top model at low.
/// Ultra keeps the top model and raises its effort to high.
fn effort_for(provider: Provider, complexity: Complexity) -> Option<String> {
    match provider {
        Provider::Codex | Provider::Claude => Some(complexity_effort(complexity).to_string()),
        Provider::Grok => None,
    }
}

fn complexity_effort(complexity: Complexity) -> &'static str {
    match complexity {
        Complexity::Low => "high",
        Complexity::Medium => "medium",
        Complexity::High => "low",
        Complexity::Ultra => "high",
    }
}

/// PURE: the snapshot half a provider is judged on.
fn headroom(usage: &UsageSnapshot, provider: Provider) -> &Headroom {
    match provider {
        Provider::Codex => &usage.codex,
        Provider::Claude => &usage.claude,
        Provider::Grok => &usage.grok,
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
        "{}: {}{tags} (orchestration {}; {}, {}, {}, claude 5h {:.0}%)",
        provider.name(),
        classification.rationale,
        if classification.orchestration {
            "yes"
        } else {
            "no"
        },
        weekly_label("claude", &usage.claude),
        weekly_label("codex", &usage.codex),
        weekly_label("grok", &usage.grok),
        usage.claude.five_hour_pct,
    )
}

fn weekly_label(name: &str, headroom: &Headroom) -> String {
    if headroom.weekly_known() {
        format!("{name} weekly {:.0}%", headroom.weekly_pct)
    } else {
        format!("{name} weekly unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::TaskContextHorizon;
    use crate::usage::{Headroom, parse_codex_rate_limits};

    /// The rules the engine routes by live in `tests/pace_routing.rs`, against the public API.
    /// What is left here is the explicit path, which no rule touches, and the rationale string.
    fn usage(codex_weekly: f64, claude_weekly: f64) -> UsageSnapshot {
        UsageSnapshot {
            codex: Headroom {
                weekly_pct: codex_weekly,
                weekly_capacity_known: true,
                ..Headroom::full()
            },
            claude: Headroom {
                weekly_pct: claude_weekly,
                weekly_capacity_known: true,
                ..Headroom::full()
            },
            grok: Headroom::closed(),
        }
    }

    fn classification(complexity: Complexity) -> Classification {
        Classification {
            orchestration: false,
            missing_connector: false,
            complexity,
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "classified for explicit route".to_string(),
            classifier_failed: false,
            invokes_implement: false,
            unlaunchable: None,
        }
    }

    /// A named provider keeps its provider pin while classification supplies the provider's own
    /// model and effort policy.
    #[test]
    fn an_explicit_provider_uses_the_classified_model_and_effort() {
        let config = Config::default();
        let codex = decide_explicit(
            Provider::Codex,
            None,
            None,
            Some(classification(Complexity::High)),
            usage(0.0, 0.0),
            &config,
        );
        assert_eq!(codex.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(codex.effort.as_deref(), Some("low"));

        let claude = decide_explicit(
            Provider::Claude,
            None,
            None,
            Some(classification(Complexity::High)),
            usage(0.0, 0.0),
            &config,
        );
        assert_eq!(claude.model.as_deref(), Some("claude-opus-5-5[1m]"));
        assert_eq!(claude.effort.as_deref(), Some("low"));
    }

    #[test]
    fn an_explicit_provider_skips_classification_but_keeps_the_usage_snapshot() {
        let config = Config::default();
        let decision = decide_explicit(
            Provider::Codex,
            None,
            None,
            None,
            usage(71.0, 50.0),
            &config,
        );
        assert_eq!(decision.provider, Provider::Codex);
        assert!(decision.classification.is_none());
        assert_eq!(decision.gate_tags(), vec!["explicit_provider"]);
        assert_eq!(decision.usage.codex.weekly_pct, 71.0);
        assert_eq!(decision.usage.claude.weekly_pct, 50.0);
        assert_eq!(decision.model, None);
        // No usage rule ran on this path, so there is no run rate to record.
        assert_eq!(decision.claude_projected_draw, None);
        assert_eq!(decision.codex_projected_draw, None);
        assert_eq!(decision.grok_projected_draw, None);

        // Fully pinned inputs stay exact when classification is absent.
        let pinned = decide_explicit(
            Provider::Claude,
            Some("sonnet".to_string()),
            Some("low".to_string()),
            None,
            usage(0.0, 0.0),
            &config,
        );
        assert_eq!(pinned.model.as_deref(), Some("sonnet"));
        assert_eq!(pinned.requested_model.as_deref(), Some("sonnet"));
        assert_eq!(pinned.effort.as_deref(), Some("low"));
        assert_eq!(decision.requested_model, None);
    }

    /// The rationale is the one line the CLI prints and the viewer shows, so it names the provider,
    /// every gate that fired, and the weekly numbers every available provider was decided on.
    #[test]
    fn the_rationale_names_the_provider_the_gates_and_every_weekly_number() {
        let config = Config::default();
        let decision = decide(
            Classification {
                orchestration: false,
                missing_connector: true,
                complexity: Complexity::High,
                task_context_horizon: TaskContextHorizon::Ordinary,
                rationale: "fixture".to_string(),
                classifier_failed: false,
                invokes_implement: false,
                unlaunchable: None,
            },
            usage(71.0, 50.0),
            1_785_400_000,
            &config,
        );
        assert!(
            decision.rationale.starts_with("codex: "),
            "{}",
            decision.rationale
        );
        assert!(decision.rationale.contains("missing_connector"));
        assert!(
            !decision.rationale.contains("capability_blocked"),
            "an unmatched miss is not a refuse: {}",
            decision.rationale
        );
        assert!(decision.rationale.contains("codex weekly 71%"));
        assert!(decision.rationale.contains("claude weekly 50%"));
        assert!(decision.rationale.contains("grok weekly unknown"));
    }

    /// `over_ceiling` is a capacity verdict; a field emptied by a matched capability with no
    /// dispatcher must not borrow it. An unmatched miss is ordinary routing, not this case.
    #[test]
    fn a_capability_emptied_field_is_not_labelled_over_ceiling() {
        let unmatched = Classification {
            orchestration: false,
            missing_connector: true,
            complexity: Complexity::Medium,
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "requires a Slack thread nobody has an inventory for".to_string(),
            classifier_failed: false,
            invokes_implement: false,
            unlaunchable: None,
        };
        let ordinary = decide(
            unmatched,
            usage(1.0, 14.0),
            1_785_400_000,
            &Config::default(),
        );
        assert!(!ordinary.capability_blocked);
        assert!(!ordinary.gates.contains(&Gate::CapabilityBlocked));
        assert!(!ordinary.gates.contains(&Gate::OverCeiling));
        assert_eq!(ordinary.provider, Provider::Codex);

        let config = Config {
            provider_capabilities: std::collections::BTreeMap::from([(
                "none".to_string(),
                vec!["Slack".to_string()],
            )]),
            ..Config::default()
        };
        let blocked = Classification {
            orchestration: false,
            missing_connector: true,
            complexity: Complexity::Medium,
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "requires a Slack thread".to_string(),
            classifier_failed: false,
            invokes_implement: false,
            unlaunchable: None,
        };
        let decision = decide(blocked, usage(1.0, 14.0), 1_785_400_000, &config);
        assert!(decision.capability_blocked);
        assert!(decision.gates.contains(&Gate::CapabilityBlocked));
        assert!(
            !decision.gates.contains(&Gate::OverCeiling),
            "capacity was 1% and 14%, nowhere near the ceiling: {}",
            decision.rationale
        );

        // A genuine capacity exhaustion with no connector question still records the gate.
        let exhausted = decide(
            Classification::fallback("ordinary work, no connector named"),
            usage(99.0, 99.0),
            1_785_400_000,
            &config,
        );
        assert!(!exhausted.capability_blocked);
        assert!(exhausted.gates.contains(&Gate::OverCeiling));
    }

    #[test]
    fn exhausted_credits_are_ineligible_for_codex_routing() {
        let codex = parse_codex_rate_limits(
            r#"{"timestamp":"2026-08-06T09:36:39.958Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"premium","limit_name":null,"primary":null,"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":null,"rate_limit_reached_type":null}}}"#,
            1_785_400_000,
        )
        .expect("the live credits payload parses");
        assert_eq!(codex.weekly_pct, 100.0);
        assert!(codex.weekly_known());
        assert!(!codex.stale);

        let decision = decide(
            Classification::fallback("fixture"),
            UsageSnapshot {
                codex,
                claude: Headroom {
                    five_hour_pct: 0.0,
                    five_hour_reset_epoch: 0,
                    weekly_pct: 10.0,
                    weekly_reset_epoch: 1_786_004_800,
                    weekly_capacity_known: true,
                    stale: false,
                },
                grok: Headroom {
                    five_hour_pct: 0.0,
                    five_hour_reset_epoch: 0,
                    weekly_pct: 10.0,
                    weekly_reset_epoch: 1_786_004_800,
                    weekly_capacity_known: true,
                    stale: false,
                },
            },
            1_785_400_000,
            &Config::default(),
        );
        assert_eq!(decision.provider, Provider::Grok);
        assert!(decision.gates.contains(&Gate::FlippedOnExhaustion));
    }
}
