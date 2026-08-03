//! Where a decision runs, as against which provider runs it. The two are orthogonal: `provider`
//! stays `codex` for a cloud job, because a cloud task draws on the same Codex weekly window and
//! every reader that keys off that column has to keep working.
//!
//! Everything here is pure. `decide` takes the resolved eligibility as an argument rather than
//! resolving it, which is what keeps the engine replayable by `pace_backtest.rs`; these cases
//! therefore touch no filesystem, no git, and no network at all.
//!
//! The reachability claim these tests are the evidence for: a cloud target is produced by exactly
//! one `match` arm in `decide`, whose pattern names `Provider::Codex` literally, and by nothing
//! else in the tree. A claude decision cannot reach it, and `decide_explicit` never builds one.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::cloud::{CloudEligibility, CloudIneligible, CloudTarget};
use agent_router_core::config::{Config, DefaultProvider};
use agent_router_core::decide::{ExecutionTarget, Gate, decide, decide_explicit};
use agent_router_core::{Headroom, Provider, UsageSnapshot};

/// The instant every case decides at, in the same range as the recorded decisions.
const NOW: i64 = 1_785_400_000;
/// Half the weekly window: a reset this far out means the window is exactly 50 percent elapsed.
const HALF_WEEK: i64 = 302_400;

const ENVIRONMENT_ID: &str = "env-abc";
const BRANCH: &str = "task/x";

/// A resolved cloud target, built fresh per use because `CloudEligibility` is passed by value into
/// `decide`.
fn eligible() -> CloudEligibility {
    CloudEligibility::Eligible(CloudTarget::resolved(
        ENVIRONMENT_ID.to_string(),
        BRANCH.to_string(),
    ))
}

/// The repository is not allowlisted, which is what every existing caller's fixture means: cloud is
/// off, so the decision is exactly what it was before this dimension existed.
fn no_cloud() -> CloudEligibility {
    CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted)
}

fn window(weekly_pct: f64, five_hour_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + HALF_WEEK,
        five_hour_pct,
        ..Headroom::full()
    }
}

/// Both providers burning at the same rate: a gap of zero, which is nowhere near the dead zone, so
/// no usage rule moves anything and the decision is decided by the configured default alone.
fn settled() -> UsageSnapshot {
    UsageSnapshot {
        claude: window(50.0, 0.0),
        codex: window(50.0, 0.0),
    }
}

fn scored(orchestration: bool, missing_connector: bool) -> Classification {
    Classification {
        orchestration,
        missing_connector,
        complexity: Complexity::High,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
    }
}

fn plain() -> Classification {
    scored(false, false)
}

/// The resolved target on a cloud decision, or a failure naming what the decision actually was.
fn cloud_target(target: &ExecutionTarget) -> &CloudTarget {
    match target {
        ExecutionTarget::CodexCloud(target) => target,
        ExecutionTarget::Local => panic!("expected a cloud target, got {target:?}"),
    }
}

/// AC 1a. An ordinary codex task in a repository that resolved an environment runs as a cloud task,
/// and the target carries the environment and branch the resolution produced rather than a value
/// re-derived downstream.
///
/// `provider` stays `codex` deliberately: `weekly_column`, `weekly_used`, `other_provider`, the
/// estimate, and the reconciler all key off that column, and a fourth provider variant would change
/// every one of them silently.
#[test]
fn an_eligible_codex_task_routes_to_the_cloud() {
    let decision = decide(plain(), settled(), NOW, eligible(), &Config::default());

    assert_eq!(decision.provider, Provider::Codex);
    assert_eq!(
        cloud_target(&decision.execution_target).environment_id(),
        ENVIRONMENT_ID
    );
    assert_eq!(cloud_target(&decision.execution_target).branch(), BRANCH);
    assert_eq!(
        decision.execution_target.tag(),
        "cloud",
        "the tag is what the log column and the printed line carry"
    );
}

/// `codex cloud exec` takes no `--model`: the model belongs to the cloud environment. Recording the
/// codex tier on a cloud row would claim the router chose a model it cannot set, in the column the
/// tuning data reads as the router's own pick.
///
/// The local decision beside it is the control. Without it this passes just as well against an
/// implementation that lost models everywhere, which is a much larger regression than the one the
/// test is about.
#[test]
fn a_cloud_decision_carries_no_model() {
    let config = Config::default();

    let cloud = decide(plain(), settled(), NOW, eligible(), &config);
    assert_eq!(
        cloud.model, None,
        "a cloud decision must not record a model the router cannot set"
    );

    let local = decide(plain(), settled(), NOW, no_cloud(), &config);
    assert_eq!(
        local.provider, cloud.provider,
        "the same provider either way"
    );
    assert_eq!(
        local.model.as_deref(),
        Some("gpt-5.6-sol"),
        "the equivalent local decision still picks its tier, so the None above is about cloud"
    );
}

/// AC 1c. Cloud is Codex-only, and the mechanism is that the arm producing a cloud target names
/// `Provider::Codex` literally. A task pinned to Claude by a missing connector runs local even
/// though the repository is fully cloud-eligible, because there is no provider-neutral cloud
/// concept for a claude decision to inhabit: Claude has no headless cloud dispatch at all.
#[test]
fn a_claude_pinned_task_stays_local_even_in_a_cloud_eligible_repo() {
    let decision = decide(
        scored(false, true),
        settled(),
        NOW,
        eligible(),
        &Config::default(),
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(decision.gates.contains(&Gate::MissingConnector));
    assert_eq!(decision.execution_target, ExecutionTarget::Local);
    assert_eq!(decision.execution_target.tag(), "local");
}

/// The target is computed after every usage rule has settled the provider, not alongside the
/// default. This task starts on Claude, is paced onto Codex by the five hour window, and must reach
/// the cloud from there: an implementation that decided the target from the pre-usage provider
/// would route it local and nothing else in the decision would look wrong.
#[test]
fn a_five_hour_paced_task_that_lands_on_codex_can_still_reach_the_cloud() {
    let mut config = Config::default();
    config.policy.default_provider = DefaultProvider::Claude;
    let usage = UsageSnapshot {
        claude: window(50.0, config.claude_five_hour_pacing_pct),
        codex: window(50.0, 0.0),
    };

    let decision = decide(plain(), usage, NOW, eligible(), &config);

    assert_eq!(
        decision.provider,
        Provider::Codex,
        "the fixture must actually pace the task off Claude, or it proves nothing"
    );
    assert!(decision.gates.contains(&Gate::FiveHourPacing));
    assert_eq!(
        cloud_target(&decision.execution_target).environment_id(),
        ENVIRONMENT_ID
    );
}

/// AC 1b, at the decision layer. Every reason a repository can fail to qualify routes local, and
/// the table is exhaustive so a reason added later has to be given an answer here rather than
/// inheriting one.
#[test]
fn an_ineligible_repo_routes_local_whatever_the_reason() {
    let config = Config::default();
    let reasons = [
        CloudIneligible::NotAllowlisted,
        CloudIneligible::NotAGitRepo,
        CloudIneligible::NoGitHubRemote,
        CloudIneligible::NoBranch,
        CloudIneligible::NoCodexAuth,
        CloudIneligible::NoCloudEnvironment,
    ];

    for reason in reasons {
        let decision = decide(
            plain(),
            settled(),
            NOW,
            CloudEligibility::Ineligible(reason),
            &config,
        );
        assert_eq!(decision.provider, Provider::Codex);
        assert_eq!(
            decision.execution_target,
            ExecutionTarget::Local,
            "{reason:?} must route local"
        );
    }
}

/// A caller who named a provider has taken control of dispatch, and there is no `--target` flag for
/// them to opt out of cloud with, so the explicit path stays local. Recording it here rather than
/// inheriting a default is what makes the boundary visible: adding cloud to the explicit path later
/// has to be a visible edit to this test rather than a silent behaviour change.
#[test]
fn an_explicit_provider_never_reaches_the_cloud() {
    let decision = decide_explicit(
        Provider::Codex,
        None,
        UsageSnapshot::full(),
        &Config::default(),
    );

    assert_eq!(decision.execution_target, ExecutionTarget::Local);
    assert_eq!(decision.execution_target.tag(), "local");
}
