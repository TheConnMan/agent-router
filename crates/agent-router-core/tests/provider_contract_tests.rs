#[cfg(target_os = "linux")]
use agent_router_core::decide::decide_explicit;
use agent_router_core::dispatch::claude::dispatch_with_binary;
use agent_router_core::dispatch::codex::{
    CodexRpc, SpawnAttempt, parse_first_turn_status, parse_thread_id, parse_thread_status,
    spawn_on_initialized_rpc, thread_read_request, thread_set_name_request, thread_start_request,
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

/// The app-server names a thread through `thread/name/set`, whose params are exactly `threadId`
/// and `name`. A name carrying spaces has to survive as one JSON string, because job names are
/// human phrases that the caller later matches verbatim.
#[test]
fn codex_thread_name_request_pins_the_app_server_method_and_params() {
    let named: Value = serde_json::from_str(&thread_set_name_request(
        3,
        "thread exact",
        "Bonus: abc 123 drain",
    ))
    .expect("name request");

    assert_eq!(named["jsonrpc"], "2.0");
    assert_eq!(named["id"], 3);
    assert_eq!(named["method"], "thread/name/set");
    assert_eq!(named["params"]["threadId"], "thread exact");
    assert_eq!(named["params"]["name"], "Bonus: abc 123 drain");
    assert_eq!(
        named["params"].as_object().expect("params object").len(),
        2,
        "params must carry exactly threadId and name, nothing extra"
    );
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

/// Reconciliation reads a thread through `thread/read`, whose params are exactly `threadId` and
/// `includeTurns`. The flag is the whole design: without it the reply carries no turn record, every
/// codex row falls through to the thread status fallback, and a job that provably finished reports
/// as unknown. It is also the easiest thing in this file to lose in a refactor.
#[test]
fn the_codex_thread_read_request_pins_the_app_server_method_and_params() {
    let read: Value =
        serde_json::from_str(&thread_read_request(2, "thread exact")).expect("thread read request");

    assert_eq!(read["jsonrpc"], "2.0");
    assert_eq!(read["id"], 2);
    assert_eq!(read["method"], "thread/read");
    assert_eq!(read["params"]["threadId"], "thread exact");
    assert_eq!(
        read["params"]["includeTurns"],
        json!(true),
        "without the turn history the reply proves nothing about the routed turn"
    );
    assert_eq!(
        read["params"].as_object().expect("params object").len(),
        2,
        "params must carry exactly threadId and includeTurns, nothing extra"
    );
}

/// Both readers over one reply, with the guards `parse_thread_id` already applies. A reply to
/// someone else's request and a reply carrying a JSON RPC error are not observations.
///
/// Observed on a real `thread/read` with `includeTurns` true: the turn array is nested inside
/// `thread`, and `result` carries the single key `thread`. The vendor schema agrees, listing
/// `thread` as the only property of `ThreadReadResponse`.
#[test]
fn a_thread_read_reply_yields_its_first_turn_status_and_its_thread_status() {
    let reply = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "thread": {
                "id": "thread exact",
                "status": {"type": "notLoaded"},
                "turns": [{"status": "completed"}, {"status": "failed"}],
            },
        }
    })
    .to_string();

    assert_eq!(
        parse_first_turn_status(&reply, 2).as_deref(),
        Some("completed"),
        "turn index 0 is the routed job, and the later turn is a human continuation"
    );
    assert_eq!(
        parse_thread_status(&reply, 2).as_deref(),
        Some("notLoaded"),
        "the thread status is still readable, as the fallback for a reply with no turns"
    );

    assert_eq!(parse_first_turn_status(&reply, 9), None);
    assert_eq!(parse_thread_status(&reply, 9), None);

    let errored = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"failed"}}"#;
    assert_eq!(parse_first_turn_status(errored, 2), None);
    assert_eq!(parse_thread_status(errored, 2), None);

    let turnless = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"thread": {"id": "thread exact", "status": {"type": "idle"}, "turns": []}}
    })
    .to_string();
    assert_eq!(
        parse_first_turn_status(&turnless, 2),
        None,
        "an empty turn array is no turn record, not an empty status"
    );
    assert_eq!(parse_thread_status(&turnless, 2).as_deref(), Some("idle"));

    assert_eq!(parse_first_turn_status("not json", 2), None);
    assert_eq!(parse_thread_status("not json", 2), None);
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

/// A spawn names the thread between creating it and starting its turn, so the job is findable
/// under the caller's name from the moment it runs.
#[test]
fn codex_spawn_names_the_thread_between_starting_it_and_its_first_turn() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Ok(r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread named"}}}"#.to_string()),
        Ok(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#.to_string()),
        Ok(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#.to_string()),
    ]);

    let attempt = spawn_on_initialized_rpc(
        &mut rpc,
        Path::new("/tmp"),
        "perform one task",
        "Bonus: abc 123",
        None,
        Some("xhigh"),
    );

    match attempt {
        SpawnAttempt::Started(thread_id) => assert_eq!(thread_id, "thread named"),
        other => panic!("expected a started thread, got {other:?}"),
    }
    assert_eq!(rpc.requests.len(), 3);
    assert_eq!(rpc.requests[0]["method"], "thread/start");
    assert_eq!(rpc.requests[0]["id"], 2);
    assert_eq!(rpc.requests[1]["method"], "thread/name/set");
    assert_eq!(rpc.requests[1]["id"], 3);
    assert_eq!(rpc.requests[1]["params"]["threadId"], "thread named");
    assert_eq!(rpc.requests[1]["params"]["name"], "Bonus: abc 123");
    assert_eq!(rpc.requests[2]["method"], "turn/start");
    assert_eq!(rpc.requests[2]["id"], 4);
    assert_eq!(rpc.requests[2]["params"]["threadId"], "thread named");
    assert_eq!(
        rpc.requests[2]["params"]["input"],
        json!([{"type": "text", "text": "perform one task"}])
    );
    assert!(rpc.replies.is_empty());
}

/// Naming is cosmetic and the thread is already alive when it is attempted, so a rejected
/// `thread/name/set` must not stop the turn or hide the thread id. Failing the dispatch here would
/// make the caller start the work a second time.
#[test]
fn codex_thread_name_failure_still_starts_the_turn_and_returns_the_thread() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Ok(r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread unnamed"}}}"#.to_string()),
        Err(Error::Command("name rejected".to_string())),
        Ok(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#.to_string()),
    ]);

    let attempt = spawn_on_initialized_rpc(
        &mut rpc,
        Path::new("/tmp"),
        "perform one task",
        "Bonus: abc 123",
        None,
        Some("xhigh"),
    );

    match attempt {
        SpawnAttempt::Started(thread_id) => assert_eq!(thread_id, "thread unnamed"),
        other => panic!("a failed name must not cost the caller the running thread, got {other:?}"),
    }
    assert_eq!(rpc.requests.len(), 3);
    assert_eq!(rpc.requests[0]["method"], "thread/start");
    assert_eq!(rpc.requests[1]["method"], "thread/name/set");
    assert_eq!(rpc.requests[2]["method"], "turn/start");
    assert_eq!(rpc.requests[2]["id"], 4);
    assert_eq!(
        rpc.requests[2]["params"]["input"],
        json!([{"type": "text", "text": "perform one task"}])
    );
    assert!(rpc.replies.is_empty());
}

#[test]
fn codex_partial_creation_is_one_visible_failure_without_a_retry() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Ok(r#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread partial"}}}"#.to_string()),
        Ok(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#.to_string()),
        Err(Error::Command("turn rejected".to_string())),
    ]);

    let attempt = spawn_on_initialized_rpc(
        &mut rpc,
        Path::new("/tmp"),
        "perform one task",
        "Bonus: abc 123",
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
    assert_eq!(rpc.requests.len(), 3);
    assert_eq!(rpc.requests[0]["method"], "thread/start");
    assert_eq!(rpc.requests[1]["method"], "thread/name/set");
    assert_eq!(rpc.requests[2]["method"], "turn/start");
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
        &[],
        false,
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
            &[],
            false,
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

/// MCP scoping reaches claude only if the argv order is exact: every `--mcp-config` first, then
/// `--strict-mcp-config`, which terminates claude's variadic config list so `--name` and the
/// prompt are not swallowed as further config paths.
#[cfg(unix)]
#[test]
fn claude_argv_places_mcp_scoping_between_the_model_flags_and_the_name() {
    let root = TempDir::new("claude-mcp");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let first = root.path.join("first.mcp.json");
    let second = root.path.join("second.mcp.json");
    fs::write(&first, r#"{"mcpServers":{}}"#).expect("write first config");
    fs::write(&second, r#"{"mcpServers":{}}"#).expect("write second config");
    let first_arg = first.to_string_lossy().to_string();
    let second_arg = second.to_string_lossy().to_string();
    let task = "route one scoped task";
    let name = truncated_title(task);

    struct ArgvCase<'a> {
        effort: Option<&'a str>,
        configs: Vec<PathBuf>,
        strict: bool,
        expected: Vec<&'a str>,
    }

    let cases = [
        ArgvCase {
            effort: Some("high"),
            configs: vec![first.clone(), second.clone()],
            strict: true,
            expected: vec![
                "--bg",
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--mcp-config",
                &first_arg,
                "--mcp-config",
                &second_arg,
                "--strict-mcp-config",
                "--name",
                &name,
                task,
            ],
        },
        ArgvCase {
            effort: None,
            configs: Vec::new(),
            strict: true,
            expected: vec![
                "--bg",
                "--model",
                "sonnet",
                "--strict-mcp-config",
                "--name",
                &name,
                task,
            ],
        },
        ArgvCase {
            effort: Some("high"),
            configs: vec![first.clone(), second.clone()],
            strict: false,
            expected: vec![
                "--bg",
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--mcp-config",
                &first_arg,
                "--mcp-config",
                &second_arg,
                "--name",
                &name,
                task,
            ],
        },
    ];

    for (index, case) in cases.into_iter().enumerate() {
        let spawn = TempDir::new("claude-mcp-spawn");
        let (binary, log) = fake_claude(&spawn.path, &json!([]));

        dispatch_with_binary(
            &binary,
            &cwd,
            task,
            &name,
            Some("sonnet"),
            case.effort,
            &case.configs,
            case.strict,
            Duration::from_millis(25),
        )
        .expect("claude dispatch");

        assert_eq!(
            wait_for_text(&log).lines().collect::<Vec<_>>(),
            case.expected,
            "argv for case {index}"
        );
    }
}

/// MCP configs hold server credentials, so an unreadable one must fail by naming the path and
/// nothing else, and must fail before claude is spawned rather than after a job is already running.
#[cfg(unix)]
#[test]
fn unreadable_mcp_config_names_the_path_without_its_body_and_spawns_nothing() {
    let root = TempDir::new("claude-mcp-unreadable");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let secret = "MCP_CONFIG_SECRET_517304";
    let source_line = format!("\"command\": \"runner\", \"env\": {{\"TOKEN\": \"{secret}\"}}");
    let body = r#"{"mcpServers":{"scoped":{SERVER}}}"#.replace("SERVER", &source_line);
    let config = root.path.join("unreadable.mcp.json");
    fs::write(&config, &body).expect("write config");
    let mut permissions = fs::metadata(&config)
        .expect("config metadata")
        .permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&config, permissions).expect("make config unreadable");
    let (binary, log) = fake_claude(&root.path, &json!([]));

    let error = dispatch_with_binary(
        &binary,
        &cwd,
        "route one scoped task",
        &truncated_title("route one scoped task"),
        Some("sonnet"),
        None,
        std::slice::from_ref(&config),
        false,
        Duration::from_millis(25),
    )
    .expect_err("an unreadable MCP config must fail");

    let path = config.display().to_string();
    for rendered in [format!("{error}"), format!("{error:?}")] {
        assert!(
            rendered.contains(&path),
            "the error hid which config failed: {rendered}"
        );
        assert!(
            !rendered.contains(secret),
            "the error exposed the config secret: {rendered}"
        );
        assert!(
            !rendered.contains(&source_line),
            "the error exposed a config source line: {rendered}"
        );
    }
    assert!(
        !log.exists(),
        "claude was spawned despite an unreadable MCP config"
    );
}

/// The unreadable case cannot prove the body is never read, because an unreadable file has no
/// body to leak. This one can: the config is readable and holds a secret, so an implementation
/// that inlined the contents onto the argv would publish it to `ps` and fail here.
#[cfg(unix)]
#[test]
fn readable_mcp_config_reaches_claude_by_path_and_never_by_its_contents() {
    let root = TempDir::new("claude-mcp-readable");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let secret = "MCP_READABLE_SECRET_884213";
    let body =
        format!(r#"{{"mcpServers":{{"scoped":{{"command":"x","env":{{"TOKEN":"{secret}"}}}}}}}}"#);
    let config = root.path.join("readable.mcp.json");
    fs::write(&config, &body).expect("write config");
    let (binary, log) = fake_claude(&root.path, &json!([]));

    dispatch_with_binary(
        &binary,
        &cwd,
        "route one scoped task",
        &truncated_title("route one scoped task"),
        Some("sonnet"),
        None,
        std::slice::from_ref(&config),
        true,
        Duration::from_millis(25),
    )
    .expect("a readable MCP config must dispatch");

    let argv = wait_for_text(&log);
    assert!(
        argv.contains(&config.display().to_string()),
        "the config path never reached claude: {argv}"
    );
    assert!(
        !argv.contains(secret),
        "the config body leaked onto the argv: {argv}"
    );
}

/// The job is spawned with `current_dir(cwd)`, so a relative config resolved there would name a
/// different file than the one preflighted. Resolution must anchor at the router process cwd.
#[cfg(unix)]
#[test]
fn relative_mcp_config_resolves_against_the_router_cwd_not_the_job_cwd() {
    let root = TempDir::new("claude-mcp-relative");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let relative = PathBuf::from("definitely-absent-relative-config.json");
    let router_cwd = std::env::current_dir().expect("router cwd");
    let (binary, log) = fake_claude(&root.path, &json!([]));

    let error = dispatch_with_binary(
        &binary,
        &cwd,
        "route one scoped task",
        &truncated_title("route one scoped task"),
        Some("sonnet"),
        None,
        std::slice::from_ref(&relative),
        false,
        Duration::from_millis(25),
    )
    .expect_err("an absent MCP config must fail");

    let rendered = format!("{error}");
    assert!(
        rendered.contains(&router_cwd.join(&relative).display().to_string()),
        "the config was not resolved against the router cwd: {rendered}"
    );
    assert!(
        !rendered.contains(&cwd.display().to_string()),
        "the config was resolved against the job cwd: {rendered}"
    );
    assert!(
        !log.exists(),
        "claude was spawned despite an absent MCP config"
    );
}

/// A path that stats fine but is not a regular file must be refused rather than handed to claude.
/// A directory stands in for the whole not-a-regular-file class because a fifo would hang the
/// router on open with no writer, turning a regression into a CI hang instead of a failure.
#[cfg(unix)]
#[test]
fn directory_mcp_config_is_refused_as_not_a_regular_file_and_spawns_nothing() {
    let root = TempDir::new("claude-mcp-directory");
    let cwd = root.path.join("working");
    fs::create_dir(&cwd).expect("create cwd");
    let config = root.path.join("config.mcp.json");
    fs::create_dir(&config).expect("create directory config");
    let (binary, log) = fake_claude(&root.path, &json!([]));

    let error = dispatch_with_binary(
        &binary,
        &cwd,
        "route one scoped task",
        &truncated_title("route one scoped task"),
        Some("sonnet"),
        None,
        std::slice::from_ref(&config),
        false,
        Duration::from_millis(25),
    )
    .expect_err("a directory MCP config must fail");

    let rendered = format!("{error}");
    assert!(
        rendered.contains(&config.display().to_string()),
        "the error hid which config failed: {rendered}"
    );
    assert!(
        rendered.contains("is not a regular file"),
        "the error did not name the failure kind: {rendered}"
    );
    assert!(
        !log.exists(),
        "claude was spawned despite a directory MCP config"
    );
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
        &[],
        false,
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
        &[],
        false,
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
        for request_id in 1..=4 {
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
        mcp_configs: &[],
        strict_mcp_config: false,
    };

    let dispatched = dispatch_decision(&decision, &request).expect("codex dispatch");
    assert_eq!(
        dispatched.job_id.as_deref(),
        Some("thread through dispatch")
    );
    let requests = server.join().expect("app server thread");
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[1]["method"], "thread/start");
    assert_eq!(requests[2]["method"], "thread/name/set");
    assert_eq!(
        requests[2]["params"]["threadId"], "thread through dispatch",
        "the name must land on the thread the dispatch just created"
    );
    assert_eq!(
        requests[2]["params"]["name"],
        json!(dispatched.job_name),
        "the job name the caller sees must be the name the thread carries"
    );
    assert_eq!(requests[3]["method"], "turn/start");
    assert!(
        requests[3]["params"].get("effort").is_none(),
        "the router must force no effort, leaving the daemon to resolve its own"
    );
    assert_eq!(requests[1]["params"]["model"], "gpt-5.6-sol");
    // The task reaches Codex verbatim. The router prepends nothing: an execution-mode preamble
    // here fought the repo's own AGENTS.md and showed up as boilerplate on every routed session.
    assert_eq!(
        requests[3]["params"]["input"][0]["text"],
        "exercise the real dispatch seam"
    );
}

/// Only claude accepts MCP scoping, so an auto decision that lands on another provider must fail
/// at the dispatch seam. Silently dropping the flags would run the job with the caller's scoping
/// ignored, which is the failure this rejection exists to prevent.
#[cfg(target_os = "linux")]
#[test]
fn mcp_scoping_on_a_non_claude_decision_fails_before_any_provider_work() {
    let root = TempDir::new("mcp-scoping-non-claude");
    let decision = decide_explicit(
        Provider::Codex,
        None,
        UsageSnapshot::full(),
        &agent_router_core::Config::default(),
    );
    assert_eq!(decision.provider, Provider::Codex);
    let configs = vec![root.path.join("scoped.mcp.json")];
    let empty: Vec<PathBuf> = Vec::new();

    for (mcp_configs, strict_mcp_config, flag) in [
        (configs.as_slice(), false, "--mcp-config"),
        (empty.as_slice(), true, "--strict-mcp-config"),
    ] {
        let request = Request {
            task: "exercise the scoping rejection",
            dir: &root.path,
            // Auto: the caller named no provider, so only the decision knows it is codex.
            provider: None,
            model: None,
            name: None,
            dry_run: false,
            mcp_configs,
            strict_mcp_config,
        };

        let error = dispatch_decision(&decision, &request)
            .expect_err("scoping must be rejected for a non-claude provider");

        let rendered = error.to_string();
        assert!(rendered.contains(flag), "the error hid {flag}: {rendered}");
        assert!(
            rendered.contains("codex"),
            "the error hid the provider that cannot honour {flag}: {rendered}"
        );
    }
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
