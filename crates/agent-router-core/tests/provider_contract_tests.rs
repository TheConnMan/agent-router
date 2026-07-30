#[cfg(target_os = "linux")]
use agent_router_core::decide::decide_explicit;
use agent_router_core::dispatch::claude::dispatch_with_binary;
use agent_router_core::dispatch::codex::{
    CodexRpc, SpawnAttempt, parse_thread_id, spawn_on_initialized_rpc, thread_start_request,
    turn_start_request,
};
#[cfg(target_os = "linux")]
use agent_router_core::dispatch::dispatch as dispatch_decision;
use agent_router_core::dispatch::opencode::ManagedClient;
#[cfg(target_os = "linux")]
use agent_router_core::run::Request;
use agent_router_core::runtime::truncated_title;
use agent_router_core::{Error, Result};
#[cfg(target_os = "linux")]
use agent_router_core::{Provider, UsageSnapshot};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
#[cfg(target_os = "linux")]
use std::{ffi::OsString, os::unix::net::UnixListener, sync::Mutex};

#[test]
fn router_build_inputs_are_independent_of_the_sibling_viewer() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = core
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let inputs = [
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        core.join("Cargo.toml"),
        workspace.join("crates/agent-router-cli/Cargo.toml"),
    ];

    for input in inputs {
        let text = fs::read_to_string(&input).expect("read build input");
        assert!(
            !text.contains("agent-viewer"),
            "{} still couples the standalone router to agent-viewer",
            input.display()
        );
    }
}

#[test]
fn unicode_job_titles_are_forty_characters_without_splitting_a_scalar() {
    let task =
        "雪かきの準備をしてから温かい飲み物を用意する作業を詳しく確認して必要な変更を実施する";
    let title = truncated_title(task);

    assert_eq!(title.chars().count(), 40);
    assert_eq!(title, task.chars().take(40).collect::<String>());
    assert!(task.starts_with(&title));
}

#[test]
fn codex_requests_pin_security_posture_and_put_effort_on_the_turn() {
    let cwd = Path::new("/tmp/work tree");
    let start: Value =
        serde_json::from_str(&thread_start_request(2, cwd, None)).expect("thread request");
    assert_eq!(start["jsonrpc"], "2.0");
    assert_eq!(start["id"], 2);
    assert_eq!(start["method"], "thread/start");
    assert_eq!(start["params"]["cwd"], "/tmp/work tree");
    assert_eq!(start["params"]["sandbox"], "danger-full-access");
    assert_eq!(start["params"]["approvalPolicy"], "never");
    assert!(start["params"].get("model").is_none());

    let turn: Value = serde_json::from_str(&turn_start_request(
        3,
        "thread exact",
        "fix the queue",
        Some("xhigh"),
    ))
    .expect("turn request");
    assert_eq!(turn["jsonrpc"], "2.0");
    assert_eq!(turn["id"], 3);
    assert_eq!(turn["method"], "turn/start");
    assert_eq!(turn["params"]["threadId"], "thread exact");
    assert_eq!(
        turn["params"]["input"],
        json!([{"type": "text", "text": "fix the queue"}])
    );
    assert_eq!(turn["params"]["effort"], "xhigh");
}

#[test]
fn codex_thread_identity_rejects_malformed_errors_and_other_response_ids() {
    assert_eq!(
        parse_thread_id(
            r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread exact"}}}"#,
            2
        )
        .as_deref(),
        Some("thread exact")
    );
    assert_eq!(
        parse_thread_id(
            r#"{"jsonrpc":"2.0","id":9,"result":{"thread":{"id":"wrong"}}}"#,
            2
        ),
        None
    );
    assert_eq!(
        parse_thread_id(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"failed"}}"#,
            2
        ),
        None
    );
    assert_eq!(
        parse_thread_id(
            r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":""}}}"#,
            2
        ),
        None
    );
    assert_eq!(parse_thread_id("not json", 2), None);
}

#[derive(Default)]
struct ScriptedRpc {
    replies: VecDeque<Result<String>>,
    requests: Vec<Value>,
}

impl ScriptedRpc {
    fn with_replies(replies: Vec<Result<String>>) -> Self {
        Self {
            replies: replies.into(),
            requests: Vec::new(),
        }
    }
}

impl CodexRpc for ScriptedRpc {
    fn request(&mut self, request_id: i64, request: &str) -> Result<String> {
        let value: Value = serde_json::from_str(request).expect("valid request");
        assert_eq!(value["id"], request_id);
        self.requests.push(value);
        self.replies.pop_front().expect("scripted reply")
    }
}

#[test]
fn codex_partial_creation_is_one_visible_failure_without_a_retry() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Ok(r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread partial"}}}"#.to_string()),
        Err(Error::Command("turn rejected".to_string())),
    ]);

    let attempt = spawn_on_initialized_rpc(
        &mut rpc,
        Path::new("/tmp"),
        "perform one task",
        None,
        Some("xhigh"),
    );

    match attempt {
        SpawnAttempt::TurnFailed { thread_id, error } => {
            assert_eq!(thread_id, "thread partial");
            assert!(error.to_string().contains("turn rejected"));
        }
        other => panic!("expected the existing thread to remain visible, got {other:?}"),
    }
    assert_eq!(rpc.requests.len(), 2);
    assert_eq!(rpc.requests[0]["method"], "thread/start");
    assert_eq!(rpc.requests[1]["method"], "turn/start");
    assert!(rpc.replies.is_empty());
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-{label}-{}-{unique}",
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
fn fake_claude(root: &Path, listing: &Value) -> (PathBuf, PathBuf) {
    let binary = root.join("claude");
    let log = root.join("spawn.argv");
    let listing = serde_json::to_string(listing).expect("listing json");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"agents\" ]; then\n\
           printf '%s\\n' {}\n\
           exit 0\n\
         fi\n\
         printf '%s\\n' \"$@\" > {}\n",
        shell_quote(&listing),
        shell_quote(&log.to_string_lossy())
    );
    fs::write(&binary, script).expect("write fake claude");
    let mut permissions = fs::metadata(&binary).expect("fake metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&binary, permissions).expect("make fake executable");
    (binary, log)
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
fn claude_uses_background_argv_and_excludes_a_prior_same_name_and_cwd() {
    let root = TempDir::new("claude");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let task = "雪".repeat(45);
    let name = truncated_title(&task);
    let listing = json!([
        {
            "id": "old",
            "sessionId": "full old",
            "cwd": cwd,
            "name": name,
            "startedAt": 1,
            "kind": "background",
            "state": "working"
        },
        {
            "id": "wrong directory",
            "sessionId": "full wrong",
            "cwd": root.path.join("other"),
            "name": name,
            "startedAt": i64::MAX,
            "kind": "background",
            "state": "working"
        },
        {
            "id": "new",
            "sessionId": "full new",
            "cwd": cwd,
            "name": name,
            "startedAt": i64::MAX,
            "kind": "background",
            "state": "working"
        }
    ]);
    let (binary, log) = fake_claude(&root.path, &listing);

    let dispatch = dispatch_with_binary(
        &binary,
        &cwd,
        &task,
        &name,
        Some("opus[1m]"),
        None,
        Duration::from_millis(250),
    )
    .expect("claude dispatch");

    assert_eq!(dispatch.job_id.as_deref(), Some("new"));
    assert_eq!(dispatch.job_name, name);
    assert_eq!(
        wait_for_text(&log).lines().collect::<Vec<_>>(),
        vec!["--bg", "--model", "opus[1m]", "--name", &name, &task]
    );
}

/// A claude job runs at the decided effort, which is what makes the trivial tier cheaper rather
/// than only differently modelled. Deleting the `--effort` argv lines fails this test.
#[cfg(unix)]
#[test]
fn claude_argv_carries_the_decided_effort_and_omits_the_flag_without_one() {
    for effort in [Some("low"), None] {
        let root = TempDir::new("claude-effort");
        let cwd = root.path.join("working");
        fs::create_dir(&cwd).expect("create cwd");
        let task = "route one trivial task";
        let name = truncated_title(task);
        let (binary, log) = fake_claude(&root.path, &json!([]));

        dispatch_with_binary(
            &binary,
            &cwd,
            task,
            &name,
            Some("sonnet"),
            effort,
            Duration::from_millis(25),
        )
        .expect("claude dispatch");

        let argv = wait_for_text(&log);
        let argv = argv.lines().collect::<Vec<_>>();
        let expected: Vec<&str> = match effort {
            Some(effort) => vec![
                "--bg", "--model", "sonnet", "--effort", effort, "--name", &name, task,
            ],
            None => vec!["--bg", "--model", "sonnet", "--name", &name, task],
        };
        assert_eq!(argv, expected, "argv for effort {effort:?}");
    }
}

#[cfg(unix)]
#[test]
fn claude_running_job_keeps_its_name_when_the_id_is_not_yet_published() {
    let root = TempDir::new("claude-null-id");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let task = "repeat the same task";
    let name = truncated_title(task);
    let listing = json!([{
        "id": "prior",
        "sessionId": "full prior",
        "cwd": cwd,
        "name": name,
        "startedAt": 1,
        "kind": "background",
        "state": "working"
    }]);
    let (binary, _) = fake_claude(&root.path, &listing);

    let dispatch = dispatch_with_binary(
        &binary,
        &cwd,
        task,
        &name,
        Some("opus[1m]"),
        None,
        Duration::from_millis(25),
    )
    .expect("the background job is running even before its id is listed");

    assert_eq!(dispatch.job_id, None);
    assert_eq!(dispatch.job_name, name);
}

/// The caller owns the job name. bonus-drain names its jobs "Bonus: <id>" and reconciles inflight
/// work by matching that exact string against `claude agents --json`, so re-deriving or truncating
/// the name here orphans a running job.
#[cfg(unix)]
#[test]
fn claude_spawns_and_finds_the_job_under_the_caller_supplied_name_verbatim() {
    let root = TempDir::new("claude-supplied-name");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let task = "drain the bonus backlog entry and report every file it changed";
    let name = "Bonus: abc-123";
    assert_ne!(
        truncated_title(task),
        name,
        "the task must not derive this name"
    );
    let listing = json!([{
        "id": "supplied name job",
        "sessionId": "full supplied name job",
        "cwd": cwd,
        "name": name,
        "startedAt": i64::MAX,
        "kind": "background",
        "state": "working"
    }]);
    let (binary, log) = fake_claude(&root.path, &listing);

    let dispatch = dispatch_with_binary(
        &binary,
        &cwd,
        task,
        name,
        Some("opus[1m]"),
        None,
        Duration::from_millis(250),
    )
    .expect("claude dispatch");

    assert_eq!(dispatch.job_name, name);
    assert_eq!(dispatch.job_id.as_deref(), Some("supplied name job"));
    assert_eq!(
        wait_for_text(&log).lines().collect::<Vec<_>>(),
        vec!["--bg", "--model", "opus[1m]", "--name", name, task]
    );
}

#[cfg(target_os = "linux")]
struct PathGuard {
    prior: Option<OsString>,
}

#[cfg(target_os = "linux")]
impl PathGuard {
    fn prepend(path: &Path) -> Self {
        let prior = std::env::var_os("PATH");
        let mut paths = vec![path.to_path_buf()];
        paths.extend(std::env::split_paths(prior.as_deref().unwrap_or_default()));
        let joined = std::env::join_paths(paths).expect("join PATH");
        unsafe {
            std::env::set_var("PATH", joined);
        }
        Self { prior }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PathGuard {
    fn drop(&mut self) {
        unsafe {
            match self.prior.take() {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn codex_decision_effort_reaches_turn_start_at_the_dispatch_boundary() {
    static ENVIRONMENT: Mutex<()> = Mutex::new(());
    let _environment = ENVIRONMENT.lock().expect("environment lock");
    let root = TempDir::new("codex-dispatch");
    let socket_path = root.path.join("app-server.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind app server socket");
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept app server client");
        let mut socket = tungstenite::accept(stream).expect("accept websocket");
        let mut requests = Vec::new();
        for request_id in 1..=3 {
            let request = socket
                .read()
                .expect("read request")
                .into_text()
                .expect("text request");
            requests.push(serde_json::from_str::<Value>(&request).expect("request JSON"));
            let result = if request_id == 2 {
                json!({"thread": {"id": "thread through dispatch"}})
            } else {
                json!({})
            };
            socket
                .send(tungstenite::Message::text(
                    json!({"jsonrpc": "2.0", "id": request_id, "result": result}).to_string(),
                ))
                .expect("write response");
        }
        requests
    });
    let daemon = json!({
        "status": "running",
        "socketPath": socket_path
    })
    .to_string();
    let binary = root.path.join("codex");
    fs::write(
        &binary,
        format!("#!/bin/sh\nprintf '%s\\n' {}\n", shell_quote(&daemon)),
    )
    .expect("write fake codex");
    let mut permissions = fs::metadata(&binary).expect("fake metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&binary, permissions).expect("make fake executable");
    let _path = PathGuard::prepend(&root.path);

    let decision = decide_explicit(
        Provider::Codex,
        None,
        UsageSnapshot::full(),
        &agent_router_core::Config::default(),
    );
    // An explicit provider is unscored, so it runs at the high tier, and the router forces no
    // reasoning effort at all.
    assert_eq!(decision.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(decision.effort, None);
    let request = Request {
        task: "exercise the real dispatch seam",
        dir: &root.path,
        provider: Some(Provider::Codex),
        model: None,
        name: None,
        dry_run: false,
    };

    let dispatched = dispatch_decision(&decision, &request).expect("codex dispatch");
    assert_eq!(
        dispatched.job_id.as_deref(),
        Some("thread through dispatch")
    );
    let requests = server.join().expect("app server thread");
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[1]["method"], "thread/start");
    assert_eq!(requests[2]["method"], "turn/start");
    assert!(
        requests[2]["params"].get("effort").is_none(),
        "the router must leave the model's own default effort in place"
    );
    assert_eq!(requests[1]["params"]["model"], "gpt-5.6-sol");
    // The task reaches Codex verbatim. The router prepends nothing: an execution-mode preamble
    // here fought the repo's own AGENTS.md and showed up as boilerplate on every routed session.
    assert_eq!(
        requests[2]["params"]["input"][0]["text"],
        "exercise the real dispatch seam"
    );
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_http_request(stream: &mut TcpStream) -> HttpRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut body_len = None;
    loop {
        let read = stream.read(&mut chunk).expect("read request");
        assert_ne!(read, 0, "request ended before its body");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = header_end(&bytes) {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let content_len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            body_len = Some((end + 4, content_len));
        }
        if let Some((start, len)) = body_len
            && bytes.len() >= start + len
        {
            break;
        }
    }

    let end = header_end(&bytes).expect("headers");
    let head = String::from_utf8(bytes[..end].to_vec()).expect("utf8 headers");
    let mut lines = head.lines();
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("method").to_string();
    let target = request_parts.next().expect("target").to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.to_string(), value.trim().to_string()))
        })
        .collect();
    let body_bytes = &bytes[end + 4..];
    let body = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body_bytes).expect("json body")
    };
    HttpRequest {
        method,
        target,
        headers,
        body,
    }
}

fn loopback_server(
    responses: Vec<(u16, &'static str, &'static str)>,
) -> (SocketAddr, JoinHandle<Vec<HttpRequest>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("server address");
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut requests = Vec::new();
        for (status, content_type, body) in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "client did not connect");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            requests.push(read_http_request(&mut stream));
            let reason = if status == 204 {
                "No Content"
            } else if status >= 400 {
                "Server Error"
            } else {
                "OK"
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write response");
        }
        requests
    });
    (address, handle)
}

#[test]
fn opencode_managed_create_and_prompt_return_the_exact_created_identity() {
    let (address, server) = loopback_server(vec![
        (200, "application/json", r#"{"id":"session/雪?exact"}"#),
        (204, "application/json", ""),
    ]);
    let client =
        ManagedClient::for_loopback_test(address, "router test secret").expect("loopback client");
    let cwd = Path::new("/tmp/space and 雪/?x=1");
    let task = "雪".repeat(45);

    let dispatch = client
        .dispatch(
            cwd,
            &task,
            &truncated_title(&task),
            Some("openai/gpt-5.6-sol"),
        )
        .expect("managed dispatch");

    assert_eq!(dispatch.job_id.as_deref(), Some("session/雪?exact"));
    assert_eq!(dispatch.job_name, truncated_title(&task));
    let requests = server.join().expect("server thread");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].target,
        "/session?directory=%2Ftmp%2Fspace%20and%20%E9%9B%AA%2F%3Fx%3D1"
    );
    assert_eq!(
        requests[1].target,
        "/session/session%2F%E9%9B%AA%3Fexact/prompt_async?directory=%2Ftmp%2Fspace%20and%20%E9%9B%AA%2F%3Fx%3D1"
    );
    assert_eq!(
        requests[0].body,
        json!({
            "title": truncated_title(&task),
            "permission": [
                {"permission": "agent-router.background", "pattern": "*", "action": "allow"}
            ],
            "model": {"providerID": "openai", "id": "gpt-5.6-sol"}
        })
    );
    assert_eq!(
        requests[1].body,
        json!({
            "parts": [{"type": "text", "text": task}],
            "model": {"providerID": "openai", "modelID": "gpt-5.6-sol"}
        })
    );
    for request in &requests {
        let authorization = request.header("authorization").expect("authorization");
        assert!(authorization.starts_with("Basic "));
        assert!(!authorization.contains("router test secret"));
    }
}

/// The same caller-owned name reaches the OpenCode session, so a job dispatched to any provider is
/// findable under the name the caller chose.
#[test]
fn opencode_session_identity_uses_the_caller_supplied_name_verbatim() {
    let (address, server) = loopback_server(vec![
        (200, "application/json", r#"{"id":"session supplied name"}"#),
        (204, "application/json", ""),
    ]);
    let client =
        ManagedClient::for_loopback_test(address, "router test secret").expect("loopback client");
    let task = "雪".repeat(45);
    let name = "Bonus: abc-123";
    assert_ne!(
        truncated_title(&task),
        name,
        "the task must not derive this name"
    );

    let dispatch = client
        .dispatch(Path::new("/tmp"), &task, name, None)
        .expect("managed dispatch");

    assert_eq!(dispatch.job_name, name);
    assert_eq!(dispatch.job_id.as_deref(), Some("session supplied name"));
    let requests = server.join().expect("server thread");
    assert_eq!(requests[0].body["title"], json!(name));
}

#[test]
fn opencode_prompt_failure_names_the_created_session_and_never_creates_another() {
    let (address, server) = loopback_server(vec![
        (
            200,
            "application/json",
            r#"{"id":"session already created"}"#,
        ),
        (500, "text/plain", "prompt rejected"),
    ]);
    let client =
        ManagedClient::for_loopback_test(address, "router test secret").expect("loopback client");

    let error = client
        .dispatch(
            Path::new("/tmp"),
            "one submission",
            &truncated_title("one submission"),
            None,
        )
        .expect_err("prompt failure must be visible");

    assert!(error.to_string().contains("session already created"));
    let requests = server.join().expect("server thread");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.starts_with("/session?"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.contains("prompt_async"))
            .count(),
        1
    );
}

#[test]
fn opencode_managed_clients_refuse_non_loopback_endpoints() {
    let remote = SocketAddr::from(([192, 0, 2, 1], 4097));
    let error = ManagedClient::for_loopback_test(remote, "secret")
        .expect_err("managed transport must stay on loopback");
    assert!(error.to_string().contains("loopback"));
}
