use crate::error::{Error, Result};
use crate::run::Dispatch;
use agent_viewer_core::{GrokLifecycle, SpawnResult};
use std::path::{Path, PathBuf};

/// IMPURE: start one headless Grok task through Agent Viewer's official lifecycle.
pub fn dispatch(cwd: &Path, task: &str, name: &str, model: Option<&str>) -> Result<Dispatch> {
    dispatch_with_binary(Path::new("grok"), cwd, task, name, model)
}

pub fn dispatch_with_binary(
    binary: &Path,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
) -> Result<Dispatch> {
    let lifecycle = GrokLifecycle::new(binary, grok_home());
    dispatch_with_lifecycle(cwd, task, name, model, |cwd, task, model| {
        lifecycle.spawn(cwd, task, model)
    })
}

/// IMPURE through `spawn`: keep identity conversion separate from the lifecycle transport.
pub fn dispatch_with_lifecycle<F>(
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    spawn: F,
) -> Result<Dispatch>
where
    F: FnOnce(&Path, &str, Option<&str>) -> agent_viewer_core::Result<SpawnResult>,
{
    let session_id = exact_session_id(spawn(cwd, task, model))?;
    Ok(Dispatch {
        job_id: Some(session_id),
        job_name: name.to_string(),
        effective_effort: None,
    })
}

pub(crate) fn spawn_with_lifecycle(
    lifecycle: &GrokLifecycle,
    cwd: &Path,
    task: &str,
    model: Option<&str>,
) -> Result<String> {
    exact_session_id(lifecycle.spawn(cwd, task, model))
}

fn exact_session_id(spawned: agent_viewer_core::Result<SpawnResult>) -> Result<String> {
    spawned
        .map_err(|error| Error::Command(format!("Grok lifecycle spawn failed: {error}")))?
        .session_id
        .filter(|session_id| !session_id.trim().is_empty())
        .ok_or_else(|| {
            Error::Command("Grok lifecycle spawn returned no session identity".to_string())
        })
}

pub(crate) fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| agent_viewer_core::home_dir().join(".grok"))
}
