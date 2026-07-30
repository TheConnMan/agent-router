//! One routed task end to end: read usage, classify (unless the caller named a provider),
//! decide, dispatch, log.

use crate::classify::classify;
use crate::config::Config;
use crate::decide::{Decision, decide, decide_explicit};
use crate::error::{Error, Result};
use crate::log::{DecisionLog, Entry};
use crate::usage::UsageSnapshot;
use agent_viewer_core::BackendKind;
use std::path::Path;

/// What the caller asked for.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub task: &'a str,
    pub dir: &'a Path,
    /// None means auto: classify and let the engine choose.
    pub provider: Option<BackendKind>,
    /// An explicit model override, only honoured on the explicit-provider path.
    pub model: Option<String>,
    /// Read-only work: the Codex execution-mode preamble is skipped.
    pub read_only: bool,
    /// Decide and log without dispatching.
    pub dry_run: bool,
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
    pub log_id: i64,
}

/// IMPURE: run one task through the router.
pub fn run(request: &Request, config: &Config) -> Result<Outcome> {
    let usage = UsageSnapshot::read();
    let decision = match request.provider {
        Some(provider) => decide_explicit(provider, request.model.clone(), usage),
        None => decide(classify(request.task, config), usage, config),
    };
    let requested = request
        .provider
        .map(|provider| provider.name())
        .unwrap_or("auto");
    let log = DecisionLog::open()?;

    if request.dry_run {
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
            log_id,
        });
    }

    let dispatched: Result<Dispatch> = Err(Error::Command(
        "dispatch is not wired yet; use --dry-run".to_string(),
    ));
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
    let log_id = log.record(&Entry {
        task: request.task,
        dir: request.dir,
        requested,
        decision: &decision,
        dry_run: false,
        job_id: job_id.as_deref(),
        job_name: job_name.as_deref(),
        outcome: &outcome,
    })?;
    let dispatch = dispatched?;
    Ok(Outcome {
        decision,
        dispatch: Some(dispatch),
        log_id,
    })
}

/// PURE: the provider a `--provider` value names. None for "auto".
pub fn parse_provider(value: &str) -> Result<Option<BackendKind>> {
    match value {
        "auto" => Ok(None),
        "codex" => Ok(Some(BackendKind::Codex)),
        "claude" => Ok(Some(BackendKind::Claude)),
        "opencode" => Ok(Some(BackendKind::Opencode)),
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
            Some(BackendKind::Codex)
        );
        assert_eq!(
            parse_provider("claude").expect("claude"),
            Some(BackendKind::Claude)
        );
        assert_eq!(
            parse_provider("opencode").expect("opencode"),
            Some(BackendKind::Opencode)
        );
        assert!(parse_provider("gpt").is_err());
    }
}
