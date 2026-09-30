//! `agent-router run --surface t3` through the real binary.
//!
//! Every invocation is hermetic: HOME, GROK_HOME, both usage caches, and the codex sessions dir
//! point into a per-test temp directory, so the decision log and config under test are this
//! fixture's own. PATH holds only the fixture's `bin` (stub claude, codex, and grok that refuse
//! everything) ahead of the system utility directories the stubs need. The t3-thread launcher is a
//! recording stub pinned with `AGENT_ROUTER_T3_THREAD_BIN`; it writes its argv one token per line
//! and its stdin byte for byte, prints canned stdout and stderr, and exits with a code read from a
//! file, so one stub serves every case. No real `~/.t3` and no real provider is ever reached.
//!
//! Routes are fully pinned (`--provider`, `--model`, `--effort`) so no classifier runs.
#![cfg(unix)]

use agent_router_core::runtime::short_job_name;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// One copy of the stub helper, included by path from the core crate's tests, as the other CLI
// suites do.
#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

const T3_THREAD_BIN_ENV: &str = "AGENT_ROUTER_T3_THREAD_BIN";
const THREAD_ID: &str = "thr_1";
const THREAD_URL: &str = "http://127.0.0.1:3773/t/thr_1";
/// The fragment of the MCP-drop warning every assertion counts.
const MCP_WARNING: &str = "ignored on the t3 surface";

/// Makes every temp directory this file creates distinct, whatever the clock does, so two tests
/// never share one HOME and therefore one `router.db`.
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
            "agent-router-t3-cli-{}-{serial}-{label}-{unique}",
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn launched_json() -> String {
    json!({
        "threadId": THREAD_ID,
        "url": THREAD_URL,
        "projectId": "proj-1",
    })
    .to_string()
}

struct T3Fixture {
    root: TempDir,
    cwd: PathBuf,
    task: String,
    launcher: PathBuf,
}

impl T3Fixture {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        let cwd = root.path.join("working directory");
        for dir in [
            root.path.join("home"),
            root.path.join("bin"),
            root.path.join("grok-home"),
            root.path.join("codex sessions"),
            root.path.join("t3"),
            cwd.clone(),
        ] {
            fs::create_dir_all(dir).expect("create fixture directory");
        }
        // Provider CLIs that refuse everything, so a route that wrongly took the background path
        // fails loudly instead of reaching anything real.
        for provider in ["claude", "codex", "grok"] {
            common::write_stub(&root.path.join("bin").join(provider), "exit 1\n");
        }
        let launcher = root.path.join("t3/t3-thread");
        let fixture = Self {
            task: "tighten the flaky retry test\n--not-a-flag line\nBACKGROUND_RUN=1".to_string(),
            launcher,
            cwd,
            root,
        };
        fixture.launcher_answers(&launched_json(), "", 0);
        let body = format!(
            "printf '%s\\n' \"$@\" > {argv}\ncat > {stdin}\ncat {err} >&2\ncat {out}\nexit \"$(cat {code})\"\n",
            argv = shell_quote(&fixture.argv_path().to_string_lossy()),
            stdin = shell_quote(&fixture.stdin_path().to_string_lossy()),
            err = shell_quote(&fixture.file("t3.err").to_string_lossy()),
            out = shell_quote(&fixture.file("t3.out").to_string_lossy()),
            code = shell_quote(&fixture.file("t3.code").to_string_lossy()),
        );
        common::write_stub(&fixture.launcher, &body);
        fixture
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.path.join("t3").join(name)
    }

    fn argv_path(&self) -> PathBuf {
        self.file("t3.argv")
    }

    fn stdin_path(&self) -> PathBuf {
        self.file("t3.stdin")
    }

    /// What the stub prints and how it exits on its next invocation.
    fn launcher_answers(&self, stdout: &str, stderr: &str, code: i32) {
        fs::write(self.file("t3.out"), stdout).expect("write the stub stdout");
        fs::write(self.file("t3.err"), stderr).expect("write the stub stderr");
        fs::write(self.file("t3.code"), code.to_string()).expect("write the stub exit code");
    }

    fn launcher_invoked(&self) -> bool {
        self.argv_path().exists()
    }

    fn launcher_argv(&self) -> Vec<String> {
        fs::read_to_string(self.argv_path())
            .unwrap_or_else(|error| panic!("the t3-thread stub never ran: {error}"))
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn launcher_stdin(&self) -> String {
        fs::read_to_string(self.stdin_path()).expect("the stub recorded its stdin")
    }

    fn write_config(&self, text: &str) {
        let dir = self.root.path.join("home/.config/agent-router");
        fs::create_dir_all(&dir).expect("create the config directory");
        fs::write(dir.join("config.toml"), text).expect("write the config");
    }

    /// The router against this fixture's HOME, caches, and PATH, with no override from the
    /// developer's own shell leaking in and none pinning the launcher.
    fn bare_router(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        for name in [
            T3_THREAD_BIN_ENV,
            "AGENT_ROUTER_CLAUDE_BIN",
            "AGENT_ROUTER_CODEX_BIN",
            "AGENT_ROUTER_GROK_BIN",
            "CODEX_HOME",
        ] {
            command.env_remove(name);
        }
        command
            .env("HOME", self.root.path.join("home"))
            .env("GROK_HOME", self.root.path.join("grok-home"))
            .env(
                "GROK_USAGE_CACHE",
                self.root.path.join("grok-usage-cache.json"),
            )
            .env(
                "CLAUDE_USAGE_CACHE",
                self.root.path.join("claude-usage-cache.json"),
            )
            .env("CODEX_SESSIONS_DIR", self.root.path.join("codex sessions"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path.join("bin").display()),
            );
        command
    }

    /// The router with the recording launcher pinned.
    fn router(&self) -> Command {
        let mut command = self.bare_router();
        command.env(T3_THREAD_BIN_ENV, &self.launcher);
        command
    }

    fn run_args(&self, command: &mut Command, args: &[&str]) -> Output {
        command
            .arg("run")
            .arg(&self.task)
            .arg("--dir")
            .arg(&self.cwd)
            .args(args)
            .output()
            .expect("run the router")
    }

    /// `run` with the launcher pinned and the given flags.
    fn run(&self, args: &[&str]) -> Output {
        self.run_args(&mut self.router(), args)
    }

    fn newest_logged_row(&self) -> Value {
        let output = self
            .router()
            .args(["log", "--json", "--limit", "5"])
            .output()
            .expect("read the decision log");
        assert!(
            output.status.success(),
            "log --json failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Value = serde_json::from_slice(&output.stdout).expect("log json");
        rows.as_array()
            .and_then(|rows| rows.first())
            .cloned()
            .expect("the log holds a row")
    }
}

const CODEX_PINNED: [&str; 6] = [
    "--provider",
    "codex",
    "--model",
    "gpt-6-sol",
    "--effort",
    "high",
];

fn succeeded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "run failed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "run --json printed no JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn with(base: &[&'static str], extra: &[&'static str]) -> Vec<&'static str> {
    base.iter().chain(extra).copied().collect()
}

/// AC2/AC4. The full t3 contract on a pinned codex route: the exact argv in `--flag=value` form
/// with the canonical project, the task on stdin and never in argv, JSON carrying the surface and
/// URL with the thread id as `job_id` and no claimed effective effort, and a decision row that
/// records surface `t3`, the thread URL, and the thread id.
#[test]
fn run_surface_t3_launches_through_t3_thread_and_reports_the_thread() {
    let fixture = T3Fixture::new("t3-codex");
    let output = fixture.run(&with(
        &CODEX_PINNED,
        &["--surface", "t3", "--name", "RS-1 Fixture", "--json"],
    ));
    let outcome = succeeded(&output);

    let canonical = fs::canonicalize(&fixture.cwd).expect("canonicalize cwd");
    assert_eq!(
        fixture.launcher_argv(),
        vec![
            format!("--project={}", canonical.display()),
            "--engine=codex".to_string(),
            "--model=gpt-6-sol".to_string(),
            "--effort=high".to_string(),
            "--title=RS-1 Fixture".to_string(),
            "--prompt=-".to_string(),
            "--json".to_string(),
        ]
    );
    assert_eq!(fixture.launcher_stdin(), fixture.task);

    assert_eq!(outcome["surface"], "t3", "{outcome}");
    assert_eq!(outcome["dispatch"]["job_id"], THREAD_ID, "{outcome}");
    assert_eq!(outcome["dispatch"]["surface"], "t3", "{outcome}");
    assert_eq!(outcome["dispatch"]["url"], THREAD_URL, "{outcome}");
    assert_eq!(
        outcome["dispatch"]["effective_effort"],
        Value::Null,
        "{outcome}"
    );
    assert!(
        !stderr(&output).contains(MCP_WARNING),
        "no MCP flags were given, so no warning: {}",
        stderr(&output)
    );

    let row = fixture.newest_logged_row();
    assert_eq!(row["surface"], "t3", "{row}");
    assert_eq!(row["thread_url"], THREAD_URL, "{row}");
    assert_eq!(row["job_id"], THREAD_ID, "{row}");
    assert_eq!(row["outcome"], "dispatched", "{row}");
}

/// AC2. The router's claude default carries `[1m]`; T3 claude threads are always 1M, so the suffix
/// is stripped, and the bare fallback alias omits `--model` entirely so t3-thread's own default
/// applies.
#[test]
fn claude_models_are_translated_for_t3_thread() {
    let fixture = T3Fixture::new("t3-claude-model");
    let output = fixture.run(&[
        "--provider",
        "claude",
        "--model",
        "claude-opus-5-5[1m]",
        "--effort",
        "high",
        "--surface",
        "t3",
        "--name",
        "Claude Model",
        "--json",
    ]);
    succeeded(&output);
    let argv = fixture.launcher_argv();
    assert!(argv.contains(&"--engine=claude".to_string()), "{argv:?}");
    assert!(
        argv.contains(&"--model=claude-opus-5-5".to_string()),
        "{argv:?}"
    );
    assert!(argv.contains(&"--effort=high".to_string()), "{argv:?}");

    let output = fixture.run(&[
        "--provider",
        "claude",
        "--model",
        "opus[1m]",
        "--effort",
        "high",
        "--surface",
        "t3",
        "--name",
        "Claude Alias",
        "--json",
    ]);
    succeeded(&output);
    let argv = fixture.launcher_argv();
    assert!(
        !argv.iter().any(|arg| arg.starts_with("--model")),
        "the bare opus alias must defer to t3-thread's default: {argv:?}"
    );
}

/// AC5. Claude MCP scoping on t3 is accepted and dropped with exactly one stderr warning; nothing
/// MCP-shaped reaches the launcher. The config path is not preflighted on t3, so a nonexistent
/// one is accepted too.
#[test]
fn claude_mcp_flags_on_t3_warn_once_and_are_not_forwarded() {
    let fixture = T3Fixture::new("t3-mcp-drop");
    let config = fixture.root.path.join("scoped.mcp.json");
    fs::write(&config, r#"{"mcpServers":{}}"#).expect("write the MCP config");
    let missing = fixture.root.path.join("no-such.mcp.json");

    for path in [&config, &missing] {
        let path = path.to_string_lossy().to_string();
        let output = fixture.run(&[
            "--provider",
            "claude",
            "--model",
            "claude-opus-5-5[1m]",
            "--effort",
            "high",
            "--surface",
            "t3",
            "--name",
            "Scoped Claude",
            "--mcp-config",
            &path,
            "--strict-mcp-config",
            "--json",
        ]);
        let outcome = succeeded(&output);
        assert_eq!(
            stderr(&output).matches(MCP_WARNING).count(),
            1,
            "exactly one warning for {path}: {}",
            stderr(&output)
        );
        assert_eq!(outcome["surface"], "t3", "{outcome}");
        let argv = fixture.launcher_argv();
        assert!(
            !argv
                .iter()
                .any(|arg| arg.starts_with("--mcp") || arg.starts_with("--strict-mcp")),
            "an MCP flag reached t3-thread: {argv:?}"
        );
        fs::remove_file(fixture.argv_path()).expect("reset the recorded argv");
    }
}

/// AC5. A dry run reaches the same MCP check, so it reports the drop the real run would make, once,
/// without invoking the launcher.
#[test]
fn claude_mcp_flags_on_a_t3_dry_run_warn_once_and_launch_nothing() {
    let fixture = T3Fixture::new("t3-mcp-dry");
    let config = fixture.root.path.join("scoped.mcp.json");
    fs::write(&config, r#"{"mcpServers":{}}"#).expect("write the MCP config");
    let config = config.to_string_lossy().to_string();

    let output = fixture.run(&[
        "--provider",
        "claude",
        "--model",
        "claude-opus-5-5[1m]",
        "--effort",
        "high",
        "--surface",
        "t3",
        "--mcp-config",
        &config,
        "--strict-mcp-config",
        "--dry-run",
        "--json",
    ]);
    let outcome = succeeded(&output);
    assert_eq!(
        stderr(&output).matches(MCP_WARNING).count(),
        1,
        "{}",
        stderr(&output)
    );
    assert_eq!(outcome["surface"], "t3", "{outcome}");
    assert!(!fixture.launcher_invoked(), "a dry run invoked t3-thread");
}

/// AC5. Codex still refuses MCP scoping on t3 exactly as on the background surface, before any
/// launch.
#[test]
fn codex_mcp_flags_on_t3_are_still_refused() {
    let fixture = T3Fixture::new("t3-mcp-codex");
    let config = fixture.root.path.join("scoped.mcp.json");
    fs::write(&config, r#"{"mcpServers":{}}"#).expect("write the MCP config");
    let config = config.to_string_lossy().to_string();

    let output = fixture.run(
        &with(&CODEX_PINNED, &["--surface", "t3", "--mcp-config"])
            .into_iter()
            .chain([config.as_str()])
            .collect::<Vec<_>>(),
    );
    assert!(
        !output.status.success(),
        "codex accepted --mcp-config on t3: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr(&output).contains("--mcp-config"),
        "{}",
        stderr(&output)
    );
    assert!(
        !fixture.launcher_invoked(),
        "t3-thread ran before the refusal"
    );
}

/// AC6. With no `--name`, the thread launches under the derived name and the router does not try
/// to rename it; the JSON says why.
#[test]
fn an_unnamed_t3_job_keeps_its_launch_title_and_reports_the_naming_skip() {
    let fixture = T3Fixture::new("t3-naming");
    let output = fixture.run(&with(&CODEX_PINNED, &["--surface", "t3", "--json"]));
    let outcome = succeeded(&output);

    assert_eq!(outcome["naming_started"], false, "{outcome}");
    let skipped = outcome["naming_skipped"]
        .as_str()
        .unwrap_or_else(|| panic!("naming_skipped must carry a reason: {outcome}"));
    assert!(skipped.contains("T3"), "the skip reason: {skipped}");
    assert!(
        fixture
            .launcher_argv()
            .contains(&format!("--title={}", short_job_name(&fixture.task))),
        "{:?}",
        fixture.launcher_argv()
    );
}

/// AC1. `[dispatch] surface = "t3"` makes t3 the default when `--surface` is omitted, and the flag
/// still overrides it.
#[test]
fn the_configured_dispatch_surface_applies_when_the_flag_is_omitted() {
    let fixture = T3Fixture::new("t3-config");
    fixture.write_config("[dispatch]\nsurface = \"t3\"\n");

    let output = fixture.run(&with(&CODEX_PINNED, &["--name", "Configured", "--json"]));
    let outcome = succeeded(&output);
    assert_eq!(outcome["surface"], "t3", "{outcome}");
    assert!(
        fixture.launcher_invoked(),
        "the configured t3 surface did not launch"
    );

    fs::remove_file(fixture.argv_path()).expect("reset the recorded argv");
    let output = fixture.run(&with(
        &CODEX_PINNED,
        &["--surface", "background", "--dry-run", "--json"],
    ));
    let outcome = succeeded(&output);
    assert_eq!(
        outcome["surface"], "background",
        "the flag beats config: {outcome}"
    );
    assert!(!fixture.launcher_invoked());
}

/// AC1. With no `[dispatch]` table (and no flag) the surface is background, exactly as before.
#[test]
fn without_a_dispatch_table_the_surface_is_background() {
    let fixture = T3Fixture::new("t3-default");
    fixture.write_config("hard_ceiling_pct = 90.0\n");

    let output = fixture.run(&with(&CODEX_PINNED, &["--dry-run", "--json"]));
    let outcome = succeeded(&output);
    assert_eq!(outcome["surface"], "background", "{outcome}");
    assert!(
        !fixture.launcher_invoked(),
        "a background route invoked t3-thread"
    );

    // No config file at all is the same default.
    fs::remove_file(
        fixture
            .root
            .path
            .join("home/.config/agent-router/config.toml"),
    )
    .expect("remove the config");
    let output = fixture.run(&with(&CODEX_PINNED, &["--dry-run", "--json"]));
    assert_eq!(succeeded(&output)["surface"], "background");
    assert!(!fixture.launcher_invoked());
}

/// AC1. An unknown surface is a usage error that names the accepted values.
#[test]
fn an_unknown_surface_is_rejected() {
    let fixture = T3Fixture::new("t3-bogus");
    let output = fixture.run(&with(&CODEX_PINNED, &["--surface", "bogus", "--json"]));
    assert!(!output.status.success(), "--surface bogus was accepted");
    assert!(
        stderr(&output).contains("background or t3"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.launcher_invoked());
}

/// AC3/AC4. Exit 3 from t3-thread is a failed launch with a clear unverified-version message that
/// carries the launcher's stderr, and the failure is still logged on the t3 surface.
#[test]
fn an_unverified_t3_version_fails_the_run_with_a_clear_message() {
    let fixture = T3Fixture::new("t3-exit3");
    fixture.launcher_answers("", "unverified T3 version 0.0.99\n", 3);

    let output = fixture.run(&with(
        &CODEX_PINNED,
        &["--surface", "t3", "--name", "Exit Three", "--json"],
    ));
    assert!(!output.status.success(), "exit 3 was reported as a launch");
    let text = stderr(&output);
    assert!(text.contains("VERIFIED_T3_VERSIONS"), "{text}");
    assert!(text.contains("unverified T3 version 0.0.99"), "{text}");
    assert_eq!(output.status.code(), Some(1));
    let failure: Value = serde_json::from_slice(&output.stdout).expect("failure JSON");
    assert_eq!(failure["launched"], false, "{failure}");
    assert!(failure.get("job_id").is_none(), "{failure}");
    assert!(failure.get("dispatch").is_none(), "{failure}");
    assert!(
        failure["error"]
            .as_str()
            .is_some_and(|message| message.contains("unverified T3 version 0.0.99")),
        "{failure}"
    );

    let row = fixture.newest_logged_row();
    assert!(
        row["outcome"]
            .as_str()
            .is_some_and(|outcome| outcome.starts_with("error:")),
        "{row}"
    );
    assert_eq!(row["surface"], "t3", "{row}");
}

/// Usage errors fail with a machine readable guarantee that no job exists.
#[test]
fn a_t3_usage_error_reports_not_launched_json_with_its_diagnostic() {
    let fixture = T3Fixture::new("t3-exit2");
    fixture.launcher_answers("", "unknown option --bad\n", 2);
    let output = fixture.run(&with(&CODEX_PINNED, &["--surface", "t3", "--json"]));
    assert_eq!(output.status.code(), Some(1));
    let failure: Value = serde_json::from_slice(&output.stdout).expect("failure JSON");
    assert_eq!(failure["launched"], false, "{failure}");
    assert!(failure.get("job_id").is_none(), "{failure}");
    assert!(failure.get("dispatch").is_none(), "{failure}");
    assert!(
        failure["error"]
            .as_str()
            .is_some_and(|message| message.contains("unknown option --bad")),
        "{failure}"
    );
    assert!(stderr(&output).contains("unknown option --bad"));
}

/// Runtime failures and contradictory thread evidence must preserve the ambiguous contract.
#[test]
fn possible_t3_launches_never_report_not_launched_json() {
    let fixture = T3Fixture::new("t3-ambiguous");
    for (code, stdout) in [
        (1, String::new()),
        (1, format!("THREAD_ID {THREAD_ID}\n")),
        (2, format!("THREAD_ID {THREAD_ID}\n")),
        (3, format!("THREAD_ID {THREAD_ID}\n")),
        (2, launched_json()),
        (3, launched_json()),
    ] {
        fixture.launcher_answers(&stdout, "runtime failure\n", code);
        let output = fixture.run(&with(&CODEX_PINNED, &["--surface", "t3", "--json"]));
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty(), "exit {code}: {output:?}");
        assert!(stderr(&output).contains("runtime failure"));
    }
}

/// AC3. An override pinned to a missing launcher names the override; with no override and no
/// skill installed under HOME, the error names the skill path it looked at.
#[test]
fn a_missing_t3_thread_launcher_is_a_clear_launch_error() {
    let fixture = T3Fixture::new("t3-missing");
    let mut pinned_missing = fixture.bare_router();
    pinned_missing.env(
        T3_THREAD_BIN_ENV,
        fixture.root.path.join("nowhere/t3-thread"),
    );
    let output = fixture.run_args(
        &mut pinned_missing,
        &with(&CODEX_PINNED, &["--surface", "t3", "--name", "Missing"]),
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(T3_THREAD_BIN_ENV),
        "{}",
        stderr(&output)
    );

    let output = fixture.run_args(
        &mut fixture.bare_router(),
        &with(&CODEX_PINNED, &["--surface", "t3", "--name", "Missing"]),
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(".claude/skills/t3-thread/t3-thread"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.launcher_invoked());
}

/// AC4. Text mode marks the dispatch as a t3 launch and prints the thread URL on its own line.
#[test]
fn text_mode_reports_the_t3_surface_and_the_thread_url() {
    let fixture = T3Fixture::new("t3-text");
    let output = fixture.run(&with(
        &CODEX_PINNED,
        &["--surface", "t3", "--name", "Text Mode"],
    ));
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(" on t3"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("url: {THREAD_URL}")),
        "{stdout}"
    );
}
