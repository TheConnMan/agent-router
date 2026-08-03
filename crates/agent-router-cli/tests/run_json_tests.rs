use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

// One copy of the stub helper, included by path from the core crate's tests. A workspace
// `test-support` dev-dependency crate is the intended follow-up; this shape keeps the helper single
// sourced without editing Cargo.toml while another stream owns it.
#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

/// Makes every temp directory this file creates distinct from every other one, whatever the clock
/// does. `fs::create_dir_all` succeeds silently on a path that already exists, so two tests deriving
/// the same path would share one HOME and therefore one `router.db`, and one fixture's `Drop` would
/// delete a live sibling's directories mid run.
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
            "agent-router-cli-{}-{serial}-{label}-{unique}",
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

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
struct CliFixture {
    root: TempDir,
    task: String,
    name: String,
    cwd: PathBuf,
    spawn_log: PathBuf,
}

#[cfg(unix)]
impl CliFixture {
    fn new(label: &str) -> Self {
        Self::listing_agent_named_with_context_horizon(label, None, "extended")
    }

    fn with_context_horizon(label: &str, task_context_horizon: &str) -> Self {
        Self::listing_agent_named_with_context_horizon(label, None, task_context_horizon)
    }

    /// `listed` is the name the fake `claude agents` listing advertises, which is what the router
    /// matches against to resolve the short id of the job it just spawned. None means the name
    /// derived from the task.
    fn listing_agent_named(label: &str, listed: Option<&str>) -> Self {
        Self::listing_agent_named_with_context_horizon(label, listed, "extended")
    }

    fn listing_agent_named_with_context_horizon(
        label: &str,
        listed: Option<&str>,
        task_context_horizon: &str,
    ) -> Self {
        let root = TempDir::new(label);
        let home = root.path.join("home");
        let bin = root.path.join("bin");
        let cwd = root.path.join("working directory");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(&bin).expect("create bin");
        fs::create_dir_all(&cwd).expect("create cwd");
        let task = "雪".repeat(45);
        let name = task.chars().take(40).collect::<String>();
        let spawn_log = root.path.join("claude.argv");
        let listed = listed.unwrap_or(&name);
        let classifier_answer = json!({
            "orchestration": false,
            "missing_connector": false,
            "complexity": "medium",
            "task_context_horizon": task_context_horizon,
            "rationale": "fixture context",
        })
        .to_string();
        let classifier_result = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": classifier_answer,
        })
        .to_string();
        let agents = json!([{
            "id": "claude exact id",
            "sessionId": "claude full id",
            "cwd": cwd,
            "name": listed,
            "startedAt": i64::MAX,
            "kind": "background",
            "state": "working"
        }]);
        let agents = serde_json::to_string(&agents).expect("agents json");
        // No interpreter line: the helper supplies exactly one, ahead of the probe guard, which is
        // what keeps a probe out of the spawn log this fixture's assertions read.
        let body = format!(
            "if [ \"$1\" = \"agents\" ]; then\n\
               printf '%s\\n' {}\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"-p\" ]; then\n\
               printf '%s\\n' {}\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&agents),
            shell_quote(&classifier_result),
            shell_quote(&spawn_log.to_string_lossy())
        );
        let fake_claude = bin.join("claude");
        common::write_stub(&fake_claude, &body);
        Self {
            root,
            task,
            name,
            cwd,
            spawn_log,
        }
    }

    /// The router binary against this fixture's fake PATH, home, and decision log.
    fn router(&self) -> Command {
        let bin = self.root.path.join("bin");
        let home = self.root.path.join("home");
        let sessions = self.root.path.join("empty codex sessions");
        fs::create_dir_all(&sessions).expect("create sessions");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .env("HOME", home)
            .env("CODEX_SESSIONS_DIR", sessions)
            .env("PATH", path);
        command
    }

    fn run_command(&self) -> Command {
        let mut command = self.router();
        command
            .arg("run")
            .arg(&self.task)
            .arg("--dir")
            .arg(&self.cwd);
        command
    }

    fn run(&self, json: bool) -> Output {
        let mut command = self.run_command();
        command.arg("--provider").arg("claude");
        if json {
            command.arg("--json");
        }
        command.output().expect("run router")
    }
}

#[cfg(unix)]
fn wait_for_text(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn run_json_preserves_provider_decision_and_dispatched_job_identity() {
    let fixture = CliFixture::new("json-identity");
    let output = fixture.run(true);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    assert_eq!(value["provider"], "claude");
    assert_eq!(value["model"], "opus[1m]");
    // The router forces no effort: the backend resolves its own.
    assert_eq!(value["effort"], Value::Null);
    assert_eq!(value["gates"], json!(["explicit_provider"]));
    assert_eq!(value["classification"], Value::Null);
    assert!(value["usage"]["claude"].is_object());
    assert!(value["usage"]["codex"].is_object());
    assert!(
        value["rationale"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(value["dispatch"]["job_id"], "claude exact id");
    assert_eq!(value["dispatch"]["job_name"], fixture.name);
    assert_eq!(value["dry_run"], false);
    assert!(value["log_id"].is_number());
    assert_eq!(value["log_error"], Value::Null);
    assert!(
        value.get("watch").is_none(),
        "the standalone router must not require a viewer"
    );

    assert_eq!(
        wait_for_text(&fixture.spawn_log)
            .lines()
            .collect::<Vec<_>>(),
        vec![
            "--bg",
            "--model",
            "opus[1m]",
            "--name",
            &fixture.name,
            &fixture.task
        ]
    );
}

#[cfg(unix)]
#[test]
fn auto_runs_log_context_horizon_without_changing_provider_or_model() {
    let ordinary_fixture = CliFixture::with_context_horizon("ordinary-context-horizon", "ordinary");
    let extended_fixture = CliFixture::with_context_horizon("extended-context-horizon", "extended");

    let ordinary_output = ordinary_fixture
        .run_command()
        .arg("--provider")
        .arg("auto")
        .arg("--dry-run")
        .arg("--json")
        .output()
        .expect("run router");
    assert!(
        ordinary_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ordinary_output.stderr)
    );
    let ordinary: Value = serde_json::from_slice(&ordinary_output.stdout).expect("router json");

    let extended_output = extended_fixture
        .run_command()
        .arg("--provider")
        .arg("auto")
        .arg("--dry-run")
        .arg("--json")
        .output()
        .expect("run router");
    assert!(
        extended_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&extended_output.stderr)
    );
    let extended: Value = serde_json::from_slice(&extended_output.stdout).expect("router json");

    assert_eq!(
        ordinary["classification"]["task_context_horizon"],
        "ordinary"
    );
    assert_eq!(
        extended["classification"]["task_context_horizon"],
        "extended"
    );
    assert_eq!(ordinary["provider"], extended["provider"]);
    assert_eq!(ordinary["model"], extended["model"]);

    let ordinary_logged = ordinary_fixture
        .router()
        .arg("log")
        .arg("--limit")
        .arg("1")
        .arg("--json")
        .output()
        .expect("read decision log");
    assert!(
        ordinary_logged.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ordinary_logged.stderr)
    );
    let ordinary_rows: Value = serde_json::from_slice(&ordinary_logged.stdout).expect("log json");
    let ordinary_row = ordinary_rows[0].as_object().expect("log row object");
    assert!(
        ordinary_row.contains_key("task_context_horizon"),
        "row: {ordinary_row:?}"
    );
    assert_eq!(ordinary_row["task_context_horizon"], "ordinary");

    let extended_logged = extended_fixture
        .router()
        .arg("log")
        .arg("--limit")
        .arg("1")
        .arg("--json")
        .output()
        .expect("read decision log");
    assert!(
        extended_logged.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&extended_logged.stderr)
    );
    let extended_rows: Value = serde_json::from_slice(&extended_logged.stdout).expect("log json");
    let extended_row = extended_rows[0].as_object().expect("log row object");
    assert!(
        extended_row.contains_key("task_context_horizon"),
        "row: {extended_row:?}"
    );
    assert_eq!(extended_row["task_context_horizon"], "extended");
}

#[cfg(unix)]
#[test]
fn human_output_reports_the_job_without_a_viewer_instruction() {
    let fixture = CliFixture::new("human-output");
    let output = fixture.run(false);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    assert!(stdout.contains("claude exact id"), "{stdout}");
    assert!(stdout.contains(&fixture.name), "{stdout}");
    assert!(!stdout.contains("agent-viewer"), "{stdout}");
    assert!(!stdout.contains("watch:"), "{stdout}");
}

/// bonus-drain reconciles inflight work by matching the name it chose against the decision log and
/// `claude agents --json`, so `--name` has to survive the whole path verbatim, not just the argv.
#[cfg(unix)]
#[test]
fn a_supplied_name_reaches_the_spawned_job_and_the_decision_log_verbatim() {
    let name = "Bonus: abc-123";
    let fixture = CliFixture::listing_agent_named("supplied-name", Some(name));
    assert_ne!(fixture.name, name, "the task must not derive this name");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--name")
        .arg(name)
        .arg("--json")
        .output()
        .expect("run router");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    assert_eq!(value["dispatch"]["job_name"], name);
    assert_eq!(value["dispatch"]["job_id"], "claude exact id");
    let argv = wait_for_text(&fixture.spawn_log);
    let expected = ["--bg", "--model", "opus[1m]", "--name", name, &fixture.task];
    assert_eq!(argv.lines().collect::<Vec<_>>(), expected);

    let logged = fixture
        .router()
        .arg("log")
        .arg("--limit")
        .arg("1")
        .arg("--json")
        .output()
        .expect("read decision log");
    assert!(
        logged.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&logged.stderr)
    );
    let rows: Value = serde_json::from_slice(&logged.stdout).expect("log json");
    assert_eq!(rows[0]["job_name"], name);
}

/// Claude's CLI exposes no effective reasoning effort anywhere: it accepts `--effort`, prints a
/// warning on a value it does not know, and exits 0 having run at its own default. So the router
/// observes nothing and the column stays null, permanently.
///
/// This is the test that has to fail if someone later fills the claude column in from the decided
/// effort, from the model, or from a config default. It asserts key presence as well as null,
/// because a missing key and a null value are the same read otherwise, and the missing key is what
/// an absent feature looks like. The codex control that proves a non null value can be recorded at
/// all is `a_codex_row_records_the_observed_effort_while_claude_and_opencode_rows_stay_null` in the
/// core suite.
#[cfg(unix)]
#[test]
fn a_claude_dispatch_records_no_effective_effort() {
    let fixture = CliFixture::new("no-effective-effort");
    let output = fixture.run(true);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    let dispatch = value["dispatch"].as_object().expect("dispatch object");
    assert!(
        dispatch.contains_key("effective_effort"),
        "the dispatch reports no effective effort at all, so null is unreadable from absent: \
         {dispatch:?}"
    );
    assert_eq!(
        dispatch["effective_effort"],
        Value::Null,
        "claude reported no effort, so the router must record none"
    );
    // Both of the values an inference would most plausibly be built from, sitting in the same
    // payload as the null the router is required to report.
    assert_eq!(value["model"], "opus[1m]");
    assert_eq!(value["effort"], Value::Null);

    let logged = fixture
        .router()
        .arg("log")
        .arg("--limit")
        .arg("1")
        .arg("--json")
        .output()
        .expect("read decision log");
    assert!(
        logged.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&logged.stderr)
    );
    let rows: Value = serde_json::from_slice(&logged.stdout).expect("log json");
    let row = rows[0].as_object().expect("log row object");

    assert_eq!(row["provider"], "claude");
    assert!(
        row.contains_key("effort") && row.contains_key("effective_effort"),
        "the log reports the decided effort and the observed effort as separate readable keys: \
         {row:?}"
    );
    assert_eq!(
        row["effort"],
        Value::Null,
        "the router decided no effort, which is what this column has always recorded"
    );
    assert_eq!(
        row["effective_effort"],
        Value::Null,
        "and claude never said what it ran at, so nothing may be written here"
    );
    assert!(
        row.contains_key("task_context_horizon"),
        "the log must distinguish an explicit route from a missing JSON key: {row:?}"
    );
    assert_eq!(
        row["task_context_horizon"],
        Value::Null,
        "an explicit provider skips classification and therefore records SQL null"
    );
}

/// The auto path picks its model from the complexity tiers, so a `--model` alongside it can only be
/// silently dropped. That has to be loud.
#[cfg(unix)]
#[test]
fn auto_provider_with_an_explicit_model_fails_naming_both_flags() {
    let fixture = CliFixture::new("auto-plus-model");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("auto")
        .arg("--model")
        .arg("sonnet")
        .output()
        .expect("run router");

    assert!(
        !output.status.success(),
        "an ignored --model must not exit zero, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--provider"), "stderr: {stderr}");
    assert!(stderr.contains("--model"), "stderr: {stderr}");
    assert!(!fixture.spawn_log.exists(), "the rejected pair ran claude");
}

/// Complementary half of the auto-plus-model guard: an explicit provider paired with an explicit
/// model must be allowed through, and the model must actually reach the spawned claude argv. Do
/// not delete this as redundant with the auto-provider rejection test above; that test only
/// covers the reject branch of the guard, this one covers the accept branch.
#[cfg(unix)]
#[test]
fn explicit_provider_with_an_explicit_model_succeeds_and_forwards_the_model() {
    let fixture = CliFixture::new("explicit-plus-model");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--model")
        .arg("sonnet")
        .output()
        .expect("run router");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        wait_for_text(&fixture.spawn_log)
            .lines()
            .collect::<Vec<_>>(),
        vec![
            "--bg",
            "--model",
            "sonnet",
            "--name",
            &fixture.name,
            &fixture.task
        ]
    );
}

/// MCP scoping is a claude only capability, so naming another provider alongside the flags must
/// exit nonzero and say which flag is the problem, rather than dispatching a job that quietly
/// ignores the scoping the caller asked for.
#[cfg(unix)]
#[test]
fn mcp_scoping_with_an_explicit_non_claude_provider_exits_nonzero() {
    let root = TempDir::new("mcp-scoping");
    let bin = root.path.join("bin");
    let home = root.path.join("home");
    let cwd = root.path.join("working");
    fs::create_dir_all(&bin).expect("create bin");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&cwd).expect("create cwd");
    let config = root.path.join("scoped.mcp.json");
    fs::write(&config, r#"{"mcpServers":{}}"#).expect("write config");
    // A sandbox PATH holding no provider binaries, so nothing real can be dispatched.
    let path = bin.display().to_string();
    let route = |provider: &str, flags: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-router"))
            .arg("run")
            .arg("must reject scoping for other providers")
            .arg("--dir")
            .arg(&cwd)
            .arg("--provider")
            .arg(provider)
            .args(flags)
            .env("HOME", &home)
            .env("PATH", &path)
            .output()
            .expect("run router")
    };

    let config_arg = config.to_string_lossy().to_string();
    for provider in ["codex", "opencode"] {
        let output = route(provider, &["--mcp-config", &config_arg]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !output.status.success(),
            "{provider} accepted --mcp-config: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("--mcp-config"),
            "{provider} did not name the rejected flag: {stderr}"
        );
        // The flag must be parsed and then refused, not rejected as unknown.
        assert!(
            !stderr.contains("unexpected argument"),
            "{provider} does not accept --mcp-config at all: {stderr}"
        );
    }

    let output = route("opencode", &["--strict-mcp-config"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "opencode accepted --strict-mcp-config: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("--strict-mcp-config"),
        "opencode did not name the rejected flag: {stderr}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "opencode does not accept --strict-mcp-config at all: {stderr}"
    );
}

/// An empty `--name` is `Some("")`, which beats the derived default name and would spawn a job with
/// an empty name. `resolve_short_id` matches agent rows by name, so that job becomes unresolvable and
/// silently orphaned, which is exactly what the flag exists to prevent.
#[cfg(unix)]
#[test]
fn an_empty_name_is_rejected_naming_the_flag() {
    let fixture = CliFixture::new("empty-name");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--name")
        .arg("")
        .output()
        .expect("run router");

    assert!(
        !output.status.success(),
        "an empty --name must not exit zero, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--name"), "stderr: {stderr}");
    assert!(
        !fixture.spawn_log.exists(),
        "the rejected empty name ran claude"
    );
}

/// Whitespace-only names must be rejected identically to an empty name: trimmed, they are just as
/// empty, and would produce the same unresolvable, orphaned job.
#[cfg(unix)]
#[test]
fn a_whitespace_only_name_is_rejected_naming_the_flag() {
    let fixture = CliFixture::new("whitespace-name");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--name")
        .arg("   ")
        .output()
        .expect("run router");

    assert!(
        !output.status.success(),
        "a whitespace-only --name must not exit zero, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--name"), "stderr: {stderr}");
    assert!(
        !fixture.spawn_log.exists(),
        "the rejected whitespace-only name ran claude"
    );
}

#[cfg(target_os = "linux")]
fn fake_opencode_cli(root: &Path, log: &Path) -> PathBuf {
    let binary = root.join("opencode");
    // This stub writes its argv unconditionally and two tests assert the CLI was never run by
    // checking that the log does not exist, so the probe guard the helper emits ahead of this body
    // is what keeps a probe from inverting those assertions. No interpreter line here: the helper
    // supplies exactly one.
    common::write_stub(
        &binary,
        &format!(
            "printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&log.to_string_lossy())
        ),
    );
    binary
}

#[cfg(target_os = "linux")]
#[test]
fn managed_opencode_security_failure_does_not_run_the_detached_cli() {
    let root = TempDir::new("managed-opencode-failure");
    let bin = root.path.join("bin");
    let home = root.path.join("home");
    let cwd = root.path.join("working");
    let run_log = root.path.join("opencode.run");
    fs::create_dir_all(&bin).expect("create bin");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fake_opencode_cli(&bin, &run_log);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_agent-router"))
        .arg("run")
        .arg("must stay managed")
        .arg("--dir")
        .arg(&cwd)
        .arg("--provider")
        .arg("opencode")
        .env("HOME", home)
        .env("PATH", path)
        .env("OPENCODE_SERVER_USERNAME", "router test")
        .env("OPENCODE_SERVER_PASSWORD", "wrong for existing servers")
        .env("OPENCODE_CONFIG_CONTENT", "not json")
        .output()
        .expect("run router");

    if output.status.success() {
        let invocation = wait_for_text(&run_log);
        panic!(
            "managed setup failure silently ran detached OpenCode CLI with argv: {invocation:?}"
        );
    }
    assert!(
        !run_log.exists(),
        "detached OpenCode CLI ran after managed setup failed"
    );
}

#[cfg(target_os = "linux")]
fn compile_rejecting_opencode(root: &Path) -> PathBuf {
    let source = root.join("rejecting_opencode.rs");
    let binary = root.join("opencode");
    fs::write(
        &source,
        r#"
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments.get(1).map(String::as_str) == Some("run") {
        fs::write(std::env::var("AGENT_ROUTER_FIXTURE_RUN_LOG").unwrap(), "run").unwrap();
        return;
    }
    let port = arguments
        .windows(2)
        .find(|pair| pair[0] == "--port")
        .and_then(|pair| pair[1].parse::<u16>().ok())
        .unwrap();
    fs::write(
        std::env::var("AGENT_ROUTER_FIXTURE_PID_FILE").unwrap(),
        std::process::id().to_string(),
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut request = [0_u8; 4096];
        let size = stream.read(&mut request).unwrap();
        let authenticated = String::from_utf8_lossy(&request[..size])
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("authorization:"));
        let status = if authenticated {
            "403 Forbidden"
        } else {
            "401 Unauthorized"
        };
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    }
}
"#,
    )
    .expect("write fixture source");
    let output = Command::new("rustc")
        .arg("--edition=2021")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile rejecting opencode");
    assert!(
        output.status.success(),
        "fixture compiler stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

#[cfg(target_os = "linux")]
fn process_exists(pid: i32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
#[test]
fn router_terminates_a_server_that_fails_authenticated_readiness() {
    let root = TempDir::new("readiness-rejection");
    let home = root.path.join("home");
    let cwd = root.path.join("working");
    let pid_file = root.path.join("server.pid");
    let run_log = root.path.join("opencode.run");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&cwd).expect("create cwd");
    compile_rejecting_opencode(&root.path);
    let path = format!(
        "{}:{}",
        root.path.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let command = "ip link set lo up && exec \"$@\"";
    let output = Command::new("unshare")
        .args(["-Urn", "sh", "-c", command, "sh"])
        .arg(env!("CARGO_BIN_EXE_agent-router"))
        .arg("run")
        .arg("must reject bad readiness")
        .arg("--dir")
        .arg(&cwd)
        .arg("--provider")
        .arg("opencode")
        .env("HOME", home)
        .env("PATH", path)
        .env("OPENCODE_CONFIG_CONTENT", "{}")
        .env("AGENT_ROUTER_FIXTURE_PID_FILE", &pid_file)
        .env("AGENT_ROUTER_FIXTURE_RUN_LOG", &run_log)
        .output()
        .expect("run router in isolated network");
    let pid = wait_for_text(&pid_file)
        .trim()
        .parse::<i32>()
        .expect("server pid");
    let still_running = process_exists(pid);
    if still_running {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
    }

    assert!(
        !still_running,
        "router left rejected managed server process {pid} running"
    );
    assert!(
        !output.status.success(),
        "rejected managed server fell through to detached CLI"
    );
    assert!(
        !run_log.exists(),
        "detached OpenCode CLI ran after readiness rejection"
    );
}
