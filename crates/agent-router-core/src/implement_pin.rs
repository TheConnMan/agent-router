//! Pin the `/implement` skill at Grok dispatch instead of trusting Grok's own resolution order.
//!
//! Measured 2026-09-13: 13 of 23 launched Grok `/implement` runs wrote no factory stage row. Repos
//! that ship their own `.claude/skills/implement` (agentos does, and its AGENTS.md names it) can
//! win resolution over the user-scope skill, so the run executes a different pipeline that never
//! calls the telemetry writer. Config-level fixes (the ignore list, the user symlink) did not
//! settle it, because nothing in the dispatch path ever CHECKED which copy won.
//!
//! This module closes that: before a Grok `/implement` launch, ask Grok itself which file the
//! `implement` skill resolves to, refuse the launch when it is not the user-scope one, and
//! otherwise prepend the absolute SKILL.md path and the absolute telemetry command to the task so
//! the run cannot drift onto a project copy mid-session.
//!
//! Claude and Codex launches are untouched: both already resolve the user-scope skill, and neither
//! showed the missing-stage-row symptom.

use crate::binary::Environment;
use crate::provider::Provider;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The user-scope implement skill, relative to `$HOME`. The `~/.grok/skills` and
/// `~/.agents/skills` entries are symlinks onto this tree, so a resolution through either one
/// canonicalizes here and passes.
const SKILL_SUFFIX: &str = ".claude/skills/implement/SKILL.md";

/// The telemetry writer beside it. This is the command whose absence is the whole measured
/// symptom, so the pin names it by absolute path rather than leaving the run to find it.
const TELEMETRY_SUFFIX: &str = ".claude/skills/implement/factory-telemetry.py";

/// The interpreter the telemetry command is invoked through, named so the prepended line is a
/// command a run can paste rather than a path it has to decide how to execute.
const TELEMETRY_INTERPRETER: &str = "python3";

/// A launch that cleared the preflight: the file Grok resolved, and the task text to dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The resolved skill path, as `grok inspect` reported it. Recorded in `decisions.note` so a
    /// later audit can tell a pinned run from an unpinned one without re-running the preflight.
    pub skill: PathBuf,
    /// The task with the two pin lines prepended.
    pub task: String,
}

/// PURE: does this task text open an `/implement` run?
///
/// Matches only at the start, and only on the whole word: a task that merely MENTIONS
/// `/implement` further down is ordinary work, and pinning it would prepend instructions to a
/// prompt that never invokes the skill.
pub fn is_implement_task(task: &str) -> bool {
    let Some(rest) = task.trim_start().strip_prefix("/implement") else {
        return false;
    };
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

/// PURE: the user-scope implement SKILL.md for a given home.
pub fn expected_skill(home: &Path) -> PathBuf {
    home.join(SKILL_SUFFIX)
}

/// PURE: the telemetry writer for a given home.
pub fn expected_telemetry(home: &Path) -> PathBuf {
    home.join(TELEMETRY_SUFFIX)
}

/// PURE: the `grok inspect --json` invocation, run from the launch directory so it reports the
/// resolution that directory would actually get. Grok has no flag to scope the inspect elsewhere,
/// and that is the point: the answer must come from the same cwd the job will run in.
pub fn inspect_command(binary: &Path, dir: &Path) -> Command {
    let mut command = Command::new(binary);
    command.current_dir(dir).arg("inspect").arg("--json");
    command
}

/// PURE: the implement skill's source path out of a `grok inspect --json` payload.
///
/// Returns the refusal sentence rather than an `Error`, because every failure here is the same
/// event to the caller: the launch cannot be pinned, and the reason belongs in the log row.
pub fn resolved_skill(inspect_json: &str) -> std::result::Result<PathBuf, String> {
    let payload: serde_json::Value = serde_json::from_str(inspect_json)
        .map_err(|error| format!("grok inspect --json did not parse: {error}"))?;
    let skills = payload
        .get("skills")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "grok inspect --json carried no skills array".to_string())?;
    let entry = skills
        .iter()
        .find(|skill| skill.get("name").and_then(serde_json::Value::as_str) == Some("implement"))
        .ok_or_else(|| {
            "grok inspect --json resolved no implement skill in this directory".to_string()
        })?;
    entry
        .get("source")
        .and_then(|source| source.get("path"))
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "the resolved implement skill carried no source path".to_string())
}

/// IMPURE (reads the filesystem): accept a resolved path only when it is a file that IS the
/// user-scope skill.
///
/// Both sides are canonicalized, so the `~/.grok/skills` and `~/.agents/skills` symlinks both pass
/// and a project copy under `<repo>/.claude/skills/implement` does not.
///
/// A side that cannot be canonicalized is a refusal, deliberately NOT a fall back to comparing the
/// two strings. String equality would accept a launch where neither file exists — the pin would
/// then name a path the run cannot read, which is the same silent-wrong-pipeline failure this
/// whole module exists to stop, dressed as a pass.
pub fn accept(resolved: &Path, expected: &Path) -> std::result::Result<(), String> {
    let real_expected = expected.canonicalize().map_err(|error| {
        format!(
            "the user-scope implement skill at {} could not be read: {error}",
            expected.display()
        )
    })?;
    let real_resolved = resolved.canonicalize().map_err(|error| {
        format!(
            "the implement skill resolves to {} in this directory, which could not be read: \
             {error}",
            resolved.display()
        )
    })?;
    if real_resolved == real_expected {
        return Ok(());
    }
    Err(format!(
        "the implement skill resolves to {} in this directory, not the user-scope {}: a \
         project-level .claude/skills/implement would run a different pipeline and write no \
         factory stage rows",
        resolved.display(),
        expected.display()
    ))
}

/// PURE: the task text a pinned Grok `/implement` run receives.
///
/// Two lines, then the original task verbatim. The run reads the named file, runs the named
/// command, and is told in the same breath that a project copy is out of scope for this run, so
/// the instruction survives even if Grok's own resolver later offers the project one.
pub fn pinned_task(task: &str, skill: &Path, telemetry: &Path) -> String {
    format!(
        "Read the implement skill from this exact absolute path and no other: {}\n\
         Run every implement telemetry call through this exact absolute command: {} {} (any \
         project-level .claude/skills/implement is ignored for this run)\n\
         {task}",
        skill.display(),
        TELEMETRY_INTERPRETER,
        telemetry.display(),
    )
}

/// IMPURE: run the preflight for one Grok `/implement` launch.
///
/// `Ok(pin)` is a launch that may proceed; `Err(reason)` is a refusal with the sentence to log.
///
/// A Grok binary that does not resolve is one of those refusals, not a propagated error. Returning
/// early here would skip the caller's logging entirely, and a launch the router killed with no row
/// at all is exactly the invisible failure this module was written to end. The resolution error is
/// carried into the reason verbatim, so the diagnosis is the same one dispatch would have given.
pub fn preflight(
    environment: &Environment,
    home: &Path,
    dir: &Path,
    task: &str,
) -> std::result::Result<Pin, String> {
    let binary = crate::binary::resolve(Provider::Grok, environment)
        .map_err(|error| format!("the implement skill pin could not run grok: {error}"))?;
    preflight_with(&binary, home, dir, task, |command| command.output())
}

/// IMPURE through `run`: the preflight body, with the subprocess injected so a test can drive
/// every branch without a Grok install.
pub fn preflight_with<F>(
    binary: &Path,
    home: &Path,
    dir: &Path,
    task: &str,
    run: F,
) -> std::result::Result<Pin, String>
where
    F: FnOnce(&mut Command) -> std::io::Result<std::process::Output>,
{
    let mut command = inspect_command(binary, dir);
    let output = run(&mut command).map_err(|error| {
        format!(
            "grok inspect --json could not run in {}: {error}",
            dir.display()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "grok inspect --json failed in {}: {}",
            dir.display(),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let resolved = resolved_skill(&stdout)?;
    let expected = expected_skill(home);
    accept(&resolved, &expected)?;
    // The PROMPT names the user-scope path, not the `~/.grok/skills` or `~/.agents/skills` symlink
    // the resolution happened to travel through: both lines then sit under the same directory, and
    // the instruction does not depend on which symlink Grok picked this time. The NOTE keeps the
    // resolved path, because that is the fact the preflight actually established.
    Ok(Pin {
        task: pinned_task(task, &expected, &expected_telemetry(home)),
        skill: resolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect_payload(path: &str) -> String {
        serde_json::json!({
            "skills": [
                {"name": "update-architecture-atlas", "source": {"type": "project", "path": "/repo/.claude/skills/update-architecture-atlas/SKILL.md"}},
                {"name": "implement", "source": {"type": "user", "path": path}},
            ]
        })
        .to_string()
    }

    /// The trigger is the opening command, not the word appearing anywhere. A task that discusses
    /// `/implement` is ordinary work and must dispatch unchanged.
    #[test]
    fn only_a_task_that_opens_with_the_implement_command_is_pinned() {
        assert!(is_implement_task("/implement RS-123"));
        assert!(is_implement_task("  /implement RS-123\nBACKGROUND_RUN=1"));
        assert!(is_implement_task("/implement"));
        assert!(!is_implement_task("audit how /implement picks a skill"));
        assert!(!is_implement_task("/implementation-notes RS-123"));
        assert!(!is_implement_task(""));
    }

    #[test]
    fn the_implement_entry_is_read_out_of_the_inspect_payload() {
        let resolved = resolved_skill(&inspect_payload("/home/me/.grok/skills/implement/SKILL.md"))
            .expect("the implement entry");
        assert_eq!(
            resolved,
            PathBuf::from("/home/me/.grok/skills/implement/SKILL.md")
        );
    }

    /// A payload with no implement entry is the same event as one resolving the wrong copy: the
    /// launch cannot be pinned. It must not fall through as "nothing to check".
    #[test]
    fn a_payload_with_no_implement_entry_is_a_refusal_not_a_pass() {
        let empty = serde_json::json!({"skills": []}).to_string();
        let reason = resolved_skill(&empty).expect_err("no implement skill resolved");
        assert!(reason.contains("resolved no implement skill"), "{reason}");
        let unparseable = resolved_skill("not json").expect_err("a malformed payload");
        assert!(unparseable.contains("did not parse"), "{unparseable}");
    }

    /// The whole point of the pin: a project shadow copy is refused even though Grok reported it
    /// as the resolution, and the sentence names both paths so an operator can delete the right
    /// file.
    #[test]
    fn a_project_shadow_copy_is_refused_and_named() {
        // Both files really exist, so this proves the REFUSAL comes from the paths differing and
        // not from either one being unreadable.
        let root = tempfile::tempdir().expect("a root");
        let home = root.path().join("home");
        let expected = expected_skill(&home);
        let shadow = root.path().join("repo/.claude/skills/implement/SKILL.md");
        for path in [&expected, &shadow] {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the skill dir");
            std::fs::write(path, "pipeline").expect("the skill");
        }

        let reason = accept(&shadow, &expected).expect_err("a project copy must not launch");

        assert!(
            reason.contains(&shadow.display().to_string())
                && reason.contains(&expected.display().to_string()),
            "the refusal must name the resolved and the expected path: {reason}"
        );
    }

    /// The `~/.grok/skills` and `~/.agents/skills` entries are symlinks onto the user-scope tree.
    /// A resolution through either is the RIGHT file, so comparing the literal strings would
    /// refuse every correct launch on this box.
    #[test]
    fn a_resolution_through_a_skills_symlink_is_accepted() {
        let home = tempfile::tempdir().expect("a home");
        let real = home.path().join(".claude/skills/implement");
        std::fs::create_dir_all(&real).expect("the user-scope skill dir");
        std::fs::write(real.join("SKILL.md"), "pipeline").expect("the skill");
        let grok_skills = home.path().join(".grok/skills");
        std::fs::create_dir_all(&grok_skills).expect("the grok skills dir");
        std::os::unix::fs::symlink(&real, grok_skills.join("implement")).expect("the symlink");

        accept(
            &grok_skills.join("implement/SKILL.md"),
            &expected_skill(home.path()),
        )
        .expect("a symlinked resolution is the user-scope skill");
    }

    /// The two lines carry absolute paths and precede the original task byte for byte: a run that
    /// only reads the first lines still gets the pin, and a dispatcher that diffs the task can see
    /// its own text survived.
    #[test]
    fn the_pin_prepends_both_absolute_lines_and_keeps_the_task_verbatim() {
        let pinned = pinned_task(
            "/implement RS-123\nBACKGROUND_RUN=1",
            Path::new("/home/me/.claude/skills/implement/SKILL.md"),
            Path::new("/home/me/.claude/skills/implement/factory-telemetry.py"),
        );
        let mut lines = pinned.lines();
        let skill_line = lines.next().expect("the skill line");
        let telemetry_line = lines.next().expect("the telemetry line");
        assert!(
            skill_line.contains("/home/me/.claude/skills/implement/SKILL.md"),
            "{skill_line}"
        );
        assert!(
            telemetry_line
                .contains("python3 /home/me/.claude/skills/implement/factory-telemetry.py"),
            "{telemetry_line}"
        );
        assert!(
            telemetry_line.contains("any project-level .claude/skills/implement is ignored"),
            "the run must be told the project copy is out of scope: {telemetry_line}"
        );
        assert!(
            pinned.ends_with("/implement RS-123\nBACKGROUND_RUN=1"),
            "the original task must survive verbatim: {pinned}"
        );
    }

    /// End to end over a stubbed inspect: a clean directory pins, and the resolved path comes back
    /// for the log row.
    #[test]
    fn a_clean_directory_pins_and_reports_the_resolved_path() {
        let home = tempfile::tempdir().expect("a home");
        let skill = expected_skill(home.path());
        std::fs::create_dir_all(skill.parent().expect("the skill dir")).expect("the skill dir");
        std::fs::write(&skill, "pipeline").expect("the skill");
        let payload = inspect_payload(&skill.to_string_lossy());

        let pin = preflight_with(
            Path::new("grok"),
            home.path(),
            Path::new("/repo"),
            "/implement RS-123",
            |_| Ok(stub_output(0, &payload, "")),
        )
        .expect("a clean directory passes the preflight");

        assert_eq!(pin.skill, skill);
        assert!(pin.task.ends_with("/implement RS-123"), "{}", pin.task);
    }

    /// String equality is not a substitute for reading the file. If both sides name the same
    /// absent path, accepting would pin the run to a SKILL.md nothing can read, which fails
    /// exactly like the project-shadow case it is supposed to prevent.
    #[test]
    fn two_identical_paths_that_do_not_exist_are_refused_rather_than_compared_as_strings() {
        let missing = Path::new("/no-such-root/.claude/skills/implement/SKILL.md");
        let reason = accept(missing, missing).expect_err("an unreadable skill must not launch");
        assert!(
            reason.contains("could not be read") && reason.contains(&missing.display().to_string()),
            "the refusal must name the unreadable path and the filesystem cause: {reason}"
        );
    }

    /// An inspect that exits non-zero is a refusal carrying its stderr, not a silent pass: a
    /// launch the router could not verify must not proceed as if it had.
    #[test]
    fn a_failing_inspect_refuses_the_launch_with_its_stderr() {
        let home = tempfile::tempdir().expect("a home");
        let reason = preflight_with(
            Path::new("grok"),
            home.path(),
            Path::new("/repo"),
            "/implement RS-123",
            |_| Ok(stub_output(1, "", "project is not trusted")),
        )
        .expect_err("an inspect failure must refuse");
        assert!(reason.contains("project is not trusted"), "{reason}");
    }

    fn stub_output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }
}
