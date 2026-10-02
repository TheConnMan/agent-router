//! Ported from agent-viewer-core `tests/grok_tests.rs` (pinned d0f9c6f) when agent-router took
//! ownership of the Grok leader client. Only the surface agent-router exercises is kept: durable
//! listing and status, transcript tail, and the scripted leader lifecycle. PR-ref scanning, the
//! durable parse cache, turn activity, model discovery, the Backend trait, listing scope, and
//! attach were dropped with the code they covered. The scripted leader's frames were recorded
//! from the real `grok agent leader` and are kept byte-for-byte.

use agent_router_core::grok_leader::{self, GrokLifecycle, Status, TailEvent};
use std::path::{Path, PathBuf};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok")
        .join(path)
}

fn assert_terminal_safe(label: &str, value: &str) {
    let unsafe_character = value.chars().any(|character| {
        matches!(
            character,
            '\u{0000}'..='\u{001f}'
                | '\u{007f}'..='\u{009f}'
                | '\u{061c}'
                | '\u{200e}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
    });
    assert!(
        !unsafe_character,
        "{label} exposed terminal control characters"
    );
}

#[test]
fn durable_display_fields_strip_terminal_and_bidi_controls() {
    let home = tempfile::tempdir().expect("temporary Grok home");
    let session_dir = home.path().join("sessions/project/session-controls");
    std::fs::create_dir_all(&session_dir).expect("durable session directory");
    let hostile =
        "safe\u{1b}]8;;https://invalid.example\u{0007}link\u{1b}]8;;\u{0007}\u{0085}\u{202e}evil";
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::json!({
            "info":{"id":"session-controls","cwd":"/home/user/project"},
            "session_summary":hostile,
            "generated_title":hostile,
            "last_turn_summary":hostile,
            "created_at":"2026-08-20T01:02:03.004Z",
            "updated_at":"2026-08-21T04:05:06.007Z",
            "num_messages":2,
            "current_model_id":"grok-4"
        })
        .to_string(),
    )
    .expect("hostile durable summary");

    let row = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("durable listing")
        .into_iter()
        .find(|row| row.id == "session-controls")
        .expect("hostile durable row remains usable");
    assert_terminal_safe("durable title", &row.title);
    assert_terminal_safe("durable summary", &row.summary);
}

#[test]
fn durable_fallback_title_uses_safe_exact_identity() {
    let home = tempfile::tempdir().expect("temporary Grok home");
    let safe_id = "session-safe-fallback";
    let session_dir = home.path().join("sessions/project").join(safe_id);
    std::fs::create_dir_all(&session_dir).expect("durable session directory");
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::json!({
            "info":{"id":safe_id,"cwd":"/home/user/project"},
            "session_summary":"\u{1b}\u{202e}",
            "generated_title":"\u{1b}\u{202e}",
            "created_at":"2026-08-20T01:02:03.004Z",
            "updated_at":"2026-08-21T04:05:06.007Z",
            "num_messages":2,
            "current_model_id":"grok-4"
        })
        .to_string(),
    )
    .expect("control only durable titles");

    let row = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("durable listing")
        .into_iter()
        .find(|row| row.id == safe_id)
        .expect("safe identity row remains addressable");
    assert_eq!(row.title, safe_id);
}

#[test]
fn durable_listing_skips_control_bearing_identity_and_cwd() {
    let home = tempfile::tempdir().expect("temporary Grok home");
    let project = home.path().join("sessions/project");
    std::fs::create_dir_all(&project).expect("durable project directory");
    let hostile_id = "session-\u{1b}\u{202e}";
    for (id, cwd) in [
        (hostile_id, "/home/user/project"),
        (
            "session-hostile-cwd",
            "/home/user/\u{1b}]0;owned\u{0007}\u{2066}",
        ),
    ] {
        let directory = project.join(id);
        std::fs::create_dir(&directory).expect("durable session directory");
        std::fs::write(
            directory.join("summary.json"),
            serde_json::json!({
                "info":{"id":id,"cwd":cwd},
                "session_summary":"Unsafe identity boundary",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z",
                "num_messages":2,
                "current_model_id":"grok-4"
            })
            .to_string(),
        )
        .expect("hostile durable summary");
    }

    let rows = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("unsafe durable identities degrade to skipped rows");
    assert!(
        rows.is_empty(),
        "control bearing session IDs and cwd values must be skipped, not altered"
    );
}

#[test]
fn transcript_events_strip_terminal_and_bidi_controls() {
    let directory = tempfile::tempdir().expect("temporary transcript directory");
    let history = directory.path().join("chat_history.jsonl");
    let hostile = "safe\u{1b}]0;owned\u{0007}\u{009b}31m\u{202e}evil";
    let records = [
        serde_json::json!({"type":"user","content":[{"type":"text","text":hostile}]}),
        serde_json::json!({
            "type":"assistant",
            "content":hostile,
            "tool_calls":[{
                "id":"tool-controls",
                "name":hostile,
                "arguments":serde_json::json!({"path":hostile}).to_string()
            }]
        }),
    ];
    std::fs::write(
        &history,
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("hostile transcript");

    let events =
        grok_leader::read_tail(Some(&history), 10).expect("bounded hostile transcript tail");
    assert_eq!(events.len(), 3);
    for event in events {
        match event {
            TailEvent::User(text) => assert_terminal_safe("user transcript", &text),
            TailEvent::Agent(text) => assert_terminal_safe("agent transcript", &text),
            TailEvent::Tool { name } => assert_terminal_safe("tool name", &name),
        }
    }
}

#[test]
fn durable_listing_skips_malformed_and_partial_records_without_losing_siblings() {
    let lifecycle = GrokLifecycle::new("missing-grok", fixture("home"));
    let rows = lifecycle.list().expect("readable durable Grok sessions");

    let ids = rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>();
    assert!(
        ids.contains(&"session-durable"),
        "valid sibling was lost: {ids:?}"
    );
    assert!(
        ids.contains(&"session-dead"),
        "valid sibling was lost: {ids:?}"
    );
    assert!(
        ids.contains(&"session-dormant"),
        "valid sibling was lost: {ids:?}"
    );
    assert!(
        ids.contains(&"session-hidden"),
        "viewer hidden filtering must not suppress the core row: {ids:?}"
    );
    assert!(
        !ids.contains(&"session-malformed"),
        "malformed JSON must be skipped"
    );
    assert!(
        !ids.contains(&"session-partial"),
        "partial summary must be skipped"
    );

    let durable = rows
        .iter()
        .find(|row| row.id == "session-durable")
        .expect("durable session");
    assert_eq!(durable.title, "Generated fixture title");
    assert_eq!(durable.summary, "Implemented durable discovery");
    assert_eq!(durable.cwd, Path::new("/home/user/project"));
    assert_eq!(durable.status, Status::Unknown);
    assert!(
        !durable.daemon_hosted,
        "disk only rows do not claim a live leader"
    );
    assert_eq!(
        durable.rollout_path.as_deref(),
        Some(
            fixture(
                "home/sessions/project-fixture-0123456789abcdef/session-durable/chat_history.jsonl"
            )
            .as_path()
        )
    );
    let hidden = rows
        .iter()
        .find(|row| row.id == "session-hidden")
        .expect("hidden durable session");
    assert!(hidden.hidden);
    assert!(!hidden.subagent);
}

#[test]
fn durable_chat_history_turn_end_without_official_terminal_update_remains_unknown() {
    let lifecycle = GrokLifecycle::new("missing-grok", fixture("home"));
    let rows = lifecycle.list().expect("durable listing");
    let row = rows
        .iter()
        .find(|row| row.id == "session-durable")
        .expect("durable session");

    assert_eq!(row.status, Status::Unknown);
}

fn write_durable_grok_session(home: &Path, session_id: &str, updates: &[String]) {
    let session_dir = home.join("sessions/project").join(session_id);
    std::fs::create_dir_all(&session_dir).expect("durable session directory");
    std::fs::write(
        session_dir.join("summary.json"),
        serde_json::json!({
            "info":{"id":session_id,"cwd":"/home/user/project"},
            "session_summary":"Durable status fixture",
            "created_at":"2026-08-20T01:02:03.004Z",
            "updated_at":"2026-08-21T04:05:06.007Z",
            "num_messages":2,
            "current_model_id":"grok-4.6"
        })
        .to_string(),
    )
    .expect("durable session summary");
    std::fs::write(session_dir.join("updates.jsonl"), updates.join("\n"))
        .expect("durable session updates");
}

fn grok_session_update(session_id: &str, method: &str, update: serde_json::Value) -> String {
    grok_activity_update_at(session_id, serde_json::json!(1787330130), method, update)
}
fn grok_activity_update_at(
    session_id: &str,
    timestamp: serde_json::Value,
    method: &str,
    update: serde_json::Value,
) -> String {
    serde_json::json!({
        "timestamp":timestamp,
        "method":method,
        "params":{"sessionId":session_id,"update":update}
    })
    .to_string()
}

fn grok_turn_completed(session_id: &str, prompt_id: &str, stop_reason: &str) -> String {
    grok_session_update(
        session_id,
        "_x.ai/session/update",
        serde_json::json!({
            "sessionUpdate":"turn_completed",
            "prompt_id":prompt_id,
            "stop_reason":stop_reason
        }),
    )
}

fn padded_grok_turn_completed(
    session_id: &str,
    prompt_id: &str,
    stop_reason: &str,
    encoded_len: usize,
) -> String {
    let mut record = serde_json::from_str::<serde_json::Value>(&grok_turn_completed(
        session_id,
        prompt_id,
        stop_reason,
    ))
    .expect("terminal update JSON");
    record["padding"] = serde_json::Value::String(String::new());
    let base_len = record.to_string().len();
    assert!(
        base_len <= encoded_len,
        "requested padded record is too small"
    );
    record["padding"] = serde_json::Value::String("x".repeat(encoded_len - base_len));
    let encoded = record.to_string();
    assert_eq!(encoded.len(), encoded_len);
    encoded
}

#[test]
fn durable_terminal_status_uses_only_unambiguous_official_turn_updates() {
    let session_id = "session-status";
    let terminal = |reason| grok_turn_completed(session_id, "prompt-one", reason);
    let user_message = grok_session_update(
        session_id,
        "session/update",
        serde_json::json!({
            "sessionUpdate":"user_message_chunk",
            "content":{"type":"text","text":"continue"}
        }),
    );
    let agent_message = grok_session_update(
        session_id,
        "session/update",
        serde_json::json!({
            "sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"done"}
        }),
    );
    let cases = vec![
        ("end turn", vec![terminal("end_turn")], Status::Done),
        ("cancelled", vec![terminal("cancelled")], Status::Done),
        ("rate limit", vec![terminal("rate_limit")], Status::Error),
        ("error", vec![terminal("error")], Status::Error),
        ("refusal", vec![terminal("refusal")], Status::Error),
        ("max tokens", vec![terminal("max_tokens")], Status::Error),
        (
            "max turn requests",
            vec![terminal("max_turn_requests")],
            Status::Error,
        ),
        (
            "unknown stop reason",
            vec![terminal("future_reason")],
            Status::Unknown,
        ),
        (
            "no terminal update",
            vec![agent_message.clone()],
            Status::Unknown,
        ),
        (
            "empty prompt identity",
            vec![grok_turn_completed(session_id, "", "end_turn")],
            Status::Unknown,
        ),
        (
            "different session identity",
            vec![grok_turn_completed(
                "session-sibling",
                "prompt-one",
                "end_turn",
            )],
            Status::Unknown,
        ),
        (
            "user message after terminal",
            vec![terminal("end_turn"), user_message.clone()],
            Status::Unknown,
        ),
        (
            "later terminal after user message",
            vec![
                terminal("end_turn"),
                user_message,
                grok_turn_completed(session_id, "prompt-two", "end_turn"),
            ],
            Status::Done,
        ),
        (
            "torn suffix after terminal",
            vec![terminal("end_turn"), "{\"timestamp\":".to_string()],
            Status::Unknown,
        ),
        (
            "malformed earlier record followed by terminal",
            vec!["not json".to_string(), terminal("end_turn")],
            Status::Done,
        ),
        (
            "nonuser update after terminal",
            vec![terminal("end_turn"), agent_message],
            Status::Done,
        ),
    ];

    let mut mismatches = Vec::new();
    for (name, updates, expected) in cases {
        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(home.path(), session_id, &updates);
        let actual = GrokLifecycle::new("missing-grok", home.path())
            .list()
            .expect("durable listing")
            .into_iter()
            .find(|row| row.id == session_id)
            .expect("durable status row")
            .status;
        if actual != expected {
            mismatches.push(format!("{name}: expected {expected:?}, got {actual:?}"));
        }
    }

    assert!(
        mismatches.is_empty(),
        "durable terminal status mismatches:\n{}",
        mismatches.join("\n")
    );
}

#[test]
#[cfg(target_os = "linux")]
fn durable_status_tail_discards_a_partial_multibyte_prefix_before_a_complete_terminal_record() {
    const STATUS_TAIL_BYTES: usize = 1024 * 1024;

    let home = tempfile::tempdir().expect("temporary Grok home");
    let session_id = "session-utf8-boundary";
    write_durable_grok_session(home.path(), session_id, &[]);
    let terminal =
        padded_grok_turn_completed(session_id, "prompt-one", "end_turn", STATUS_TAIL_BYTES - 2);
    let prefix = "prefix";
    let updates = format!("{prefix}é\n{terminal}");
    assert_eq!(
        updates.len() - STATUS_TAIL_BYTES,
        prefix.len() + 1,
        "the retained tail must begin on the second byte of the multibyte character"
    );
    std::fs::write(
        home.path()
            .join("sessions/project")
            .join(session_id)
            .join("updates.jsonl"),
        updates,
    )
    .expect("UTF8 boundary updates");

    let row = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("durable listing")
        .into_iter()
        .find(|row| row.id == session_id)
        .expect("UTF8 boundary status row");
    assert_eq!(row.status, Status::Done);
}

#[test]
#[cfg(target_os = "linux")]
fn durable_status_uses_only_the_last_one_mib_per_session() {
    const STATUS_TAIL_BYTES: usize = 1024 * 1024;

    let home = tempfile::tempdir().expect("temporary Grok home");
    let session_id = "session-per-tail-budget";
    write_durable_grok_session(home.path(), session_id, &[]);
    let terminal = grok_turn_completed(session_id, "prompt-one", "end_turn");
    let later_unrelated = padded_grok_turn_completed(
        "session-unrelated",
        "prompt-two",
        "end_turn",
        STATUS_TAIL_BYTES,
    );
    std::fs::write(
        home.path()
            .join("sessions/project")
            .join(session_id)
            .join("updates.jsonl"),
        format!("{terminal}\n{later_unrelated}\n"),
    )
    .expect("per session budget updates");

    let row = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("durable listing")
        .into_iter()
        .find(|row| row.id == session_id)
        .expect("per session budget row");
    assert_eq!(
        row.status,
        Status::Unknown,
        "terminal evidence older than the one MiB tail must not be read"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn durable_status_aggregate_budget_is_deterministic_and_leaves_later_sessions_unknown() {
    const STATUS_TAIL_BYTES: usize = 1024 * 1024;
    const BUDGETED_SESSIONS: usize = 32;
    const SESSION_COUNT: usize = BUDGETED_SESSIONS + 2;

    let home = tempfile::tempdir().expect("temporary Grok home");
    for index in 0..SESSION_COUNT {
        let session_id = format!("session-{index:03}");
        let terminal =
            padded_grok_turn_completed(&session_id, "prompt-one", "end_turn", STATUS_TAIL_BYTES);
        write_durable_grok_session(home.path(), &session_id, &[terminal]);
    }

    let rows = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("aggregate budget listing");
    assert_eq!(rows.len(), SESSION_COUNT);
    for index in 0..SESSION_COUNT {
        let session_id = format!("session-{index:03}");
        let row = rows
            .iter()
            .find(|row| row.id == session_id)
            .expect("aggregate budget row");
        let expected = if index < BUDGETED_SESSIONS {
            Status::Done
        } else {
            Status::Unknown
        };
        assert_eq!(
            row.status, expected,
            "aggregate status budget selection changed for {session_id}"
        );
    }
}

#[test]
fn official_chat_history_tail_ignores_a_torn_last_record() {
    let history = fixture(
        "home/sessions/project-fixture-0123456789abcdef/session-durable/chat_history.jsonl",
    );
    let events = grok_leader::read_tail(Some(&history), 3).expect("Grok transcript tail");

    assert_eq!(
        events,
        vec![
            TailEvent::Tool {
                name: "read_file".to_string(),
            },
            TailEvent::User("latest request with context".to_string()),
            TailEvent::Agent("latest answer".to_string()),
        ]
    );
}

#[test]
#[cfg(unix)]
fn transcript_tail_rejects_a_symlink_instead_of_following_it() {
    let directory = tempfile::tempdir().expect("temporary transcript directory");
    let external = directory.path().join("external-history.jsonl");
    std::fs::write(
        &external,
        r#"{"type":"assistant","content":"must not cross the symlink boundary"}"#,
    )
    .expect("external transcript");
    let linked = directory.path().join("chat_history.jsonl");
    std::os::unix::fs::symlink(&external, &linked).expect("transcript symlink");

    let result = grok_leader::read_tail(Some(&linked), 10);
    assert!(
        result.is_err(),
        "a Grok transcript symlink must be rejected without reading its target"
    );
}

#[test]
fn transcript_tail_without_a_rollout_path_is_empty() {
    assert!(
        grok_leader::read_tail(None, 10)
            .expect("no transcript is not an error")
            .is_empty()
    );
}

#[test]
#[cfg(unix)]
fn durable_listing_skips_symlinked_and_oversized_summaries() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("temporary Grok home");
    let project = home.path().join("sessions/project");
    std::fs::create_dir_all(&project).expect("project sessions directory");
    let summary = |id: &str, text: &str| {
        serde_json::json!({
            "info":{"id":id,"cwd":"/home/user/project"},
            "session_summary":text,
            "created_at":"2026-08-20T01:02:03.004Z",
            "updated_at":"2026-08-21T04:05:06.007Z",
            "num_messages":2,
            "current_model_id":"grok-4"
        })
        .to_string()
    };

    let symlinked = project.join("session-symlinked");
    std::fs::create_dir(&symlinked).expect("symlinked session directory");
    let external = home.path().join("external-summary.json");
    std::fs::write(&external, summary("session-symlinked", "external title"))
        .expect("external summary");
    symlink(&external, symlinked.join("summary.json")).expect("summary symlink");

    let oversized = project.join("session-oversized");
    std::fs::create_dir(&oversized).expect("oversized session directory");
    let large_text = "x".repeat(64 * 1024 * 1024 + 1);
    std::fs::write(
        oversized.join("summary.json"),
        summary("session-oversized", &large_text),
    )
    .expect("oversized valid summary");

    let rows = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("durable listing degrades safely");
    let unsafe_ids = rows
        .iter()
        .filter(|row| row.id == "session-symlinked" || row.id == "session-oversized")
        .map(|row| row.id.as_str())
        .collect::<Vec<_>>();
    assert!(
        unsafe_ids.is_empty(),
        "unsafe durable summaries were followed or read without a cap: {unsafe_ids:?}"
    );
}

#[test]
fn durable_listing_has_an_aggregate_byte_budget_and_deterministic_selection() {
    const SESSION_COUNT: usize = 80;

    let home = tempfile::tempdir().expect("temporary Grok home");
    let project = home.path().join("sessions/project");
    std::fs::create_dir_all(&project).expect("project sessions directory");
    let large_summary = "x".repeat(900 * 1024);
    for index in 0..SESSION_COUNT {
        let id = format!("session-{index:03}");
        let directory = project.join(&id);
        std::fs::create_dir(&directory).expect("durable session directory");
        let body = serde_json::json!({
            "info":{"id":id,"cwd":"/home/user/project"},
            "session_summary":large_summary.as_str(),
            "created_at":"2026-08-20T01:02:03.004Z",
            "updated_at":"2026-08-21T04:05:06.007Z",
            "num_messages":2,
            "current_model_id":"grok-4"
        });
        std::fs::write(directory.join("summary.json"), body.to_string())
            .expect("bounded durable summary");
    }

    let listed = GrokLifecycle::new("missing-grok", home.path())
        .list()
        .expect("budgeted durable listing");
    let ids = listed.into_iter().map(|row| row.id).collect::<Vec<_>>();
    assert!(
        !ids.is_empty(),
        "the aggregate budget must retain useful rows"
    );
    assert!(
        ids.len() < SESSION_COUNT,
        "the aggregate budget must bound total summary bytes"
    );
    let expected = (0..ids.len())
        .map(|index| format!("session-{index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(
        ids, expected,
        "budget selection must be deterministic by durable path order"
    );
}

#[cfg(unix)]
mod protocol {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

    #[derive(Clone, Debug)]
    struct CapturedFrame {
        prefix: [u8; 4],
        body: Value,
    }

    struct ScriptedLeader {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        frames: Arc<Mutex<Vec<CapturedFrame>>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    #[derive(Default)]
    struct LeaderBehavior {
        oversized: bool,
        prompt_error: bool,
        prompt_error_after_roster: bool,
        reverse_permission: bool,
        roster_error: bool,
    }

    impl ScriptedLeader {
        fn start(home: &Path, oversized: bool) -> ScriptedLeader {
            let roster: Value = serde_json::from_str(
                &fs::read_to_string(fixture("roster.json")).expect("roster fixture"),
            )
            .expect("roster JSON");
            Self::start_named(home, "", roster, oversized, false, false)
        }

        fn start_named(
            home: &Path,
            suffix: &str,
            roster: Value,
            oversized: bool,
            prompt_error: bool,
            reverse_permission: bool,
        ) -> ScriptedLeader {
            Self::start_config(
                home,
                suffix,
                roster,
                LeaderBehavior {
                    oversized,
                    prompt_error,
                    reverse_permission,
                    ..LeaderBehavior::default()
                },
            )
        }

        fn start_with_roster_error(home: &Path, suffix: &str) -> ScriptedLeader {
            Self::start_config(
                home,
                suffix,
                json!({"sessions":[]}),
                LeaderBehavior {
                    roster_error: true,
                    ..LeaderBehavior::default()
                },
            )
        }

        fn start_with_prompt_error_after_roster(home: &Path, roster: Value) -> ScriptedLeader {
            Self::start_config(
                home,
                "",
                roster,
                LeaderBehavior {
                    prompt_error_after_roster: true,
                    ..LeaderBehavior::default()
                },
            )
        }

        fn start_config(
            home: &Path,
            suffix: &str,
            roster: Value,
            behavior: LeaderBehavior,
        ) -> ScriptedLeader {
            fs::create_dir_all(home).expect("leader home");
            let socket = home.join(format!("leader{suffix}.sock"));
            let _ = fs::remove_file(&socket);
            let lock = home.join(format!("leader{suffix}.lock"));
            fs::write(&lock, std::process::id().to_string()).expect("leader lock");
            let listener = UnixListener::bind(&socket).expect("scripted leader socket");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let stop = Arc::new(AtomicBool::new(false));
            let frames = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let thread_frames = Arc::clone(&frames);
            let thread_socket = socket.clone();
            let thread_lock = lock.clone();
            let LeaderBehavior {
                oversized,
                prompt_error,
                prompt_error_after_roster,
                reverse_permission,
                roster_error,
            } = behavior;
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => handle_connection(
                            stream,
                            ConnectionScript {
                                captured: &thread_frames,
                                roster: &roster,
                                oversized,
                                prompt_error,
                                prompt_error_after_roster,
                                reverse_permission,
                                roster_error,
                                socket: &thread_socket,
                                lock: &thread_lock,
                            },
                        ),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            ScriptedLeader {
                socket,
                stop,
                frames,
                thread: Some(thread),
            }
        }

        fn captured(&self) -> Vec<CapturedFrame> {
            self.frames.lock().expect("captured frames").clone()
        }
    }

    impl Drop for ScriptedLeader {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.socket);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn read_frame(stream: &mut UnixStream) -> io::Result<Option<([u8; 4], Value)>> {
        let mut prefix = [0_u8; 4];
        match stream.read_exact(&mut prefix) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        let len = u32::from_be_bytes(prefix) as usize;
        let mut body = vec![0; len];
        stream.read_exact(&mut body)?;
        let value = serde_json::from_slice(&body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Some((prefix, value)))
    }

    fn framed(value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("frame JSON");
        let mut bytes = Vec::with_capacity(4 + body.len());
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn send_fragmented(stream: &mut UnixStream, value: &Value) -> io::Result<()> {
        let bytes = framed(value);
        stream.write_all(&bytes[..2])?;
        thread::sleep(Duration::from_millis(1));
        stream.write_all(&bytes[2..])
    }

    fn send_acp_response(stream: &mut UnixStream, id: Value, result: Value) -> io::Result<()> {
        let notification = json!({
            "type": "acp",
            "payload": json!({
                "jsonrpc": "2.0",
                "method": "_x.ai/sessions/changed",
                "params": {}
            }).to_string()
        });
        let response = json!({
            "type": "acp",
            "payload": json!({"jsonrpc":"2.0", "id":id, "result":result}).to_string()
        });
        let unrelated = json!({
            "type": "acp",
            "payload": json!({
                "jsonrpc":"2.0",
                "id":"unrelated-request",
                "result":{"ignored":true}
            }).to_string()
        });
        let mut bytes = framed(&notification);
        bytes.extend_from_slice(&framed(&unrelated));
        bytes.extend_from_slice(&framed(&response));
        stream.write_all(&bytes)
    }

    struct ConnectionScript<'a> {
        captured: &'a Arc<Mutex<Vec<CapturedFrame>>>,
        roster: &'a Value,
        oversized: bool,
        prompt_error: bool,
        prompt_error_after_roster: bool,
        reverse_permission: bool,
        roster_error: bool,
        socket: &'a Path,
        lock: &'a Path,
    }

    fn handle_connection(mut stream: UnixStream, script: ConnectionScript<'_>) {
        let ConnectionScript {
            captured,
            roster,
            oversized,
            prompt_error,
            prompt_error_after_roster,
            reverse_permission,
            roster_error,
            socket,
            lock,
        } = script;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("leader read timeout");
        let mut deferred_prompt_id = None;
        let mut prompt_submitted = false;
        loop {
            let Ok(Some((prefix, outer))) = read_frame(&mut stream) else {
                return;
            };
            captured.lock().expect("capture frame").push(CapturedFrame {
                prefix,
                body: outer.clone(),
            });
            match outer.get("type").and_then(Value::as_str) {
                Some("register") => {
                    if oversized {
                        let _ = stream.write_all(&(MAX_FRAME_BYTES + 1).to_be_bytes());
                        return;
                    }
                    let registered = json!({
                        "type":"registered",
                        "client_id":7,
                        "ready":true,
                        "leader_protocol_version":1,
                        "leader_binary_version":"0.1.150",
                        "leader_capabilities":{
                            "control_v1":true,
                            "runtime_cpu_profile":false,
                            "profile_formats":[],
                            "workspace_exposure":false,
                            "relaunch_v1":false
                        }
                    });
                    if send_fragmented(&mut stream, &registered).is_err() {
                        return;
                    }
                }
                Some("control") => {
                    let request_id = outer["request_id"].clone();
                    let response = json!({
                        "type":"control_result",
                        "request_id":request_id,
                        "result":{"Ok":{
                            "type":"leader_info",
                            "pid":std::process::id(),
                            "socket_path":socket.display().to_string(),
                            "lock_path":lock.display().to_string(),
                            "ws_url_suffix":"",
                            "leader_protocol_version":1,
                            "leader_binary_version":"0.1.150",
                            "profiling_supported":false,
                            "profiling_compiled_in":false,
                            "cpu_profile_active":false,
                            "cpu_profile_stopping":false,
                            "profile_started_at":null,
                            "profile_formats":[]
                        }}
                    });
                    if send_fragmented(&mut stream, &response).is_err() {
                        return;
                    }
                }
                Some("acp") => {
                    let Some(payload) = outer.get("payload").and_then(Value::as_str) else {
                        return;
                    };
                    let Ok(request) = serde_json::from_str::<Value>(payload) else {
                        return;
                    };
                    let Some(id) = request.get("id").cloned() else {
                        continue;
                    };
                    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                    let direct_extension_params = match method {
                        "_x.ai/sessions/list" | "_x.ai/models/list" => {
                            request.get("params") == Some(&json!({}))
                        }
                        "_x.ai/session/rename" => {
                            request.get("params")
                                == Some(&json!({
                                    "sessionId":"session-selected",
                                    "title":"A precise title"
                                }))
                        }
                        "_x.ai/session/delete" => {
                            matches!(
                                request.get("params"),
                                Some(params)
                                    if params == &json!({
                                        "sessionId":"session-selected",
                                        "cwd":"/home/user/project"
                                    })
                                        || params == &json!({
                                            "sessionId":"session-rejected",
                                            "cwd":"/home/user/rejected"
                                        })
                                        || params.get("sessionId")
                                            == Some(&json!("session-delete-cwd"))
                            )
                        }
                        _ => true,
                    };
                    if !direct_extension_params {
                        let response = json!({
                            "type":"acp",
                            "payload":json!({
                                "jsonrpc":"2.0",
                                "id":id,
                                "error":{
                                    "code":-32602,
                                    "message":"direct extension params required"
                                }
                            }).to_string()
                        });
                        if stream.write_all(&framed(&response)).is_err() {
                            return;
                        }
                        continue;
                    }
                    if method == "session/prompt" && prompt_error {
                        let response = json!({
                            "type":"acp",
                            "payload":json!({
                                "jsonrpc":"2.0",
                                "id":id,
                                "error":{
                                    "code":-32000,
                                    "message":"prompt rejected\u{001b}]0;owned\u{0007}\u{0085}\u{202e}evil"
                                }
                            }).to_string()
                        });
                        if stream.write_all(&framed(&response)).is_err() {
                            return;
                        }
                        continue;
                    }
                    if method == "session/prompt" && prompt_error_after_roster {
                        deferred_prompt_id = Some(id);
                        continue;
                    }
                    if method == "session/prompt" {
                        prompt_submitted = true;
                        continue;
                    }
                    if method == "_x.ai/sessions/list" && roster_error {
                        let response = json!({
                            "type":"acp",
                            "payload":json!({
                                "jsonrpc":"2.0",
                                "id":id,
                                "error":{"code":-32001,"message":"roster unavailable"}
                            }).to_string()
                        });
                        if stream.write_all(&framed(&response)).is_err() {
                            return;
                        }
                        continue;
                    }
                    if method == "_x.ai/sessions/list" && reverse_permission {
                        let reverse = json!({
                            "type":"acp",
                            "payload":json!({
                                "jsonrpc":"2.0",
                                "id":id,
                                "method":"session/request_permission",
                                "params":{
                                    "sessionId":"session-alpha",
                                    "toolCall":{"toolCallId":"permission-collision"},
                                    "options":[]
                                }
                            }).to_string()
                        });
                        if stream.write_all(&framed(&reverse)).is_err() {
                            return;
                        }
                        let Ok(Some((prefix, outer))) = read_frame(&mut stream) else {
                            return;
                        };
                        captured.lock().expect("capture frame").push(CapturedFrame {
                            prefix,
                            body: outer.clone(),
                        });
                        let Some(payload) = outer.get("payload").and_then(Value::as_str) else {
                            return;
                        };
                        let Ok(response) = serde_json::from_str::<Value>(payload) else {
                            return;
                        };
                        if response.get("id") != Some(&id)
                            || response.pointer("/result/outcome/outcome")
                                != Some(&json!("cancelled"))
                            || response.get("method").is_some()
                        {
                            return;
                        }
                    }
                    let result = match method {
                        "initialize" => json!({
                            "protocolVersion":1,
                            "agentCapabilities":{},
                            "authMethods":[]
                        }),
                        "session/new" => json!({"sessionId":"session-alpha"}),
                        "_x.ai/sessions/list" => {
                            let mut result = roster.clone();
                            if prompt_submitted
                                && let Some(sessions) =
                                    result.get_mut("sessions").and_then(Value::as_array_mut)
                                && !sessions.iter().any(|session| {
                                    session.get("sessionId").and_then(Value::as_str)
                                        == Some("session-alpha")
                                })
                            {
                                sessions.push(json!({
                                    "sessionId":"session-alpha",
                                    "title":"Spawned",
                                    "cwd":"/home/user/project",
                                    "activity":"working",
                                    "resident":true,
                                    "lastChangeUnixMs":1787289000000_i64
                                }));
                            }
                            json!({"result":result})
                        }
                        "_x.ai/session/rename" => json!({"success":true}),
                        "_x.ai/session/delete" => json!({
                            "success":request
                                .pointer("/params/sessionId")
                                .and_then(Value::as_str)
                                != Some("session-rejected")
                        }),
                        _ => json!({}),
                    };
                    if send_acp_response(&mut stream, id, result).is_err() {
                        return;
                    }
                    if method == "_x.ai/sessions/list"
                        && let Some(prompt_id) = deferred_prompt_id.take()
                    {
                        let response = json!({
                            "type":"acp",
                            "payload":json!({
                                "jsonrpc":"2.0",
                                "id":prompt_id,
                                "error":{
                                    "code":-32000,
                                    "message":"detached prompt rejected after roster"
                                }
                            }).to_string()
                        });
                        if stream.write_all(&framed(&response)).is_err() {
                            return;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn leader_home() -> (tempfile::TempDir, ScriptedLeader) {
        let temp = tempfile::tempdir().expect("temporary Grok home");
        std::os::unix::fs::symlink(fixture("home/sessions"), temp.path().join("sessions"))
            .expect("durable session fixture link");
        let leader = ScriptedLeader::start(temp.path(), false);
        (temp, leader)
    }

    fn acp_requests(frames: &[CapturedFrame]) -> Vec<Value> {
        frames
            .iter()
            .filter(|frame| frame.body["type"] == "acp")
            .filter_map(|frame| frame.body["payload"].as_str())
            .filter_map(|payload| serde_json::from_str(payload).ok())
            .collect()
    }

    #[test]
    fn roster_precedence_maps_dead_to_error_and_dormant_to_idle() {
        let (home, leader) = leader_home();
        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("merged Grok listing");

        let durable = rows.iter().find(|row| row.id == "session-durable").unwrap();
        assert_eq!(durable.title, "Resident title");
        assert_eq!(durable.cwd, Path::new("/home/user/project"));
        assert_eq!(durable.status, Status::Working);
        assert!(durable.daemon_hosted);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "session-dead")
                .unwrap()
                .status,
            Status::Error
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "session-dormant")
                .unwrap()
                .status,
            Status::Idle
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "leader-only")
                .unwrap()
                .status,
            Status::NeedsInput { reason: None }
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.id == "session-durable")
                .count(),
            1,
            "resident roster row must replace its durable sibling"
        );

        let requests = acp_requests(&leader.captured());
        let roster = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/sessions/list")
            .expect("official roster extension request");
        assert_eq!(roster["params"], json!({}));
        assert!(roster["params"].get("method").is_none());
        assert!(roster["params"].get("params").is_none());
    }

    #[test]
    fn resident_roster_status_wins_over_a_durable_terminal_fallback() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(
            home.path(),
            "session-status",
            &[grok_turn_completed(
                "session-status",
                "prompt-one",
                "end_turn",
            )],
        );
        let _leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[{
                "sessionId":"session-status",
                "title":"Resident status",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787330131000_i64
            }]}),
            false,
            false,
            false,
        );

        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("merged Grok listing");
        let matches = rows
            .iter()
            .filter(|row| row.id == "session-status")
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].status, Status::Working);
        assert!(matches[0].daemon_hosted);
    }

    #[test]
    fn dormant_roster_without_a_durable_terminal_is_idle() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(home.path(), "session-status", &[]);
        let _leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[{
                "sessionId":"session-status",
                "title":"Unloaded after triage",
                "cwd":"/home/user/project",
                "activity":"dormant",
                "resident":false,
                "lastChangeUnixMs":1787330131000_i64
            }]}),
            false,
            false,
            false,
        );

        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("merged Grok listing");
        let matches = rows
            .iter()
            .filter(|row| row.id == "session-status")
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].status, Status::Idle);
        assert!(!matches[0].daemon_hosted);
    }

    #[test]
    fn dormant_roster_preserves_a_durable_completed_turn() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(
            home.path(),
            "session-status",
            &[grok_turn_completed(
                "session-status",
                "prompt-one",
                "end_turn",
            )],
        );
        let _leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[{
                "sessionId":"session-status",
                "title":"Completed resident session",
                "cwd":"/home/user/project",
                "activity":"dormant",
                "resident":true,
                "lastChangeUnixMs":1787330131000_i64
            }]}),
            false,
            false,
            false,
        );

        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("merged Grok listing");
        let matches = rows
            .iter()
            .filter(|row| row.id == "session-status")
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].status, Status::Done);
        assert!(matches[0].daemon_hosted);
    }

    #[test]
    fn idle_roster_does_not_override_a_durable_completed_turn() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(
            home.path(),
            "session-status",
            &[grok_turn_completed(
                "session-status",
                "prompt-one",
                "end_turn",
            )],
        );
        write_durable_grok_session(home.path(), "session-idle", &[]);
        let _leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[
                {
                    "sessionId":"session-status",
                    "title":"Completed resident session",
                    "cwd":"/home/user/project",
                    "activity":"idle",
                    "resident":true,
                    "lastChangeUnixMs":1787330129000_i64
                },
                {
                    "sessionId":"session-idle",
                    "title":"Idle resident session",
                    "cwd":"/home/user/project",
                    "activity":"idle",
                    "resident":true,
                    "lastChangeUnixMs":1787330129000_i64
                }
            ]}),
            false,
            false,
            false,
        );

        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("merged Grok listing");
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "session-idle")
                .expect("idle control row")
                .status,
            Status::Idle
        );
        let completed = rows
            .iter()
            .find(|row| row.id == "session-status")
            .expect("completed status row");
        assert_eq!(completed.status, Status::Done);
        assert!(completed.daemon_hosted);
    }

    #[test]
    fn spawn_registers_before_acp_and_preserves_identity_model_and_prompt_target() {
        let (home, leader) = leader_home();
        let result = GrokLifecycle::new("/bin/true", home.path())
            .spawn(
                Path::new("/home/user/project"),
                "implement framing",
                Some("grok-4-fast"),
            )
            .expect("official Grok spawn");
        assert_eq!(result.session_id.as_deref(), Some("session-alpha"));

        let frames = leader.captured();
        assert_eq!(frames.first().unwrap().body["type"], "register");
        assert_eq!(frames.first().unwrap().body["client_type"], "agent-router");
        assert_eq!(frames.first().unwrap().body["mode"], "stdio");
        assert_eq!(
            frames.first().unwrap().body["capabilities"]["default_model"],
            "grok-4-fast"
        );
        for frame in &frames {
            let encoded = serde_json::to_vec(&frame.body).unwrap();
            assert_eq!(u32::from_be_bytes(frame.prefix) as usize, encoded.len());
        }
        let requests = acp_requests(&frames);
        assert_eq!(requests.first().unwrap()["method"], "initialize");
        assert_eq!(
            requests.first().unwrap()["params"]["_meta"]["clientType"],
            "agent-router"
        );
        let new = requests
            .iter()
            .find(|request| request["method"] == "session/new")
            .unwrap();
        assert_eq!(new["params"]["cwd"], "/home/user/project");
        assert_eq!(
            new["params"],
            json!({"cwd":"/home/user/project", "mcpServers":[]}),
            "session/new must carry exactly the cwd and an empty MCP server list"
        );
        let prompt = requests
            .iter()
            .find(|request| request["method"] == "session/prompt")
            .unwrap();
        assert_eq!(prompt["params"]["sessionId"], "session-alpha");
        assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "implement framing");
        let prompt_index = requests
            .iter()
            .position(|request| request["method"] == "session/prompt")
            .expect("prompt request position");
        let roster_index = requests
            .iter()
            .position(|request| request["method"] == "_x.ai/sessions/list")
            .expect("post prompt roster request");
        assert!(prompt_index < roster_index);
    }

    #[test]
    fn detached_spawn_surfaces_prompt_error_after_nonworking_roster() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start_with_prompt_error_after_roster(
            home.path(),
            json!({"sessions":[{
                "sessionId":"session-alpha",
                "title":"Spawned",
                "cwd":"/home/user/project",
                "activity":"idle",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
        );

        let error = GrokLifecycle::new("/bin/true", home.path())
            .spawn(
                Path::new("/home/user/project"),
                "detached nonworking kickoff",
                None,
            )
            .expect_err("a nonworking roster must not hide the following prompt error");

        assert_eq!(
            error.to_string(),
            "command failed: Grok session/prompt request failed: detached prompt rejected after roster"
        );
    }

    #[test]
    fn detached_spawn_surfaces_prompt_error_after_nonmatching_roster() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start_with_prompt_error_after_roster(
            home.path(),
            json!({"sessions":[{
                "sessionId":"session-sibling",
                "title":"Sibling",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
        );

        let error = GrokLifecycle::new("/bin/true", home.path())
            .spawn(
                Path::new("/home/user/project"),
                "detached nonmatching kickoff",
                None,
            )
            .expect_err("an unrelated working row must not hide the following prompt error");

        assert_eq!(
            error.to_string(),
            "command failed: Grok session/prompt request failed: detached prompt rejected after roster"
        );
    }

    #[test]
    fn detached_spawn_requires_the_exact_working_session_to_be_resident() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start_with_prompt_error_after_roster(
            home.path(),
            json!({"sessions":[{
                "sessionId":"session-alpha",
                "title":"Spawned but detached",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":false,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
        );

        let error = GrokLifecycle::new("/bin/true", home.path())
            .spawn(
                Path::new("/home/user/project"),
                "detached nonresident kickoff",
                None,
            )
            .expect_err("a nonresident working row must not confirm detached execution");

        assert_eq!(
            error.to_string(),
            "command failed: Grok session/prompt request failed: detached prompt rejected after roster"
        );
    }

    #[test]
    fn detached_spawn_registers_yolo_mode_and_preserves_optional_default_model() {
        let registration_for = |model: Option<&str>| {
            let (home, leader) = leader_home();
            GrokLifecycle::new("/bin/true", home.path())
                .spawn(Path::new("/home/user/project"), "detached kickoff", model)
                .expect("official Grok spawn");
            leader
                .captured()
                .into_iter()
                .find(|frame| frame.body["type"] == "register")
                .expect("leader registration")
                .body
        };

        let selected_model = registration_for(Some("grok-4-fast"));
        let runtime_default = registration_for(None);
        assert_eq!(
            selected_model["capabilities"]["default_model"],
            "grok-4-fast"
        );
        assert!(runtime_default["capabilities"]["default_model"].is_null());
        assert_eq!(selected_model["capabilities"]["yolo_mode"], true);
        assert_eq!(runtime_default["capabilities"]["yolo_mode"], true);
    }

    #[test]
    fn spawn_cancels_colliding_reverse_permission_and_still_correlates_roster() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[{
                "sessionId":"session-alpha",
                "title":"Spawned",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
            false,
            false,
            true,
        );

        let result = GrokLifecycle::new("/bin/true", home.path())
            .spawn(
                Path::new("/home/user/project"),
                "permission collision",
                None,
            )
            .expect("spawn must survive a colliding reverse permission request");
        assert_eq!(result.session_id.as_deref(), Some("session-alpha"));

        let responses = acp_requests(&leader.captured())
            .into_iter()
            .filter(|message| message.get("method").is_none() && message.get("result").is_some())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["result"]["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn cancel_is_scoped_to_the_selected_session() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[
                {
                    "sessionId":"session-selected",
                    "title":"Selected",
                    "cwd":"/home/user/project",
                    "activity":"working",
                    "resident":true,
                    "lastChangeUnixMs":1787289000000_i64
                },
                {
                    "sessionId":"session-sibling",
                    "title":"Sibling",
                    "cwd":"/home/user/project",
                    "activity":"working",
                    "resident":true,
                    "lastChangeUnixMs":1787289000000_i64
                }
            ]}),
            false,
            false,
            false,
        );
        GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect("session cancel");

        let requests = acp_requests(&leader.captured());
        let cancels = requests
            .iter()
            .filter(|request| request["method"] == "session/cancel")
            .collect::<Vec<_>>();
        assert_eq!(cancels.len(), 1);
        assert_eq!(cancels[0]["params"]["sessionId"], "session-selected");
        let wire = serde_json::to_string(&cancels).unwrap();
        assert!(!wire.contains("session-sibling"));
    }

    #[test]
    fn cancel_treats_only_refused_leader_endpoints_as_already_stopped() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let home = tempfile::tempdir().expect("temporary Grok home");
        let stale_socket = home.path().join("leader.sock");
        let stale_listener = UnixListener::bind(&stale_socket).expect("stale leader socket");
        drop(stale_listener);
        let stale_lock = home.path().join("leader.lock");
        fs::write(&stale_lock, std::process::id().to_string()).expect("stale leader lock");
        let socket_inode = fs::symlink_metadata(&stale_socket)
            .expect("stale socket metadata")
            .ino();
        let lock_contents = fs::read_to_string(&stale_lock).expect("stale lock contents");

        let session_summary = home
            .path()
            .join("sessions/project/session-selected/summary.json");
        fs::create_dir_all(session_summary.parent().expect("session directory"))
            .expect("durable session directory");
        fs::write(
            &session_summary,
            json!({
                "info":{"id":"session-selected","cwd":"/home/user/project"},
                "session_summary":"Stopped session",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z"
            })
            .to_string(),
        )
        .expect("durable session summary");

        let binary = home.path().join("fake-grok");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf started > \"$GROK_HOME/start-marker\"\n",
        )
        .expect("fake Grok binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("executable fake Grok binary");

        GrokLifecycle::new(&binary, home.path())
            .cancel("session-selected")
            .expect("refused leader endpoint means the session is already stopped");

        assert!(
            !home.path().join("start-marker").exists(),
            "cancel must not start or restart the official leader"
        );
        assert_eq!(
            fs::symlink_metadata(&stale_socket)
                .expect("stale socket remains")
                .ino(),
            socket_inode,
            "cancel must not replace or delete shared leader runtime state"
        );
        assert_eq!(
            fs::read_to_string(&stale_lock).expect("stale lock remains"),
            lock_contents,
            "cancel must not replace or delete shared leader runtime state"
        );
        assert!(
            session_summary.is_file(),
            "cancel must not delete the durable session"
        );
    }

    #[test]
    fn cancel_surfaces_a_substantive_later_registration_error_after_a_refused_candidate() {
        let home = tempfile::tempdir().expect("temporary Grok home");

        let refused_socket = home.path().join("leader-a.sock");
        let refused_listener = UnixListener::bind(&refused_socket).expect("refused leader socket");
        drop(refused_listener);
        fs::write(
            home.path().join("leader-a.lock"),
            std::process::id().to_string(),
        )
        .expect("refused leader lock");

        let malformed_socket = home.path().join("leader-b.sock");
        let malformed_listener =
            UnixListener::bind(&malformed_socket).expect("malformed leader socket");
        fs::write(
            home.path().join("leader-b.lock"),
            std::process::id().to_string(),
        )
        .expect("malformed leader lock");
        let malformed_thread = thread::spawn(move || {
            let (mut stream, _) = malformed_listener
                .accept()
                .expect("registration connection");
            let registration = read_frame(&mut stream)
                .expect("registration frame")
                .expect("registration body");
            assert_eq!(registration.1["type"], "register");
            stream
                .write_all(&framed(&json!({"type":"not_registered"})))
                .expect("malformed registration response");
        });

        let error = GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect_err("a malformed reachable candidate must not look already stopped");
        malformed_thread.join().expect("malformed leader thread");

        assert_eq!(
            error.to_string(),
            "command failed: Grok leader registration failed"
        );
    }

    #[test]
    fn cancel_without_discovered_leader_endpoints_is_already_stopped() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("temporary Grok home");
        let session_summary = home
            .path()
            .join("sessions/project/session-selected/summary.json");
        fs::create_dir_all(session_summary.parent().expect("session directory"))
            .expect("durable session directory");
        fs::write(
            &session_summary,
            json!({
                "info":{"id":"session-selected","cwd":"/home/user/project"},
                "session_summary":"Stopped session",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z"
            })
            .to_string(),
        )
        .expect("durable session summary");

        let binary = home.path().join("fake-grok");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf started > \"$GROK_HOME/start-marker\"\n",
        )
        .expect("fake Grok binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("executable fake Grok binary");

        GrokLifecycle::new(&binary, home.path())
            .cancel("session-selected")
            .expect("no discovered leader means the session is already stopped");

        assert!(
            !home.path().join("start-marker").exists(),
            "cancel must not start or restart the official leader"
        );
        assert!(
            session_summary.is_file(),
            "cancel must not delete the durable session"
        );
    }

    #[test]
    fn cancel_rejects_a_selected_session_without_a_reachable_owner() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let nonowner = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[{
                "sessionId":"session-sibling",
                "title":"Sibling",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
            false,
            false,
            false,
        );

        let error = GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect_err("a reachable nonowner must not make cancellation succeed");

        assert!(
            error
                .to_string()
                .contains("no reachable Grok leader owns session session-selected"),
            "unexpected cancellation error: {error:?}"
        );
        assert!(
            !acp_requests(&nonowner.captured())
                .iter()
                .any(|request| request["method"] == "session/cancel"),
            "a reachable nonowner must not receive cancel"
        );
    }

    #[test]
    fn cancel_is_sent_only_to_the_leader_that_owns_the_selected_session() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let first = ScriptedLeader::start_named(
            home.path(),
            "-a",
            json!({"sessions":[{
                "sessionId":"session-sibling",
                "title":"Sibling",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
            false,
            false,
            false,
        );
        let owner = ScriptedLeader::start_named(
            home.path(),
            "-b",
            json!({"sessions":[{
                "sessionId":"session-selected",
                "title":"Selected",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
            false,
            false,
            false,
        );

        GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect("owner scoped cancel");

        let first_cancels = acp_requests(&first.captured())
            .into_iter()
            .filter(|request| request["method"] == "session/cancel")
            .count();
        let owner_cancels = acp_requests(&owner.captured())
            .into_iter()
            .filter(|request| request["method"] == "session/cancel")
            .collect::<Vec<_>>();
        assert_eq!(
            first_cancels, 0,
            "a nonowner leader must not receive cancel"
        );
        assert_eq!(owner_cancels.len(), 1);
        assert_eq!(owner_cancels[0]["params"]["sessionId"], "session-selected");
    }

    #[test]
    fn cancel_skips_a_failing_first_leader_and_reaches_the_later_owner() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let failing = ScriptedLeader::start_with_roster_error(home.path(), "-a");
        let owner = ScriptedLeader::start_named(
            home.path(),
            "-b",
            json!({"sessions":[{
                "sessionId":"session-selected",
                "title":"Selected",
                "cwd":"/home/user/project",
                "activity":"working",
                "resident":true,
                "lastChangeUnixMs":1787289000000_i64
            }]}),
            false,
            false,
            false,
        );

        GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect("later healthy owner must receive cancel");
        let failing_requests = acp_requests(&failing.captured());
        assert!(
            failing_requests
                .iter()
                .any(|request| request["method"] == "initialize"),
            "first leader must initialize successfully before its roster failure"
        );
        assert!(
            failing_requests
                .iter()
                .any(|request| request["method"] == "_x.ai/sessions/list"),
            "first leader must fail during cancel ownership lookup"
        );
        assert!(
            !failing_requests
                .iter()
                .any(|request| request["method"] == "session/cancel"),
            "the failing nonowner must not receive cancel"
        );
        let cancels = acp_requests(&owner.captured())
            .into_iter()
            .filter(|request| request["method"] == "session/cancel")
            .collect::<Vec<_>>();
        assert_eq!(cancels.len(), 1);
        assert_eq!(cancels[0]["params"]["sessionId"], "session-selected");
    }

    #[test]
    fn conflicting_roster_activity_is_unknown_independent_of_leader_suffix_order() {
        let resolve = |first_activity: &str, second_activity: &str| {
            let home = tempfile::tempdir().expect("temporary Grok home");
            let roster = |activity: &str| {
                json!({"sessions":[{
                    "sessionId":"session-conflict",
                    "title":"Conflicting session",
                    "cwd":"/home/user/project",
                    "activity":activity,
                    "resident":true,
                    "lastChangeUnixMs":1787289000000_i64
                }]})
            };
            let _first = ScriptedLeader::start_named(
                home.path(),
                "-a",
                roster(first_activity),
                false,
                false,
                false,
            );
            let _second = ScriptedLeader::start_named(
                home.path(),
                "-b",
                roster(second_activity),
                false,
                false,
                false,
            );
            GrokLifecycle::new("/bin/true", home.path())
                .list()
                .expect("merged roster")
                .into_iter()
                .find(|row| row.id == "session-conflict")
                .expect("conflicting roster row")
                .status
        };

        let working_then_idle = resolve("working", "idle");
        let idle_then_working = resolve("idle", "working");
        assert_eq!(
            [working_then_idle, idle_then_working],
            [Status::Unknown, Status::Unknown]
        );
    }

    #[test]
    fn prompt_json_rpc_failure_is_surfaced_without_creating_a_second_session() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[]}),
            false,
            true,
            false,
        );
        let error = GrokLifecycle::new("/bin/true", home.path())
            .spawn(Path::new("/home/user/project"), "rejected prompt", None)
            .expect_err("the official prompt JSON RPC error must surface");
        assert!(
            error.to_string().contains("prompt rejected"),
            "prompt failure must retain the official error, got {error:?}"
        );
        let requests = acp_requests(&leader.captured());
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/new")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/prompt")
                .count(),
            1
        );
    }

    #[test]
    fn leader_error_text_strips_terminal_and_bidi_controls() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[]}),
            false,
            true,
            false,
        );
        let error = GrokLifecycle::new("/bin/true", home.path())
            .spawn(Path::new("/home/user/project"), "rejected prompt", None)
            .expect_err("hostile official leader error must surface safely");
        assert_terminal_safe("leader error", &error.to_string());
    }

    #[test]
    fn spawn_without_a_model_does_not_invent_registration_metadata() {
        let (home, leader) = leader_home();
        let result = GrokLifecycle::new("/bin/true", home.path())
            .spawn(Path::new("/home/user/project"), "use runtime default", None)
            .expect("default model Grok spawn");
        assert_eq!(result.session_id.as_deref(), Some("session-alpha"));

        let registration = leader
            .captured()
            .into_iter()
            .find(|frame| frame.body["type"] == "register")
            .expect("leader registration");
        assert!(registration.body["capabilities"]["default_model"].is_null());
    }

    #[test]
    fn rename_and_delete_require_raw_success_and_direct_extension_params() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let session_dir = home.path().join("sessions/project/session-selected");
        fs::create_dir_all(&session_dir).expect("durable session directory");
        fs::write(
            session_dir.join("summary.json"),
            serde_json::json!({
                "info":{"id":"session-selected","cwd":"/home/user/project"},
                "session_summary":"Selected durable session",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z",
                "num_messages":2,
                "current_model_id":"grok-4"
            })
            .to_string(),
        )
        .expect("durable selected summary");
        let leader = ScriptedLeader::start(home.path(), false);
        let lifecycle = GrokLifecycle::new("/bin/true", home.path());
        lifecycle
            .rename("session-selected", "A precise title")
            .expect("official rename");
        lifecycle
            .delete("session-selected")
            .expect("official delete");

        let requests = acp_requests(&leader.captured());
        let rename = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/session/rename")
            .unwrap();
        assert_eq!(
            rename["params"],
            json!({"sessionId":"session-selected", "title":"A precise title"})
        );
        assert!(rename["params"].get("method").is_none());
        assert!(rename["params"].get("params").is_none());
        let delete = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/session/delete")
            .unwrap();
        assert_eq!(
            delete["params"],
            json!({"sessionId":"session-selected", "cwd":"/home/user/project"})
        );
        assert!(delete["params"].get("method").is_none());
        assert!(delete["params"].get("params").is_none());
    }

    #[test]
    fn delete_uses_durable_cwd_in_official_direct_extension_params() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let session_dir = home.path().join("sessions/project/session-delete-cwd");
        fs::create_dir_all(&session_dir).expect("durable session directory");
        fs::write(
            session_dir.join("summary.json"),
            serde_json::json!({
                "info":{
                    "id":"session-delete-cwd",
                    "cwd":"/home/theconnman/git/curietech/agentos"
                },
                "session_summary":"Disposable delete target",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z",
                "num_messages":2,
                "current_model_id":"grok-4"
            })
            .to_string(),
        )
        .expect("durable delete summary");
        let leader = ScriptedLeader::start(home.path(), false);

        GrokLifecycle::new("/bin/true", home.path())
            .delete("session-delete-cwd")
            .expect("official delete request");

        let requests = acp_requests(&leader.captured());
        let delete = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/session/delete")
            .expect("official delete extension request");
        assert_eq!(
            delete["params"],
            json!({
                "sessionId":"session-delete-cwd",
                "cwd":"/home/theconnman/git/curietech/agentos"
            })
        );
        assert!(delete["params"].get("method").is_none());
        assert!(delete["params"].get("params").is_none());
    }

    #[test]
    fn raw_unsuccessful_mutation_response_is_rejected_as_a_failure() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let session_dir = home.path().join("sessions/project/session-rejected");
        fs::create_dir_all(&session_dir).expect("durable session directory");
        fs::write(
            session_dir.join("summary.json"),
            serde_json::json!({
                "info":{"id":"session-rejected","cwd":"/home/user/rejected"},
                "session_summary":"Rejected durable session",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z",
                "num_messages":2,
                "current_model_id":"grok-4"
            })
            .to_string(),
        )
        .expect("durable rejected summary");
        let leader = ScriptedLeader::start(home.path(), false);
        let error = GrokLifecycle::new("/bin/true", home.path())
            .delete("session-rejected")
            .expect_err("raw success false must reject the mutation");
        let requests = acp_requests(&leader.captured());
        let delete = requests
            .iter()
            .find(|request| request["method"] == "_x.ai/session/delete")
            .expect("official delete extension request");
        assert_eq!(
            delete["params"],
            json!({"sessionId":"session-rejected", "cwd":"/home/user/rejected"})
        );
        assert!(
            error.to_string().to_ascii_lowercase().contains("fail"),
            "an unsuccessful official mutation must report failure, got {error:?}"
        );
    }

    #[test]
    fn hostile_caller_ids_are_rejected_with_terminal_safe_mutation_errors() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        fs::create_dir(home.path().join("sessions")).expect("empty durable sessions root");
        let _leader = ScriptedLeader::start_named(
            home.path(),
            "",
            json!({"sessions":[]}),
            false,
            false,
            false,
        );
        let lifecycle = GrokLifecycle::new("/bin/true", home.path());
        let hostile_id = "session-\u{1b}]0;owned\u{0007}\u{202e}";
        let errors = [
            lifecycle.cancel(hostile_id),
            lifecycle.rename(hostile_id, "safe title"),
            lifecycle.delete(hostile_id),
        ];
        for result in errors {
            let error = result.expect_err("control bearing mutation identity must be rejected");
            assert_terminal_safe("mutation error", &error.to_string());
        }
    }

    #[test]
    fn symlinked_sessions_root_is_not_read_or_used_for_delete() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let external = tempfile::tempdir().expect("external sessions root");
        let session_dir = external.path().join("project/session-symlink-root");
        fs::create_dir_all(&session_dir).expect("external durable session directory");
        fs::write(
            session_dir.join("summary.json"),
            serde_json::json!({
                "info":{
                    "id":"session-symlink-root",
                    "cwd":"/home/user/external"
                },
                "session_summary":"External durable row",
                "created_at":"2026-08-20T01:02:03.004Z",
                "updated_at":"2026-08-21T04:05:06.007Z",
                "num_messages":2,
                "current_model_id":"grok-4"
            })
            .to_string(),
        )
        .expect("external durable summary");
        std::os::unix::fs::symlink(external.path(), home.path().join("sessions"))
            .expect("symlinked sessions root");
        let leader = ScriptedLeader::start(home.path(), false);
        let lifecycle = GrokLifecycle::new("/bin/true", home.path());

        let rows = lifecycle.list().expect("safe durable listing");
        let deletion = lifecycle.delete("session-symlink-root");
        let delete_requests = acp_requests(&leader.captured())
            .into_iter()
            .filter(|request| request["method"] == "_x.ai/session/delete")
            .count();
        assert!(
            !rows.iter().any(|row| row.id == "session-symlink-root"),
            "a symlinked sessions root must not expose durable records"
        );
        assert!(
            deletion.is_err(),
            "delete must reject a record reached through a symlinked sessions root"
        );
        assert_eq!(
            delete_requests, 0,
            "unsafe durable storage must be rejected before an extension request"
        );
    }

    #[test]
    fn oversized_leader_frame_is_rejected_at_the_official_cap() {
        let temp = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start(temp.path(), true);
        let error = GrokLifecycle::new("/bin/true", temp.path())
            .diagnostics()
            .expect_err("a frame over 64 MiB must be rejected");
        let message = error.to_string().to_ascii_lowercase();
        assert!(
            message.contains("64") || message.contains("frame") || message.contains("large"),
            "cap rejection must be actionable, got {message:?}"
        );
    }

    #[test]
    fn symlinked_leader_socket_is_not_discovered_or_connected() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let external_home = tempfile::tempdir().expect("external leader home");
        let _external = ScriptedLeader::start(external_home.path(), false);
        std::os::unix::fs::symlink(
            external_home.path().join("leader.sock"),
            home.path().join("leader.sock"),
        )
        .expect("symlinked leader socket");
        std::os::unix::fs::symlink(
            external_home.path().join("leader.lock"),
            home.path().join("leader.lock"),
        )
        .expect("symlinked leader lock");

        let diagnostics = GrokLifecycle::new("/bin/true", home.path())
            .diagnostics()
            .expect("unsafe candidate degrades to unavailable");
        assert_eq!(diagnostics.leader_count, 0);
        assert!(!diagnostics.registered);
        assert!(diagnostics.methods.is_empty());
    }

    #[test]
    fn malformed_leader_registration_degrades_to_unregistered_diagnostics() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let socket = home.path().join("leader.sock");
        fs::write(
            home.path().join("leader.lock"),
            std::process::id().to_string(),
        )
        .expect("leader lock");
        let listener = UnixListener::bind(&socket).expect("malformed leader socket");
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("diagnostic connection");
            let _ = read_frame(&mut stream).expect("registration request");
            stream
                .write_all(&framed(&json!({"type":"unexpected"})))
                .expect("malformed registration response");
        });

        let diagnostics = GrokLifecycle::new("/bin/true", home.path())
            .diagnostics()
            .expect("malformed leader must not fail diagnostics");
        thread.join().expect("malformed leader thread");
        assert_eq!(diagnostics.leader_count, 1);
        assert!(!diagnostics.registered);
        assert!(diagnostics.methods.is_empty());
    }

    #[test]
    fn lifecycle_requires_the_persistent_leader_and_never_starts_one() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(home.path(), "session-durable", &[]);
        let binary = home.path().join("fake-grok");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf invoked > \"$(dirname \"$0\")/invoked\"\n",
        )
        .expect("fake official Grok binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("executable fake Grok binary");

        let lifecycle = GrokLifecycle::new(&binary, home.path());
        let errors = [
            lifecycle
                .spawn(
                    Path::new("/home/user/project"),
                    "require persistent leader",
                    None,
                )
                .expect_err("missing persistent leader must refuse spawn"),
            lifecycle
                .rename("session-durable", "Persistent title")
                .expect_err("missing persistent leader must refuse rename"),
            lifecycle
                .delete("session-durable")
                .expect_err("missing persistent leader must refuse delete"),
        ];
        for error in errors {
            assert_eq!(
                error.to_string(),
                "command failed: persistent Grok leader is not reachable; start grok-agent-leader.service",
                "missing leader error must name the persistent service"
            );
        }
        assert!(
            !home.path().join("invoked").exists(),
            "agent-router must never start or replace the persistent leader"
        );
    }

    #[test]
    fn cancel_rejects_a_session_claimed_by_multiple_leaders() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let roster = json!({"sessions":[{
            "sessionId":"session-selected",
            "title":"Selected",
            "cwd":"/home/user/project",
            "activity":"working",
            "resident":true,
            "lastChangeUnixMs":1787289000000_i64
        }]});
        let first =
            ScriptedLeader::start_named(home.path(), "-a", roster.clone(), false, false, false);
        let second = ScriptedLeader::start_named(home.path(), "-b", roster, false, false, false);

        let error = GrokLifecycle::new("/bin/true", home.path())
            .cancel("session-selected")
            .expect_err("an ambiguous owner must not be cancelled");

        assert_eq!(
            error.to_string(),
            "command failed: multiple Grok leaders claim session session-selected"
        );
        for leader in [&first, &second] {
            assert!(
                !acp_requests(&leader.captured())
                    .iter()
                    .any(|request| request["method"] == "session/cancel"),
                "no leader may receive cancel when ownership is ambiguous"
            );
        }
    }

    #[test]
    fn rename_surfaces_a_leader_rejection_as_a_command_failure() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let _leader = ScriptedLeader::start(home.path(), false);
        let error = GrokLifecycle::new("/bin/true", home.path())
            .rename("session-other", "A different title")
            .expect_err("a rename the leader rejects must fail");
        assert!(
            error.to_string().starts_with("command failed: "),
            "a rejected rename must surface as a command failure, got {error:?}"
        );
    }

    #[test]
    fn registered_leader_reports_its_methods_and_probes_with_the_router_identity() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        let leader = ScriptedLeader::start(home.path(), false);
        let diagnostics = GrokLifecycle::new("/bin/true", home.path())
            .diagnostics()
            .expect("reachable leader diagnostics");

        assert_eq!(diagnostics.binary, Path::new("/bin/true"));
        assert_eq!(diagnostics.home, home.path());
        assert!(diagnostics.binary_available);
        assert_eq!(diagnostics.leader_count, 1);
        assert!(diagnostics.registered);
        for method in [
            "initialize",
            "session/new",
            "session/prompt",
            "session/cancel",
            "x.ai/sessions/list",
            "x.ai/session/rename",
            "x.ai/session/delete",
        ] {
            assert!(
                diagnostics.methods.iter().any(|known| known == method),
                "registered diagnostics must advertise {method}: {:?}",
                diagnostics.methods
            );
        }

        let frames = leader.captured();
        let register = frames
            .iter()
            .find(|frame| frame.body["type"] == "register")
            .expect("leader registration");
        assert_eq!(register.body["client_type"], "agent-router");
        let probe = frames
            .iter()
            .find(|frame| frame.body["type"] == "control")
            .expect("leader identity probe");
        assert_eq!(probe.body["command"], json!({"type":"get_leader_info"}));
        assert!(
            probe.body["request_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("agent-router-control-")),
            "probe request id must carry the agent-router prefix: {}",
            probe.body["request_id"]
        );
    }

    #[test]
    fn diagnostics_without_any_leader_are_unregistered() {
        let home = tempfile::tempdir().expect("empty Grok home");
        let diagnostics = GrokLifecycle::new(home.path().join("missing-grok"), home.path())
            .diagnostics()
            .expect("no leader is not an error");
        assert!(!diagnostics.binary_available);
        assert_eq!(diagnostics.leader_count, 0);
        assert!(!diagnostics.registered);
        assert!(diagnostics.methods.is_empty());
    }

    #[test]
    fn stale_refused_leader_socket_makes_lifecycle_actions_report_the_service() {
        let home = tempfile::tempdir().expect("temporary Grok home");
        write_durable_grok_session(home.path(), "session-durable", &[]);
        let stale_socket = home.path().join("leader.sock");
        drop(UnixListener::bind(&stale_socket).expect("stale leader socket"));
        fs::write(
            home.path().join("leader.lock"),
            std::process::id().to_string(),
        )
        .expect("stale leader lock");

        let error = GrokLifecycle::new("/bin/true", home.path())
            .spawn(Path::new("/home/user/project"), "stale leader", None)
            .expect_err("a refused leader socket must not spawn");
        assert_eq!(
            error.to_string(),
            "command failed: persistent Grok leader is not reachable; start grok-agent-leader.service"
        );
        let rows = GrokLifecycle::new("/bin/true", home.path())
            .list()
            .expect("listing degrades to durable rows when the leader is refused");
        assert!(rows.iter().any(|row| row.id == "session-durable"));
    }
}
