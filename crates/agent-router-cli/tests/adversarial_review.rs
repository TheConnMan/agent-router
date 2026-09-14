#![cfg(unix)]

#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

use agent_router_core::log::{DecisionLog, ReviewRow};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

/// How long any lifecycle poll in this suite waits before it fails the test.
///
/// Every wait here is bounded on purpose: a review that never settles, a worker that never
/// starts, or a cancel that never reaches the reviewer must fail its test with the last output it
/// saw, never hang the suite.
const POLL_DEADLINE: Duration = Duration::from_secs(10);

/// Pause between polls. Short enough that a settled lifecycle is observed promptly, long enough
/// that the poll does not itself contend with the router's own 250ms row polling.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a cancelled reviewer has to actually die. Separate from `POLL_DEADLINE` because this
/// is the acceptance criterion's own bound, not a fixture convenience.
const CANCEL_DEADLINE: Duration = Duration::from_secs(5);

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

fn write_grok_usage(path: &Path, weekly_pct: f64) {
    write_file(
        &path.join("logs/unified.jsonl"),
        &format!(
            "{}\n",
            json!({
                "msg": "billing: fetched credits config",
                "ctx": {
                    "subscriptionTier": "SuperGrok Plus",
                    "config": {
                        "creditUsagePercent": weekly_pct,
                        "currentPeriod": {
                            "type": "USAGE_PERIOD_TYPE_WEEKLY",
                            "end": "2099-01-07T00:00:00Z",
                        },
                    },
                },
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

        // The BLOCK mode below is what makes a review observable while it is still in flight: the
        // stub publishes its own pid and then waits for a file this test writes, so a test can
        // cancel it, kill the caller, or let a `--timeout` expire against a reviewer that is
        // provably still running. It is keyed on its own variable and sits between the existing
        // FAIL and DELAY modes, so every current test keeps its exact fixture behavior.
        //
        // The ORPHAN mode after it is the opposite shape: the reviewer exits immediately, having
        // left a descendant holding the stdout and stderr it inherited. The reviewer process is
        // gone, so anything reading those pipes to end-of-file is waiting on the descendant's
        // whole lifetime rather than on the reviewer's.
        let claude_body = format!(
            "printf '%s\\n' \"$@\" >> {}\n\
             if [ \"${{AGENT_ROUTER_FIXTURE_REVIEW_FAIL:-0}}\" = \"1\" ]; then\n\
               printf 'review provider failed\\n' >&2\n\
               exit 17\n\
             fi\n\
             if [ -n \"${{AGENT_ROUTER_FIXTURE_REVIEW_BLOCK:-}}\" ]; then\n\
               printf '%s' \"$$\" > \"$AGENT_ROUTER_FIXTURE_REVIEW_BLOCK/started\"\n\
               while [ ! -f \"$AGENT_ROUTER_FIXTURE_REVIEW_BLOCK/release\" ]; do\n\
                 sleep 0.05\n\
               done\n\
             fi\n\
             if [ -n \"${{AGENT_ROUTER_FIXTURE_REVIEW_ORPHAN:-}}\" ]; then\n\
               printf '%s' \"$$\" > \"$AGENT_ROUTER_FIXTURE_REVIEW_ORPHAN/started\"\n\
               sleep 60 &\n\
               printf '%s' \"$!\" > \"$AGENT_ROUTER_FIXTURE_REVIEW_ORPHAN/orphan\"\n\
               exit 0\n\
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

    fn grok_state_dir(&self) -> PathBuf {
        self.root.path.join("grok-home")
    }

    fn db_path(&self) -> PathBuf {
        self.root
            .path
            .join("home/.local/state/agent-router/router.db")
    }

    fn reviews(&self) -> Vec<ReviewRow> {
        DecisionLog::open_at(&self.db_path())
            .expect("open the fixture log")
            .recent_reviews(10)
            .expect("read reviews")
    }

    /// The directory the blocking reviewer publishes its pid into and watches for its release
    /// file. Created here rather than by the stub: the stub redirects into it, and a redirect into
    /// a missing directory would fail the reviewer instead of blocking it.
    fn block_dir(&self) -> PathBuf {
        let dir = self.root.path.join("blocking reviewer");
        fs::create_dir_all(&dir).expect("create the blocking reviewer directory");
        dir
    }

    /// The directory the orphaning reviewer publishes its own pid and its descendant's pid into.
    /// Created here for the same reason as `block_dir`: the stub redirects into it, and a redirect
    /// into a missing directory would fail the reviewer instead of orphaning a live descendant.
    fn orphan_dir(&self) -> PathBuf {
        let dir = self.root.path.join("orphaning reviewer");
        fs::create_dir_all(&dir).expect("create the orphaning reviewer directory");
        dir
    }

    fn command(&self) -> Command {
        self.command_for("codex", &self.cwd)
    }

    fn command_for(&self, primary: &str, dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .arg("adversarial-review")
            .arg("Review this working tree for regressions")
            .arg("--primary")
            .arg(primary)
            .arg("--dir")
            .arg(dir);
        self.apply_env(&mut command);
        command
    }

    /// `agent-router review <args...>` under the same environment as the review that created the
    /// row. Factored out with `command_for` rather than duplicated: a `review status` reading a
    /// different HOME would read a different router.db and report every id as unknown, which is a
    /// fixture bug that looks exactly like a product bug.
    fn review_subcommand(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command.arg("review");
        for arg in args {
            command.arg(arg);
        }
        self.apply_env(&mut command);
        command
    }

    /// The one environment both `adversarial-review` and its `review` subcommands run under.
    fn apply_env(&self, command: &mut Command) {
        let home = self.root.path.join("home");
        let bin = self.root.path.join("bin");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .env("HOME", home)
            .env("GROK_HOME", self.grok_state_dir())
            .env(
                "GROK_USAGE_CACHE",
                self.root.path.join("grok-usage-cache.json"),
            )
            .env("XDG_CONFIG_HOME", self.root.path.join("home/.config"))
            .env("CODEX_SESSIONS_DIR", &self.sessions)
            .env("CLAUDE_USAGE_CACHE", &self.usage_cache)
            .env("AGENT_ROUTER_CLAUDE_REVIEW_BIN", bin.join("claude-review"))
            .env("AGENT_ROUTER_CODEX_REVIEW_BIN", bin.join("codex"))
            .env("PATH", path);
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

/// The single provenance line a review prints before any provider work exists, parsed back into
/// the id it carries.
///
/// Asserted as an exact shape rather than a `contains`, and without a regex crate, because the
/// line is the whole of the command's stderr on a successful review: an accidental second line of
/// chatter has to fail here. `agent-router-cli` carries no dev-dependencies and this must not add
/// one.
fn started_review_id(stderr: &str) -> i64 {
    const PREFIX: &str = "agent-router: adversarial review ";
    const SUFFIX: &str = " started\n";
    assert!(
        stderr.starts_with(PREFIX)
            && stderr.ends_with(SUFFIX)
            && stderr.len() > PREFIX.len() + SUFFIX.len(),
        "stderr is not exactly the started line: {stderr:?}"
    );
    let digits = &stderr[PREFIX.len()..stderr.len() - SUFFIX.len()];
    assert!(
        digits.bytes().all(|byte| byte.is_ascii_digit()),
        "the review id is not all ASCII digits: {stderr:?}"
    );
    digits
        .parse()
        .unwrap_or_else(|error| panic!("the review id {digits:?} did not parse: {error}"))
}

/// Wait, bounded, for a path the blocking reviewer creates.
fn wait_for_path(path: &Path, what: &str) {
    let deadline = Instant::now() + POLL_DEADLINE;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{what}: {} never appeared within {POLL_DEADLINE:?}",
            path.display()
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The pid the blocking reviewer published, read back once the file actually carries one.
///
/// The stub creates `started` and writes into it as two steps, so the file can be observed to
/// exist while still empty. Re-reading until it is non-empty is the difference between probing a
/// real pid and parsing `""`.
fn reviewer_pid(block: &Path) -> u32 {
    let started = block.join("started");
    let deadline = Instant::now() + POLL_DEADLINE;
    loop {
        if let Ok(contents) = fs::read_to_string(&started) {
            let published = contents.trim();
            if !published.is_empty() {
                return published.parse().unwrap_or_else(|error| {
                    panic!("the reviewer pid {published:?} did not parse: {error}")
                });
            }
        }
        assert!(
            Instant::now() < deadline,
            "the blocking reviewer never published a pid at {}",
            started.display()
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Whether a pid is still signalable. `kill -0` through `sh` rather than a `libc` or `nix`
/// dependency: this crate has no dev-dependencies and a process-liveness probe is not a reason to
/// acquire the first one.
fn process_is_alive(pid: u32) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid}"))
        .status()
        .expect("probe the reviewer process")
        .success()
}

/// Poll `review status <id>` until it exits `expected`, bounded, panicking with the last output.
fn poll_review_status(fixture: &ReviewFixture, id: i64, expected: i32, extra: &[&str]) -> Output {
    let id = id.to_string();
    let mut args = vec!["status", id.as_str()];
    args.extend_from_slice(extra);
    let deadline = Instant::now() + POLL_DEADLINE;
    loop {
        let output = fixture
            .review_subcommand(&args)
            .output()
            .expect("run review status");
        if output.status.code() == Some(expected) {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "review {id} never reached exit {expected} within {POLL_DEADLINE:?}\nlast exit: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        );
        std::thread::sleep(POLL_INTERVAL);
    }
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
    // The whole of stderr is the one provenance line and nothing else. `started_review_id` is an
    // exact-shape check, so a second line of chatter fails here.
    assert!(started_review_id(&text(&output.stderr)) > 0);
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
fn registered_grok_reviewer_requires_an_authoritative_live_leader() {
    let fixture = ReviewFixture::new("grok leader unavailable", Some(23.0));
    write_grok_usage(&fixture.grok_state_dir(), 1.0);

    let output = fixture.run_json();
    assert_exit(&output, 0);
    let value = parse_json(&output);

    assert_eq!(value["reviewer_provider"], "claude");
    let grok = candidate_provenance(&value, "grok");
    assert_eq!(grok["weekly_pct"], Value::Null);
    assert_eq!(grok["eligible"], false);
    assert!(
        grok["rejection_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("leader")),
        "unexpected Grok rejection: {grok}"
    );
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

    // A failure raised before a pending row existed still lands in the ledger with its reason and
    // envelope, and reading it back by id names the row: a fresh fixture holds exactly one review.
    let status = fixture
        .review_subcommand(&["status", "1", "--json"])
        .output()
        .expect("run review status on the pre-id failure");
    assert_exit(&status, 1);
    let stored = parse_json(&status);
    assert_eq!(stored["status"], "failed");
    assert_eq!(stored["review_id"], 1);
    assert_eq!(stored["reason"], value["reason"]);
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

#[test]
fn a_review_exit_writes_one_reviews_row() {
    let fixture = ReviewFixture::new("persist review", Some(23.0));
    let output = fixture.run_json();
    assert_exit(&output, 0);
    let value = parse_json(&output);

    let rows = fixture.reviews();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.exit_status, 0);
    assert_eq!(row.primary, "codex");
    assert_eq!(row.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(
        row.reviewer_model.as_deref(),
        value["reviewer_model"].as_str()
    );
    assert_eq!(row.rationale, value["rationale"].as_str().unwrap());
    assert_eq!(
        row.body_bytes,
        i64::try_from(value["result"].as_str().unwrap().len()).unwrap()
    );
    assert_eq!(row.dir, fixture.cwd.to_string_lossy());
    assert!(row.ts > 0);
    let provenance: Value = serde_json::from_str(&row.usage_provenance).expect("provenance json");
    assert_eq!(provenance, value["usage_provenance"]);
}

#[test]
fn a_read_only_reviews_db_does_not_change_the_review_exit_status() {
    let fixture = ReviewFixture::new("readonly reviews db", Some(23.0));
    DecisionLog::open_at(&fixture.db_path()).expect("create the fixture log");
    fs::set_permissions(fixture.db_path(), fs::Permissions::from_mode(0o444))
        .expect("make the log read only");

    let output = fixture.run_json();
    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["result"], "completed review body");

    let reviews = DecisionLog::open_at(&fixture.db_path())
        .expect("reopen the read only log")
        .recent_reviews(10)
        .expect("read reviews");
    assert!(reviews.is_empty());
}

/// A bounded wait gives up with a review id and loses nothing: the id was on stderr before any
/// provider work existed, the row is durable, and `review status` returns the eventual retained
/// result byte for byte.
#[test]
fn timeout_returns_pending_with_an_id_and_status_attaches_to_the_retained_result() {
    let fixture = ReviewFixture::new("timeout pending", Some(23.0));
    let block = fixture.block_dir();
    let output = fixture
        .command()
        .arg("--json")
        .arg("--timeout")
        .arg("1")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_BLOCK", &block)
        .output()
        .expect("run a bounded adversarial review");

    assert_exit(&output, 4);
    let value = parse_json(&output);
    assert_eq!(value["status"], "pending");
    let id = value["review_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("review_id is not an integer: {value}"));
    assert!(id > 0, "{value}");
    // The printed id and the persisted id are one thing, not two.
    assert_eq!(started_review_id(&text(&output.stderr)), id);

    // The stub only runs after the id exists and has been printed, so `started` appearing at all
    // proves the id was persisted before any provider work began.
    wait_for_path(
        &block.join("started"),
        "the detached reviewer never started, so the timeout returned without work in flight",
    );

    let pending = fixture
        .review_subcommand(&["status", &id.to_string(), "--json"])
        .output()
        .expect("read the pending review");
    assert_exit(&pending, 4);
    assert_eq!(parse_json(&pending)["status"], "pending");
    assert_eq!(parse_json(&pending)["review_id"].as_i64(), Some(id));

    write_file(&block.join("release"), "");
    let completed = poll_review_status(&fixture, id, 0, &[]);
    // The retained result is byte-identical to what a synchronous run prints.
    assert_eq!(text(&completed.stdout), "completed review body\n");

    let completed = poll_review_status(&fixture, id, 0, &["--json"]);
    let value = parse_json(&completed);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["review_id"].as_i64(), Some(id));
    assert_eq!(value["result"], "completed review body");
    assert_eq!(value["primary_provider"], "codex");
    assert_eq!(value["reviewer_provider"], "claude");
    // These two would be gone if the terminal outcome envelope were reconstructed from the row's
    // own columns instead of retained: the reviews table has no column for either.
    assert_eq!(value["usage"]["weekly_pct"], 23.0);
    assert!(
        value["reviewer_model"]
            .as_str()
            .is_some_and(|model| !model.is_empty()),
        "{value}"
    );

    // Start plus finish is an insert and an update, never two rows.
    let rows = fixture.reviews();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status.as_deref(), Some("completed"));
    assert_eq!(rows[0].exit_status, 0);
    assert!(rows[0].body_bytes > 0, "{:?}", rows[0]);
    assert!(rows[0].outcome_json.is_some(), "{:?}", rows[0]);
}

/// Cancel stops the reviewer that is actually running, and the cancel is what settles the row.
#[test]
fn cancel_stops_the_in_flight_reviewer() {
    let fixture = ReviewFixture::new("cancel in flight", Some(23.0));
    let block = fixture.block_dir();
    let output = fixture
        .command()
        .arg("--json")
        .arg("--timeout")
        .arg("0")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_BLOCK", &block)
        .output()
        .expect("run an adversarial review that returns immediately");

    assert_exit(&output, 4);
    let id = parse_json(&output)["review_id"]
        .as_i64()
        .expect("review_id is an integer");
    assert_eq!(started_review_id(&text(&output.stderr)), id);

    let pid = reviewer_pid(&block);
    assert!(
        process_is_alive(pid),
        "the reviewer {pid} was already gone before the cancel"
    );

    let cancel = fixture
        .review_subcommand(&["cancel", &id.to_string()])
        .output()
        .expect("cancel the review");
    assert_exit(&cancel, 0);
    assert!(
        text(&cancel.stdout).contains(&format!("review {id} cancelled")),
        "{}",
        text(&cancel.stdout)
    );

    let deadline = Instant::now() + CANCEL_DEADLINE;
    while process_is_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the cancelled reviewer {pid} was still running after {CANCEL_DEADLINE:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }

    let status = fixture
        .review_subcommand(&["status", &id.to_string()])
        .output()
        .expect("read the cancelled review");
    assert_exit(&status, 1);
    // The worker may append its cleanup outcome to this reason afterwards, so match the state
    // rather than the whole sentence.
    assert!(
        text(&status.stderr).contains("cancelled"),
        "{}",
        text(&status.stderr)
    );

    let status = fixture
        .review_subcommand(&["status", &id.to_string(), "--json"])
        .output()
        .expect("read the cancelled review as json");
    assert_exit(&status, 1);
    let value = parse_json(&status);
    assert_eq!(value["status"], "cancelled");
    assert_eq!(value["review_id"].as_i64(), Some(id));
    assert_eq!(value["result"], Value::Null);

    // The compare-and-set from the caller's side: a settled review cannot be settled twice.
    let second = fixture
        .review_subcommand(&["cancel", &id.to_string()])
        .output()
        .expect("cancel the review a second time");
    assert_exit(&second, 1);
    assert!(
        text(&second.stderr).contains("cancelled"),
        "the second cancel does not name the current state: {}",
        text(&second.stderr)
    );

    // Never released: had the cancel not actually killed the reviewer, the stub would still be
    // looping and the pid probe above could not have succeeded.
    assert!(!block.join("release").exists());
    let rows = fixture.reviews();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status.as_deref(), Some("cancelled"));
    assert_eq!(rows[0].exit_status, 1);
}

/// Cancel has to be observed even when the reviewer's pipes outlive the reviewer.
///
/// The ORPHAN stub exits at once and leaves a descendant holding the stdout and stderr it
/// inherited, so a worker that drains those pipes to end-of-file before it looks at the row is
/// blocked for the descendant's whole 60s lifetime. During that window the cancel lands on the row
/// but the worker never reaches its settle path, so `reviewer stopped` is never appended: without
/// a cancellation-aware drain the poll below exhausts `CANCEL_DEADLINE` and fails. With one, the
/// worker abandons the drain and the note appears within a second.
#[test]
fn cancel_is_observed_while_draining_an_orphaned_reviewer_pipe() {
    let fixture = ReviewFixture::new("orphaned pipe cancel", Some(23.0));
    let orphan = fixture.orphan_dir();
    let output = fixture
        .command()
        .arg("--json")
        .arg("--timeout")
        .arg("0")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_ORPHAN", &orphan)
        .output()
        .expect("run an adversarial review that returns immediately");

    assert_exit(&output, 4);
    let id = parse_json(&output)["review_id"]
        .as_i64()
        .expect("review_id is an integer");
    assert_eq!(started_review_id(&text(&output.stderr)), id);

    wait_for_path(
        &orphan.join("started"),
        "the orphaning reviewer never ran, so nothing was holding the worker's pipes open",
    );
    wait_for_path(
        &orphan.join("orphan"),
        "the orphaning reviewer never published the descendant holding its pipes",
    );

    let cancel = fixture
        .review_subcommand(&["cancel", &id.to_string()])
        .output()
        .expect("cancel the review");
    assert_exit(&cancel, 0);

    // The row is where the worker's own observation shows up. The cancel above only writes the
    // cancelled state; `reviewer stopped` is appended by the worker's settle path, so it appearing
    // at all is proof the worker got out of the drain and looked at the row.
    let deadline = Instant::now() + CANCEL_DEADLINE;
    loop {
        let rows = fixture.reviews();
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .unwrap_or_else(|| panic!("review {id} has no row at all: {rows:?}"));
        if row
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("reviewer stopped"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the worker never observed the cancel while its pipes were held open, within \
             {CANCEL_DEADLINE:?}\nlast row: {row:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }

    let status = fixture
        .review_subcommand(&["status", &id.to_string(), "--json"])
        .output()
        .expect("read the cancelled review as json");
    assert_exit(&status, 1);
    let value = parse_json(&status);
    assert_eq!(value["status"], "cancelled");
    assert_eq!(value["review_id"].as_i64(), Some(id));
    assert_eq!(value["result"], Value::Null);

    // Best effort, and last: the descendant is orphaned by design and nothing else in this fixture
    // reaps it, so leaving it behind would leak a `sleep 60` out of every run of this test.
    let descendant = fs::read_to_string(orphan.join("orphan")).unwrap_or_default();
    let descendant = descendant.trim();
    if !descendant.is_empty() && descendant.bytes().all(|byte| byte.is_ascii_digit()) {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("kill {descendant}"))
            .status();
    }
}

/// The literal reproduction in issue #10: SIGKILL the caller mid-review and the paid work is
/// still there afterwards, addressable by the id the caller had already printed.
#[test]
fn a_killed_caller_does_not_lose_the_review() {
    let fixture = ReviewFixture::new("killed caller", Some(23.0));
    let block = fixture.block_dir();
    let mut child = fixture
        .command()
        .env("AGENT_ROUTER_FIXTURE_REVIEW_BLOCK", &block)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn a waiting adversarial review");

    wait_for_path(
        &block.join("started"),
        "the reviewer never started, so there was nothing in flight to orphan",
    );
    child.kill().expect("SIGKILL the waiting caller");
    let output = child.wait_with_output().expect("reap the killed caller");
    assert_eq!(output.status.code(), None, "the caller was not signalled");
    let id = started_review_id(&text(&output.stderr));

    write_file(&block.join("release"), "");
    let completed = poll_review_status(&fixture, id, 0, &[]);
    assert_eq!(text(&completed.stdout), "completed review body\n");

    let rows = fixture.reviews();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status.as_deref(), Some("completed"));
    assert!(rows[0].outcome_json.is_some(), "{:?}", rows[0]);
}

#[test]
fn review_status_on_an_unknown_id_fails_and_names_it() {
    let fixture = ReviewFixture::new("unknown review", Some(23.0));
    let output = fixture
        .review_subcommand(&["status", "4242"])
        .output()
        .expect("read an unknown review");

    assert_exit(&output, 1);
    assert!(
        text(&output.stderr).contains("unknown review 4242"),
        "{}",
        text(&output.stderr)
    );
    assert!(text(&output.stdout).is_empty());
}

/// The request travels to the detached worker on a fresh argv, so a request whose text starts
/// with a dash has to survive that hop rather than be read there as a flag.
#[test]
fn a_request_beginning_with_a_dash_survives_the_worker_re_exec() {
    const REQUEST: &str = "--- not a flag: review this tree ---";
    let fixture = ReviewFixture::new("dash request", Some(23.0));
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
    command
        .arg("adversarial-review")
        .arg("--primary")
        .arg("codex")
        .arg("--dir")
        .arg(&fixture.cwd)
        .arg("--json")
        .arg("--")
        .arg(REQUEST);
    fixture.apply_env(&mut command);
    let output = command
        .output()
        .expect("run a dash-prefixed review request");

    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["result"], "completed review body");
    let invocation = argv(&fixture.claude_log);
    assert!(
        invocation.iter().any(|arg| arg.contains(REQUEST)),
        "the request did not reach the reviewer intact: {invocation:?}"
    );
}

/// The `--provider`/`--model` pin surface. Every case here runs through the same fixture as the
/// automatic policy above, so a pin that leaked into the automatic path would fail those tests.
fn pinned(fixture: &ReviewFixture, primary: &str, args: &[&str]) -> Output {
    let mut command = fixture.command_for(primary, &fixture.cwd);
    command.arg("--json");
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run pinned adversarial review")
}

fn flag_value(invocation: &[String], flag: &str) -> Option<String> {
    invocation
        .iter()
        .position(|arg| arg == flag)
        .and_then(|index| invocation.get(index + 1).cloned())
}

#[test]
fn automatic_selection_records_no_pin_and_keeps_the_configured_high_tier() {
    let fixture = ReviewFixture::new("automatic unchanged", Some(23.0));
    let output = fixture.run_json();
    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["requested_provider"], Value::Null);
    assert_eq!(value["requested_model"], Value::Null);
    assert_eq!(value["reviewer_provider"], "claude");
    assert_eq!(value["reviewer_model"], "fable");
    assert_eq!(
        flag_value(&argv(&fixture.claude_log), "--model").as_deref(),
        Some("fable")
    );
}

#[test]
fn an_explicit_fable_pin_reaches_the_claude_argv_exactly_and_stays_ephemeral() {
    let fixture = ReviewFixture::new("fable pin", Some(35.0));
    let output = pinned(
        &fixture,
        "codex",
        &["--provider", "claude", "--model", "fable"],
    );

    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["primary_provider"], "codex");
    assert_eq!(value["requested_provider"], "claude");
    assert_eq!(value["requested_model"], "fable");
    assert_eq!(value["reviewer_provider"], "claude");
    assert_eq!(value["reviewer_model"], "fable");
    assert_eq!(value["usage"]["weekly_pct"], 35.0);
    assert_eq!(value["result"], "completed review body");
    assert!(
        value["rationale"]
            .as_str()
            .is_some_and(|why| why.contains("requested explicitly") && why.contains("codex")),
        "{}",
        value["rationale"]
    );

    assert!(!fixture.codex_log.exists(), "the pin invoked codex");
    let invocation = argv(&fixture.claude_log);
    assert_eq!(flag_value(&invocation, "--model").as_deref(), Some("fable"));
    assert!(invocation.iter().any(|arg| arg == "-p"));
    assert!(
        invocation
            .iter()
            .any(|arg| arg == "--no-session-persistence")
    );
    assert!(!invocation.iter().any(|arg| arg == "--bg"));
    assert!(!invocation.iter().any(|arg| arg == "--name"));
    assert!(
        !invocation
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
    );
    assert_eq!(
        flag_value(&invocation, "--permission-mode").as_deref(),
        Some("plan")
    );
    assert_eq!(
        flag_value(&invocation, "--tools").as_deref(),
        Some("Read,Glob,Grep")
    );
    assert!(invocation.iter().any(|arg| arg == "--strict-mcp-config"));

    let rows = fixture.reviews();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].exit_status, 0);
    assert_eq!(rows[0].reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(rows[0].reviewer_model.as_deref(), Some("fable"));
    assert!(rows[0].rationale.contains("requested explicitly"));
}

#[test]
fn a_provider_pin_without_a_model_runs_the_configured_high_tier() {
    let fixture = ReviewFixture::new("provider only pin", Some(35.0));
    let output = pinned(&fixture, "codex", &["--provider", "claude"]);

    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["requested_provider"], "claude");
    assert_eq!(value["requested_model"], Value::Null);
    assert_eq!(value["reviewer_model"], "fable");
}

#[test]
fn a_pinned_codex_reviewer_keeps_the_read_only_ephemeral_sandbox_with_the_exact_model() {
    let fixture = ReviewFixture::new("codex pin", None);
    write_codex_usage(&fixture.sessions, 17);
    let output = pinned(
        &fixture,
        "claude",
        &["--provider", "codex", "--model", "gpt-5.6-terra"],
    );

    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["reviewer_provider"], "codex");
    assert_eq!(value["reviewer_model"], "gpt-5.6-terra");
    assert_eq!(value["requested_model"], "gpt-5.6-terra");
    assert!(!fixture.claude_log.exists());
    let invocation = argv(&fixture.codex_log);
    assert_eq!(
        flag_value(&invocation, "--model").as_deref(),
        Some("gpt-5.6-terra")
    );
    assert_eq!(
        flag_value(&invocation, "--sandbox").as_deref(),
        Some("read-only")
    );
    assert!(invocation.iter().any(|arg| arg == "--ephemeral"));
    assert!(!invocation.iter().any(|arg| arg == "--bg"));
}

#[test]
fn pinning_the_primary_provider_fails_before_any_invocation() {
    let fixture = ReviewFixture::new("primary pin", Some(35.0));
    let output = pinned(
        &fixture,
        "claude",
        &["--provider", "claude", "--model", "fable"],
    );

    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");
    assert_eq!(value["primary_provider"], "claude");
    assert_eq!(value["requested_provider"], "claude");
    assert_eq!(value["requested_model"], "fable");
    assert_eq!(value["reviewer_provider"], Value::Null);
    assert_eq!(value["reviewer_model"], Value::Null);
    assert!(
        value["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("primary")),
        "{}",
        value["reason"]
    );
    assert!(!fixture.claude_log.exists());
    assert!(!fixture.codex_log.exists());
    let rows = fixture.reviews();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].exit_status, 1);
    assert_eq!(rows[0].reviewer_provider, None);
    // The reviews table has no requested_* columns, so the rationale is where a rejected pin has
    // to be distinguishable from an automatic failure.
    assert!(
        rows[0]
            .rationale
            .contains("claude requested explicitly with model fable"),
        "{}",
        rows[0].rationale
    );
}

#[test]
fn a_pin_is_still_reported_when_the_config_fails_to_load() {
    let fixture = ReviewFixture::new("config failure pin", Some(35.0));
    write_file(
        &fixture
            .root
            .path
            .join("home/.config/agent-router/config.toml"),
        "this is not toml = = =\n",
    );
    let output = pinned(
        &fixture,
        "codex",
        &["--provider", "claude", "--model", "fable"],
    );

    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");
    assert_eq!(value["requested_provider"], "claude");
    assert_eq!(value["requested_model"], "fable");
    assert_eq!(value["reviewer_provider"], Value::Null);
    assert!(
        value["rationale"]
            .as_str()
            .is_some_and(|why| why.contains("claude requested explicitly with model fable")),
        "{}",
        value["rationale"]
    );
    assert!(!fixture.claude_log.exists());
    assert!(!fixture.codex_log.exists());
}

#[test]
fn a_model_without_an_explicit_provider_is_rejected() {
    let fixture = ReviewFixture::new("orphan model", Some(35.0));
    for args in [
        vec!["--model", "fable"],
        vec!["--provider", "auto", "--model", "fable"],
    ] {
        let output = pinned(&fixture, "codex", &args);
        assert_exit(&output, 1);
        let value = parse_json(&output);
        assert_eq!(value["status"], "failed");
        assert_eq!(value["requested_model"], "fable");
        assert!(
            value["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("--model requires an explicit --provider")),
            "{}",
            value["reason"]
        );
    }
    assert!(!fixture.claude_log.exists());
    assert!(!fixture.codex_log.exists());
}

#[test]
fn a_model_pin_on_grok_and_a_malformed_model_are_rejected() {
    let fixture = ReviewFixture::new("bad model pins", Some(35.0));

    let output = pinned(
        &fixture,
        "codex",
        &["--provider", "grok", "--model", "grok-4"],
    );
    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");
    assert!(
        value["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("grok")),
        "{}",
        value["reason"]
    );

    for malformed in ["--model=--bg", "--model=", "--model=fable opus"] {
        let output = pinned(&fixture, "codex", &["--provider", "claude", malformed]);
        assert_exit(&output, 1);
        let value = parse_json(&output);
        assert_eq!(value["status"], "failed", "{malformed}");
        assert_eq!(value["reviewer_model"], Value::Null, "{malformed}");
        assert!(
            value["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("model")),
            "{malformed}: {}",
            value["reason"]
        );
    }

    let output = pinned(&fixture, "codex", &["--provider", "opencode"]);
    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");

    assert!(!fixture.claude_log.exists());
    assert!(!fixture.codex_log.exists());
}

#[test]
fn an_ineligible_pin_skips_with_exit_three_and_never_falls_back() {
    // Grok is the primary, so codex is the eligible automatic alternative the pin must not use.
    for (label, weekly_pct, expected) in [("ceiling", Some(90.0), "90"), ("stale", None, "stale")] {
        let fixture = ReviewFixture::new(label, weekly_pct);
        write_codex_usage(&fixture.sessions, 17);
        let output = pinned(
            &fixture,
            "grok",
            &["--provider", "claude", "--model", "fable"],
        );

        assert_exit(&output, 3);
        let value = parse_json(&output);
        assert_eq!(value["status"], "skipped", "{label}");
        assert_eq!(value["primary_provider"], "grok");
        assert_eq!(value["requested_provider"], "claude");
        assert_eq!(value["requested_model"], "fable");
        assert_eq!(value["reviewer_provider"], Value::Null, "{label}");
        assert_eq!(value["reviewer_model"], Value::Null, "{label}");
        assert_eq!(value["result"], Value::Null, "{label}");
        assert!(
            value["reason"].as_str().is_some_and(|reason| {
                reason.starts_with("requested reviewer claude is not eligible: ")
                    && reason.contains(expected)
            }),
            "{label}: {}",
            value["reason"]
        );
        assert!(
            value["rationale"]
                .as_str()
                .is_some_and(|why| why.contains(expected)),
            "{label}: {}",
            value["rationale"]
        );
        let claude = candidate_provenance(&value, "claude");
        assert_eq!(claude["eligible"], false);
        assert!(
            !fixture.codex_log.exists(),
            "{label}: the pin fell back to codex"
        );
        assert!(!fixture.claude_log.exists(), "{label}");
        let rows = fixture.reviews();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].exit_status, 3);
        assert_eq!(rows[0].reviewer_provider, None);
    }
}

#[test]
fn a_pinned_grok_reviewer_still_needs_the_authoritative_leader() {
    let fixture = ReviewFixture::new("grok pin", Some(35.0));
    write_grok_usage(&fixture.grok_state_dir(), 1.0);
    let output = pinned(&fixture, "codex", &["--provider", "grok"]);

    assert_exit(&output, 3);
    let value = parse_json(&output);
    assert_eq!(value["status"], "skipped");
    assert_eq!(value["requested_provider"], "grok");
    assert_eq!(value["reviewer_provider"], Value::Null);
    let grok = candidate_provenance(&value, "grok");
    assert_eq!(grok["eligible"], false);
    // The authoritative probe fails one step earlier on a box with no grok binary (CI runners),
    // so the reason is either the missing leader or the unresolvable executable. Both are the
    // grok availability gate refusing the pin; neither may become a fallback.
    assert!(
        grok["rejection_reason"].as_str().is_some_and(|reason| {
            reason.contains("leader") || reason.contains("grok executable")
        }),
        "{grok}"
    );
    assert!(
        value["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("requested reviewer grok is not eligible: ")),
        "{}",
        value["reason"]
    );
    assert!(
        !fixture.claude_log.exists(),
        "the grok pin fell back to claude"
    );
}

#[test]
fn a_pinned_claude_reviewer_honors_the_reserve_as_a_floor() {
    let fixture = ReviewFixture::new("reserve floor", Some(70.0));
    let output = pinned(
        &fixture,
        "codex",
        &["--provider", "claude", "--model", "fable"],
    );

    assert_exit(&output, 3);
    let value = parse_json(&output);
    assert_eq!(value["status"], "skipped");
    assert!(
        value["rationale"]
            .as_str()
            .is_some_and(|why| why.contains("reserve") && why.contains("70.0")),
        "{}",
        value["rationale"]
    );
    assert!(!fixture.claude_log.exists());

    // Text mode prints the reason alone, so the reason itself has to carry the refusing gate.
    let output = fixture
        .command()
        .arg("--provider")
        .arg("claude")
        .arg("--model")
        .arg("fable")
        .output()
        .expect("run text mode pinned review");
    assert_exit(&output, 3);
    assert!(text(&output.stdout).is_empty());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("requested reviewer claude is not eligible")
            && stderr.contains("reserve")
            && stderr.contains("70.0"),
        "{stderr}"
    );

    // The operator's reserve setting is what decides it: zero the reserve and the same reading
    // passes, so the refusal above is the configured reserve rather than the raw ceiling.
    write_file(
        &fixture
            .root
            .path
            .join("home/.config/agent-router/config.toml"),
        "config_version = 4\n\n[classifier]\nengine = \"codex\"\n\n[adversarial_review]\nclaude_usage_reserve_pct = 0.0\n",
    );
    let output = pinned(
        &fixture,
        "codex",
        &["--provider", "claude", "--model", "fable"],
    );
    assert_exit(&output, 0);
    let value = parse_json(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["reviewer_model"], "fable");
}

#[test]
fn a_pinned_reviewer_failure_is_reported_with_the_pin_and_no_fallback() {
    let fixture = ReviewFixture::new("pinned failure", Some(35.0));
    let output = fixture
        .command_for("grok", &fixture.cwd)
        .arg("--json")
        .arg("--provider")
        .arg("claude")
        .arg("--model")
        .arg("fable")
        .env("AGENT_ROUTER_FIXTURE_REVIEW_FAIL", "1")
        .output()
        .expect("run failing pinned review");

    assert_exit(&output, 1);
    let value = parse_json(&output);
    assert_eq!(value["status"], "failed");
    assert_eq!(value["requested_model"], "fable");
    assert_eq!(value["reviewer_provider"], "claude");
    assert_eq!(value["reviewer_model"], "fable");
    assert!(
        value["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("review provider failed"))
    );
    assert_eq!(value["result"], Value::Null);
    assert!(
        !fixture.codex_log.exists(),
        "the failed pin fell back to codex"
    );
    let rows = fixture.reviews();
    assert_eq!(rows[0].exit_status, 1);
    assert_eq!(rows[0].reviewer_model.as_deref(), Some("fable"));
}

#[test]
fn help_documents_the_pin_flags_honestly() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-router"))
        .arg("adversarial-review")
        .arg("--help")
        .output()
        .expect("print help");
    assert_exit(&output, 0);
    let help = text(&output.stdout);
    assert!(help.contains("--provider"), "{help}");
    assert!(help.contains("--model"), "{help}");
    assert!(help.contains("primary"), "{help}");
    assert!(help.contains("eligib"), "{help}");
}
