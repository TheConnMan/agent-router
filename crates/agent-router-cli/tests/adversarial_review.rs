#![cfg(unix)]

#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

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
            "agent_router_adversarial_review_{}_{}_{}_{}",
            std::process::id(),
            serial,
            label,
            unique
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
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

fn claude_result(text: &str) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": text,
    })
    .to_string()
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(path, contents).expect("write fixture");
}

fn write_claude_usage(path: &Path, weekly_pct: f64) {
    write_file(
        path,
        &json!({
            "five_hour": {
                "utilization": 11.0,
                "resets_at": "2099-01-01T00:00:00Z"
            },
            "seven_day": {
                "utilization": weekly_pct,
                "resets_at": "2099-01-07T00:00:00Z"
            }
        })
        .to_string(),
    );
}

fn write_codex_usage(path: &Path, weekly_pct: i64) {
    let resets_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        + 3_600;
    write_file(
        &path.join("rollout.jsonl"),
        &format!(
            "{}\n",
            json!({
                "payload": {
                    "rate_limits": {
                        "primary": {
                            "window_minutes": 10080,
                            "used_percent": weekly_pct,
                            "resets_at": resets_at,
                        },
                        "secondary": null,
                    }
                }
            })
        ),
    );
}

struct ReviewFixture {
    root: TempDir,
    cwd: PathBuf,
    claude_log: PathBuf,
    codex_log: PathBuf,
    usage_cache: PathBuf,
    sessions: PathBuf,
}

impl ReviewFixture {
    fn new(label: &str, weekly_pct: Option<f64>) -> Self {
        let root = TempDir::new(label);
        let home = root.path.join("home");
        let config_home = home.join(".config");
        let bin = root.path.join("bin");
        let cwd = root.path.join("working tree");
        let sessions = root.path.join("empty codex sessions");
        let claude_log = root.path.join("claude calls");
        let codex_log = root.path.join("codex calls");
        let usage_cache = root.path.join("claude usage.json");
        fs::create_dir_all(&bin).expect("create binary directory");
        fs::create_dir_all(&cwd).expect("create working tree");
        fs::create_dir_all(&sessions).expect("create codex sessions");
        write_file(
            &config_home.join("agent-router/config.toml"),
            "config_version = 4\n\n[classifier]\nengine = \"codex\"\n",
        );
        if let Some(weekly_pct) = weekly_pct {
            write_claude_usage(&usage_cache, weekly_pct);
        }

        let claude_body = format!(
            "printf '%s\\n' \"$@\" >> {}\n\
             if [ \"${{AGENT_ROUTER_FIXTURE_REVIEW_FAIL:-0}}\" = \"1\" ]; then\n\
               printf 'review provider failed\\n' >&2\n\
               exit 17\n\
             fi\n\
             if [ -n \"${{AGENT_ROUTER_FIXTURE_REVIEW_DELAY:-}}\" ]; then\n\
               sleep \"$AGENT_ROUTER_FIXTURE_REVIEW_DELAY\"\n\
             fi\n\
             printf '%s\\n' {}\n",
            shell_quote(&claude_log.to_string_lossy()),
            shell_quote(&claude_result("completed review body")),
        );
        common::write_stub(&bin.join("claude-review"), &claude_body);
        common::write_stub(&bin.join("claude"), &claude_body);

        let codex_body = format!(
            "printf '%s\\n' \"$@\" >> {}\n\
             if [ -n \"${{AGENT_ROUTER_FIXTURE_REVIEW_DELAY:-}}\" ]; then\n\
               sleep \"$AGENT_ROUTER_FIXTURE_REVIEW_DELAY\"\n\
             fi\n\
             printf '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"codex completed review\"}}}}\\n'\n",
            shell_quote(&codex_log.to_string_lossy()),
        );
        common::write_stub(&bin.join("codex"), &codex_body);

        Self {
            root,
            cwd,
            claude_log,
            codex_log,
            usage_cache,
            sessions,
        }
    }

    fn command(&self) -> Command {
        self.command_for("codex", &self.cwd)
    }

    fn command_for(&self, primary: &str, dir: &Path) -> Command {
        let home = self.root.path.join("home");
        let bin = self.root.path.join("bin");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .arg("adversarial-review")
            .arg("Review this working tree for regressions")
            .arg("--primary")
            .arg(primary)
            .arg("--dir")
            .arg(dir)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", self.root.path.join("home/.config"))
            .env("CODEX_SESSIONS_DIR", &self.sessions)
            .env("CLAUDE_USAGE_CACHE", &self.usage_cache)
            .env("AGENT_ROUTER_CLAUDE_REVIEW_BIN", bin.join("claude-review"))
            .env("AGENT_ROUTER_CODEX_REVIEW_BIN", bin.join("codex"))
            .env("PATH", path);
        command
    }

    fn run_json(&self) -> Output {
        self.command()
            .arg("--json")
            .output()
            .expect("run adversarial review")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("command output is utf8")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected status\nstdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn parse_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            text(&output.stdout),
            text(&output.stderr)
        )
    })
}

fn argv(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("read provider invocation")
        .lines()
        .map(str::to_string)
        .collect()
}

fn candidate_provenance<'a>(value: &'a Value, provider: &str) -> &'a Value {
    value["usage_provenance"]
        .as_array()
        .expect("usage provenance array")
        .iter()
        .find(|candidate| candidate["provider"] == provider)
        .unwrap_or_else(|| panic!("missing usage provenance for {provider}: {}", value))
}

#[test]
fn completed_json_excludes_primary_skips_classifier_and_waits_for_review() {
    let fixture = ReviewFixture::new("completed", Some(23.0));
    let started = Instant::now();
    let output = fixture
        .command()
        .arg("--json")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_DELAY", "0.20")
        .output()
        .expect("run adversarial review");
    let elapsed = started.elapsed();

    assert_exit(&output, 0);
    assert!(
        elapsed >= Duration::from_millis(175),
        "command returned before the reviewer completed: {elapsed:?}"
    );
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["primary_provider"], "codex");
    assert_eq!(value["reviewer_provider"], "claude");
    assert!(
        value["reviewer_model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
    assert_eq!(value["usage"]["weekly_pct"], 23.0);
    assert_eq!(value["usage"]["weekly_capacity_known"], true);
    assert_eq!(value["usage"]["stale"], false);
    assert_eq!(value["reason"], Value::Null);
    assert_eq!(value["result"], "completed review body");
    assert!(value["rationale"].as_str().is_some_and(|why| {
        why.contains("claude") && why.contains("23") && why.contains("codex")
    }));

    assert!(
        !fixture.codex_log.exists(),
        "the excluded primary or the normal classifier invoked codex: {}",
        fs::read_to_string(&fixture.codex_log).unwrap_or_default()
    );
    let invocation = argv(&fixture.claude_log);
    assert!(!invocation.iter().any(|arg| arg == "--bg"));
    assert!(
        !invocation
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
    );
    let permission = invocation
        .iter()
        .position(|arg| arg == "--permission-mode")
        .expect("review invocation pins a permission mode");
    assert_eq!(
        invocation.get(permission + 1).map(String::as_str),
        Some("plan")
    );
    assert!(invocation.iter().any(|arg| arg == "--strict-mcp-config"));
    assert!(
        invocation
            .iter()
            .any(|arg| arg.contains("Review this working tree for regressions"))
    );
}

#[test]
fn completed_human_output_is_the_review_body() {
    let fixture = ReviewFixture::new("human", Some(23.0));
    let output = fixture
        .command()
        .output()
        .expect("run human adversarial review");

    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "completed review body\n");
    assert!(text(&output.stderr).is_empty());
}

#[test]
fn registered_reviewer_provenance_includes_grok_and_excludes_grok_primary() {
    let fixture = ReviewFixture::new("grok reviewer provenance", Some(23.0));

    let output = fixture.run_json();
    assert_exit(&output, 0);
    let value = parse_json(&output);
    let grok = candidate_provenance(&value, "grok");
    assert_eq!(grok["provider"], "grok");

    let output = fixture
        .command_for("grok", &fixture.cwd)
        .arg("--json")
        .output()
        .expect("run Grok-primary adversarial review");

    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["primary_provider"], "grok");
    assert_ne!(value["reviewer_provider"], "grok");
    let grok = candidate_provenance(&value, "grok");
    assert_eq!(grok["eligible"], false);
}

#[test]
fn stale_or_over_limit_alternative_returns_json_skip_and_exit_three() {
    for (label, weekly_pct, expected_rationale) in
        [("stale", None, "stale"), ("ceiling", Some(90.0), "90")]
    {
        let fixture = ReviewFixture::new(label, weekly_pct);
        let output = fixture.run_json();

        assert_exit(&output, 3);
        let value = parse_json(&output);
        assert_eq!(value["status"], "skipped");
        assert_eq!(value["primary_provider"], "codex");
        assert_eq!(value["reviewer_provider"], Value::Null);
        assert_eq!(value["reviewer_model"], Value::Null);
        assert_eq!(value["usage"], Value::Null);
        assert_eq!(value["reason"], "no eligible alternative provider");
        assert_eq!(value["result"], Value::Null);
        assert!(
            value["rationale"]
                .as_str()
                .is_some_and(|why| why.contains("claude") && why.contains(expected_rationale))
        );
        let claude = candidate_provenance(&value, "claude");
        assert_eq!(claude["provider"], "claude");
        assert_eq!(claude["eligible"], false);
        assert!(claude["rejection_reason"].is_string());
        let codex = candidate_provenance(&value, "codex");
        assert_eq!(codex["provider"], "codex");
        assert_eq!(codex["eligible"], false);
        assert!(codex["rejection_reason"].is_string());
        assert_eq!(codex["weekly_pct"], Value::Null);
        assert_eq!(codex["stale"], true);
        if weekly_pct.is_none() {
            assert_eq!(claude["weekly_pct"], Value::Null);
            assert_eq!(claude["stale"], true);
        } else {
            assert_eq!(weekly_pct, Some(90.0));
            assert_eq!(claude["weekly_pct"], 90.0);
            assert_eq!(claude["stale"], false);
            assert!(
                claude["rejection_reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("90"))
            );
        }
        assert!(!fixture.claude_log.exists());
        assert!(!fixture.codex_log.exists());
    }
}

#[test]
fn invalid_directory_fails_before_an_empty_candidate_set_can_skip() {
    let fixture = ReviewFixture::new("invalid directory", None);
    let missing = fixture.root.path.join("does not exist");
    let output = fixture
        .command_for("codex", &missing)
        .arg("--json")
        .output()
        .expect("run invalid directory adversarial review");

    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");
    assert_eq!(value["primary_provider"], "codex");
    assert_eq!(value["reviewer_provider"], Value::Null);
    assert_eq!(value["reviewer_model"], Value::Null);
    assert_eq!(value["result"], Value::Null);
    assert!(value["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("does not exist") && reason.contains("directory")));
    assert!(!fixture.claude_log.exists());
    assert!(!fixture.codex_log.exists());
}

#[test]
fn claude_primary_runs_codex_synchronously_in_a_read_only_sandbox() {
    let fixture = ReviewFixture::new("claude primary", None);
    write_file(
        &fixture
            .root
            .path
            .join("home/.config/agent-router/config.toml"),
        "config_version = 4\n\n[classifier]\nengine = \"claude\"\n",
    );
    write_codex_usage(&fixture.sessions, 17);
    let started = Instant::now();
    let output = fixture
        .command_for("claude", &fixture.cwd)
        .arg("--json")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_DELAY", "0.20")
        .output()
        .expect("run codex adversarial review");

    assert_exit(&output, 0);
    assert!(
        started.elapsed() >= Duration::from_millis(175),
        "command returned before the codex reviewer completed"
    );
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["primary_provider"], "claude");
    assert_eq!(value["reviewer_provider"], "codex");
    assert_eq!(value["usage"]["weekly_pct"], 17.0);
    assert_eq!(value["result"], "codex completed review");
    assert!(
        !fixture.claude_log.exists(),
        "the excluded primary or normal classifier invoked claude: {}",
        fs::read_to_string(&fixture.claude_log).unwrap_or_default()
    );
    let invocation = argv(&fixture.codex_log);
    assert!(!invocation.iter().any(|arg| arg == "--bg"));
    let sandbox = invocation
        .iter()
        .position(|arg| arg == "--sandbox")
        .expect("codex review pins a sandbox");
    assert_eq!(
        invocation.get(sandbox + 1).map(String::as_str),
        Some("read-only")
    );
}

#[test]
fn invocation_failure_returns_json_failure_and_exit_one() {
    let fixture = ReviewFixture::new("failure", Some(23.0));
    let output = fixture
        .command()
        .arg("--json")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_FAIL", "1")
        .output()
        .expect("run failing adversarial review");

    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");
    assert_eq!(value["primary_provider"], "codex");
    assert_eq!(value["reviewer_provider"], "claude");
    assert!(
        value["reviewer_model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
    assert_eq!(value["usage"]["weekly_pct"], 23.0);
    assert!(
        value["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("review provider failed"))
    );
    assert_eq!(value["result"], Value::Null);
    assert!(!fixture.codex_log.exists());
    assert!(!argv(&fixture.claude_log).is_empty());
}
