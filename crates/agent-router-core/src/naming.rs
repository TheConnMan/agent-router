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
    /// The provider took the name. `row` says what became of the decision row, which is a
    /// separate question: the job is correctly named either way, but a row left on the launch name
    /// is a disagreement somebody may have to repair by hand, so it is never silently dropped.
    Renamed { name: String, row: Row },
    /// Nothing was renamed, for a reason that is not an error: no usable title, no job id, or a
    /// name a person changed in the meantime.
    Skipped(String),
    /// The rename was attempted and the provider refused or could not be reached.
    Failed(String),
}

/// What became of the decision row after the provider took the new name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Updated,
    /// There was no row to update: the dispatch could not write one, or it has since gone.
    Absent,
    /// The row could not be written. Carried verbatim, because this is the one outcome where the
    /// decision log and the provider disagree and the reason is the whole diagnosis.
    Failed(String),
}

impl Naming {
    pub fn describe(&self) -> String {
        match self {
            Naming::Renamed { name, row } => {
                let row = match row {
                    Row::Updated => "decision row updated".to_string(),
                    Row::Absent => "no decision row to update".to_string(),
                    Row::Failed(error) => format!(
                        "DECISION ROW NOT UPDATED, it still carries the launch name: {error}"
                    ),
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
    //
    // The failure is carried through whole. A worker that logged only "no usable title" hid a
    // refused title, a timeout, and an unlaunchable CLI behind one sentence, and the first real job
    // to hit it could not be diagnosed without rebuilding the binary.
    let name = match crate::classify::job_name_with(ctx, &job.task, &ctx.config.classifier.naming())
    {
        Ok(name) => name,
        Err(failure) => return Naming::Skipped(failure.describe()),
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
            let row = match job.log_id {
                None => Row::Absent,
                Some(id) => record_rename(ctx, id, &name),
            };
            Naming::Renamed { name, row }
        }
        Err(error) => Naming::Failed(error.to_string()),
    }
}

/// IMPURE: put the new name on the decision row, retried once.
///
/// The provider has already taken the name by the time this runs, so a failure here cannot be
/// undone by failing: it leaves the row and the session disagreeing, and the only useful thing to
/// do with it is say so. `rusqlite` already waits out a busy database, so the single retry is for
/// the writer that was still holding the lock when that wait expired, and there is no second one:
/// naming is bounded work on a job that is already running.
fn record_rename(ctx: &Context, log_id: i64, name: &str) -> Row {
    let attempt =
        || crate::log::DecisionLog::open_in(&ctx.home).and_then(|log| log.rename_job(log_id, name));
    let result = match attempt() {
        Err(_) => {
            std::thread::sleep(std::time::Duration::from_millis(250));
            attempt()
        }
        settled => settled,
    };
    match result {
        Ok(true) => Row::Updated,
        Ok(false) => Row::Absent,
        Err(error) => Row::Failed(error.to_string()),
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
/// ONE read serves both the guard and the mutation, deliberately. Reading the file to decide
/// whether to rename and then handing the job to a writer that reads it again would let a rename
/// made in that window be read as ours and overwritten: the guard would be checking bytes the
/// write never saw. Here the name that is compared and the object that is written are the same
/// parse.
///
/// That narrows the window; it does not close it. Claude's own worker writes this file while the
/// job runs, and nothing in claude's format offers a compare-and-swap, so a write landing between
/// this read and `replace_atomic` is lost. Agent Viewer's own rename accepts exactly this race, for
/// the same reason and against the same writer.
///
/// The three fields are the ones Agent Viewer's writer sets, for the reasons it gives:
/// `nameSource: "user"` is what stops claude's auto-titler overwriting the name later, and claude
/// stamps `updatedAt` on every state write, so leaving it stale would make the rename invisible to
/// anything sorting or invalidating on it. The write itself is Agent Viewer's `replace_atomic`,
/// which preserves the file's mode and never exposes a half-written state.json.
fn rename_claude(jobs_root: &Path, short_id: &str, launch_name: &str, name: &str) -> Result<bool> {
    let path = agent_viewer_core::claude::job_state_path_in(jobs_root, short_id);
    // A missing file means the job is gone. That is an Err the worker reports and drops, never a
    // reason to create one: a blind write would fabricate a job state with no respawn contract.
    let mut state: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let Some(object) = state.as_object_mut() else {
        return Err(Error::Command(format!(
            "{} is not a JSON object",
            path.display()
        )));
    };
    let current = object.get("name").and_then(serde_json::Value::as_str);
    if let Some(current) = current
        && current != launch_name
        && current != name
    {
        return Ok(false);
    }
    object.insert("name".to_string(), serde_json::Value::from(name));
    object.insert("nameSource".to_string(), serde_json::Value::from("user"));
    object.insert(
        "updatedAt".to_string(),
        serde_json::Value::from(agent_viewer_core::claude::iso8601_utc_millis(
            std::time::SystemTime::now(),
        )),
    );
    agent_viewer_core::claude::replace_atomic(&path, &serde_json::to_string_pretty(&state)?)
        .map_err(|error| Error::Command(format!("Claude job rename failed: {error}")))?;
    Ok(true)
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

    /// A row that could not be written after a successful rename is the one case where the log and
    /// the session disagree, and the reason is the whole diagnosis. It must reach the naming log
    /// verbatim rather than reading as an ordinary success.
    #[test]
    fn a_failed_row_update_is_reported_in_full_beside_the_successful_rename() {
        let naming = Naming::Renamed {
            name: "RS-123 Nightly Scheduler Audit".to_string(),
            row: Row::Failed("database is locked".to_string()),
        };
        let described = naming.describe();
        assert!(
            described.contains("RS-123 Nightly Scheduler Audit"),
            "{described}"
        );
        assert!(
            described.contains("DECISION ROW NOT UPDATED"),
            "{described}"
        );
        assert!(described.contains("database is locked"), "{described}");
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
