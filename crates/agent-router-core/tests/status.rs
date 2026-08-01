//! Commit 1 at the core level: the pure state classifier, the pure monotonicity rule, and the
//! codex reconciliation assembly driven over a scripted app-server rather than a live daemon.
//!
//! Nothing here opens the real decision log, and nothing here starts a codex daemon. Every
//! assertion drives a public function of `agent_router_core::status` or of the codex transport.
//!
//! The scripted RPC below is the same double `provider_contract_tests.rs` uses. A Rust integration
//! test is its own crate, so the type cannot be imported across the two files and is restated here
//! in the same shape rather than shared.

use agent_router_core::dispatch::codex::{CodexRpc, thread_states_on_rpc};
use agent_router_core::status::{Observation, State, classify, settle};
use agent_router_core::{Error, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

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

/// One `thread/read` reply in the shape the app-server returns it: a thread carrying a status
/// object, and the turn history the rollout on disk keeps whether or not the thread is loaded.
///
/// Observed on a real `thread/read` with `includeTurns` true: the turn array is nested inside
/// `thread`, and `result` carries the single key `thread`. The vendor schema agrees, listing
/// `thread` as the only property of `ThreadReadResponse`.
fn read_reply(id: i64, thread_status: &str, turn_statuses: &[&str]) -> String {
    let turns: Vec<Value> = turn_statuses
        .iter()
        .map(|status| json!({"status": status}))
        .collect();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "thread": {
                "id": "a read thread",
                "status": {"type": thread_status},
                "turns": turns,
            },
        }
    })
    .to_string()
}

/// What the assembly observed for one thread. A missing key is a row the fold dropped, which is a
/// failure in its own right: every id asked about must come back with an answer.
fn observation(states: &BTreeMap<String, Observation>, thread_id: &str) -> Observation {
    states
        .get(thread_id)
        .unwrap_or_else(|| panic!("no observation for {thread_id}, got {states:?}"))
        .clone()
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// The mapping table, one case per row, with every expected state written out rather than derived.
/// The two unrecognized strings are the guard that matters most: a backend shipping a new state
/// value must land on `unknown` rather than on whichever neighbour a catch all arm points at.
#[test]
fn a_pure_classifier_maps_every_known_backend_state_and_refuses_the_unknown_ones() {
    let cases: Vec<(Observation, State)> = vec![
        (
            Observation::ClaudeState("working".to_string()),
            State::Running,
        ),
        (
            Observation::ClaudeState("done".to_string()),
            State::Completed,
        ),
        // An operator killing a healthy job is not a routing failure, and `stopped` says nothing
        // about whether the work succeeded.
        (
            Observation::ClaudeState("stopped".to_string()),
            State::Unknown,
        ),
        (
            Observation::ClaudeState("quiescing".to_string()),
            State::Unknown,
        ),
        // The bounded recent window: absence is equally consistent with completed, crashed at
        // startup, and never started.
        (Observation::Absent, State::Unknown),
        (
            Observation::CodexTurn("completed".to_string()),
            State::Completed,
        ),
        (Observation::CodexTurn("failed".to_string()), State::Failed),
        (
            Observation::CodexTurn("interrupted".to_string()),
            State::Unknown,
        ),
        (
            Observation::CodexTurn("inProgress".to_string()),
            State::Running,
        ),
        (Observation::CodexTurn("queued".to_string()), State::Unknown),
        // The fallback, consulted only when there is no turn record at all.
        (
            Observation::CodexThread("systemError".to_string()),
            State::Failed,
        ),
        (
            Observation::CodexThread("notLoaded".to_string()),
            State::Unknown,
        ),
        (Observation::CodexThread("idle".to_string()), State::Unknown),
        (
            Observation::CodexThread("active".to_string()),
            State::Unknown,
        ),
        (Observation::Unavailable, State::Unknown),
        (Observation::Unsupported, State::Unknown),
    ];

    for (observation, want) in cases {
        assert_eq!(
            classify(observation.clone()),
            want,
            "{observation:?} must classify {want:?}"
        );
    }
}

/// The rule that keeps a proven fact from being regressed into ignorance, over the whole cross
/// product of stored outcome and fresh state. `unknown` writes over exactly three outcomes; every
/// other state always writes, so forward progress survives and a later reading of a live backend
/// always beats stored history.
#[test]
fn the_monotonicity_rule_admits_forward_progress_and_refuses_regression_to_unknown() {
    let currents = [
        "dispatched",
        "running",
        "completed",
        "failed",
        "unknown",
        "dry-run",
        "error: boom",
    ];

    // Writing `unknown` over `unknown` is intentional: it refreshes the reconciliation stamp, and
    // that column means the time of the last reading that landed.
    let overwritable = ["dispatched", "running", "unknown"];
    for current in currents {
        let want = if overwritable.contains(&current) {
            Some(State::Unknown)
        } else {
            None
        };
        assert_eq!(
            settle(current, State::Unknown),
            want,
            "an unknown reading over a stored {current:?}"
        );
    }

    for current in currents {
        for observed in [State::Running, State::Completed, State::Failed] {
            assert_eq!(
                settle(current, observed),
                Some(observed),
                "a {observed:?} reading over a stored {current:?} is fresh backend evidence"
            );
        }
    }

    // Named on its own because it reads like a bug to anyone who does not know why it is allowed.
    assert_eq!(
        settle("completed", State::Running),
        Some(State::Running),
        "a backend reporting the job running again is evidence, not a regression"
    );
}

/// The router calls `turn/start` exactly once, so turn index 0 is the routed job. A later turn is a
/// human continuing the thread by hand, and its outcome is a fact about that follow up work rather
/// than about where the task was sent. Reading the last turn inverts both halves of this test.
#[test]
fn the_first_turn_decides_the_codex_state_and_later_turns_do_not() {
    let mut rpc =
        ScriptedRpc::with_replies(vec![Ok(read_reply(2, "idle", &["completed", "failed"]))]);
    let states = thread_states_on_rpc(&mut rpc, &ids(&["a routed thread"]));
    assert_eq!(
        classify(observation(&states, "a routed thread")),
        State::Completed,
        "the routed turn completed and a human's later turn failed"
    );

    let mut rpc =
        ScriptedRpc::with_replies(vec![Ok(read_reply(2, "idle", &["failed", "completed"]))]);
    let states = thread_states_on_rpc(&mut rpc, &ids(&["a routed thread"]));
    assert_eq!(
        classify(observation(&states, "a routed thread")),
        State::Failed,
        "the routed turn failed and a human's later turn recovered it"
    );
}

/// The live observation this design was built around: the turn history comes from the rollout on
/// disk, so it survives the daemon never having loaded the thread. Reading `thread.status` first
/// would report `unknown` for a job that provably finished.
#[test]
fn a_notloaded_thread_with_a_completed_first_turn_reconciles_to_completed() {
    let mut rpc = ScriptedRpc::with_replies(vec![Ok(read_reply(2, "notLoaded", &["completed"]))]);

    let states = thread_states_on_rpc(&mut rpc, &ids(&["an unloaded thread"]));

    assert_eq!(
        classify(observation(&states, "an unloaded thread")),
        State::Completed,
        "a turn record saying completed is proof, whatever the daemon has loaded"
    );
}

/// The fallback proves a fault and nothing else. `idle` in particular is reached by an interrupted
/// thread, by a thread whose turn errored, and by a thread that never received its turn, so it
/// cannot mean the work succeeded.
#[test]
fn a_codex_thread_with_no_turn_record_never_reports_completed() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Ok(read_reply(2, "idle", &[])),
        Ok(read_reply(3, "active", &[])),
        Ok(read_reply(4, "notLoaded", &[])),
        Ok(read_reply(5, "systemError", &[])),
    ]);

    let states = thread_states_on_rpc(
        &mut rpc,
        &ids(&[
            "an idle thread",
            "an active thread",
            "an unloaded thread",
            "a faulted thread",
        ]),
    );

    for thread_id in ["an idle thread", "an active thread", "an unloaded thread"] {
        assert_eq!(
            classify(observation(&states, thread_id)),
            State::Unknown,
            "{thread_id} has no turn record, so nothing about it is proven"
        );
    }
    assert_eq!(
        classify(observation(&states, "a faulted thread")),
        State::Failed,
        "the daemon reporting the thread itself faulted is a fault of the dispatch"
    );

    for thread_id in states.keys() {
        assert_ne!(
            classify(observation(&states, thread_id)),
            State::Completed,
            "{thread_id} reached completed with no turn record to prove it"
        );
    }
}

/// A codex leg with nothing answering is a partial report, never a command failure and never a
/// window of `unknown` writes justified by an outage. Every id asked about comes back, every one
/// of them says why, and a claude observation read in the same window still classifies normally.
#[test]
fn codex_reconciliation_reports_unavailable_for_every_row_when_no_daemon_answers() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Err(Error::Command("no daemon answered".to_string())),
        Err(Error::Command("no daemon answered".to_string())),
        Err(Error::Command("no daemon answered".to_string())),
    ]);

    let asked = ids(&["first thread", "second thread", "third thread"]);
    let states = thread_states_on_rpc(&mut rpc, &asked);

    assert_eq!(
        states.len(),
        asked.len(),
        "an unreachable daemon dropped rows instead of reporting them: {states:?}"
    );
    for thread_id in &asked {
        assert_eq!(
            observation(&states, thread_id),
            Observation::Unavailable,
            "{thread_id} must say the router could not ask"
        );
        assert_eq!(classify(observation(&states, thread_id)), State::Unknown);
    }

    assert_eq!(
        classify(Observation::ClaudeState("done".to_string())),
        State::Completed,
        "a codex outage must not cost a claude row in the same window its reconciliation"
    );
}

/// The fold collects per row rather than propagating with `?`, so one bad thread costs only itself.
#[test]
fn one_erroring_thread_read_affects_only_its_own_row() {
    let mut rpc = ScriptedRpc::with_replies(vec![
        Ok(read_reply(2, "idle", &["completed"])),
        Err(Error::Command("thread read rejected".to_string())),
        Ok(read_reply(4, "idle", &["failed"])),
    ]);

    let states = thread_states_on_rpc(
        &mut rpc,
        &ids(&["first thread", "second thread", "third thread"]),
    );

    assert_eq!(
        classify(observation(&states, "first thread")),
        State::Completed
    );
    assert_eq!(
        observation(&states, "second thread"),
        Observation::Unavailable,
        "the erroring read must report itself as unasked, not as an outcome"
    );
    assert_eq!(
        classify(observation(&states, "second thread")),
        State::Unknown
    );
    assert_eq!(
        classify(observation(&states, "third thread")),
        State::Failed,
        "the read after the failing one still ran and reported its own outcome, so the fold \
         carried on past the error"
    );

    // One read per row, and the request ids count from 2 because the handshake owns id 1.
    assert_eq!(rpc.requests.len(), 3);
    for (offset, request) in rpc.requests.iter().enumerate() {
        assert_eq!(request["method"], "thread/read");
        assert_eq!(request["id"], offset as i64 + 2);
    }
}
