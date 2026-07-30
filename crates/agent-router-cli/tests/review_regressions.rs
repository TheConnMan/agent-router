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
            "agent_router_review_regressions_{}_{}_{}",
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

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serialize fixture string")
}

fn write_empty_router_config(home: &Path) {
    write_file(
        &home.join(".config/agent-router/config.toml"),
        "[parity]\nroots = []\nexceptions = []\n",
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

fn run_parity(home: &Path, current_dir: &Path, root: &Path) -> Output {
    router_command(home, current_dir)
        .arg("parity")
        .arg("--root")
        .arg(root)
        .output()
        .expect("run agent-router parity")
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("command output is utf8")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit status\nstdout:\n{}\nstderr:\n{}",
        output_text(&output.stdout),
        output_text(&output.stderr)
    );
}

fn assert_no_raw_controls(text: &str, stream: &str) {
    for (offset, character) in text.char_indices() {
        assert!(
            !character.is_control() || character == '\n',
            "{stream} contains raw control U+{:04X} at byte {offset}: {text:?}",
            character as u32
        );
    }
}

fn assert_escaped_between(
    text: &str,
    prefix: &str,
    suffix: &str,
    codepoint: u32,
    short_escape: Option<&str>,
) {
    let text = text.to_ascii_lowercase();
    let prefix = prefix.to_ascii_lowercase();
    let suffix = suffix.to_ascii_lowercase();
    let mut escapes = vec![
        format!("\\u{{{codepoint:x}}}"),
        format!("\\u{codepoint:04x}"),
        format!("\\x{codepoint:02x}"),
    ];
    if let Some(short_escape) = short_escape {
        escapes.push(short_escape.to_string());
    }
    assert!(
        escapes
            .iter()
            .any(|escape| text.contains(&format!("{prefix}{escape}{suffix}"))),
        "missing escaped U+{codepoint:04X} between {prefix:?} and {suffix:?}: {text:?}"
    );
}

#[test]
fn malformed_codex_toml_does_not_echo_secret_source_text() {
    let tree = TempTree::new("malformed_secret");
    let home = tree.path().join("home");
    let project = tree.path().join("project");
    let secret = "PRIVATE_TOKEN_SENTINEL_7A91C3";
    let offending_line = format!(r#"env = {{ ACCESS_TOKEN = "{secret}" invalid }}"#);
    fs::create_dir_all(&home).expect("create home");
    write_empty_router_config(&home);
    write_file(
        &project.join(".codex/config.toml"),
        &format!("[mcp_servers.private]\ncommand = \"runner\"\n{offending_line}\n"),
    );

    let output = run_parity(&home, tree.path(), &project);

    assert_exit(&output, 2);
    let stdout = output_text(&output.stdout);
    let stderr = output_text(&output.stderr);
    assert!(
        stderr.contains("parity scan error"),
        "malformed project config did not reach the parity scan path: {stderr}"
    );
    for (stream_name, stream) in [("stdout", stdout), ("stderr", stderr)] {
        assert!(
            !stream.contains(secret),
            "{stream_name} leaked the malformed secret: {stream}"
        );
        assert!(
            !stream.contains(&offending_line),
            "{stream_name} leaked the offending source line: {stream}"
        );
    }
}

#[test]
fn human_parity_output_escapes_repository_fields_and_intentional_reasons() {
    let tree = TempTree::new("human_control_escaping");
    let home = tree.path().join("home");
    let root = tree.path().join("projects");
    let drift_project =
        root.join("path\u{1b}]8;;https://evil.invalid\u{7}link\u{1b}]8;;\u{7}closed\nnext");
    let intentional_project = root.join("intentional");
    let server = "server\u{1b}[31mred\u{1b}[0m\tend";
    let command = "runner\rreturn\nline";
    let reason = "approved\u{1b}]0;owned\u{7}bell\nsecond\rthird\tfourth";
    fs::create_dir_all(&home).expect("create home");

    write_file(
        &drift_project.join(".mcp.json"),
        &format!(
            "{{\"mcpServers\":{{{}:{{\"command\":{},\"args\":[]}}}}}}\n",
            json_string(server),
            json_string(command)
        ),
    );
    write_file(
        &intentional_project.join(".mcp.json"),
        r#"{"mcpServers":{"intentional":{"command":"claude-runner","args":[]}}}
"#,
    );
    write_file(
        &intentional_project.join(".codex/config.toml"),
        "[mcp_servers.intentional]\ncommand = \"codex-runner\"\nargs = []\n",
    );
    write_file(
        &home.join(".config/agent-router/config.toml"),
        &format!(
            "[parity]\nroots = []\n\n\
             [[parity.exceptions]]\n\
             path = {}\n\
             server = \"intentional\"\n\
             kind = \"command_differs\"\n\
             reason = {}\n",
            json_string(
                fs::canonicalize(&intentional_project)
                    .expect("canonical intentional project")
                    .to_string_lossy()
                    .as_ref()
            ),
            json_string(reason)
        ),
    );

    let output = run_parity(&home, tree.path(), &root);

    assert_exit(&output, 1);
    let stdout = output_text(&output.stdout);
    let stderr = output_text(&output.stderr);
    assert_no_raw_controls(&stdout, "stdout");
    assert_no_raw_controls(&stderr, "stderr");

    assert_escaped_between(&stdout, "path", "]8;;https", 0x1b, None);
    assert_escaped_between(&stdout, "evil.invalid", "link", 0x07, None);
    assert_escaped_between(&stdout, "closed", "next", 0x0a, Some("\\n"));
    assert_escaped_between(&stdout, "server", "[31mred", 0x1b, None);
    assert_escaped_between(&stdout, "[0m", "end", 0x09, Some("\\t"));
    assert_escaped_between(&stdout, "runner", "return", 0x0d, Some("\\r"));
    assert_escaped_between(&stdout, "return", "line", 0x0a, Some("\\n"));
    assert_escaped_between(&stdout, "approved", "]0;owned", 0x1b, None);
    assert_escaped_between(&stdout, "owned", "bell", 0x07, None);
    assert_escaped_between(&stdout, "bell", "second", 0x0a, Some("\\n"));
    assert_escaped_between(&stdout, "second", "third", 0x0d, Some("\\r"));
    assert_escaped_between(&stdout, "third", "fourth", 0x09, Some("\\t"));
}
