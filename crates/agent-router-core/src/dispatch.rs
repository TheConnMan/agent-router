//! Dispatch, entirely through agent-viewer-core's `Backend::spawn`. The router never builds a
//! backend command of its own and never writes a backend-owned store.

use crate::error::{Error, Result};
use crate::run::{Dispatch, Request};
use agent_viewer_core::backend::Backend;
use agent_viewer_core::{BackendKind, Session};
use std::path::Path;

/// Prepended to every write-capable Codex dispatch. Without it a long structured prompt sends
/// Codex into a planning spiral that spends the budget on plan files and lands no code.
pub const CODEX_PREAMBLE: &str = "Edit the files directly, right now, with your own tools. Do not \
     write a plan file as a substitute for editing, do not spawn claude, codex, or any other \
     subprocess, and make your first file edit within your first two tool calls.";

/// How long to poll `claude agents` for the new job's short id. `claude --bg` prints no id, so
/// this is the only way to get one, and missing it is not a failure: the job is running either
/// way and its name still finds it.
const CLAUDE_ID_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How far back a matching claude job may have been created and still count as ours.
const CLAUDE_ID_RECENCY_MS: i64 = 120_000;

/// IMPURE: spawn the job the decision chose.
pub fn dispatch(decision: &crate::decide::Decision, request: &Request) -> Result<Dispatch> {
    if !request.dir.is_dir() {
        return Err(Error::Command(format!(
            "{} is not a directory",
            request.dir.display()
        )));
    }
    let name = agent_viewer_core::spawn::truncated_title(request.task);
    match decision.provider {
        BackendKind::Codex => {
            let prompt = codex_prompt(request.task, request.read_only);
            // The daemon route, exactly as the viewer spawns it: no daemon lifecycle of our own,
            // and no exec fallback. NOTE: reasoning effort cannot be carried here -
            // `thread_start_request` takes only cwd and model, so the decision's `xhigh` is
            // recorded in the log but not transmitted.
            let backend = agent_viewer_core::codex::CodexBackend::new(
                agent_viewer_core::default_codex_home(),
            );
            let spawned = backend.spawn(request.dir, &prompt, decision.model.as_deref())?;
            Ok(Dispatch {
                job_id: spawned.session_id,
                job_name: name,
            })
        }
        BackendKind::Claude => {
            let mut backend = agent_viewer_core::claude::ClaudeBackend::new();
            backend.spawn(request.dir, request.task, decision.model.as_deref())?;
            let job_id = resolve_claude_short_id(&mut backend, &name, request.dir);
            Ok(Dispatch {
                job_id,
                job_name: name,
            })
        }
        BackendKind::Opencode => {
            let backend = agent_viewer_core::opencode::OpencodeBackend::new();
            let spawned = backend.spawn(request.dir, request.task, decision.model.as_deref())?;
            Ok(Dispatch {
                job_id: spawned.session_id,
                job_name: name,
            })
        }
    }
}

/// PURE: the prompt a Codex dispatch actually receives. Read-only work is the one case that
/// must NOT carry the preamble: a diagnosis or research run told to edit files immediately does
/// the wrong thing.
pub fn codex_prompt(task: &str, read_only: bool) -> String {
    if read_only {
        return task.to_string();
    }
    format!("{CODEX_PREAMBLE}\n\n{task}")
}

/// IMPURE: poll `claude agents --json --all` until a job matching name + dir + recency appears,
/// or the deadline passes. None on a miss.
fn resolve_claude_short_id(
    backend: &mut agent_viewer_core::claude::ClaudeBackend,
    name: &str,
    dir: &Path,
) -> Option<String> {
    let since = agent_viewer_core::spawn::now_ms() - CLAUDE_ID_RECENCY_MS;
    let deadline = std::time::Instant::now() + CLAUDE_ID_TIMEOUT;
    loop {
        if let Ok(sessions) = backend.list()
            && let Some(short_id) = pick_claude_job(&sessions, name, dir, since)
        {
            return Some(short_id);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// PURE: the short id of the newest claude job whose name and cwd match ours and which was
/// created no earlier than `since_ms`. The recency bound is what keeps a previous job with the
/// same first 40 characters of prompt from being reported as this dispatch.
pub fn pick_claude_job(
    sessions: &[Session],
    name: &str,
    dir: &Path,
    since_ms: i64,
) -> Option<String> {
    let dir = canonical(dir);
    sessions
        .iter()
        .filter(|session| session.backend == BackendKind::Claude)
        .filter(|session| session.title == name)
        .filter(|session| canonical(&session.cwd) == dir)
        .filter(|session| session.created_at_ms >= since_ms)
        .max_by_key(|session| session.created_at_ms)
        .and_then(|session| session.short_id.clone())
        .filter(|short_id| !short_id.is_empty())
}

/// The path with symlinks resolved when that is possible, so `/tmp` and a symlinked `/tmp`
/// compare equal; the path itself otherwise.
fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_viewer_core::{SessionOrigin, Status};

    #[test]
    fn a_write_capable_codex_dispatch_carries_the_execution_mode_preamble() {
        let prompt = codex_prompt("fix the failing test", false);
        assert!(prompt.starts_with("Edit the files directly, right now, with your own tools."));
        assert!(prompt.contains("make your first file edit within your first two tool calls."));
        assert!(prompt.ends_with("fix the failing test"));
    }

    #[test]
    fn a_read_only_codex_dispatch_is_the_task_alone() {
        assert_eq!(
            codex_prompt("diagnose why the queue stalls", true),
            "diagnose why the queue stalls"
        );
    }

    fn session(short_id: &str, title: &str, cwd: &str, created_at_ms: i64) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: format!("full-{short_id}"),
            short_id: Some(short_id.to_string()),
            origin: SessionOrigin::Background,
            title: title.to_string(),
            cwd: std::path::PathBuf::from(cwd),
            git_branch: None,
            status: Status::Working,
            created_at_ms,
            updated_at_ms: created_at_ms,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    #[test]
    fn the_newest_matching_job_wins() {
        let sessions = vec![
            session("aaa", "do a thing", "/tmp", 1_000),
            session("bbb", "do a thing", "/tmp", 2_000),
        ];
        assert_eq!(
            pick_claude_job(&sessions, "do a thing", Path::new("/tmp"), 500),
            Some("bbb".to_string())
        );
    }

    #[test]
    fn a_job_with_the_same_name_in_another_directory_is_not_ours() {
        let sessions = vec![session("aaa", "do a thing", "/home", 2_000)];
        assert_eq!(
            pick_claude_job(&sessions, "do a thing", Path::new("/tmp"), 500),
            None
        );
    }

    #[test]
    fn an_older_job_with_the_same_name_and_dir_is_not_ours() {
        // The exact confusion the recency bound exists for: two dispatches of the same task in
        // the same dir, where the first must not be reported as the second.
        let sessions = vec![session("old", "do a thing", "/tmp", 1_000)];
        assert_eq!(
            pick_claude_job(&sessions, "do a thing", Path::new("/tmp"), 5_000),
            None
        );
    }

    #[test]
    fn a_different_backend_or_a_missing_short_id_never_matches() {
        let mut foreign = session("ccc", "do a thing", "/tmp", 2_000);
        foreign.backend = BackendKind::Codex;
        assert_eq!(
            pick_claude_job(&[foreign], "do a thing", Path::new("/tmp"), 500),
            None
        );

        let mut idless = session("", "do a thing", "/tmp", 2_000);
        idless.short_id = None;
        assert_eq!(
            pick_claude_job(&[idless], "do a thing", Path::new("/tmp"), 500),
            None
        );

        let mut empty = session("", "do a thing", "/tmp", 2_000);
        empty.short_id = Some(String::new());
        assert_eq!(
            pick_claude_job(&[empty], "do a thing", Path::new("/tmp"), 500),
            None
        );
    }

    #[test]
    fn the_job_name_is_the_first_forty_characters_of_the_task() {
        // The name is viewer-core's, and the resolver has to match on exactly that.
        let task = "a task whose prompt runs well past the forty character mark and keeps going";
        let name = agent_viewer_core::spawn::truncated_title(task);
        assert_eq!(name.chars().count(), 40);
        let sessions = vec![session("aaa", &name, "/tmp", 2_000)];
        assert_eq!(
            pick_claude_job(&sessions, &name, Path::new("/tmp"), 500),
            Some("aaa".to_string())
        );
    }

    #[test]
    fn a_dispatch_into_a_missing_directory_fails_before_any_spawn() {
        let decision = crate::decide::decide_explicit(
            BackendKind::Codex,
            None,
            crate::usage::UsageSnapshot::full(),
        );
        let dir = std::path::PathBuf::from("/definitely/not/a/directory");
        let request = Request {
            task: "t",
            dir: &dir,
            provider: Some(BackendKind::Codex),
            model: None,
            read_only: false,
            dry_run: false,
        };
        let err = dispatch(&decision, &request).expect_err("must refuse");
        assert!(err.to_string().contains("is not a directory"), "{err}");
    }
}
