//! One routed task end to end: read usage, classify (unless the caller named a provider),
//! decide, dispatch, log.

use crate::classify::classify;
use crate::config::Config;
use crate::decide::{Decision, decide, decide_explicit};
use crate::error::{Error, Result};
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
        None => decide(classify(request.task, config), usage, config),
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
        let log_id = log.record(&Entry {
            task: request.task,
            dir: request.dir,
            requested,
            decision: &decision,
            dry_run: true,
            job_id: None,
            job_name: None,
            outcome: "dry-run",
        })?;
        return Ok(Outcome {
            decision,
            dispatch: None,
            log_id: Some(log_id),
            log_error: None,
        });
    }

    let dispatched = crate::dispatch::dispatch(&decision, request);
    // The decision is logged either way: a dispatch that failed is exactly the row worth
    // keeping, and losing it would hide the failure from the tuning data.
    let (job_id, job_name, outcome) = match &dispatched {
        Ok(dispatch) => (
            dispatch.job_id.clone(),
            Some(dispatch.job_name.clone()),
            "dispatched".to_string(),
        ),
        Err(e) => (None, None, format!("error: {e}")),
    };
    let recorded = log.record(&Entry {
        task: request.task,
        dir: request.dir,
        requested,
        decision: &decision,
        dry_run: false,
        job_id: job_id.as_deref(),
        job_name: job_name.as_deref(),
        outcome: &outcome,
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
    })
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
