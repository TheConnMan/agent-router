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
/// Every mechanism here is one this crate already owns: the Codex app-server call it makes at
/// dispatch, the Claude `state.json` writer below, and the Grok leader client in
/// [`crate::grok_leader`].
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
            crate::grok_leader::GrokLifecycle::new(binary, ctx.grok_home())
                .rename(job_id, name)
                .map_err(|error| Error::Command(format!("Grok session rename failed: {error}")))?;
            Ok(true)
        }
    }
}

/// `$CLAUDE_CONFIG_DIR/jobs` when set, else `{home}/.claude/jobs`: the same precedence Claude
/// itself uses for its jobs root, resolved from this context's home rather than the process
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
/// this read and `replace_atomic` is lost. Any outside rename of a Claude job accepts exactly this
/// race, for the same reason and against the same writer.
///
/// The three fields are the ones a human rename sets: `nameSource: "user"` is what stops claude's
/// auto-titler overwriting the name later, and claude stamps `updatedAt` on every state write, so
/// leaving it stale would make the rename invisible to anything sorting or invalidating on it. The
/// write itself is [`replace_atomic`], which preserves the file's mode and never exposes a
/// half-written state.json.
fn rename_claude(jobs_root: &Path, short_id: &str, launch_name: &str, name: &str) -> Result<bool> {
    let path = job_state_path_in(jobs_root, short_id)?;
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
        serde_json::Value::from(iso8601_utc_millis(std::time::SystemTime::now())),
    );
    replace_atomic(&path, &serde_json::to_string_pretty(&state)?)
        .map_err(|error| Error::Command(format!("Claude job rename failed: {error}")))?;
    Ok(true)
}

/// True when `short_id` is a single path component that can be joined under the jobs root.
/// Empty, `.`, `..`, anything with a path separator, and anything Path would split into more than
/// one component are refused so a hostile `claude agents` listing cannot walk out of the jobs root.
fn is_safe_job_short_id(short_id: &str) -> bool {
    if short_id.is_empty() || short_id == "." || short_id == ".." {
        return false;
    }
    if short_id.contains('/') || short_id.contains('\\') {
        return false;
    }
    let mut components = Path::new(short_id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(name)), None) => name == std::ffi::OsStr::new(short_id),
        _ => false,
    }
}

/// PURE: `<jobs_root>/<short_id>/state.json`. Refuses a short id that is not a single path
/// component.
fn job_state_path_in(jobs_root: &Path, short_id: &str) -> Result<PathBuf> {
    if !is_safe_job_short_id(short_id) {
        return Err(Error::Command(
            "command failed: Claude job identity is not a single path component".to_string(),
        ));
    }
    Ok(jobs_root.join(short_id).join("state.json"))
}

/// PURE: `SystemTime` as `YYYY-MM-DDTHH:MM:SS.mmmZ`, the exact shape JavaScript's `toISOString()`
/// produces, because the value lands in a field claude writes with that call. Pre-epoch times
/// clamp to the epoch (they cannot occur for a job state and are not worth a signed path).
fn iso8601_utc_millis(time: std::time::SystemTime) -> String {
    let since = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, millis) = (since.as_secs(), since.subsec_millis());
    let (days, secs_of_day) = ((secs / 86_400) as i64, secs % 86_400);
    // days_to_civil (Howard Hinnant's civil-from-days), shifted to an era starting 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    )
}

/// IMPURE: create `path` for writing, owner read/write only, failing if it already exists. The
/// security property is that the file is NEVER group/other readable, not even for the instant
/// between creation and a later chmod. umask can only clear bits, so 0600 is an upper bound.
#[cfg(unix)]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    std::os::unix::fs::OpenOptionsExt::mode(
        std::fs::OpenOptions::new().write(true).create_new(true),
        0o600,
    )
    .open(path)
}

/// IMPURE: replace `path`'s contents with `body` atomically: write a temp file beside it, then
/// rename over the target, so a concurrent reader (the claude daemon) sees either the old file or
/// the new one, never a partial write. Mirrors claude's own state writer.
///
/// The temp file inherits the target's mode BEFORE the rename. Claude writes state.json 0600 while
/// the jobs dir itself is traversable, so leaving the temp at the umask default would quietly
/// publish that job's intent, output, respawn flags, and transcript path to every local user.
///
/// REPLACE, never create: a missing target is an error. `claude rm` can unlink state.json between
/// the caller's read and this write, and resurrecting the file there would leave a ghost job behind
/// the removal.
#[cfg(unix)]
fn replace_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.agent-router.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    // Also the freshness check: metadata on the target is what proves it still exists.
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path)?.permissions());
    // Create OWNER-ONLY, before a single byte is written: chmod-after-write leaves a window in
    // which another local user can open the temp and read the whole job state.
    let write = |tmp: &Path| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = create_owner_only(tmp)?;
        file.write_all(body.as_bytes())?;
        // Widen to the target's own mode only once the content is in place.
        file.set_permissions(
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode),
        )?;
        Ok(())
    };
    if let Err(error) = write(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Claude job state is only renamed on unix; elsewhere the write is refused with the text the
/// rename has always reported there.
#[cfg(not(unix))]
fn replace_atomic(_path: &Path, _body: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "claude does not support this action",
    ))
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

    #[test]
    fn iso8601_utc_millis_formats_known_instants() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(iso8601_utc_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_utc_millis(UNIX_EPOCH + Duration::from_millis(1_785_095_531_007)),
            "2026-07-26T19:52:11.007Z"
        );
        assert_eq!(
            iso8601_utc_millis(UNIX_EPOCH + Duration::from_secs(1_709_209_845)),
            "2024-02-29T12:30:45.000Z"
        );
        assert_eq!(
            iso8601_utc_millis(UNIX_EPOCH + Duration::from_secs(1_735_689_599)),
            "2024-12-31T23:59:59.000Z"
        );
    }

    #[test]
    fn job_state_path_in_refuses_anything_but_one_path_component() {
        let root = Path::new("/jobs");
        for short_id in ["", ".", "..", "../escape", "foo/bar", "foo\\bar", "/abs"] {
            match job_state_path_in(root, short_id) {
                Err(Error::Command(message)) => assert_eq!(
                    message, "command failed: Claude job identity is not a single path component",
                    "{short_id:?}"
                ),
                other => panic!("{short_id:?} was not refused: {other:?}"),
            }
        }
        assert_eq!(
            job_state_path_in(root, "ab12").expect("a plain short id"),
            Path::new("/jobs/ab12/state.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_atomic_keeps_an_owner_only_target_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, "old").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        replace_atomic(&path, "new body").expect("replace");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new body");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn replace_atomic_refuses_a_missing_target_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");

        let error = replace_atomic(&path, "body").expect_err("a missing target is an error");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!path.exists(), "the target was created");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }
}
