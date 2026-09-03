use crate::binary::GROK_BIN_ENV;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::run::Dispatch;
use agent_viewer_core::{GrokLifecycle, SpawnResult};
use std::path::Path;

/// IMPURE: start one headless Grok task through Agent Viewer's official lifecycle.
pub fn dispatch(
    ctx: &Context,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
) -> Result<Dispatch> {
    let binary = crate::binary::resolve(Provider::Grok, &ctx.environment)?;
    dispatch_with_binary(&binary, ctx.grok_home(), cwd, task, name, model)
}

pub fn dispatch_with_binary(
    binary: &Path,
    grok_home: &Path,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
) -> Result<Dispatch> {
    let lifecycle = GrokLifecycle::new(binary, grok_home);
    dispatch_from(binary, cwd, task, name, model, |cwd, task, model| {
        lifecycle.spawn(cwd, task, model)
    })
}

/// IMPURE through `spawn`: keep identity conversion separate from the lifecycle transport.
///
/// No resolved path reaches here, so a launch diagnosis names the provider's binary name — the
/// same string this path has always named. [`dispatch_with_binary`] carries the resolved path.
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
    dispatch_from(
        Path::new(Provider::Grok.name()),
        cwd,
        task,
        name,
        model,
        spawn,
    )
}

/// IMPURE through `spawn`: the shared body, told which binary a launch failure should name.
fn dispatch_from<F>(
    binary: &Path,
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    spawn: F,
) -> Result<Dispatch>
where
    F: FnOnce(&Path, &str, Option<&str>) -> agent_viewer_core::Result<SpawnResult>,
{
    let session_id = exact_session_id(binary, spawn(cwd, task, model))?;
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
    // `GrokLifecycle` does not expose the path it was built from, so the review lane names the
    // provider's binary name here exactly as it always has.
    exact_session_id(
        Path::new(Provider::Grok.name()),
        lifecycle.spawn(cwd, task, model),
    )
}

fn exact_session_id(
    binary: &Path,
    spawned: agent_viewer_core::Result<SpawnResult>,
) -> Result<String> {
    spawned
        .map_err(|error| match error {
            // The residue after resolution: the binary was there and the lifecycle's own exec then
            // failed. `binary::launch_error` owns which io kinds that covers — ENOENT, and the
            // `EACCES` a lost exec bit or a `noexec` mount raises, since the mode heuristic that
            // selected the binary cannot tell whether THIS process may execute it. Formatting
            // either into the existing string would report the production io text wearing a
            // prefix. Every io kind it declines, and every non-io lifecycle failure, keeps the
            // existing message byte-identically, because other tests assert on it.
            agent_viewer_core::Error::Io(error) => {
                match crate::binary::launch_error(binary, GROK_BIN_ENV, error) {
                    launch @ Error::Launch(_) => launch,
                    // `Error::Io`'s `Display` is the io error's own, and
                    // `agent_viewer_core::Error::Io` is `#[error(transparent)]`, so this is the
                    // same sentence the unclassified arm produced.
                    Error::Io(error) => lifecycle_failure(&error),
                    other => other,
                }
            }
            error => lifecycle_failure(&error),
        })?
        .session_id
        .filter(|session_id| !session_id.trim().is_empty())
        .ok_or_else(|| {
            Error::Command("Grok lifecycle spawn returned no session identity".to_string())
        })
}

/// PURE: a Grok lifecycle failure that is not a launch failure, in the words it has always used.
fn lifecycle_failure(error: &dyn std::fmt::Display) -> Error {
    Error::Command(format!("Grok lifecycle spawn failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_io(kind: std::io::ErrorKind) -> agent_viewer_core::Result<SpawnResult> {
        Err(agent_viewer_core::Error::Io(std::io::Error::from(kind)))
    }

    /// The half B10 missed: a resolved binary that lost its exec bit, or that sits on a `noexec`
    /// mount, fails the exec with `EACCES` rather than `ENOENT`. It is the same event — the CLI
    /// never ran — so it must reach the decision row as `Launch`, not flattened into `Command`.
    /// The variant is the assertion: the `Command` message reads plausibly either way.
    #[test]
    fn a_lifecycle_permission_denied_after_resolution_is_a_launch_failure() {
        let error = dispatch_with_lifecycle(
            Path::new("/tmp"),
            "score this",
            "Fixture Job",
            None,
            |_, _, _| spawn_io(std::io::ErrorKind::PermissionDenied),
        )
        .expect_err("an unexecutable binary is a launch failure");

        assert!(
            matches!(error, Error::Launch(_)),
            "a lifecycle EACCES is a launch failure: {error:?}"
        );
    }

    /// The bound on that widening: an io fault that is not about the binary is a different event,
    /// and reporting a full disk or a broken pipe as a missing CLI would make the launch
    /// diagnosis worthless. It keeps its variant and its sentence.
    #[test]
    fn a_lifecycle_io_fault_that_is_not_about_the_binary_stays_a_command_failure() {
        let error = dispatch_with_lifecycle(
            Path::new("/tmp"),
            "score this",
            "Fixture Job",
            None,
            |_, _, _| spawn_io(std::io::ErrorKind::BrokenPipe),
        )
        .expect_err("a broken pipe still fails the dispatch");

        assert!(
            matches!(error, Error::Command(_)),
            "only a missing or unusable binary becomes Launch: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "Grok lifecycle spawn failed: {}",
                std::io::Error::from(std::io::ErrorKind::BrokenPipe)
            ),
            "the existing message must survive byte-identically"
        );
    }

    /// A launch diagnosis that named `grok` rather than the path resolution actually picked would
    /// send an operator to the wrong file. The dispatch path carries the resolved binary.
    #[test]
    fn the_resolved_binary_is_what_a_launch_failure_names() {
        let resolved = Path::new("/opt/pinned/grok");
        let error = dispatch_from(
            resolved,
            Path::new("/tmp"),
            "score this",
            "Fixture Job",
            None,
            |_, _, _| spawn_io(std::io::ErrorKind::NotFound),
        )
        .expect_err("a lifecycle ENOENT is a launch failure");

        assert!(matches!(error, Error::Launch(_)), "{error:?}");
        assert!(
            error.to_string().contains("/opt/pinned/grok"),
            "the resolved path must reach the diagnosis: {error}"
        );
    }
}
