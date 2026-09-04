//! `scripts/check-version-bump.sh` through its real external contract.
//!
//! Every test builds a throwaway git repository, runs the script as a subprocess with that
//! repository as the working directory, and asserts on exit status and user-facing output only.
//! Nothing here reads the script's source: the acceptance criterion is that the gate is executed
//! against a violating input, and a grep proves only that a string is present, never that the
//! check runs.
//!
//! `git` is not mocked. It is the real dependency of the thing under test, and a mocked `git`
//! would test the fixture's idea of a merge rather than a merge.
//!
//! Unix only, because the thing under test is a bash script invoked directly, which is also what
//! makes a missing executable bit fail here rather than first in CI. Same idiom as the sibling
//! `stats_cli.rs`.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

/// One of the gated paths, standing in for "routing behavior changed". The
/// tests assert the script names it back, because a failure that does not say which file tripped
/// it sends the developer looking.
const GATED: &str = "crates/agent-router-core/src/decide.rs";

/// Two paths the gate must ignore: a doc, and a core source file deliberately outside the list
/// because it reports on decisions rather than making them. A gate that fires on these is a gate
/// that gets disabled.
const UNGATED_DOC: &str = "README.md";
const UNGATED_SRC: &str = "crates/agent-router-core/src/stats.rs";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-version-gate-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The root manifest shape the gate parses: `version` under `[workspace.package]`, the one number
/// both crates inherit and every decision-log row is stamped with.
fn manifest(version: &str) -> String {
    format!(
        "[workspace]\nresolver = \"3\"\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"{version}\"\n"
    )
}

fn write_file(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

/// Strip parent-repo git identity so fixtures can `git init` their own trees.
///
/// A pre-push hook exports GIT_DIR pointing at this repository. Without this, fixture `git init`
/// tries to reconfigure the parent instead of creating a throwaway repo.
fn isolate_from_parent_git(command: &mut Command) -> &mut Command {
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
}

/// Identity is passed inline on every invocation so the fixture works on a box with no git identity
/// configured, and signing is disabled so a globally configured signing key cannot block a commit.
fn git(repo: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    isolate_from_parent_git(&mut command);
    let output = command
        .current_dir(repo)
        .args([
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "--all"]);
    git(repo, &["commit", "--message", message]);
}

fn head_sha(repo: &Path) -> String {
    let mut command = Command::new("git");
    isolate_from_parent_git(&mut command);
    let output = command
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("run git rev-parse");
    assert!(
        output.status.success(),
        "git rev-parse HEAD failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-version-bump.sh")
}

/// Runs the gate the way CI and a developer run it: the script as an executable, the repository
/// under test as the working directory, and `GITHUB_BASE_REF` removed from the *child* environment.
/// Removing it from the parent with `std::env::set_var` would race, since these tests run in
/// parallel threads inside one process.
fn run_gate(repo: &Path, base: Option<&str>) -> Output {
    let mut command = Command::new(script());
    isolate_from_parent_git(&mut command);
    command.current_dir(repo).env_remove("GITHUB_BASE_REF");
    if let Some(base) = base {
        command.arg(base);
    }
    command.output().expect("run check-version-bump.sh")
}

/// The same invocation as `run_gate`, with `GITHUB_BASE_REF` set on the *child* process instead of
/// removed: the shape CI hands the gate on every pull request. Set on the child for the same reason
/// `run_gate` removes it there, since `std::env::set_var` would race across these parallel tests.
fn run_gate_with_github_base_ref(repo: &Path, base_ref: &str) -> Output {
    let mut command = Command::new(script());
    isolate_from_parent_git(&mut command);
    command
        .current_dir(repo)
        .env("GITHUB_BASE_REF", base_ref)
        .output()
        .expect("run check-version-bump.sh")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// A repository with one commit on `main`: the root manifest at `version`, the gated file, and both
/// ungated files. The caller commits the second state itself.
fn init_repo(repo: &Path, version: &str) {
    git(repo, &["init", "-b", "main"]);
    write_file(repo, "Cargo.toml", &manifest(version));
    write_file(repo, GATED, "// base\n");
    write_file(repo, UNGATED_DOC, "base\n");
    write_file(repo, UNGATED_SRC, "// base\n");
    commit_all(repo, "base");
}

/// The shape `CLAUDE.md` prescribes and 18 of the last 20 merges on `main` took: a base commit on
/// `main`, a branch, a commit on the branch, and a `--no-ff` merge back. HEAD is a merge commit, so
/// the script must reach its `HEAD^1` tier with no argument and no `GITHUB_BASE_REF`.
fn build_merge_repo(repo: &Path, edits: &[(&str, &str)]) {
    init_repo(repo, "0.2.0");
    git(repo, &["checkout", "-b", "task/change"]);
    for (relative, contents) in edits {
        write_file(repo, relative, contents);
    }
    commit_all(repo, "change on the branch");
    git(repo, &["checkout", "main"]);
    git(repo, &["merge", "--no-ff", "--no-edit", "task/change"]);
}

#[test]
fn a_routing_change_without_a_version_bump_is_rejected() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");
    let base = head_sha(&temp.path);

    write_file(&temp.path, GATED, "// a threshold moved\n");
    commit_all(&temp.path, "change routing");

    let output = run_gate(&temp.path, Some(&base));
    let stderr = stderr_of(&output);

    assert!(
        !output.status.success(),
        "a routing change with no bump must fail: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("decide.rs"),
        "the failure must name the file that tripped the gate: {stderr}"
    );
    assert!(
        stderr.contains("Cargo.toml"),
        "the failure must name where the fix goes: {stderr}"
    );
    // Half of the pairing with `a_version_that_goes_backwards_is_rejected`. The two failures need
    // different fixes, so one message must not read as the other.
    assert!(
        !stderr.to_lowercase().contains("backwards"),
        "the no-bump failure must not read as the went-backwards failure: {stderr}"
    );
}

#[test]
fn a_routing_change_with_a_version_bump_is_accepted() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");
    let base = head_sha(&temp.path);

    write_file(&temp.path, GATED, "// a threshold moved\n");
    write_file(&temp.path, "Cargo.toml", &manifest("0.3.0"));
    commit_all(&temp.path, "change routing and bump");

    let output = run_gate(&temp.path, Some(&base));

    assert!(
        output.status.success(),
        "a bumped routing change must pass: {}",
        stderr_of(&output)
    );
}

#[test]
fn a_change_outside_the_routing_paths_needs_no_bump() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");
    let base = head_sha(&temp.path);

    write_file(&temp.path, UNGATED_DOC, "a documentation edit\n");
    write_file(&temp.path, UNGATED_SRC, "// a reporting edit\n");
    commit_all(&temp.path, "change nothing that routes");

    let output = run_gate(&temp.path, Some(&base));

    assert!(
        output.status.success(),
        "a change outside the gated paths must not demand a bump: {}",
        stderr_of(&output)
    );
}

#[test]
fn a_version_that_goes_backwards_is_rejected() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");
    let base = head_sha(&temp.path);

    write_file(&temp.path, GATED, "// a threshold moved\n");
    write_file(&temp.path, "Cargo.toml", &manifest("0.1.9"));
    commit_all(&temp.path, "change routing and lower the version");

    let output = run_gate(&temp.path, Some(&base));
    let stderr = stderr_of(&output);

    assert!(
        !output.status.success(),
        "a version that moved down is not a bump: {}",
        stdout_of(&output)
    );
    // The other half of the pairing above: this failure is fixed by raising the number past the
    // base, not by touching an unchanged one, so it must say so in its own words.
    assert!(
        stderr.to_lowercase().contains("backwards"),
        "the went-backwards failure must be distinguishable from the no-bump failure: {stderr}"
    );
}

#[test]
fn a_base_ref_that_cannot_be_resolved_fails_rather_than_passing() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");

    write_file(&temp.path, GATED, "// a threshold moved\n");
    commit_all(&temp.path, "change routing");

    // Well formed and absent, which is what a shallow checkout looks like from inside the script.
    let output = run_gate(&temp.path, Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"));
    let stderr = stderr_of(&output);

    assert!(
        !output.status.success(),
        "an unresolvable base must fail rather than skip: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("fetch-depth"),
        "the failure must name the checkout setting that causes it: {stderr}"
    );
}

#[test]
fn a_non_merge_head_with_no_base_skips_without_failing() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");

    write_file(&temp.path, GATED, "// a threshold moved\n");
    commit_all(&temp.path, "change routing");

    let output = run_gate(&temp.path, None);
    let stdout = stdout_of(&output);

    assert!(
        output.status.success(),
        "a linear HEAD with nothing to compare against is a documented skip, not a failure: {}",
        stderr_of(&output)
    );
    assert!(
        stdout.to_lowercase().contains("skip"),
        "a skip must say it skipped, so it stays a deliberate hole rather than a silent one: {stdout}"
    );
}

/// The test that proves the gate would have fired on the history that produced the incident: a
/// local `--no-ff` merge to `main`, no argument, no `GITHUB_BASE_REF`.
#[test]
fn a_merge_to_main_without_a_version_bump_is_rejected() {
    let temp = TempDir::new();
    build_merge_repo(&temp.path, &[(GATED, "// a threshold moved\n")]);

    let output = run_gate(&temp.path, None);
    let stderr = stderr_of(&output);

    assert!(
        !output.status.success(),
        "an unbumped merge to main must fail: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("decide.rs"),
        "the failure must name the file the merged branch changed: {stderr}"
    );
}

#[test]
fn a_merge_to_main_with_a_version_bump_is_accepted() {
    let temp = TempDir::new();
    build_merge_repo(
        &temp.path,
        &[
            (GATED, "// a threshold moved\n"),
            ("Cargo.toml", &manifest("0.3.0")),
        ],
    );

    let output = run_gate(&temp.path, None);

    assert!(
        output.status.success(),
        "a merge that bumped on the branch must pass: {}",
        stderr_of(&output)
    );
}

#[test]
fn a_merge_touching_only_ungated_paths_needs_no_bump() {
    let temp = TempDir::new();
    build_merge_repo(
        &temp.path,
        &[
            (UNGATED_DOC, "a documentation edit\n"),
            (UNGATED_SRC, "// a reporting edit\n"),
        ],
    );

    let output = run_gate(&temp.path, None);

    assert!(
        output.status.success(),
        "enabling the merge path must not become a bump requirement on every merge: {}",
        stderr_of(&output)
    );
}

/// A depth-1 clone whose HEAD is a merge commit: `HEAD^2` does not resolve, so an implementation
/// that folds "not a merge" and "parents unreachable" into one branch concludes there is nothing to
/// compare against and exits 0. Both the unresolvable-base test and the skip test pass under that
/// implementation, so this is the only case that catches it.
#[test]
fn a_shallow_clone_of_a_merge_fails_loudly_rather_than_skipping() {
    let temp = TempDir::new();
    let source = temp.path.join("source");
    fs::create_dir_all(&source).expect("create source directory");
    build_merge_repo(&source, &[(GATED, "// a threshold moved\n")]);

    let clone = temp.path.join("clone");
    let url = format!("file://{}", source.display());
    let destination = clone.to_str().expect("utf8 clone path");
    // `file://` rather than a plain path: git ignores `--depth` on a local-path clone.
    git(&temp.path, &["clone", "--depth", "1", &url, destination]);

    // Assert the fixture's premise rather than trusting it. If a future git version fetches the
    // parents anyway, this test must say so instead of passing while exercising nothing.
    assert_eq!(
        head_sha(&clone),
        head_sha(&source),
        "the clone's HEAD must be the merge commit"
    );
    for parent in ["HEAD^1", "HEAD^2"] {
        let mut command = Command::new("git");
        isolate_from_parent_git(&mut command);
        let resolved = command
            .current_dir(&clone)
            .args(["rev-parse", "--verify", "-q", parent])
            .output()
            .expect("run git rev-parse");
        assert!(
            !resolved.status.success(),
            "{parent} must not resolve in a depth-1 clone, or this fixture is not shallow"
        );
    }

    let output = run_gate(&clone, None);
    let stderr = stderr_of(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a shallow clone must fail loudly rather than degrade into a no-op: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("fetch-depth"),
        "the failure must name the checkout setting that causes it: {stderr}"
    );
}

/// Renaming a gated file is the false-green twin of the shallow-clone case: `git diff --name-only`
/// detects renames by default and prints only the destination, so the gated path never appears in
/// the diff and the gate reports there was nothing to check. Permanent, too, since after the move
/// the list points at a path that no longer exists.
#[test]
fn renaming_a_gated_file_away_still_requires_a_version_bump() {
    let temp = TempDir::new();
    init_repo(&temp.path, "0.2.0");

    // Rename detection is a similarity comparison, so the file needs real content. A three-line stub
    // is reported as a delete plus an add, which trips the gate through the delete and would make
    // this test pass while exercising nothing about renames.
    let body: String = (1..=200)
        .map(|line| format!("// routing rule {line}\n"))
        .collect();
    write_file(&temp.path, GATED, &body);
    commit_all(&temp.path, "fill in the routing logic");
    let base = head_sha(&temp.path);

    let moved = "crates/agent-router-core/src/routing.rs";
    git(&temp.path, &["mv", GATED, moved]);
    commit_all(&temp.path, "move the routing logic to another file");

    // Assert the fixture's premise rather than trusting it, the same way the shallow-clone test
    // does: if git reported a delete plus an add here, the gate would fire for the wrong reason.
    let mut command = Command::new("git");
    isolate_from_parent_git(&mut command);
    let diff = command
        .current_dir(&temp.path)
        .args(["diff", "--name-only", &format!("{base}...HEAD")])
        .output()
        .expect("run git diff");
    assert_eq!(
        String::from_utf8_lossy(&diff.stdout).trim(),
        moved,
        "git must report this as a rename, or the fixture is a delete plus an add"
    );

    let output = run_gate(&temp.path, Some(&base));
    let stderr = stderr_of(&output);

    assert!(
        !output.status.success(),
        "moving a gated file changes routing and must still demand a bump: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("decide.rs"),
        "the failure must name the gated path that moved: {stderr}"
    );
}

/// A merge commit whose object exceeds the pipe buffer, which is what a long merge body or an
/// embedded `mergetag` block produces. The parent parse quits at the header boundary while `git` is
/// still writing, so `git` takes SIGPIPE and `pipefail` turns the whole gate into a 141 with nothing
/// on either stream: a red build with an empty log, in the one script whose diagnostics the design
/// leans on.
#[test]
fn a_merge_with_a_large_commit_message_does_not_die_silently() {
    let temp = TempDir::new();
    let repo = temp.path.join("repo");
    fs::create_dir_all(&repo).expect("create repository directory");
    init_repo(&repo, "0.2.0");

    git(&repo, &["checkout", "-b", "task/change"]);
    write_file(&repo, GATED, "// a threshold moved\n");
    commit_all(&repo, "change on the branch");
    git(&repo, &["checkout", "main"]);

    // Committed separately from the merge so the message arrives through a file: a 200000 character
    // argv entry exceeds the per-argument limit on Linux. The message lives outside the working tree
    // so it cannot be mistaken for a fixture file.
    git(&repo, &["merge", "--no-ff", "--no-commit", "task/change"]);
    let message_path = temp.path.join("merge-message.txt");
    fs::write(&message_path, "a".repeat(200_000)).expect("write merge message");
    let message_arg = message_path.to_str().expect("utf8 message path");
    git(&repo, &["commit", "--file", message_arg]);

    // The premise is a commit object past the 64 KiB pipe buffer. Well past, so the test does not
    // sit on the boundary where the race can go either way.
    let mut command = Command::new("git");
    isolate_from_parent_git(&mut command);
    let size = command
        .current_dir(&repo)
        .args(["cat-file", "-s", "HEAD"])
        .output()
        .expect("run git cat-file");
    let size: usize = String::from_utf8_lossy(&size.stdout)
        .trim()
        .parse()
        .expect("commit object size");
    assert!(
        size > 128 * 1024,
        "the commit object must be well past the pipe buffer, or the fixture proves nothing: {size}"
    );

    let output = run_gate(&repo, None);
    let stderr = stderr_of(&output);

    // The expected outcome, not merely "not 141": the merged branch changed a gated path without a
    // bump, so a gate that survives this commit object still has to reject it.
    assert_eq!(
        output.status.code(),
        Some(1),
        "a large commit object must not abort the gate: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("decide.rs"),
        "the failure must name the file the merged branch changed: {stderr}"
    );
}

/// Base resolution tier 2, the branch every pull request takes in CI. HEAD here is an ordinary
/// commit, so the merge tier cannot rescue this test: if `GITHUB_BASE_REF` were ignored, or resolved
/// without the `origin/` prefix to the local branch that is already at HEAD, the gate would find
/// nothing to compare against and exit 0.
#[test]
fn a_pull_request_resolves_its_base_from_github_base_ref() {
    let temp = TempDir::new();
    let source = temp.path.join("source");
    fs::create_dir_all(&source).expect("create source directory");
    init_repo(&source, "0.2.0");

    // A real clone, so `origin/main` is a genuine remote-tracking ref. A local branch named
    // "origin/main" would satisfy the script while proving nothing about the CI path.
    let clone = temp.path.join("clone");
    let source_path = source.to_str().expect("utf8 source path");
    let destination = clone.to_str().expect("utf8 clone path");
    git(&temp.path, &["clone", source_path, destination]);

    write_file(&clone, GATED, "// a threshold moved\n");
    commit_all(&clone, "change routing");

    let output = run_gate_with_github_base_ref(&clone, "main");
    let stderr = stderr_of(&output);

    assert!(
        !output.status.success(),
        "a pull request that changes routing without a bump must fail: {}",
        stdout_of(&output)
    );
    assert!(
        stderr.contains("decide.rs"),
        "the failure must name the file that tripped the gate: {stderr}"
    );
}
