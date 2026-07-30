use serde_json::{Value, json};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("agent-router-cli-{}-{unique}", std::process::id()));
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
    fn new() -> Self {
        let root = TempDir::new();
        let home = root.path.join("home");
        let bin = root.path.join("bin");
        let cwd = root.path.join("working directory");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(&bin).expect("create bin");
        fs::create_dir_all(&cwd).expect("create cwd");
        let task = "雪".repeat(45);
        let name = task.chars().take(40).collect::<String>();
        let spawn_log = root.path.join("claude.argv");
        let agents = json!([{
            "id": "claude exact id",
            "sessionId": "claude full id",
            "cwd": cwd,
            "name": name,
            "startedAt": i64::MAX,
            "kind": "background",
            "state": "working"
        }]);
        let agents = serde_json::to_string(&agents).expect("agents json");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"agents\" ]; then\n\
               printf '%s\\n' {}\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&agents),
            shell_quote(&spawn_log.to_string_lossy())
        );
        let fake_claude = bin.join("claude");
        fs::write(&fake_claude, script).expect("write fake claude");
        let mut permissions = fs::metadata(&fake_claude)
            .expect("fake metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fake_claude, permissions).expect("make fake executable");
        Self {
            root,
            task,
            name,
            cwd,
            spawn_log,
        }
    }

    fn run(&self, json: bool) -> Output {
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
            .arg("run")
            .arg(&self.task)
            .arg("--dir")
            .arg(&self.cwd)
            .arg("--provider")
            .arg("claude")
            .env("HOME", home)
            .env("CODEX_SESSIONS_DIR", sessions)
            .env("PATH", path);
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
    let fixture = CliFixture::new();
    let output = fixture.run(true);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("router json");

    assert_eq!(value["provider"], "claude");
    assert_eq!(value["model"], "opus[1m]");
    // An explicit provider is unscored, so it runs at the standard complexity tier.
    assert_eq!(value["effort"], "high");
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
            "--effort",
            "high",
            "--name",
            &fixture.name,
            &fixture.task
        ]
    );
}

#[cfg(unix)]
#[test]
fn human_output_reports_the_job_without_a_viewer_instruction() {
    let fixture = CliFixture::new();
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

#[cfg(target_os = "linux")]
fn fake_opencode_cli(root: &Path, log: &Path) -> PathBuf {
    let binary = root.join("opencode");
    fs::write(
        &binary,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            shell_quote(&log.to_string_lossy())
        ),
    )
    .expect("write fake opencode");
    let mut permissions = fs::metadata(&binary).expect("fake metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&binary, permissions).expect("make fake executable");
    binary
}

#[cfg(target_os = "linux")]
#[test]
fn managed_opencode_security_failure_does_not_run_the_detached_cli() {
    let root = TempDir::new();
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
    let root = TempDir::new();
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
