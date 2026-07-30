use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::{Dispatch, Request};

pub mod claude;
pub mod codex;
pub mod opencode;

pub const CODEX_PREAMBLE: &str = "Edit the files directly, right now, with your own tools. Do not \
     write a plan file as a substitute for editing, do not spawn claude, codex, or any other \
     subprocess, and make your first file edit within your first two tool calls.";

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
            &codex_prompt(request.task, request.read_only),
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

pub fn codex_prompt(task: &str, read_only: bool) -> String {
    if read_only {
        task.to_string()
    } else {
        format!("{CODEX_PREAMBLE}\n\n{task}")
    }
}
