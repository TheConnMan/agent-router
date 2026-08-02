//! Feature 4 through the real binary: `agent-router doctor`, and the fail open usage marker as it
//! surfaces on `agent-router usage` and on a logged decision.
//!
//! `claude_headroom()` reads the machine wide `/tmp/claude-usage-cache.json`, which pointing HOME
//! at a temp directory does not isolate, so nothing here fixes what Claude's usage read must
//! return. Codex is the provider every deterministic usage assertion is made against, isolated by
//! `CODEX_SESSIONS_DIR`. Where Claude is asserted at all, it is asserted against `usage --json`'s
//! own report of the same read, so the two surfaces have to agree whichever way this box reads.
//!
//! PATH holds only what the fixture put on it, so a provider binary this box happens to have
//! installed cannot decide a check.
//!
//! The whole file is unix only because the fixture stubs its provider binaries as shell scripts,
//! which is the idiom `run_json_tests.rs` and `dry_run_estimate_cli.rs` already use.
#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// One copy of the stub helper, included by path from the core crate's tests. A workspace
// `test-support` dev-dependency crate is the intended follow-up; this shape keeps the helper single
// sourced without editing Cargo.toml while another stream owns it.
#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

/// A rollout directory with nothing in it, so the codex reader has no `rate_limits` event to read
/// and fails open.
const IDLE_SESSIONS: &str = "idle codex sessions";
/// A rollout directory carrying a real `rate_limits` event, so the codex reader answers live.
const LIVE_SESSIONS: &str = "live codex sessions";

const DRY_RUN_TASK: &str = "a task decided without readable credentials";

/// Makes every temp directory this file creates distinct from every other one, whatever the clock
/// does. `fs::create_dir_all` succeeds silently on a path that already exists, so two tests deriving
/// the same path would share one HOME and therefore one `router.db`, and one fixture's `Drop` would
/// delete a live sibling's directories mid run.
///
/// Doctor prints the paths it resolved its provider binaries at, so a label reaches stdout and
/// `check_line` matches on `contains`. A label must therefore carry no token an assertion greps
/// for, `opencode` above all: `missing-opencode` made the `claude_on_path` line answer the opencode
/// lookup.
static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-doctor-cli-{}-{serial}-{label}-{unique}",
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

/// A stub that refuses every invocation. It exists to be found on PATH, and to make the codex
/// app-server probe fail fast rather than reach a real daemon on this box.
fn write_refusing_stub(bin: &Path, name: &str) {
    common::write_stub(&bin.join(name), "exit 1\n");
}

fn write_live_rollout(sessions: &Path) {
    let resets_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        + 3_600;
    let line = json!({
        "payload": {
            "rate_limits": {
                "primary": {
                    "window_minutes": 10080,
                    "used_percent": 67,
                    "resets_at": resets_at,
                },
                "secondary": null,
            }
        }
    })
    .to_string();
    fs::write(sessions.join("rollout.jsonl"), format!("{line}\n"))
        .expect("write the codex rollout");
}

struct DoctorFixture {
    root: TempDir,
}

impl DoctorFixture {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        for child in [
            "home",
            "bin",
            "extra bin",
            "working directory",
            IDLE_SESSIONS,
            LIVE_SESSIONS,
        ] {
            fs::create_dir_all(root.path.join(child)).expect("create fixture directory");
        }
        write_refusing_stub(&root.path.join("bin"), "claude");
        write_refusing_stub(&root.path.join("bin"), "codex");
        write_refusing_stub(&root.path.join("extra bin"), "opencode");
        write_live_rollout(&root.path.join(LIVE_SESSIONS));
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path.join("home")
    }

    /// The credential file the router reads its OAuth token from, in the shape the reader pointers
    /// into. Without this the Claude usage read has nothing to authenticate with.
    fn write_credentials(&self) {
        let directory = self.home().join(".claude");
        fs::create_dir_all(&directory).expect("create the claude directory");
        fs::write(
            directory.join(".credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "a fixture token"}}).to_string(),
        )
        .expect("write the credentials");
    }

    /// The router binary against this fixture's home, rollout directory, and PATH. `with_opencode`
    /// is the only difference between the two runs the opencode test compares.
    fn router(&self, sessions: &str, with_opencode: bool) -> Command {
        let mut path = self.root.path.join("bin").display().to_string();
        if with_opencode {
            path = format!("{path}:{}", self.root.path.join("extra bin").display());
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .env("HOME", self.home())
            .env("CODEX_SESSIONS_DIR", self.root.path.join(sessions))
            .env("PATH", path);
        command
    }

    fn doctor(&self, sessions: &str, with_opencode: bool) -> Output {
        self.router(sessions, with_opencode)
            .arg("doctor")
            .output()
            .expect("run the router")
    }

    fn doctor_stdout(&self, sessions: &str, with_opencode: bool) -> String {
        String::from_utf8(self.doctor(sessions, with_opencode).stdout).expect("utf8 stdout")
    }

    fn usage_stdout(&self, sessions: &str) -> String {
        let output = self
            .router(sessions, false)
            .arg("usage")
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "usage failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 stdout")
    }

    /// What this box's Claude usage read actually returned, as the router itself reports it. The
    /// shared cache is not isolated by HOME, so this is the only honest oracle for Claude here.
    fn claude_is_stale(&self, sessions: &str) -> bool {
        let output = self
            .router(sessions, false)
            .args(["usage", "--json"])
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "usage --json failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("usage json");
        value["claude"]["stale"]
            .as_bool()
            .unwrap_or_else(|| panic!("the snapshot must carry the marker: {value}"))
    }
}

/// The line a named check is reported on, so an assertion about one check cannot be satisfied by
/// another check's line.
fn check_line(stdout: &str, name: &str) -> String {
    stdout
        .lines()
        .find(|line| line.contains(name))
        .unwrap_or_else(|| panic!("doctor must report a {name} check, got:\n{stdout}"))
        .to_string()
}

/// The line one provider is reported on in `usage` output, which is never the header.
fn provider_line(stdout: &str, provider: &str) -> String {
    stdout
        .lines()
        .find(|line| line.starts_with(provider))
        .unwrap_or_else(|| panic!("usage must report {provider}, got:\n{stdout}"))
        .to_string()
}

/// The acceptance criterion. Unreadable credentials make Claude read as completely unused, which
/// makes it win every headroom tiebreak, so this is a failure and doctor has to name the file.
#[test]
fn doctor_exits_nonzero_and_names_credentials_when_they_are_missing() {
    let fixture = DoctorFixture::new("credentials-missing");

    let output = fixture.doctor(LIVE_SESSIONS, true);
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    assert_eq!(
        output.status.code(),
        Some(1),
        "missing credentials leave the router routing on input it cannot trust, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(".claude/.credentials.json"),
        "doctor must name the file it could not read:\n{stdout}"
    );
}

/// A fail open read and a genuinely idle provider report the same two zeroes, so doctor has to say
/// which one it got. `fail-open` and `live` are the words the `usage` command prints for the same
/// distinction, so both surfaces use the same two.
#[test]
fn doctor_reports_a_fail_open_usage_read_as_such_rather_than_as_idle() {
    let fixture = DoctorFixture::new("read-provenance");
    fixture.write_credentials();

    let fail_open = fixture.doctor_stdout(IDLE_SESSIONS, false);
    let codex_fail_open = check_line(&fail_open, "codex_rate_limits");
    assert!(
        codex_fail_open.contains("fail-open"),
        "no rollout carried a rate_limits event, so this read is fail open: {codex_fail_open}"
    );
    assert!(
        !codex_fail_open.contains("live"),
        "a fail open read reported as live is the misread the marker exists to kill: \
         {codex_fail_open}"
    );

    let live = fixture.doctor_stdout(LIVE_SESSIONS, false);
    let codex_live = check_line(&live, "codex_rate_limits");
    assert!(
        codex_live.contains("live") && !codex_live.contains("fail-open"),
        "a rollout with a rate_limits event is a live read: {codex_live}"
    );
    assert_ne!(
        codex_fail_open, codex_live,
        "the two reads are reported identically, so the provenance never reaches the output"
    );

    // Claude's cache is machine wide, so which read this box gives is not fixed here. What is
    // fixed is that doctor reports the same provenance the snapshot itself carries.
    let claude_line = check_line(&live, "claude_usage");
    if fixture.claude_is_stale(LIVE_SESSIONS) {
        assert!(
            claude_line.contains("fail-open") && !claude_line.contains("live"),
            "the snapshot reports a fail open claude read, doctor does not: {claude_line}"
        );
    } else {
        assert!(
            claude_line.contains("live") && !claude_line.contains("fail-open"),
            "the snapshot reports a live claude read, doctor does not: {claude_line}"
        );
    }
}

/// Opencode is a provider the router can route to, not one it needs, so its absence is reported
/// and then costs nothing. Installing it or not must not move doctor's exit code.
#[test]
fn a_missing_opencode_is_not_a_failure() {
    let fixture = DoctorFixture::new("absent-extra-provider");
    fixture.write_credentials();

    let without = fixture.doctor(LIVE_SESSIONS, false);
    let with = fixture.doctor(LIVE_SESSIONS, true);
    let stdout = String::from_utf8(without.stdout.clone()).expect("utf8 stdout");

    let line = check_line(&stdout, "opencode");
    assert!(
        line.to_ascii_lowercase().contains("warn"),
        "a missing opencode is reported, as a warning: {line}"
    );
    assert_eq!(
        without.status.code(),
        with.status.code(),
        "installing opencode moved doctor's exit code, so its absence is a failure after all, \
         stdout without it:\n{stdout}"
    );

    // Every other check passes or warns in this fixture, so the exit code is zero, unless this
    // box's shared claude cache happens to be unreadable and fails the claude usage check.
    if !fixture.claude_is_stale(LIVE_SESSIONS) {
        assert_eq!(
            without.status.code(),
            Some(0),
            "a run whose only non passing check is a warning must exit zero, stdout:\n{stdout}"
        );
    }
}

/// The second half of the acceptance criterion, read through the CLI rather than the struct: the
/// row a dispatch leaves behind records that it was decided on a read nobody could trust.
#[test]
fn a_decision_logged_with_unreadable_credentials_carries_the_stale_marker() {
    let fixture = DoctorFixture::new("stale-marker");

    let dry_run = fixture
        .router(IDLE_SESSIONS, false)
        .args(["run", DRY_RUN_TASK, "--dir"])
        .arg(fixture.root.path.join("working directory"))
        .args(["--provider", "codex", "--dry-run"])
        .output()
        .expect("run the router");
    assert!(
        dry_run.status.success(),
        "the dry run failed, stderr: {}",
        String::from_utf8_lossy(&dry_run.stderr)
    );

    let logged = fixture
        .router(IDLE_SESSIONS, false)
        .args(["log", "--json", "--limit", "1"])
        .output()
        .expect("run the router");
    assert!(
        logged.status.success(),
        "log --json failed, stderr: {}",
        String::from_utf8_lossy(&logged.stderr)
    );
    let rows: Value = serde_json::from_slice(&logged.stdout).expect("log json");
    let row = &rows[0];
    assert_eq!(row["task"], DRY_RUN_TASK, "the row under test: {row}");

    // Codex is the deterministic half: the rollout directory is empty, so its read fails open.
    assert_eq!(
        row["codex_usage_stale"],
        json!(true),
        "a row decided on a fail open codex read must say so: {row}"
    );
    // Claude's cache is machine wide, so the row is checked against the reader's own answer.
    assert_eq!(
        row["claude_usage_stale"],
        json!(fixture.claude_is_stale(IDLE_SESSIONS)),
        "the logged claude marker disagrees with the snapshot the router read: {row}"
    );
}

/// `Config::load()` writes a default file when one is absent, so a `config_parses` check built on
/// it would create the very state it was asked to report on.
#[test]
fn doctor_does_not_create_a_config_file_it_was_asked_to_check() {
    let fixture = DoctorFixture::new("config-untouched");
    let config = fixture.home().join(".config/agent-router/config.toml");
    assert!(
        !config.exists(),
        "the fixture home starts without a config file"
    );

    let stdout = fixture.doctor_stdout(LIVE_SESSIONS, true);

    check_line(&stdout, "config_parses");
    assert!(
        !config.exists(),
        "a diagnostic command created the state it was diagnosing: {}",
        config.display()
    );
}

/// A fail open claude printing `0.0%  0.0%` with nothing else on the line is exactly the
/// indistinguishable case the marker exists to kill, so the row says where its numbers came from.
#[test]
fn the_usage_command_names_its_source_as_live_or_fail_open() {
    let fixture = DoctorFixture::new("usage-provenance");

    let idle = fixture.usage_stdout(IDLE_SESSIONS);
    let header = idle.lines().next().expect("usage prints a header");
    assert!(
        header.contains("source"),
        "the source column needs a heading: {header}"
    );
    let codex_idle = provider_line(&idle, "codex");
    assert!(
        codex_idle.contains("fail-open") && !codex_idle.contains("live"),
        "no rollout carried a rate_limits event, so this read is fail open: {codex_idle}"
    );

    let live = fixture.usage_stdout(LIVE_SESSIONS);
    let codex_live = provider_line(&live, "codex");
    assert!(
        codex_live.contains("live") && !codex_live.contains("fail-open"),
        "a rollout with a rate_limits event is a live read: {codex_live}"
    );

    // Claude's read is not fixed here, but whichever it is, the line has to say which.
    let claude_line = provider_line(&live, "claude");
    let expected = if fixture.claude_is_stale(LIVE_SESSIONS) {
        "fail-open"
    } else {
        "live"
    };
    assert!(
        claude_line.contains(expected),
        "the snapshot reports a {expected} claude read, the usage line does not: {claude_line}"
    );
}
