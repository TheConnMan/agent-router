//! The Grok `/implement` skill pin, end to end through `run()` and out of the decision log.
//!
//! The measured failure this guards against is not a crash: it is a Grok `/implement` run that
//! launches happily, resolves a project-level `.claude/skills/implement`, executes a different
//! pipeline, and writes no factory stage row. Nothing downstream can tell that run from a good
//! one, so the only place to catch it is before the launch.
//!
//! Every assertion below therefore reads the PERSISTED row. A test that inspected the in-memory
//! `Outcome` would not prove that `decisions.task` carries the two pin lines the job actually
//! received, or that `decisions.note` names the file the preflight accepted.

#![cfg(unix)]

mod common;

use agent_router_core::binary::{Environment, GROK_BIN_ENV};
use agent_router_core::config::Config;
use agent_router_core::log::{DecisionLog, Row};
use agent_router_core::run::{Request, run};
use agent_router_core::{Context, Provider};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const TASK: &str = "/implement RS-123 pin the implement skill\nBACKGROUND_RUN=1";

/// A Grok stub whose `inspect --json` answers the way the real one does: the project copy wins
/// when the launch directory ships one, and the user-scope symlink wins otherwise.
///
/// Resolution is computed from the cwd rather than hardcoded, so the two directions of this suite
/// exercise one stub and a fixture cannot accidentally assert against a constant.
fn fake_grok(root: &Path, home: &Path) -> PathBuf {
    let binary = root.join("grok");
    let user_scope = home.join(".grok/skills/implement/SKILL.md");
    common::write_stub(
        &binary,
        &format!(
            "printf '%s\\n' \"$@\" >> {log}\n\
             if [ \"$1\" = \"inspect\" ]; then\n\
            \x20 if [ -f \"$PWD/.claude/skills/implement/SKILL.md\" ]; then\n\
            \x20   resolved=\"$PWD/.claude/skills/implement/SKILL.md\"\n\
            \x20 else\n\
            \x20   resolved='{user_scope}'\n\
            \x20 fi\n\
            \x20 printf '{{\"skills\":[{{\"name\":\"implement\",\"source\":{{\"type\":\"user\",\"path\":\"%s\"}}}}]}}\\n' \"$resolved\"\n\
            \x20 exit 0\n\
             fi\n\
             exit 0\n",
            log = root.join("grok-argv.log").display(),
            user_scope = user_scope.display(),
        ),
    );
    binary
}

/// A home carrying the user-scope implement skill, plus the `~/.grok/skills` symlink Grok
/// actually resolves through on this box.
fn user_scope_home(root: &Path) -> PathBuf {
    let home = root.join("home");
    let skill_dir = home.join(".claude/skills/implement");
    fs::create_dir_all(&skill_dir).expect("the user-scope skill dir");
    fs::write(skill_dir.join("SKILL.md"), "the implement pipeline").expect("the skill");
    fs::write(skill_dir.join("factory-telemetry.py"), "# telemetry").expect("the telemetry writer");
    let grok_skills = home.join(".grok/skills");
    fs::create_dir_all(&grok_skills).expect("the grok skills dir");
    std::os::unix::fs::symlink(&skill_dir, grok_skills.join("implement")).expect("the symlink");
    home
}

fn context(root: &Path, home: &Path) -> Context {
    let binary = fake_grok(root, home);
    let environment = Environment::new(
        None,
        Some(home.to_path_buf()),
        BTreeMap::from([(GROK_BIN_ENV.to_string(), OsString::from(&binary))]),
    );
    Context::new(environment, home.to_path_buf(), Config::default())
        .with_claude_usage_cache(root.join("claude-usage.json"))
        .with_grok_usage_cache(root.join("grok-usage.json"))
        .with_codex_sessions_dir(root.join("codex-sessions"))
}

/// A dry run, because the pin and its refusal both land BEFORE dispatch: this exercises the whole
/// preflight and the whole logging path without needing a live Grok session.
fn request<'a>(task: &'a str, dir: &'a Path, provider: Provider) -> Request<'a> {
    Request {
        task,
        dir,
        provider: Some(provider),
        model: None,
        effort: None,
        name: Some("Grok Implement Pin".to_string()),
        dry_run: true,
        mcp_configs: &[],
        strict_mcp_config: false,
    }
}

fn newest_row(ctx: &Context) -> Row {
    let log = DecisionLog::open_in(&ctx.home).expect("open the decision log");
    log.recent(1)
        .expect("read the decision log")
        .into_iter()
        .next()
        .expect("one row was recorded")
}

/// A launch directory shipping its own `.claude/skills/implement` is exactly the agentos shape
/// that produced the measured silent failures. It must not dispatch, and the row must say why in
/// terms an operator can act on.
#[test]
fn a_launch_directory_with_a_project_shadow_copy_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = user_scope_home(root.path());
    let work = root.path().join("shadowed-repo");
    let shadow = work.join(".claude/skills/implement");
    fs::create_dir_all(&shadow).expect("the project shadow copy");
    fs::write(shadow.join("SKILL.md"), "a different pipeline").expect("the shadow skill");
    let ctx = context(root.path(), &home);

    let outcome = run(&request(TASK, &work, Provider::Grok), &ctx).expect("the run completes");

    let reason = outcome
        .skill_pin_blocked
        .as_deref()
        .expect("a shadowed directory must be refused");
    assert!(
        reason.contains(&shadow.join("SKILL.md").display().to_string()),
        "the refusal must name the file that won resolution: {reason}"
    );
    assert!(
        outcome.dispatch.is_none(),
        "a refused launch must dispatch nothing"
    );

    let row = newest_row(&ctx);
    assert_eq!(row.outcome, "skill-pin-blocked");
    assert_eq!(
        row.note.as_deref(),
        Some(reason),
        "the refusal reason must be readable off the row, not only off the process that made it"
    );
    assert_eq!(
        row.task, TASK,
        "a refused launch must record the task it was asked to run, unpinned"
    );
}

/// The other direction: a clean directory resolves the user-scope skill through the symlink, the
/// two pin lines lead the recorded task, and the note names the accepted file.
#[test]
fn a_clean_launch_directory_pins_the_prompt_and_records_the_resolved_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = user_scope_home(root.path());
    let work = root.path().join("clean-repo");
    fs::create_dir_all(&work).expect("the launch dir");
    let ctx = context(root.path(), &home);

    let outcome = run(&request(TASK, &work, Provider::Grok), &ctx).expect("the run completes");

    assert!(
        outcome.skill_pin_blocked.is_none(),
        "a clean directory must not be refused: {:?}",
        outcome.skill_pin_blocked
    );

    let row = newest_row(&ctx);
    let mut lines = row.task.lines();
    let skill_line = lines.next().expect("the skill line");
    let telemetry_line = lines.next().expect("the telemetry line");
    let skill = home.join(".claude/skills/implement/SKILL.md");
    let telemetry = home.join(".claude/skills/implement/factory-telemetry.py");
    assert!(
        skill_line.contains(&skill.display().to_string()),
        "line 1 must be the absolute user-scope SKILL.md path, not the symlink it resolved \
         through: {skill_line}"
    );
    assert!(
        telemetry_line.contains(&format!("python3 {}", telemetry.display())),
        "line 2 must be the absolute telemetry command: {telemetry_line}"
    );
    assert!(
        telemetry_line.contains("any project-level .claude/skills/implement is ignored"),
        "the pin must tell the run a project copy is out of scope: {telemetry_line}"
    );
    assert!(
        row.task.ends_with(TASK),
        "the original task must survive the pin verbatim: {}",
        row.task
    );
    assert_eq!(
        row.note.as_deref(),
        Some(
            format!(
                "implement skill pinned to {}",
                home.join(".grok/skills/implement/SKILL.md").display()
            )
            .as_str()
        ),
        "the note must name the path the preflight actually resolved and accepted, which is the \
         symlink Grok reported"
    );
}

/// A Grok binary that does not resolve must still produce a ROW. Returning the resolution error
/// straight out of the preflight would kill the launch before the log is even opened, and a
/// refusal with no row is the invisible failure this whole feature exists to end.
#[test]
fn an_unresolvable_grok_binary_is_a_logged_refusal_not_a_silent_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = user_scope_home(root.path());
    let work = root.path().join("clean-repo");
    fs::create_dir_all(&work).expect("the launch dir");
    let environment = Environment::new(
        None,
        Some(home.clone()),
        BTreeMap::from([(
            GROK_BIN_ENV.to_string(),
            OsString::from(root.path().join("no-such-grok")),
        )]),
    );
    let ctx = Context::new(environment, home.clone(), Config::default())
        .with_claude_usage_cache(root.path().join("claude-usage.json"))
        .with_grok_usage_cache(root.path().join("grok-usage.json"))
        .with_codex_sessions_dir(root.path().join("codex-sessions"));

    let outcome = run(&request(TASK, &work, Provider::Grok), &ctx).expect("the run completes");

    let reason = outcome
        .skill_pin_blocked
        .as_deref()
        .expect("an unresolvable binary must refuse, not error out");
    assert!(reason.contains("could not run grok"), "{reason}");
    let row = newest_row(&ctx);
    assert_eq!(row.outcome, "skill-pin-blocked");
    assert_eq!(row.note.as_deref(), Some(reason));
}

/// The gate is Grok plus `/implement`, and both halves matter. A Claude launch of the same task
/// must not run the preflight at all: proved by the stub's argv log, which only exists once the
/// binary has been invoked.
#[test]
fn a_claude_launch_of_the_same_task_never_runs_the_preflight() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = user_scope_home(root.path());
    let work = root.path().join("clean-repo");
    fs::create_dir_all(&work).expect("the launch dir");
    let ctx = context(root.path(), &home);

    let outcome = run(&request(TASK, &work, Provider::Claude), &ctx).expect("the run completes");

    assert!(outcome.skill_pin_blocked.is_none());
    let row = newest_row(&ctx);
    assert_eq!(
        row.task, TASK,
        "a claude launch must dispatch the task unchanged"
    );
    assert_eq!(row.note, None, "a claude launch writes no pin note");
    assert!(
        !root.path().join("grok-argv.log").exists(),
        "the grok binary must never be invoked for a claude launch"
    );
}

/// The other half of the gate: a Grok launch whose task is ordinary work is not an `/implement`
/// run, so it is dispatched verbatim and pays for no inspect call.
#[test]
fn a_grok_launch_that_is_not_an_implement_run_is_left_alone() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = user_scope_home(root.path());
    let work = root.path().join("clean-repo");
    fs::create_dir_all(&work).expect("the launch dir");
    let ctx = context(root.path(), &home);
    let ordinary = "audit how /implement resolves its skill";

    let outcome = run(&request(ordinary, &work, Provider::Grok), &ctx).expect("the run completes");

    assert!(outcome.skill_pin_blocked.is_none());
    let row = newest_row(&ctx);
    assert_eq!(row.task, ordinary);
    assert_eq!(row.note, None);
    assert!(
        !root.path().join("grok-argv.log").exists(),
        "a task that only mentions /implement must not trigger an inspect"
    );
}
