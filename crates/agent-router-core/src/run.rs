//! One routed task end to end: read usage, classify (unless the caller named a provider),
//! decide, dispatch, log.

use crate::classify::classify;
use crate::config::Config;
use crate::decide::{Decision, decide, decide_explicit};
use crate::error::{Error, Result};
use crate::estimate::Estimate;
use crate::log::{DecisionLog, Entry};
use crate::provider::Provider;
use crate::usage::UsageSnapshot;
use std::path::{Path, PathBuf};

/// What the caller asked for.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub task: &'a str,
    pub dir: &'a Path,
    /// None means auto: classify and let the engine choose.
    pub provider: Option<Provider>,
    /// An explicit model override. Requires an explicit provider: pairing it with auto is
    /// rejected rather than silently dropped.
    pub model: Option<String>,
    /// The job name. None derives it from the task.
    pub name: Option<String>,
    /// Decide and log without dispatching.
    pub dry_run: bool,
    /// MCP config paths forwarded to a claude job, by path only: they may carry server secrets, so
    /// they are never read or logged here.
    pub mcp_configs: &'a [PathBuf],
    /// Replace the claude job's inherited MCP servers with the named configs.
    pub strict_mcp_config: bool,
}

/// What the dispatch produced.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Dispatch {
    /// The backend's own identity: codex thread id, claude short id, opencode session id.
    pub job_id: Option<String>,
    /// The name the job is findable by, which is how a claude job with no resolved short id is
    /// still locatable.
    pub job_name: String,
    /// The reasoning effort the backend reported the job will run at, which is a different fact
    /// from the effort the router decided. Populated by codex alone, from the `thread/start`
    /// reply. None for claude and opencode, permanently: neither exposes one, so there is nothing
    /// observed to record and an inferred value here would read as an observed one.
    pub effective_effort: Option<String>,
    /// The cloud task's URL, as `codex cloud exec` printed it. None on every local dispatch, where
    /// there is no such thing rather than one that went unread. The task id parsed out of it is in
    /// `job_id`, so a cloud job is findable by the same field as every other backend's.
    pub cloud_task_url: Option<String>,
}

/// The whole outcome, including the decision log row id.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub decision: Decision,
    pub dispatch: Option<Dispatch>,
    /// None when the row could not be written. A job that is already running must still be
    /// reported to the caller, so a logging failure downgrades to `log_error` rather than
    /// swallowing the job identity behind an Err.
    pub log_id: Option<i64>,
    pub log_error: Option<String>,
    /// The projected weekly draw, populated on the dry run path only. A real dispatch is not asking
    /// what a job would cost, it is spending it, so there is nothing to project.
    pub estimate: Option<Estimate>,
}

/// IMPURE: run one task through the router.
pub fn run(request: &Request, config: &Config) -> Result<Outcome> {
    // The auto path picks its model from the complexity tiers, so an override there could only be
    // dropped. Rejecting the pair is the only way the caller hears about it.
    if request.provider.is_none() && request.model.is_some() {
        return Err(Error::Command(
            "--model requires an explicit --provider: the auto path chooses its own model"
                .to_string(),
        ));
    }
    // An empty or whitespace only name beats the derived default because it is Some, so it would
    // reach the spawned job and orphan it. A loud error is correct, since a caller passing an
    // empty name believes it set a specific one.
    if let Some(name) = &request.name
        && name.trim().is_empty()
    {
        return Err(Error::Command(
            "--name must not be empty or whitespace only".to_string(),
        ));
    }
    if !request.dir.is_dir() {
        return Err(Error::Command(format!(
            "target directory does not exist: {}",
            request.dir.display()
        )));
    }
    let usage = UsageSnapshot::read();
    let decision = match request.provider {
        Some(provider) => decide_explicit(provider, request.model.clone(), usage, config),
        // `decide` is pure and takes the instant it decides at, so the clock is read here, on the
        // impure side, and after the usage snapshot the run rate rules measure against it.
        None => {
            // Resolved here for the same reason and never inside `decide`: this reads git, the
            // filesystem, and an HTTP endpoint, and `decide` staying pure is what lets
            // `pace_backtest` replay a recorded row at the instant it was decided. Only the auto
            // path resolves it, so an explicit --provider run makes no git call and no network
            // call at all.
            let cloud = crate::cloud::eligibility(request.dir, config);
            decide(
                classify(request.task, config),
                usage,
                crate::usage::now_epoch(),
                cloud,
                config,
            )
        }
    };
    let requested = request
        .provider
        .map(|provider| provider.name())
        .unwrap_or("auto");
    let log = DecisionLog::open()?;

    if request.dry_run {
        // A dry run is how a caller checks an invocation before committing to it, so the claude
        // only scoping flags refuse here too, for the provider the decision landed on: reporting a
        // clean route would hide that the real run would have dropped them.
        crate::dispatch::reject_mcp_scoping(request, decision.provider)?;
        // Projected before this run's own row is written, so the dry run is never a sample of
        // itself. It dispatched nothing, so it drew nothing.
        let estimate = crate::estimate::project(&log, &decision)?;
        let log_id = log.record(&Entry {
            task: request.task,
            dir: request.dir,
            requested,
            decision: &decision,
            dry_run: true,
            job_id: None,
            job_name: None,
            outcome: "dry-run",
            // A dry run dispatched nothing, so no backend said anything about an effort.
            effective_effort: None,
            // Nor did it submit a cloud task, so there is no URL. This is what makes a cloud dry
            // run a projection rather than a submission: `run` returns above `dispatch`, so no
            // `codex cloud exec` process is ever created.
            cloud_task_url: None,
        })?;
        return Ok(Outcome {
            decision,
            dispatch: None,
            log_id: Some(log_id),
            log_error: None,
            estimate: Some(estimate),
        });
    }

    let dispatched = crate::dispatch::dispatch(&decision, request);
    // The decision is logged either way: a dispatch that failed is exactly the row worth
    // keeping, and losing it would hide the failure from the tuning data.
    let fields = recorded_fields(&dispatched);
    let recorded = log.record(&Entry {
        task: request.task,
        dir: request.dir,
        requested,
        decision: &decision,
        dry_run: false,
        job_id: fields.job_id.as_deref(),
        job_name: fields.job_name.as_deref(),
        outcome: &fields.outcome,
        effective_effort: fields.effective_effort.as_deref(),
        cloud_task_url: fields.cloud_task_url.as_deref(),
    });
    // The dispatch decides the result, not the logging: once a job is running, returning Err
    // because a row could not be written would hide the job identity from the caller, who would
    // then reasonably retry and run the task twice.
    let dispatch = dispatched?;
    let (log_id, log_error) = match recorded {
        Ok(id) => (Some(id), None),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(Outcome {
        decision,
        dispatch: Some(dispatch),
        log_id,
        log_error,
        estimate: None,
    })
}

/// What one dispatch result contributes to its log row.
///
/// Both `effective_effort` and `cloud_task_url` are the backend's own report, carried straight
/// through: a run that dropped either here would write a row indistinguishable from a backend that
/// reports none at all, so this seam is where an observed value either survives or silently
/// disappears.
///
/// Named fields rather than a positional tuple, and the reason is the seam itself. A fifth member
/// would put four consecutive `Option<String>` values in a destructure, in the one function whose
/// whole job is not losing a field to its neighbour, and a transposed effort and url would satisfy
/// every type check and every assertion that only looks at whether a value arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedFields {
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    pub effective_effort: Option<String>,
    pub cloud_task_url: Option<String>,
    pub outcome: String,
}

/// PURE: the fields one dispatch result contributes to its log row. A failure observed nothing, so
/// every option is None and the outcome carries the backend's own message.
pub fn recorded_fields(dispatched: &Result<Dispatch>) -> RecordedFields {
    match dispatched {
        Ok(dispatch) => RecordedFields {
            job_id: dispatch.job_id.clone(),
            job_name: Some(dispatch.job_name.clone()),
            effective_effort: dispatch.effective_effort.clone(),
            cloud_task_url: dispatch.cloud_task_url.clone(),
            outcome: "dispatched".to_string(),
        },
        Err(e) => RecordedFields {
            job_id: None,
            job_name: None,
            effective_effort: None,
            cloud_task_url: None,
            outcome: format!("error: {e}"),
        },
    }
}

/// PURE: the provider a `--provider` value names. None for "auto".
pub fn parse_provider(value: &str) -> Result<Option<Provider>> {
    match value {
        "auto" => Ok(None),
        "codex" => Ok(Some(Provider::Codex)),
        "claude" => Ok(Some(Provider::Claude)),
        "opencode" => Ok(Some(Provider::Opencode)),
        other => Err(Error::Command(format!(
            "unknown provider {other:?}: expected auto, codex, claude, or opencode"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_values_map_to_backends_and_auto_maps_to_none() {
        assert_eq!(parse_provider("auto").expect("auto"), None);
        assert_eq!(
            parse_provider("codex").expect("codex"),
            Some(Provider::Codex)
        );
        assert_eq!(
            parse_provider("claude").expect("claude"),
            Some(Provider::Claude)
        );
        assert_eq!(
            parse_provider("opencode").expect("opencode"),
            Some(Provider::Opencode)
        );
        assert!(parse_provider("gpt").is_err());
    }
}
