use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agent_router_parity_cli_{}_{}_{}",
            std::process::id(),
            serial,
            label
        ));
        fs::create_dir_all(&path).expect("create isolated test directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(path, contents).expect("write fixture");
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonical fixture path")
        .to_string_lossy()
        .into_owned()
}

fn toml_path(path: &Path) -> String {
    serde_json::to_string(&canonical(path)).expect("quote fixture path")
}

fn write_router_config(home: &Path, contents: &str) {
    write_file(&home.join(".config/agent-router/config.toml"), contents);
}

fn write_configured_roots(home: &Path, roots: &[&Path]) {
    let roots = roots
        .iter()
        .map(|root| toml_path(root))
        .collect::<Vec<_>>()
        .join(", ");
    write_router_config(
        home,
        &format!("[parity]\nroots = [{roots}]\nexceptions = []\n"),
    );
}

fn write_empty_parity_config(home: &Path) {
    write_router_config(home, "[parity]\nroots = []\nexceptions = []\n");
}

fn write_intentional_exception(
    home: &Path,
    project: &Path,
    server: &str,
    kind: &str,
    reason: &str,
) {
    write_router_config(
        home,
        &format!(
            "[parity]\nroots = []\n\n\
             [[parity.exceptions]]\n\
             path = {}\n\
             server = {server:?}\n\
             kind = {kind:?}\n\
             reason = {reason:?}\n",
            toml_path(project)
        ),
    );
}

fn write_aligned_project(project: &Path) {
    write_file(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "shared": {
      "command": "runner",
      "args": ["serve", "--safe"]
    }
  }
}
"#,
    );
    write_file(
        &project.join(".codex/config.toml"),
        r#"[mcp_servers.shared]
command = "runner"
args = ["serve", "--safe"]
"#,
    );
}

fn write_missing_in_codex_project(project: &Path, server: &str) {
    write_file(
        &project.join(".mcp.json"),
        &format!(
            r#"{{
  "mcpServers": {{
    "{server}": {{
      "command": "runner",
      "args": ["serve"]
    }}
  }}
}}
"#
        ),
    );
}

fn write_standalone_claude_project(project: &Path) {
    write_file(
        &project.join("CLAUDE.md"),
        "# Project instructions\n\nClaude specific instructions.\n",
    );
}

fn write_command_difference(project: &Path) {
    write_file(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "build": {
      "command": "claude-runner",
      "args": ["serve"]
    }
  }
}
"#,
    );
    write_file(
        &project.join(".codex/config.toml"),
        r#"[mcp_servers.build]
command = "codex-runner"
args = ["serve"]
"#,
    );
}

fn write_command_difference_with_values(project: &Path, claude_value: &str, codex_value: &str) {
    write_file(
        &project.join(".mcp.json"),
        &format!(
            r#"{{
  "mcpServers": {{
    "private": {{
      "command": "claude-runner",
      "args": ["serve"],
      "env": {{
        "CREDENTIAL": "{claude_value}"
      }}
    }}
  }}
}}
"#
        ),
    );
    write_file(
        &project.join(".codex/config.toml"),
        &format!(
            r#"[mcp_servers.private]
command = "codex-runner"
args = ["serve"]
env = {{ CREDENTIAL = "{codex_value}" }}
"#
        ),
    );
}

fn router_command(home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
    command
        .current_dir(current_dir)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

fn run_parity(home: &Path, current_dir: &Path, roots: &[&Path], json: bool) -> Output {
    let mut command = router_command(home, current_dir);
    command.arg("parity");
    for root in roots {
        command.arg("--root").arg(root);
    }
    if json {
        command.arg("--json");
    }
    command.output().expect("run agent-router parity")
}

fn diagnostic(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit status\n{}",
        diagnostic(output)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is utf8")
}

fn parse_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON: {error}\n{}", diagnostic(output)))
}

fn json_contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(actual) => actual == expected,
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| json_contains_string(value, expected)),
        _ => false,
    }
}

fn assert_path_order(text: &str, first: &str, second: &str) {
    let first_offset = text
        .find(first)
        .unwrap_or_else(|| panic!("missing first path {first:?} in {text:?}"));
    let second_offset = text
        .find(second)
        .unwrap_or_else(|| panic!("missing second path {second:?} in {text:?}"));
    assert!(
        first_offset < second_offset,
        "paths are not sorted: {first:?} must precede {second:?}\n{text}"
    );
}

#[test]
fn cli_roots_override_configured_roots() {
    let tree = TempTree::new("cli_root_precedence");
    let home = tree.path().join("home");
    let configured_project = tree.path().join("configured_project");
    let cli_project = tree.path().join("cli_project");
    fs::create_dir_all(&home).expect("create home");
    write_standalone_claude_project(&configured_project);
    write_aligned_project(&cli_project);
    write_configured_roots(&home, &[&configured_project]);

    let output = run_parity(&home, tree.path(), &[&cli_project], false);

    assert_exit(&output, 0);
    let report = stdout(&output);
    assert!(
        report.to_ascii_lowercase().contains("aligned"),
        "{}",
        diagnostic(&output)
    );
    assert!(report.contains(&canonical(&cli_project)), "{report}");
    assert!(
        !report.contains(&canonical(&configured_project)),
        "configured root leaked into CLI scoped report\n{report}"
    );
}

#[test]
fn configured_roots_are_used_when_no_cli_root_is_supplied() {
    let tree = TempTree::new("configured_root");
    let home = tree.path().join("home");
    let configured_project = tree.path().join("configured_project");
    fs::create_dir_all(&home).expect("create home");
    write_missing_in_codex_project(&configured_project, "configured");
    write_configured_roots(&home, &[&configured_project]);

    let output = run_parity(&home, tree.path(), &[], false);

    assert_exit(&output, 1);
    let report = stdout(&output);
    assert!(report.contains(&canonical(&configured_project)), "{report}");
    assert!(report.contains("configured"), "{report}");
}

#[test]
fn current_directory_is_the_fallback_when_no_roots_are_configured() {
    let tree = TempTree::new("current_directory_fallback");
    let home = tree.path().join("home");
    let project = tree.path().join("working_project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_standalone_claude_project(&project);

    let output = run_parity(&home, &project, &[], false);

    assert_exit(&output, 1);
    let report = stdout(&output);
    assert!(report.contains(&canonical(&project)), "{report}");
    assert!(
        report.to_ascii_lowercase().contains("standalone"),
        "{report}"
    );
}

#[test]
fn aligned_human_and_json_reports_are_deterministic_and_equivalent() {
    let tree = TempTree::new("deterministic_aligned");
    let home = tree.path().join("home");
    let root = tree.path().join("projects");
    let first_project = root.join("alpha");
    let second_project = root.join("nested/zulu");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_aligned_project(&second_project);
    write_aligned_project(&first_project);

    let human_first = run_parity(&home, tree.path(), &[&root], false);
    let human_second = run_parity(&home, tree.path(), &[&root], false);
    let json_first = run_parity(&home, tree.path(), &[&root], true);
    let json_second = run_parity(&home, tree.path(), &[&root], true);

    for output in [&human_first, &human_second, &json_first, &json_second] {
        assert_exit(output, 0);
    }
    assert_eq!(human_first.stdout, human_second.stdout);
    assert_eq!(human_first.stderr, human_second.stderr);
    assert_eq!(json_first.stdout, json_second.stdout);
    assert_eq!(json_first.stderr, json_second.stderr);

    let first_path = canonical(&first_project);
    let second_path = canonical(&second_project);
    let human = stdout(&human_first);
    let json_text = stdout(&json_first);
    let json = parse_json(&json_first);
    assert!(human.to_ascii_lowercase().contains("aligned"), "{human}");
    assert!(
        json_contains_string(&json, "aligned"),
        "JSON report lacks aligned status\n{json_text}"
    );
    assert_path_order(&human, &first_path, &second_path);
    assert_path_order(&json_text, &first_path, &second_path);
}

#[test]
fn intentional_difference_is_visible_with_its_reason_and_exits_zero() {
    let tree = TempTree::new("intentional");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    let reason = "Different launchers are required by the local build";
    fs::create_dir_all(&home).expect("create home");
    write_command_difference(&project);
    write_intentional_exception(&home, &project, "build", "command_differs", reason);

    let human = run_parity(&home, tree.path(), &[&project], false);
    let json = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&human, 0);
    assert_exit(&json, 0);
    let human_report = stdout(&human);
    let json_report = parse_json(&json);
    assert!(human_report.contains(reason), "{human_report}");
    assert!(
        human_report.to_ascii_lowercase().contains("intentional"),
        "{human_report}"
    );
    assert!(
        json_contains_string(&json_report, "intentional"),
        "{}",
        diagnostic(&json)
    );
    assert!(json_contains_string(&json_report, "command_differs"));
    assert!(json_contains_string(&json_report, reason));
}

#[test]
fn unmatched_missing_server_drift_exits_one() {
    let tree = TempTree::new("unmatched_drift");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_missing_in_codex_project(&project, "claude_only");

    let human = run_parity(&home, tree.path(), &[&project], false);
    let json = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&human, 1);
    assert_exit(&json, 1);
    let human_report = stdout(&human);
    let json_report = parse_json(&json);
    assert!(human_report.contains("claude_only"), "{human_report}");
    assert!(
        human_report.to_ascii_lowercase().contains("drift"),
        "{human_report}"
    );
    assert!(json_contains_string(&json_report, "drift"));
    assert!(json_contains_string(&json_report, "missing_in_codex"));
    assert!(json_contains_string(&json_report, "claude_only"));
}

#[test]
fn malformed_project_config_is_a_parity_scan_error_with_exit_two() {
    let tree = TempTree::new("malformed_project");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_file(
        &project.join(".codex/config.toml"),
        "[mcp_servers.broken\ncommand = \"runner\"\n",
    );

    let output = run_parity(&home, tree.path(), &[&project], false);

    assert_exit(&output, 2);
    let error = diagnostic(&output);
    assert!(
        error.contains(".codex/config.toml"),
        "scan error lacks project file context\n{error}"
    );
    assert!(!error.contains("unrecognized subcommand"), "{error}");
}

#[test]
fn malformed_router_config_is_a_parity_configuration_error_with_exit_two() {
    let tree = TempTree::new("malformed_router");
    let home = tree.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    write_router_config(&home, "hard_ceiling_pct = \"not a number\"\n");

    let output = run_parity(&home, tree.path(), &[], false);

    assert_exit(&output, 2);
    let error = diagnostic(&output);
    assert!(error.contains("hard_ceiling_pct"), "{error}");
    assert!(!error.contains("unrecognized subcommand"), "{error}");
}

#[test]
fn invalid_run_provider_preserves_the_existing_exit_one_contract() {
    let tree = TempTree::new("invalid_run");
    let home = tree.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let output = router_command(&home, tree.path())
        .args([
            "run",
            "do not dispatch",
            "--provider",
            "not-a-provider",
            "--dry-run",
        ])
        .output()
        .expect("run agent-router");

    assert_exit(&output, 1);
    assert!(
        diagnostic(&output).contains("unknown provider"),
        "{}",
        diagnostic(&output)
    );
}

#[test]
fn recursive_nested_projects_are_reported_once_in_sorted_order() {
    let tree = TempTree::new("recursive");
    let home = tree.path().join("home");
    let root = tree.path().join("projects");
    let first_project = root.join("alpha");
    let second_project = root.join("nested/zulu");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_missing_in_codex_project(&second_project, "zulu_server");
    write_standalone_claude_project(&first_project);

    let human = run_parity(&home, tree.path(), &[&root, &first_project], false);
    let json = run_parity(&home, tree.path(), &[&root, &first_project], true);

    assert_exit(&human, 1);
    assert_exit(&json, 1);
    let first_path = canonical(&first_project);
    let second_path = canonical(&second_project);
    let human_report = stdout(&human);
    let json_report = stdout(&json);
    assert_path_order(&human_report, &first_path, &second_path);
    assert_path_order(&json_report, &first_path, &second_path);
    assert_eq!(
        human_report.matches(&first_path).count(),
        1,
        "overlapping roots duplicated a project\n{human_report}"
    );
    assert_eq!(
        json_report.matches(&first_path).count(),
        1,
        "overlapping roots duplicated a project\n{json_report}"
    );
}

#[test]
fn environment_values_are_excluded_from_human_and_json_differences() {
    let tree = TempTree::new("secret_exclusion");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    let claude_value = "CLAUDE_SECRET_SENTINEL_2718";
    let codex_value = "CODEX_SECRET_SENTINEL_3141";
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_command_difference_with_values(&project, claude_value, codex_value);

    let human = run_parity(&home, tree.path(), &[&project], false);
    let json = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&human, 1);
    assert_exit(&json, 1);
    for output in [&human, &json] {
        let report = diagnostic(output);
        assert!(
            report.contains("CREDENTIAL"),
            "safe environment key is missing from the rendered projection\n{report}"
        );
        assert!(
            !report.contains(claude_value),
            "Claude environment value leaked\n{report}"
        );
        assert!(
            !report.contains(codex_value),
            "Codex environment value leaked\n{report}"
        );
    }
}

/// One server name paired with the kind reported for it, so a difference set can be compared
/// without spelling the tuple out at every call site.
type ServerKind<'a> = (&'a str, &'a str);

fn write_global_claude(home: &Path, contents: &str) {
    write_file(&home.join(".claude.json"), contents);
}

fn write_global_codex(home: &Path, contents: &str) {
    write_file(&home.join(".codex/config.toml"), contents);
}

/// The two absolute paths the global entry names, derived from the canonical home so a symlinked
/// temp directory does not make the assertion disagree with the report.
fn global_paths(home: &Path) -> (String, String) {
    let home = PathBuf::from(canonical(home));
    (
        home.join(".claude.json").to_string_lossy().into_owned(),
        home.join(".codex/config.toml")
            .to_string_lossy()
            .into_owned(),
    )
}

fn global_entry(report: &Value) -> &Value {
    report
        .get("global")
        .unwrap_or_else(|| panic!("parity JSON lacks a global entry: {report}"))
}

fn difference_kinds(entry: &Value) -> Vec<ServerKind<'_>> {
    entry
        .get("differences")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("entry lacks a differences array: {entry}"))
        .iter()
        .map(|difference| {
            (
                difference["server"].as_str().unwrap_or("(no server)"),
                difference["kind"].as_str().unwrap_or("(no kind)"),
            )
        })
        .collect()
}

fn project_entry<'a>(report: &'a Value, root: &str) -> &'a Value {
    report["projects"]
        .as_array()
        .unwrap_or_else(|| panic!("parity JSON lacks a projects array: {report}"))
        .iter()
        .find(|project| project["root"].as_str() == Some(root))
        .unwrap_or_else(|| panic!("no project entry for {root}: {report}"))
}

/// The global entry names both files and carries its own status. The key names asserted here are
/// the contract the human and JSON renderings are read through: `claude_path`, `codex_path`,
/// `status`, and `differences`.
#[test]
fn parity_json_includes_a_global_entry() {
    let tree = TempTree::new("global_entry");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_global_claude(
        &home,
        r#"{
  "mcpServers": {
    "shared": {
      "command": "runner",
      "args": ["serve"]
    }
  }
}
"#,
    );
    write_global_codex(
        &home,
        r#"[mcp_servers.shared]
command = "runner"
args = ["serve"]
"#,
    );
    write_aligned_project(&project);

    let output = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&output, 0);
    let report = parse_json(&output);
    let global = global_entry(&report);
    let (claude_path, codex_path) = global_paths(&home);
    assert_eq!(
        global["claude_path"].as_str(),
        Some(claude_path.as_str()),
        "{}",
        diagnostic(&output)
    );
    assert_eq!(
        global["codex_path"].as_str(),
        Some(codex_path.as_str()),
        "{}",
        diagnostic(&output)
    );
    assert_eq!(
        global["status"].as_str(),
        Some("aligned"),
        "{}",
        diagnostic(&output)
    );
    assert_eq!(difference_kinds(global), Vec::new());
}

/// The global comparison can move the exit code with no project difference anywhere in the scan,
/// which is the whole point of folding both vectors into the report status.
#[test]
fn a_global_only_difference_is_drift_and_exits_one() {
    let tree = TempTree::new("global_only_drift");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_global_claude(
        &home,
        r#"{"mcpServers":{"global_claude_only":{"command":"runner"}}}"#,
    );
    write_aligned_project(&project);

    let output = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&output, 1);
    let report = parse_json(&output);
    assert_eq!(
        report["status"].as_str(),
        Some("drift"),
        "{}",
        diagnostic(&output)
    );
    assert_eq!(
        difference_kinds(global_entry(&report)),
        vec![("global_claude_only", "missing_in_codex")]
    );
    for project_entry in report["projects"]
        .as_array()
        .expect("parity JSON lacks a projects array")
    {
        assert_eq!(
            difference_kinds(project_entry),
            Vec::new(),
            "a global difference was attributed to a project\n{}",
            diagnostic(&output)
        );
    }
}

/// An operator excepts a global difference by recording the home directory as the exception path,
/// because that is the root a global difference is reported under.
#[test]
fn a_global_difference_covered_by_a_home_scoped_exception_exits_zero() {
    let tree = TempTree::new("global_intentional");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    let reason = "the global claude server is deliberately not mirrored into codex";
    fs::create_dir_all(&home).expect("create home");
    write_global_claude(
        &home,
        r#"{"mcpServers":{"global_claude_only":{"command":"runner"}}}"#,
    );
    write_intentional_exception(
        &home,
        &home,
        "global_claude_only",
        "missing_in_codex",
        reason,
    );
    write_aligned_project(&project);

    let human = run_parity(&home, tree.path(), &[&project], false);
    let json = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&human, 0);
    assert_exit(&json, 0);
    let report = parse_json(&json);
    let global = global_entry(&report);
    assert_eq!(
        report["status"].as_str(),
        Some("intentional"),
        "{}",
        diagnostic(&json)
    );
    assert_eq!(
        global["status"].as_str(),
        Some("intentional"),
        "{}",
        diagnostic(&json)
    );
    assert_eq!(
        global["differences"][0]["intentional_reason"].as_str(),
        Some(reason),
        "{}",
        diagnostic(&json)
    );
    assert!(stdout(&human).contains(reason), "{}", diagnostic(&human));
}

#[test]
fn an_unparseable_global_claude_json_is_a_scan_error_with_exit_two() {
    let tree = TempTree::new("global_malformed_claude");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_global_claude(&home, "{");
    write_aligned_project(&project);

    let output = run_parity(&home, tree.path(), &[&project], false);

    assert_exit(&output, 2);
    let error = diagnostic(&output);
    let (claude_path, _) = global_paths(&home);
    assert!(
        error.contains(&claude_path),
        "scan error lacks the offending global file\n{error}"
    );
}

#[test]
fn an_unparseable_global_codex_config_is_a_scan_error_with_exit_two() {
    let tree = TempTree::new("global_malformed_codex");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_global_codex(&home, "[mcp_servers.broken\ncommand = \"runner\"\n");
    write_aligned_project(&project);

    let output = run_parity(&home, tree.path(), &[&project], false);

    assert_exit(&output, 2);
    let error = diagnostic(&output);
    let (_, codex_path) = global_paths(&home);
    assert!(
        error.contains(&codex_path),
        "scan error lacks the offending global file\n{error}"
    );
}

/// `~/.claude.json` carries real MCP credentials, so the global projection must expose environment
/// keys and never environment values, exactly as the project scoped projection already does.
#[test]
fn global_environment_values_are_excluded_from_human_and_json_output() {
    let tree = TempTree::new("global_secret_exclusion");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    let claude_value = "GLOBAL_CLAUDE_SECRET_SENTINEL_2718";
    let codex_value = "GLOBAL_CODEX_SECRET_SENTINEL_3141";
    fs::create_dir_all(&home).expect("create home");
    write_empty_parity_config(&home);
    write_global_claude(
        &home,
        &format!(
            r#"{{
  "mcpServers": {{
    "private": {{
      "command": "claude-runner",
      "args": ["serve"],
      "env": {{
        "CREDENTIAL": "{claude_value}"
      }}
    }}
  }}
}}
"#
        ),
    );
    write_global_codex(
        &home,
        &format!(
            r#"[mcp_servers.private]
command = "codex-runner"
args = ["serve"]
env = {{ CREDENTIAL = "{codex_value}" }}
"#
        ),
    );
    write_aligned_project(&project);

    let human = run_parity(&home, tree.path(), &[&project], false);
    let json = run_parity(&home, tree.path(), &[&project], true);

    assert_exit(&human, 1);
    assert_exit(&json, 1);
    for output in [&human, &json] {
        let report = diagnostic(output);
        assert!(
            report.contains("CREDENTIAL"),
            "safe environment key is missing from the rendered global projection\n{report}"
        );
        assert!(
            !report.contains(claude_value),
            "global Claude environment value leaked\n{report}"
        );
        assert!(
            !report.contains(codex_value),
            "global Codex environment value leaked\n{report}"
        );
    }
}

/// `.codex/config.toml` is one of the discovery markers, so a scan rooted at or above `$HOME` makes
/// `$HOME` itself a project candidate whose root equals the global entry's root. This pins the
/// accepted consequence rather than asserting machinery that separates them: the two stay distinct
/// entries because they live in separate vectors and compare different Claude side files, and one
/// home scoped exception covers both because `push_difference` matches on root.
#[test]
fn home_as_a_project_candidate_and_the_global_entry_are_reported_separately() {
    let tree = TempTree::new("global_home_overlap");
    let home = tree.path().join("home");
    let reason = "the home directory is deliberately asymmetric";
    fs::create_dir_all(&home).expect("create home");
    write_global_claude(
        &home,
        r#"{"mcpServers":{"global_only":{"command":"runner"}}}"#,
    );
    write_global_codex(&home, "[mcp_servers.codex_global]\ncommand = \"runner\"\n");
    write_empty_parity_config(&home);

    let drift = run_parity(&home, tree.path(), &[&home], true);

    assert_exit(&drift, 1);
    let report = parse_json(&drift);
    let home_path = canonical(&home);
    let mut global_kinds = difference_kinds(global_entry(&report));
    global_kinds.sort_unstable();
    assert_eq!(
        global_kinds,
        vec![
            ("codex_global", "missing_in_claude"),
            ("global_only", "missing_in_codex"),
        ],
        "{}",
        diagnostic(&drift)
    );
    assert_eq!(
        difference_kinds(project_entry(&report, &home_path)),
        vec![("codex_global", "missing_in_claude")],
        "the home project entry compares .mcp.json, not .claude.json\n{}",
        diagnostic(&drift)
    );

    write_router_config(
        &home,
        &format!(
            "[parity]\nroots = []\n\n\
             [[parity.exceptions]]\n\
             path = {}\n\
             reason = {reason:?}\n",
            toml_path(&home)
        ),
    );

    let excepted = run_parity(&home, tree.path(), &[&home], true);

    assert_exit(&excepted, 0);
    let report = parse_json(&excepted);
    assert_eq!(
        report["status"].as_str(),
        Some("intentional"),
        "{}",
        diagnostic(&excepted)
    );
    assert_eq!(
        global_entry(&report)["status"].as_str(),
        Some("intentional"),
        "one home scoped exception must cover the global entry\n{}",
        diagnostic(&excepted)
    );
    assert_eq!(
        project_entry(&report, &home_path)["status"].as_str(),
        Some("intentional"),
        "the same exception must cover the home directory as a project\n{}",
        diagnostic(&excepted)
    );
}
