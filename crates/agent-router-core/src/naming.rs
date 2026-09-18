//! Asynchronous session naming: one generative title, applied to a job that is already running.
//!
//! A good title needs a model, and a model call is far slower than the launch it would otherwise
//! sit in front of. So the router launches with whatever name it already has and hands the naming
//! work to a detached worker, which renames the exact launched session afterwards by the identity
//! the dispatch resolved.
//!
//! Naming is cosmetic and the job is already running, so every failure here is reported and
//! dropped. Nothing in this module may fail, stop, or relaunch a job.

use crate::context::Context;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::runtime::{router_log_path, spawn_detached};
use std::path::{Path, PathBuf};

/// Everything the detached worker needs to name one launched job.
#[derive(Debug, Clone, PartialEq)]
pub struct NameJob {
    pub provider: Provider,
    /// The backend's own identity. None means the dispatch resolved none, and nothing can be
    /// renamed by a name alone: a job would have to be found by the very field being changed.
    pub job_id: Option<String>,
    /// The name the job launched with. The manual-rename guard compares against exactly this.
    pub launch_name: String,
    /// The decision row to keep in step with the provider. None when the row could not be written.
    pub log_id: Option<i64>,
    pub task: String,
}

/// What one naming attempt did. Every variant is a normal outcome; none of them is a job failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Naming {
    /// The provider took the name, and the decision row was updated when there was one.
    Renamed { name: String, row_updated: bool },
    /// Nothing was renamed, for a reason that is not an error: no usable title, no job id, or a
    /// name a person changed in the meantime.
    Skipped(String),
    /// The rename was attempted and the provider refused or could not be reached.
    Failed(String),
}

impl Naming {
    pub fn describe(&self) -> String {
        match self {
            Naming::Renamed { name, row_updated } => {
                let row = if *row_updated {
                    "decision row updated"
                } else {
                    "no decision row updated"
                };
                format!("renamed to {name:?} ({row})")
            }
            Naming::Skipped(why) => format!("skipped: {why}"),
            Naming::Failed(why) => format!("failed: {why}"),
        }
    }
}

/// IMPURE: generate a title and apply it. The whole worker body, minus process plumbing.
pub fn name_job(ctx: &Context, job: &NameJob) -> Naming {
    let Some(job_id) = job.job_id.as_deref().filter(|id| !id.is_empty()) else {
        return Naming::Skipped(format!(
            "{} dispatch resolved no job id, so there is no session to address",
            job.provider.name()
        ));
    };
    // One call, through the naming engine rather than the scoring engine, and no retry. The
    // scoring call already had its chance to return a title; a worker only exists because it did
    // not, and a second failure is a title this box cannot generate today, not a flake worth
    // paying for twice.
    let Some(name) =
        crate::classify::job_name_with(ctx, &job.task, &ctx.config.classifier.naming())
    else {
        return Naming::Skipped("the naming model returned no usable title".to_string());
    };
    if name == job.launch_name {
        return Naming::Skipped("the generated title matches the launch name".to_string());
    }
    match rename(ctx, job.provider, job_id, &job.launch_name, &name) {
        Ok(false) => Naming::Skipped(format!(
            "{} reports a name this router did not set, so a manual rename was kept",
            job.provider.name()
        )),
        Ok(true) => {
            let row_updated = job.log_id.is_some_and(|id| {
                crate::log::DecisionLog::open_in(&ctx.home)
                    .and_then(|log| log.rename_job(id, &name))
                    .unwrap_or(false)
            });
            Naming::Renamed { name, row_updated }
        }
        Err(error) => Naming::Failed(error.to_string()),
    }
}

/// IMPURE: rename one launched session in its provider's own store.
///
/// `Ok(false)` means the provider reports a name neither the router set nor the one being written,
/// so a person renamed it and their title is kept. Only Codex and Claude can answer that question;
/// Grok's rename RPC reports success and nothing else, so a Grok job is always renamed.
///
/// Every mechanism here is Agent Viewer's, either its crate directly or, for Codex, the app-server
/// call this crate already makes at dispatch.
pub fn rename(
    ctx: &Context,
    provider: Provider,
    job_id: &str,
    launch_name: &str,
    name: &str,
) -> Result<bool> {
    match provider {
        Provider::Codex => crate::dispatch::codex::rename_thread(ctx, job_id, launch_name, name),
        Provider::Claude => rename_claude(&claude_jobs_root(&ctx.home), job_id, launch_name, name),
        Provider::Grok => {
            let binary = crate::binary::resolve(Provider::Grok, &ctx.environment)?;
            agent_viewer_core::GrokLifecycle::new(binary, ctx.grok_home())
                .rename(job_id, name)
                .map_err(|error| Error::Command(format!("Grok session rename failed: {error}")))?;
            Ok(true)
        }
    }
}

/// `$CLAUDE_CONFIG_DIR/jobs` when set, else `{home}/.claude/jobs`: the same precedence Agent
/// Viewer's own jobs root uses, resolved from this context's home rather than the process
/// environment so a test home is honoured.
pub fn claude_jobs_root(home: &Path) -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        Some(config_dir) => PathBuf::from(config_dir).join("jobs"),
        None => home.join(".claude").join("jobs"),
    }
}

/// IMPURE: Claude's rename, which is a read-modify-write of the job's `state.json`.
///
/// The guard reads that same file first. Claude stamps `nameSource` on every write, and Agent
/// Viewer's writer sets it to `user` exactly so Claude's own auto-titler leaves the name alone, so
/// a name that is neither the launch name nor the new one is somebody's deliberate rename.
///
/// The write itself is `ClaudeBackend::rename`, not a local copy of it: that writer preserves the
/// job's respawn contract, stamps `nameSource` and `updatedAt`, and writes atomically, and a second
/// implementation of it here is how this file and Claude's format drift apart.
fn rename_claude(jobs_root: &Path, short_id: &str, launch_name: &str, name: &str) -> Result<bool> {
    let path = agent_viewer_core::claude::job_state_path_in(jobs_root, short_id);
    let state: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let current = state.get("name").and_then(serde_json::Value::as_str);
    if let Some(current) = current
        && current != launch_name
        && current != name
    {
        return Ok(false);
    }
    let session = claude_session(short_id, current.unwrap_or(launch_name));
    agent_viewer_core::backend::Backend::rename(
        &agent_viewer_core::claude::ClaudeBackend::with_binary_and_jobs_root(
            "claude",
            jobs_root.to_path_buf(),
        ),
        &session,
        name,
    )
    .map_err(|error| Error::Command(format!("Claude job rename failed: {error}")))?;
    Ok(true)
}

/// PURE: the minimal session `ClaudeBackend::rename` addresses.
///
/// It reads `short_id` and nothing else, but `Session` is Agent Viewer's whole row type and has no
/// `Default`, so the remaining fields are filled with empties. They are never read, and building
/// this here rather than fabricating a row from a listing keeps the rename addressed by the exact
/// identity the dispatch resolved.
fn claude_session(short_id: &str, title: &str) -> agent_viewer_core::Session {
    agent_viewer_core::Session {
        backend: agent_viewer_core::BackendKind::Claude,
        id: short_id.to_string(),
        short_id: Some(short_id.to_string()),
        origin: agent_viewer_core::SessionOrigin::Background,
        title: title.to_string(),
        cwd: PathBuf::new(),
        git_branch: None,
        status: agent_viewer_core::Status::Unknown,
        created_at_ms: 0,
        updated_at_ms: 0,
        hidden: false,
        companion: false,
        subagent: false,
        summary: String::new(),
        pid: None,
        rollout_path: None,
        pr_refs: Vec::new(),
        daemon_hosted: false,
    }
}

/// IMPURE: start the detached worker that will name `job`.
///
/// `setsid`, through the same `spawn_detached` the adversarial review worker uses, is what makes
/// the naming outlive the router process: the router returns its dispatch result and exits while
/// the model call is still in flight.
///
/// A worker that cannot be started is reported to the caller and changes nothing else. The job is
/// already running under its launch name.
pub fn spawn_worker(ctx: &Context, job: &NameJob) -> Result<std::process::Child> {
    let exe = std::env::current_exe().map_err(Error::Io)?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("name-worker")
        .arg("--provider")
        .arg(job.provider.name())
        .arg("--launch-name")
        .arg(&job.launch_name);
    if let Some(job_id) = &job.job_id {
        command.arg("--job-id").arg(job_id);
    }
    if let Some(log_id) = job.log_id {
        command.arg("--log-id").arg(log_id.to_string());
    }
    // `--` keeps a task whose own text starts with a dash from being read as a flag on the
    // worker's fresh argv.
    command.arg("--").arg(&job.task);
    spawn_detached(command, &router_log_path(&ctx.home, "naming"), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> String {
        serde_json::json!({
            "name": name,
            "nameSource": "auto",
            "respawn": {"prompt": "keep me"}
        })
        .to_string()
    }

    fn jobs_root_with(short_id: &str, body: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join(short_id);
        std::fs::create_dir_all(&dir).expect("job dir");
        std::fs::write(dir.join("state.json"), body).expect("state.json");
        root
    }

    #[test]
    fn a_claude_job_still_carrying_its_launch_name_is_renamed_in_place() {
        let root = jobs_root_with("ab12cd", &state("Audit Scheduled Background Agents"));
        let renamed = rename_claude(
            root.path(),
            "ab12cd",
            "Audit Scheduled Background Agents",
            "RS-123 Nightly Scheduler Audit",
        )
        .expect("the rename must succeed");

        assert!(
            renamed,
            "a job with its launch name is the router's to name"
        );
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("ab12cd/state.json")).expect("read back"),
        )
        .expect("state.json stays json");
        assert_eq!(written["name"], "RS-123 Nightly Scheduler Audit");
        assert_eq!(
            written["nameSource"], "user",
            "the name must outrank claude's own auto-titler"
        );
        assert_eq!(
            written["respawn"]["prompt"], "keep me",
            "a rename must never drop the respawn contract"
        );
    }

    /// The whole point of the guard: a person who renamed the job between launch and this call
    /// outranks a generated title, and their name must survive untouched.
    #[test]
    fn a_claude_job_renamed_by_hand_keeps_the_name_a_person_gave_it() {
        let root = jobs_root_with("ab12cd", &state("My Own Title"));
        let renamed = rename_claude(
            root.path(),
            "ab12cd",
            "Audit Scheduled Background Agents",
            "RS-123 Nightly Scheduler Audit",
        )
        .expect("a kept name is not an error");

        assert!(!renamed, "a manual rename is kept, not overwritten");
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("ab12cd/state.json")).expect("read back"),
        )
        .expect("state.json stays json");
        assert_eq!(written["name"], "My Own Title");
        assert_eq!(
            written["nameSource"], "auto",
            "a skipped rename must not write the file at all"
        );
    }

    /// A job whose state file is gone is a job that no longer exists. That is an Err the worker
    /// reports and drops; it is never a reason to fail or relaunch anything.
    #[test]
    fn a_missing_claude_job_state_file_is_an_error_rather_than_a_silent_success() {
        let root = tempfile::tempdir().expect("tempdir");
        let error = rename_claude(root.path(), "ab12cd", "Launch Name Here", "New Name Here")
            .expect_err("a job with no state file cannot be renamed");
        assert!(matches!(error, Error::Io(_)), "{error:?}");
    }

    #[test]
    fn a_job_with_no_resolved_identity_is_skipped_rather_than_guessed_at() {
        let ctx = Context::new(
            crate::binary::Environment::default(),
            PathBuf::from("/nonexistent"),
            crate::config::Config::default(),
        );
        let naming = name_job(
            &ctx,
            &NameJob {
                provider: Provider::Claude,
                job_id: None,
                launch_name: "Audit Scheduled Background Agents".to_string(),
                log_id: None,
                task: "audit scheduled background agents".to_string(),
            },
        );
        assert!(
            matches!(&naming, Naming::Skipped(why) if why.contains("no job id")),
            "{naming:?}"
        );
    }
}
