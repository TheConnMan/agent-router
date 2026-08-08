//! One routed task end to end: read usage, classify (unless the caller named a provider),
//! decide, dispatch, log.

use crate::classify::classify_with_name;
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
    let (decision, generated_name) = match request.provider {
        // A named provider needs nothing scored, but it still gets a generated title. Without one
        // the job falls back to `short_job_name`, which is the first few words of the task title
        // cased, and every job dispatched with a provider named was findable only by that.
        //
        // Two cases skip the call rather than waste it: a caller that supplied `--name` has
        // already decided the name, and a dry run dispatches nothing, so there is nothing to name.
        Some(provider) => (
            decide_explicit(provider, request.model.clone(), usage, config),
            (request.name.is_none() && !request.dry_run)
                .then(|| crate::classify::job_name(request.task, config))
                .flatten(),
        ),
        // `decide` is pure and takes the instant it decides at, so the clock is read here, on the
        // impure side, and after the usage snapshot the run rate rules measure against it.
        None => {
            let classified = classify_with_name(request.task, config);
            (
                decide(
                    classified.classification,
                    usage,
                    crate::usage::now_epoch(),
                    config,
                ),
                classified.job_name,
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
        })?;
        return Ok(Outcome {
            decision,
            dispatch: None,
            log_id: Some(log_id),
            log_error: None,
            estimate: Some(estimate),
        });
    }

    let dispatch_request = Request {
        task: request.task,
        dir: request.dir,
        provider: request.provider,
        model: request.model.clone(),
        name: request.name.clone().or(generated_name),
        dry_run: request.dry_run,
        mcp_configs: request.mcp_configs,
        strict_mcp_config: request.strict_mcp_config,
    };
    let dispatched = crate::dispatch::dispatch(&decision, &dispatch_request);
    // The decision is logged either way: a dispatch that failed is exactly the row worth
    // keeping, and losing it would hide the failure from the tuning data.
    let (job_id, job_name, effective_effort, outcome) = recorded_fields(&dispatched);
    let recorded = log.record(&Entry {
        task: request.task,
        dir: request.dir,
        requested,
        decision: &decision,
        dry_run: false,
        job_id: job_id.as_deref(),
        job_name: job_name.as_deref(),
        outcome: &outcome,
        effective_effort: effective_effort.as_deref(),
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

/// PURE: the job id, job name, effective effort, and outcome one dispatch result contributes to
/// its log row. The effective effort is the backend's own report, carried straight through: a run
/// that dropped it here would write a row indistinguishable from a backend that reports no effort
/// at all, so this seam is where the observed value either survives or silently disappears.
pub fn recorded_fields(
    dispatched: &Result<Dispatch>,
) -> (Option<String>, Option<String>, Option<String>, String) {
    match dispatched {
        Ok(dispatch) => (
            dispatch.job_id.clone(),
            Some(dispatch.job_name.clone()),
            dispatch.effective_effort.clone(),
            "dispatched".to_string(),
        ),
        Err(e) => (None, None, None, format!("error: {e}")),
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
