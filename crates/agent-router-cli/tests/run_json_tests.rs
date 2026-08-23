use agent_router_core::runtime::short_job_name;
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
    /// The name derived from the task alone, with no model involved. Every dispatch used to carry
    /// this; one that still does is a dispatch whose naming call was skipped or came back unusable.
    name: String,
    /// The title the fake classifier answers with, which both routes now name a job by.
    classifier_name: String,
    cwd: PathBuf,
    spawn_log: PathBuf,
    /// One line per `claude -p` invocation, so a test can assert the naming call happened, or that
    /// it was skipped, rather than inferring either from the name that came out.
    classifier_log: PathBuf,
    /// What the fake `claude -p` answers, replaceable per test.
    classifier_answer_file: PathBuf,
}

/// The envelope `claude -p --output-format json` wraps the model's text in.
#[cfg(unix)]
fn claude_result(text: &str) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": text,
    })
    .to_string()
}

#[cfg(unix)]
impl CliFixture {
    fn new(label: &str) -> Self {
        Self::listing_agent_named_with_context_horizon(label, None, "extended", false)
    }

    fn with_context_horizon(label: &str, task_context_horizon: &str) -> Self {
        Self::listing_agent_named_with_context_horizon(label, None, task_context_horizon, false)
    }

    /// `listed` is the name the fake `claude agents` listing advertises, which is what the router
    /// matches against to resolve the short id of the job it just spawned. None means the name
    /// derived from the task.
    fn listing_agent_named(label: &str, listed: Option<&str>) -> Self {
        Self::listing_agent_named_with_context_horizon(label, listed, "extended", false)
    }

    fn auto_claude_job(label: &str, listed: &str) -> Self {
        Self::listing_agent_named_with_context_horizon(label, Some(listed), "extended", true)
    }

    fn listing_agent_named_with_context_horizon(
        label: &str,
        listed: Option<&str>,
        task_context_horizon: &str,
        orchestration: bool,
    ) -> Self {
        let root = TempDir::new(label);
        let home = root.path.join("home");
        let bin = root.path.join("bin");
        let cwd = root.path.join("working directory");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(&bin).expect("create bin");
        fs::create_dir_all(&cwd).expect("create cwd");
        let task = "/implement RS-123 rename background sessions".to_string();
        let name = short_job_name(&task);
        let classifier_name = "RS-123 Input Box Searching";
        let spawn_log = root.path.join("claude.argv");
        let classifier_log = root.path.join("claude.-p.calls");
        // The model titles a job on both routes now, so that is the name the listing advertises
        // and the name the router matches its own spawn against to resolve a short id.
        let listed = listed.unwrap_or(classifier_name);
        let classifier_answer = json!({
            "orchestration": orchestration,
            "missing_connector": false,
            "complexity": "medium",
            "task_context_horizon": task_context_horizon,
            "rationale": "fixture context",
            "job_name": classifier_name,
        })
        .to_string();
        // Held in a file the stub cats rather than baked into the stub body, so a test can replace
        // what the model answers without a second stub or a second constructor.
        let classifier_answer_file = root.path.join("classifier.answer");
        fs::write(&classifier_answer_file, claude_result(&classifier_answer))
            .expect("write the classifier answer");
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
               printf 'called\\n' >> {}\n\
               cat {}\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&agents),
            shell_quote(&classifier_log.to_string_lossy()),
            shell_quote(&classifier_answer_file.to_string_lossy()),
            shell_quote(&spawn_log.to_string_lossy())
        );
        let fake_claude = bin.join("claude");
        common::write_stub(&fake_claude, &body);
        Self {
            root,
            task,
            name,
            classifier_name: classifier_name.to_string(),
            cwd,
            spawn_log,
            classifier_log,
            classifier_answer_file,
        }
    }

    /// Replace what the fake `claude -p` answers with, so a test can drive the naming call's
    /// failure paths.
    fn answers_with(&self, text: &str) {
        fs::write(&self.classifier_answer_file, claude_result(text))
            .expect("rewrite the classifier answer");
    }

    fn answers_with_complexity(&self, complexity: &str) {
        self.answers_with(
            &json!({
                "orchestration": false,
                "missing_connector": false,
                "complexity": complexity,
                "task_context_horizon": "ordinary",
                "rationale": "fixture complexity",
                "job_name": self.classifier_name,
            })
            .to_string(),
        );
    }

    fn with_task(mut self, task: &str) -> Self {
        self.task = task.to_string();
        self.name = short_job_name(task);
        self
    }

    /// How many times the fake claude was asked to answer a prompt.
    fn classifier_calls(&self) -> usize {
        fs::read_to_string(&self.classifier_log)
            .map(|log| log.lines().count())
            .unwrap_or(0)
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
            .env("GROK_HOME", self.root.path.join("grok-home"))
            .env(
                "GROK_USAGE_CACHE",
                self.root.path.join("grok-usage-cache.json"),
            )
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
    assert_eq!(value["effort"], "medium");
    assert_eq!(value["gates"], json!(["explicit_provider"]));
    assert_eq!(value["classification"]["complexity"], "medium");
    assert!(value["usage"]["claude"].is_object());
    assert!(value["usage"]["codex"].is_object());
    assert!(
        value["rationale"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(value["dispatch"]["job_id"], "claude exact id");
    assert_eq!(value["dispatch"]["job_name"], fixture.classifier_name);
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
            "--effort",
            "medium",
            "--name",
            &fixture.classifier_name,
            &fixture.task
        ]
    );
}

/// A named provider still classifies its downstream model and effort. The same classifier answer
/// supplies the job title, so one call handles both decisions.
#[cfg(unix)]
#[test]
fn an_explicit_provider_still_names_its_job_with_the_model() {
    let fixture = CliFixture::new("explicit-job-name");
    let output = fixture.run(true);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    assert_ne!(
        fixture.classifier_name, fixture.name,
        "the derived name must differ, or this proves nothing"
    );
    assert_eq!(value["dispatch"]["job_name"], fixture.classifier_name);
    assert_eq!(
        fixture.classifier_calls(),
        1,
        "one naming call, and no second one"
    );
    assert_eq!(value["classification"]["complexity"], "medium");
    assert_eq!(value["provider"], "claude");
    assert_eq!(value["gates"], json!(["explicit_provider"]));
}

/// The title is cosmetic, so a naming call that answers nothing usable must cost the job nothing:
/// it dispatches under the derived name instead of failing or going unnamed.
#[cfg(unix)]
#[test]
fn an_unusable_title_leaves_an_explicit_job_on_its_derived_name() {
    for answer in [
        "I cannot name this task.",
        r#"{"job_name":"Renaming: background, sessions!"}"#,
        r#"{"job_name":"Rename Background Sessions"}"#,
    ] {
        let fixture = CliFixture::listing_agent_named("unusable-title", None);
        fixture.answers_with(answer);
        let output = fixture
            .run_command()
            .arg("--provider")
            .arg("claude")
            .arg("--json")
            .output()
            .expect("run router");
        assert!(
            output.status.success(),
            "answer {answer:?} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("router json");
        assert_eq!(
            value["dispatch"]["job_name"], fixture.name,
            "answer {answer:?} must leave the derived name in place"
        );
    }
}

/// A dry run dispatches nothing, but a provider only pin still needs classification for its model
/// and effort.
#[cfg(unix)]
#[test]
fn a_provider_only_dry_run_still_classifies_downstream_values() {
    let dry = CliFixture::new("skip-naming-dry-run");
    let output = dry
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--dry-run")
        .output()
        .expect("run router");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(dry.classifier_calls(), 1);
}

/// A classifier-reported connector miss cannot silently become a Claude dispatch: the configured
/// inventory establishes no provider capability for an absent connector.
#[cfg(unix)]
#[test]
fn an_unavailable_capability_is_reported_without_dispatching_claude() {
    let fixture = CliFixture::new("capability-blocked");
    fixture.answers_with(
        &json!({
            "orchestration": false,
            "missing_connector": true,
            "complexity": "medium",
            "task_context_horizon": "ordinary",
            "rationale": "requires unavailable service",
            "job_name": fixture.classifier_name,
        })
        .to_string(),
    );
    let output = fixture
        .run_command()
        .arg("--json")
        .output()
        .expect("run router");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    assert_eq!(
        value["capability_blocked"].as_str(),
        Some(
            "required capability is absent from the configured connector inventory; no provider was dispatched"
        )
    );
    assert_eq!(value["dispatch"], Value::Null);
    assert_eq!(value["dry_run"], false);
    assert!(
        value["gates"]
            .as_array()
            .is_some_and(|gates| gates.contains(&Value::String("capability_blocked".to_string())))
    );
    assert!(
        !fixture.spawn_log.exists(),
        "a capability block must never start the Claude job"
    );
}

/// Grok and OpenCode have no derived model or effort, so naming either provider must not disclose
/// the task to a different provider merely to compute values that will be discarded.
#[cfg(unix)]
#[test]
fn explicit_providers_without_derived_values_skip_classification() {
    for provider in ["grok", "opencode"] {
        let fixture = CliFixture::new(&format!("{provider}-no-classifier"));
        let output = fixture
            .run_command()
            .arg("--provider")
            .arg(provider)
            .arg("--dry-run")
            .arg("--json")
            .output()
            .expect("run router");
        assert!(
            output.status.success(),
            "{provider} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("router json");
        assert_eq!(value["provider"], provider);
        assert_eq!(value["model"], Value::Null);
        assert_eq!(value["effort"], Value::Null);
        assert_eq!(value["classification"], Value::Null);
        assert_eq!(fixture.classifier_calls(), 0, "provider {provider}");
    }
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
    assert!(
        matches!(ordinary["provider"].as_str(), Some("codex" | "claude")),
        "automatic routing must select only a capacity backed provider: {ordinary}"
    );

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

/// Provider names in help are a public contract. Omitting Grok here leaves an otherwise supported
/// explicit dispatch undiscoverable from the CLI itself.
#[cfg(unix)]
#[test]
fn run_help_lists_grok_as_an_explicit_provider() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-router"))
        .args(["run", "--help"])
        .output()
        .expect("run router help");
    assert!(
        output.status.success(),
        "run --help failed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("grok"),
        "run --help must list the Grok provider: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(unix)]
#[test]
fn auto_route_uses_the_classifier_generated_job_name() {
    let fixture = CliFixture::auto_claude_job("classifier-job-name", "RS-123 Input Box Searching");
    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("auto")
        .arg("--json")
        .output()
        .expect("run router");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");
    assert_eq!(value["dispatch"]["job_name"], "RS-123 Input Box Searching");
    assert_eq!(value["dispatch"]["job_id"], "claude exact id");
    assert_eq!(
        wait_for_text(&fixture.spawn_log)
            .lines()
            .collect::<Vec<_>>(),
        vec![
            "--bg",
            "--model",
            "opus[1m]",
            "--effort",
            "medium",
            "--name",
            "RS-123 Input Box Searching",
            &fixture.task
        ]
    );
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
    assert!(stdout.contains(&fixture.classifier_name), "{stdout}");
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
    let expected = [
        "--bg",
        "--model",
        "opus[1m]",
        "--effort",
        "medium",
        "--name",
        name,
        &fixture.task,
    ];
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

    assert_eq!(fixture.classifier_calls(), 1);
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
    assert_eq!(value["model"], "opus[1m]");
    assert_eq!(value["effort"], "medium");

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
        row["effort"], "medium",
        "the log must retain the effort the router decided"
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
        row["task_context_horizon"], "extended",
        "a provider only pin still records its routing classification"
    );
}

#[cfg(unix)]
#[test]
fn pinned_codex_maps_low_and_high_without_moving_provider() {
    for (label, task, complexity, model, effort) in [
        ("codex-low", "say hi", "low", "gpt-5.6-luna", "low"),
        (
            "codex-high",
            "/implement redesign the router architecture",
            "high",
            "gpt-5.6-sol",
            "high",
        ),
    ] {
        let fixture = CliFixture::new(label).with_task(task);
        fixture.answers_with_complexity(complexity);

        let output = fixture
            .run_command()
            .arg("--provider")
            .arg("codex")
            .arg("--dry-run")
            .arg("--json")
            .output()
            .expect("run router");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

        assert_eq!(value["provider"], "codex", "task {task:?}");
        assert_eq!(value["model"], model, "task {task:?}");
        assert_eq!(value["effort"], effort, "task {task:?}");
        assert_eq!(
            value["classification"]["complexity"], complexity,
            "task {task:?}"
        );
        assert_eq!(fixture.classifier_calls(), 1, "task {task:?}");
        assert!(!fixture.spawn_log.exists(), "dry run dispatched {task:?}");
    }
}

#[cfg(unix)]
#[test]
fn pinned_claude_classifies_but_stays_on_claude() {
    let fixture = CliFixture::new("claude-low").with_task("say hi");
    fixture.answers_with_complexity("low");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--dry-run")
        .arg("--json")
        .output()
        .expect("run router");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    assert_eq!(value["provider"], "claude");
    assert_eq!(value["model"], "sonnet");
    assert_eq!(value["effort"], "low");
    assert_eq!(value["classification"]["complexity"], "low");
    assert_eq!(fixture.classifier_calls(), 1);
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

/// A provider and model pin keeps the supplied model exact while classification derives effort.
#[cfg(unix)]
#[test]
fn provider_and_model_pins_preserve_model_and_derive_only_effort() {
    let name = "Pinned Model Job";
    let fixture = CliFixture::listing_agent_named("explicit-plus-model", Some(name));
    fixture.answers_with_complexity("high");

    let output = fixture
        .run_command()
        .arg("--provider")
        .arg("claude")
        .arg("--model")
        .arg("claude-custom-model")
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

    assert_eq!(value["provider"], "claude");
    assert_eq!(value["model"], "claude-custom-model");
    assert_eq!(value["effort"], "high");
    assert_eq!(value["classification"]["complexity"], "high");
    assert_eq!(fixture.classifier_calls(), 1);

    assert_eq!(
        wait_for_text(&fixture.spawn_log)
            .lines()
            .collect::<Vec<_>>(),
        vec![
            "--bg",
            "--model",
            "claude-custom-model",
            "--effort",
            "high",
            "--name",
            name,
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
    let classifier_log = root.path.join("classifier.argv");
    common::write_stub(
        &bin.join("claude"),
        &format!(
            "printf '%s\\n' \"$@\" >> {}\nexit 1\n",
            shell_quote(&classifier_log.to_string_lossy())
        ),
    );
    let grok_log = root.path.join("grok.argv");
    common::write_stub(
        &bin.join("grok"),
        &format!(
            "printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&grok_log.to_string_lossy())
        ),
    );
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
            .env("GROK_HOME", root.path.join("grok-home"))
            .env("GROK_USAGE_CACHE", root.path.join("grok-usage-cache.json"))
            .env("PATH", &path)
            .output()
            .expect("run router")
    };

    let config_arg = config.to_string_lossy().to_string();
    for provider in ["codex", "grok", "opencode"] {
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
        assert!(
            !classifier_log.exists(),
            "{provider} disclosed the rejected task to the classifier"
        );
        if provider == "grok" {
            assert!(
                !grok_log.exists(),
                "Grok was invoked before --mcp-config was refused"
            );
        }
    }

    for provider in ["grok", "opencode"] {
        let output = route(provider, &["--strict-mcp-config"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{provider} accepted --strict-mcp-config: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("--strict-mcp-config"),
            "{provider} did not name the rejected flag: {stderr}"
        );
        assert!(
            !stderr.contains("unexpected argument"),
            "{provider} does not accept --strict-mcp-config at all: {stderr}"
        );
        assert!(
            !classifier_log.exists(),
            "{provider} disclosed the rejected task to the classifier"
        );
        if provider == "grok" {
            assert!(
                !grok_log.exists(),
                "Grok was invoked before --strict-mcp-config was refused"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn explicit_grok_rejects_reasoning_effort_instead_of_logging_an_ignored_value() {
    let root = TempDir::new("grok-effort");
    let cwd = root.path.join("working");
    fs::create_dir_all(&cwd).expect("create cwd");

    let output = Command::new(env!("CARGO_BIN_EXE_agent-router"))
        .arg("run")
        .arg("review the router")
        .arg("--dir")
        .arg(&cwd)
        .arg("--provider")
        .arg("grok")
        .arg("--model")
        .arg("grok-4")
        .arg("--effort")
        .arg("high")
        .arg("--name")
        .arg("Grok Effort Rejection")
        .arg("--dry-run")
        .arg("--json")
        .env("HOME", root.path.join("home"))
        .env("GROK_HOME", root.path.join("grok-home"))
        .env("GROK_USAGE_CACHE", root.path.join("grok-usage-cache.json"))
        .output()
        .expect("run explicit Grok effort rejection");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Grok does not support --effort"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn explicit_grok_usage_honors_the_same_grok_home_as_its_lifecycle() {
    let root = TempDir::new("grok-home-usage");
    let home = root.path.join("home");
    let grok_home = root.path.join("custom-grok-home");
    let cwd = root.path.join("working");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(grok_home.join("logs")).expect("create Grok logs");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::write(
        grok_home.join("logs/unified.jsonl"),
        format!(
            "{}\n",
            json!({
                "msg": "billing: fetched credits config",
                "ctx": {
                    "subscriptionTier": "SuperGrok Plus",
                    "config": {
                        "creditUsagePercent": 37.5,
                        "currentPeriod": {
                            "type": "USAGE_PERIOD_TYPE_WEEKLY",
                            "end": "2099-01-07T00:00:00Z",
                        },
                    },
                },
            })
        ),
    )
    .expect("write Grok billing log");

    let output = Command::new(env!("CARGO_BIN_EXE_agent-router"))
        .arg("run")
        .arg("review the router")
        .arg("--dir")
        .arg(&cwd)
        .arg("--provider")
        .arg("grok")
        .arg("--model")
        .arg("grok-4")
        .arg("--name")
        .arg("Grok Home Usage")
        .arg("--dry-run")
        .arg("--json")
        .env("HOME", &home)
        .env("GROK_HOME", &grok_home)
        .env("GROK_USAGE_CACHE", root.path.join("grok-usage-cache.json"))
        .output()
        .expect("run explicit Grok dry run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");
    assert_eq!(value["usage"]["grok"]["weekly_pct"], 37.5);
    assert_eq!(value["usage"]["grok"]["weekly_capacity_known"], true);
    assert_eq!(value["usage"]["grok"]["stale"], false);
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
        .env("GROK_HOME", root.path.join("grok-home"))
        .env("GROK_USAGE_CACHE", root.path.join("grok-usage-cache.json"))
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
        .env("GROK_HOME", root.path.join("grok-home"))
        .env("GROK_USAGE_CACHE", root.path.join("grok-usage-cache.json"))
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
