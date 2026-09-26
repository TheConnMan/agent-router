use crate::config::Surface;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::{Dispatch, Request};

pub mod claude;
pub mod codex;
pub mod grok;
pub mod t3;

pub fn dispatch(
    ctx: &Context,
    decision: &crate::decide::Decision,
    request: &Request,
) -> Result<Dispatch> {
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
        .unwrap_or_else(|| crate::runtime::short_job_name(request.task));
    if request.surface == Surface::T3 {
        // The single launch fork: everything above, and every gate before this call, ran exactly
        // as it does for a background launch. MCP scoping already passed `reject_mcp_scoping`,
        // which only a claude route can; T3 has no MCP flags, so the configs are deliberately not
        // forwarded (or even preflighted), and the caller prints the warning `run` attached.
        return t3::dispatch(
            ctx,
            request.dir,
            request.task,
            &name,
            decision.provider,
            decision.model.as_deref(),
            decision.effort.as_deref(),
        );
    }
    match decision.provider {
        Provider::Codex => codex::dispatch(
            ctx,
            request.dir,
            request.task,
            &name,
            decision.model.as_deref(),
            decision.effort.as_deref(),
        ),
        Provider::Claude => claude::dispatch(
            ctx,
            request.dir,
            request.task,
            &name,
            decision.model.as_deref(),
            decision.effort.as_deref(),
            request.mcp_configs,
            request.strict_mcp_config,
        ),
        Provider::Grok => grok::dispatch(
            ctx,
            request.dir,
            request.task,
            &name,
            decision.model.as_deref(),
        ),
    }
}

/// Only claude takes MCP scoping, so every other provider refuses it here rather than up front in
/// the CLI: an auto route that lands on codex or grok must fail exactly like an explicit one,
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
