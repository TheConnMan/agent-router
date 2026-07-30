use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::{Dispatch, Request};

pub mod claude;
pub mod codex;
pub mod opencode;

pub fn dispatch(decision: &crate::decide::Decision, request: &Request) -> Result<Dispatch> {
    if !request.dir.is_dir() {
        return Err(Error::Command(format!(
            "{} is not a directory",
            request.dir.display()
        )));
    }
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| crate::runtime::truncated_title(request.task));
    match decision.provider {
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
        ),
        Provider::Opencode => opencode::dispatch(
            request.dir,
            request.task,
            &name,
            decision.model.as_deref(),
            decision.effort.as_deref(),
        ),
    }
}
