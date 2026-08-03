use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::{Dispatch, Request};

pub mod claude;
pub mod codex;
pub mod codex_cloud;
pub mod opencode;

pub fn dispatch(decision: &crate::decide::Decision, request: &Request) -> Result<Dispatch> {
    if !request.dir.is_dir() {
        return Err(Error::Command(format!(
            "{} is not a directory",
            request.dir.display()
        )));
    }
    reject_mcp_scoping(request, decision.provider)?;
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| crate::runtime::truncated_title(request.task));
    // Matched on the target FIRST, and the cloud arm dispatches from inside that arm without ever
    // reading `decision.provider`. There is no expression here in which a provider value could steer
    // into or out of the cloud backend: the `CodexCloud` variant names its own backend, and `decide`
    // is the only thing that can build one. Only the local arm reads the provider at all. A runtime
    // check on the provider would put that guarantee in a condition somebody can later relax; this
    // way a claude or opencode decision has no path to reach `codex_cloud::dispatch` at all.
    match &decision.execution_target {
        crate::decide::ExecutionTarget::CodexCloud(target) => {
            codex_cloud::dispatch(request.task, &name, target)
        }
        crate::decide::ExecutionTarget::Local => match decision.provider {
            Provider::Codex => codex::dispatch(
                request.dir,
                request.task,
                &name,
                decision.model.as_deref(),
                decision.effort.as_deref(),
            ),
            Provider::Claude => claude::dispatch(
                request.dir,
                request.task,
                &name,
                decision.model.as_deref(),
                decision.effort.as_deref(),
                request.mcp_configs,
                request.strict_mcp_config,
            ),
            Provider::Opencode => opencode::dispatch(
                request.dir,
                request.task,
                &name,
                decision.model.as_deref(),
                decision.effort.as_deref(),
            ),
        },
    }
}

/// Only claude takes MCP scoping, so every other provider refuses it here rather than up front in
/// the CLI: an auto route that lands on codex or opencode must fail exactly like an explicit one,
/// instead of silently running the job with the caller's scoping dropped.
///
/// Claude is exempted inside the helper, so a caller guards nothing itself and a provider added
/// later cannot skip the refusal by forgetting to call it.
pub(crate) fn reject_mcp_scoping(request: &Request, provider: Provider) -> Result<()> {
    if provider == Provider::Claude {
        return Ok(());
    }
    let provider = provider.name();
    if !request.mcp_configs.is_empty() {
        return Err(Error::Command(format!(
            "--mcp-config is a claude only flag, but this task routed to {provider}: rerun with \
             --provider claude, or drop --mcp-config and configure {provider} servers in its own \
             configuration"
        )));
    }
    if request.strict_mcp_config {
        return Err(Error::Command(format!(
            "--strict-mcp-config is a claude only flag, but this task routed to {provider}: rerun \
             with --provider claude, or drop --strict-mcp-config, which {provider} has no \
             equivalent for"
        )));
    }
    Ok(())
}
