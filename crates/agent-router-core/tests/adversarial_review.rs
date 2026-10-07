use agent_router_core::adversarial_review::{
    ReviewOutcome, ReviewProvider, ReviewRequest, ReviewStatus, ReviewerPin,
    review_pinned_with_providers, review_with_providers, reviewer_pin,
};
use agent_router_core::log::{DecisionLog, ReviewEntry};
use agent_router_core::{Error, Provider, Result};
use std::cell::Cell;
use std::path::Path;

const PRIORITY: [Provider; 3] = [Provider::Codex, Provider::Grok, Provider::Claude];

struct StubReviewer<'a> {
    provider: &'a str,
    model: &'a str,
    availability: std::result::Result<(), &'a str>,
    result: Result<&'a str>,
    calls: Cell<usize>,
}

impl<'a> StubReviewer<'a> {
    fn successful(provider: &'a str, model: &'a str, result: &'a str) -> Self {
        Self {
            provider,
            model,
            availability: Ok(()),
            result: Ok(result),
            calls: Cell::new(0),
        }
    }

    fn failing_with(provider: &'a str, model: &'a str, message: &'a str) -> Self {
        Self {
            provider,
            model,
            availability: Ok(()),
            result: Err(Error::Command(message.to_string())),
            calls: Cell::new(0),
        }
    }

    fn unavailable(provider: &'a str, model: &'a str, reason: &'a str) -> Self {
        Self {
            provider,
            model,
            availability: Err(reason),
            result: Ok("must not run"),
            calls: Cell::new(0),
        }
    }
}

impl ReviewProvider for StubReviewer<'_> {
    fn provider_name(&self) -> &str {
        self.provider
    }

    fn reviewer_model(&self) -> &str {
        self.model
    }

    fn authoritative_availability(&self) -> std::result::Result<(), String> {
        self.availability.map_err(str::to_string)
    }

    fn review(&self, request: &ReviewRequest<'_>) -> Result<String> {
        self.calls.set(self.calls.get() + 1);
        assert_eq!(request.body, "Review this working tree for regressions");
        assert_eq!(request.dir, Path::new("/tmp/review target"));
        self.result
            .as_ref()
            .map(|body| (*body).to_string())
            .map_err(|error| match error {
                Error::Command(message) => Error::Command(message.clone()),
                other => Error::Command(other.to_string()),
            })
    }
}

fn request<'a>(primary_provider: &'a str) -> ReviewRequest<'a> {
    ReviewRequest {
        primary_provider,
        body: "Review this working tree for regressions",
        dir: Path::new("/tmp/review target"),
    }
}

fn pin(provider: Provider, model: Option<&str>) -> ReviewerPin {
    ReviewerPin {
        provider,
        model: model.map(str::to_string),
    }
}

/// A review that did not complete may surface as an `Err` or as a non-completed outcome; either
/// way the user sees no review body.
fn assert_not_completed(result: &Result<ReviewOutcome>) {
    if let Ok(outcome) = result {
        assert_ne!(outcome.status, ReviewStatus::Completed, "{outcome:?}");
        assert_eq!(outcome.result, None);
    }
}

#[test]
fn candidates_follow_priority_order_minus_the_primary() {
    let codex = StubReviewer::successful("codex", "gpt", "wrong");
    let grok = StubReviewer::successful("grok", "default", "grok review");
    let claude = StubReviewer::successful("claude", "opus", "wrong");

    // Registration order deliberately differs from the priority order.
    let outcome = review_with_providers(&request("codex"), &[&claude, &codex, &grok], &PRIORITY)
        .expect("grok completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.primary_provider, "codex");
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("grok"));
    assert_eq!(outcome.reviewer_model.as_deref(), Some("default"));
    assert_eq!(outcome.result.as_deref(), Some("grok review"));
    assert_eq!(outcome.fallback_from, None);
    assert_eq!(outcome.usage, None);
    assert_eq!(outcome.requested_provider, None);
    assert_eq!(outcome.requested_model, None);
    assert_eq!(codex.calls.get(), 0, "the primary provider was invoked");
    assert_eq!(grok.calls.get(), 1);
    assert_eq!(claude.calls.get(), 0);
    assert!(outcome.rationale.contains("codex"), "{}", outcome.rationale);
    assert!(outcome.rationale.contains("grok"), "{}", outcome.rationale);

    // With claude primary the first candidate is codex.
    let codex = StubReviewer::successful("codex", "gpt", "codex review");
    let grok = StubReviewer::successful("grok", "default", "wrong");
    let claude = StubReviewer::successful("claude", "opus", "wrong");
    let outcome = review_with_providers(&request("claude"), &[&claude, &grok, &codex], &PRIORITY)
        .expect("codex completes");
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("codex"));
    assert_eq!(codex.calls.get(), 1);
    assert_eq!(grok.calls.get(), 0);
    assert_eq!(claude.calls.get(), 0);
}

#[test]
fn a_custom_priority_is_respected() {
    let grok = StubReviewer::successful("grok", "default", "wrong");
    let claude = StubReviewer::successful("claude", "opus", "claude review");

    let outcome = review_with_providers(
        &request("codex"),
        &[&grok, &claude],
        &[Provider::Claude, Provider::Grok, Provider::Codex],
    )
    .expect("claude completes");

    assert_eq!(outcome.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(claude.calls.get(), 1);
    assert_eq!(grok.calls.get(), 0);
}

#[test]
fn a_rate_limited_candidate_fails_over_to_the_next() {
    let codex = StubReviewer::failing_with(
        "codex",
        "gpt",
        "codex review failed: rate limit exceeded, usage quota reached",
    );
    let grok = StubReviewer::successful("grok", "default", "grok review");
    let claude = StubReviewer::successful("claude", "opus", "must not run");

    let outcome = review_with_providers(&request("claude"), &[&codex, &grok, &claude], &PRIORITY)
        .expect("failover completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("grok"));
    assert_eq!(outcome.fallback_from.as_deref(), Some("codex"));
    assert_eq!(outcome.result.as_deref(), Some("grok review"));
    assert_eq!(codex.calls.get(), 1);
    assert_eq!(grok.calls.get(), 1);
    assert_eq!(claude.calls.get(), 0);
    assert!(outcome.rationale.contains("codex"), "{}", outcome.rationale);
    assert!(
        outcome.rationale.contains("rate limit exceeded"),
        "{}",
        outcome.rationale
    );
    assert!(outcome.rationale.contains("grok"), "{}", outcome.rationale);

    let codex_row = outcome
        .usage_provenance
        .iter()
        .find(|candidate| candidate.provider == "codex")
        .expect("the failed candidate is in the provenance");
    assert!(!codex_row.eligible);
    assert!(
        codex_row
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("rate limit"))
    );
    let grok_row = outcome
        .usage_provenance
        .iter()
        .find(|candidate| candidate.provider == "grok")
        .expect("the completing candidate is in the provenance");
    assert!(grok_row.eligible);
    assert!(
        outcome
            .usage_provenance
            .iter()
            .all(|candidate| candidate.provider != "claude"),
        "the primary is never a candidate"
    );
}

#[test]
fn fallback_from_names_the_candidate_immediately_before_the_completing_one() {
    let codex = StubReviewer::failing_with("codex", "gpt", "usage limit reached");
    let grok = StubReviewer::failing_with("grok", "default", "authentication failed");
    let claude = StubReviewer::successful("claude", "opus", "claude review");

    let outcome = review_with_providers(&request("gemini"), &[&codex, &grok, &claude], &PRIORITY)
        .expect("third candidate completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.fallback_from.as_deref(), Some("grok"));
    assert!(
        outcome.rationale.contains("usage limit reached"),
        "{}",
        outcome.rationale
    );
    assert!(
        outcome.rationale.contains("authentication failed"),
        "{}",
        outcome.rationale
    );
    assert_eq!(codex.calls.get(), 1);
    assert_eq!(grok.calls.get(), 1);
    assert_eq!(claude.calls.get(), 1);
}

#[test]
fn an_authoritative_availability_refusal_skips_to_the_next_candidate() {
    let codex = StubReviewer::unavailable("codex", "gpt", "codex binary not found");
    let grok = StubReviewer::successful("grok", "default", "grok review");

    let outcome = review_with_providers(&request("claude"), &[&codex, &grok], &PRIORITY)
        .expect("grok completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("grok"));
    assert_eq!(outcome.fallback_from.as_deref(), Some("codex"));
    assert_eq!(codex.calls.get(), 0, "an unavailable reviewer was invoked");
    assert_eq!(grok.calls.get(), 1);
    assert!(
        outcome.rationale.contains("codex binary not found"),
        "{}",
        outcome.rationale
    );
}

#[test]
fn a_completed_review_with_findings_never_fails_over() {
    let codex = StubReviewer::successful(
        "codex",
        "gpt",
        "BLOCKING: the migration drops data. Verdict: REQUEST-CHANGES",
    );
    let grok = StubReviewer::successful("grok", "default", "must not run");

    let outcome = review_with_providers(&request("claude"), &[&codex, &grok], &PRIORITY)
        .expect("codex completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("codex"));
    assert_eq!(outcome.fallback_from, None);
    assert!(
        outcome
            .result
            .as_deref()
            .is_some_and(|body| body.contains("REQUEST-CHANGES"))
    );
    assert_eq!(codex.calls.get(), 1);
    assert_eq!(grok.calls.get(), 0);
}

#[test]
fn when_every_candidate_fails_the_outcome_is_failed_and_lists_each_error() {
    let codex = StubReviewer::failing_with("codex", "gpt", "rate limit exceeded");
    let grok = StubReviewer::unavailable("grok", "default", "grok is not installed");
    let claude = StubReviewer::failing_with("claude", "opus", "claude exited 1");

    let outcome = review_with_providers(&request("gemini"), &[&codex, &grok, &claude], &PRIORITY)
        .expect("exhausting the list is a reported failure");

    assert_eq!(outcome.status, ReviewStatus::Failed);
    assert_eq!(outcome.result, None);
    let reason = outcome.reason.as_deref().unwrap_or_default();
    for expected in [
        "codex: rate limit exceeded",
        "grok: grok is not installed",
        "claude: claude exited 1",
    ] {
        assert!(
            reason.contains(expected),
            "{expected} missing from {reason}"
        );
    }
    assert_eq!(codex.calls.get(), 1);
    assert_eq!(grok.calls.get(), 0);
    assert_eq!(claude.calls.get(), 1);
}

#[test]
fn a_primary_only_registry_has_no_candidate_and_invokes_nothing() {
    let codex = StubReviewer::successful("codex", "gpt", "wrong");

    let outcome = review_with_providers(&request("codex"), &[&codex], &PRIORITY)
        .expect("no candidate is a reported skip");

    assert_eq!(outcome.status, ReviewStatus::Skipped, "{outcome:?}");
    assert_eq!(outcome.status.exit_status(), 3);
    assert_eq!(outcome.result, None);
    assert_eq!(codex.calls.get(), 0);
}

#[test]
fn when_every_candidate_is_unavailable_the_outcome_is_skipped_and_names_each_reason() {
    let codex = StubReviewer::successful("codex", "gpt", "wrong");
    let grok = StubReviewer::unavailable("grok", "default", "grok is not installed");
    let claude = StubReviewer::unavailable("claude", "opus", "claude binary not found");

    let outcome = review_with_providers(&request("codex"), &[&codex, &grok, &claude], &PRIORITY)
        .expect("nothing could run is a reported skip");

    assert_eq!(outcome.status, ReviewStatus::Skipped, "{outcome:?}");
    assert_eq!(outcome.status.exit_status(), 3);
    assert_eq!(outcome.result, None);
    let reason = outcome.reason.as_deref().unwrap_or_default();
    for expected in ["grok is not installed", "claude binary not found"] {
        assert!(
            reason.contains(expected),
            "{expected} missing from {reason}"
        );
    }
    assert_eq!(codex.calls.get(), 0);
    assert_eq!(grok.calls.get(), 0);
    assert_eq!(claude.calls.get(), 0);
}

#[test]
fn an_empty_body_fails_without_invoking_any_provider() {
    for body in ["", "  \n\t "] {
        let codex = StubReviewer::successful("codex", "gpt", "must not run");
        let grok = StubReviewer::successful("grok", "default", "must not run");
        let request = ReviewRequest {
            primary_provider: "claude",
            body,
            dir: Path::new("/tmp/review target"),
        };

        let outcome = review_with_providers(&request, &[&codex, &grok], &PRIORITY)
            .expect("an empty body is a reported failure");
        assert_eq!(outcome.status, ReviewStatus::Failed, "{outcome:?}");
        let reason = outcome.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("review request body is empty"), "{reason}");
        assert_eq!(codex.calls.get() + grok.calls.get(), 0);

        let outcome =
            review_pinned_with_providers(&request, &[&codex, &grok], &pin(Provider::Grok, None))
                .expect("an empty body is a reported failure");
        assert_eq!(outcome.status, ReviewStatus::Failed, "{outcome:?}");
        let reason = outcome.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("review request body is empty"), "{reason}");
        assert_eq!(codex.calls.get() + grok.calls.get(), 0);
    }
}

#[test]
fn a_cancelled_review_does_not_fail_over() {
    let codex = StubReviewer::failing_with("codex", "gpt", "review cancelled");
    let grok = StubReviewer::successful("grok", "default", "must not run");

    let result = review_with_providers(&request("claude"), &[&codex, &grok], &PRIORITY);

    assert_not_completed(&result);
    match &result {
        Err(error) => assert!(error.to_string().contains("cancelled"), "{error}"),
        Ok(outcome) => assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("cancelled")),
            "{outcome:?}"
        ),
    }
    assert_eq!(codex.calls.get(), 1);
    assert_eq!(
        grok.calls.get(),
        0,
        "a cancel fell through to the next reviewer"
    );
}

#[test]
fn a_pin_runs_only_the_pinned_provider_even_when_it_is_last_in_priority() {
    let codex = StubReviewer::successful("codex", "gpt", "must not run");
    let grok = StubReviewer::successful("grok", "default", "must not run");
    let claude = StubReviewer::successful("claude", "fable", "fable review");

    let outcome = review_pinned_with_providers(
        &request("gemini"),
        &[&codex, &grok, &claude],
        &pin(Provider::Claude, Some("fable")),
    )
    .expect("the pin completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.requested_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.requested_model.as_deref(), Some("fable"));
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.reviewer_model.as_deref(), Some("fable"));
    assert_eq!(outcome.result.as_deref(), Some("fable review"));
    assert_eq!(outcome.fallback_from, None);
    assert!(outcome.rationale.contains("requested explicitly"));
    assert_eq!(codex.calls.get(), 0);
    assert_eq!(grok.calls.get(), 0);
    assert_eq!(claude.calls.get(), 1);
}

#[test]
fn explicit_pin_without_a_model_records_the_registered_model_as_the_actual_one() {
    let pinned = StubReviewer::successful("claude", "opus[1m]", "review");

    let outcome =
        review_pinned_with_providers(&request("codex"), &[&pinned], &pin(Provider::Claude, None))
            .expect("completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.requested_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.requested_model, None);
    assert_eq!(outcome.reviewer_model.as_deref(), Some("opus[1m]"));
}

#[test]
fn a_failing_pin_does_not_fail_over() {
    let codex = StubReviewer::successful("codex", "gpt", "must not run");
    let pinned = StubReviewer::failing_with("claude", "fable", "rate limit exceeded");

    let result = review_pinned_with_providers(
        &request("grok"),
        &[&codex, &pinned],
        &pin(Provider::Claude, Some("fable")),
    );

    assert_not_completed(&result);
    if let Ok(outcome) = &result {
        assert_eq!(outcome.fallback_from, None);
    }
    assert_eq!(pinned.calls.get(), 1);
    assert_eq!(codex.calls.get(), 0, "the pin failed over");
}

#[test]
fn an_unavailable_pin_does_not_fail_over() {
    let codex = StubReviewer::successful("codex", "gpt", "must not run");
    let pinned = StubReviewer::unavailable("grok", "default", "grok is not installed");

    let result = review_pinned_with_providers(
        &request("claude"),
        &[&codex, &pinned],
        &pin(Provider::Grok, None),
    );

    assert_not_completed(&result);
    let outcome = result.expect("an unavailable pin is a reported failure");
    assert_eq!(outcome.status, ReviewStatus::Failed, "pins never skip");
    assert_eq!(pinned.calls.get(), 0);
    assert_eq!(codex.calls.get(), 0, "the pin failed over");
}

#[test]
fn a_pin_naming_an_unregistered_reviewer_fails_without_invocation() {
    let only = StubReviewer::successful("claude", "fable", "wrong");

    let outcome =
        review_pinned_with_providers(&request("codex"), &[&only], &pin(Provider::Grok, None))
            .expect("an unregistered pin is a reported failure");

    assert_eq!(outcome.status, ReviewStatus::Failed);
    assert_eq!(outcome.requested_provider.as_deref(), Some("grok"));
    assert_eq!(outcome.reviewer_provider, None);
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("grok") && reason.contains("not registered")),
        "{:?}",
        outcome.reason
    );
    assert_eq!(only.calls.get(), 0);
}

#[test]
fn a_registered_reviewer_that_would_run_a_different_model_than_requested_is_refused() {
    let substituted = StubReviewer::successful("claude", "opus[1m]", "wrong");

    let outcome = review_pinned_with_providers(
        &request("codex"),
        &[&substituted],
        &pin(Provider::Claude, Some("fable")),
    )
    .expect("a model mismatch is a reported failure");

    assert_eq!(outcome.status, ReviewStatus::Failed);
    assert_eq!(outcome.requested_model.as_deref(), Some("fable"));
    assert_eq!(outcome.reviewer_model, None);
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("fable") && reason.contains("opus[1m]")),
        "{:?}",
        outcome.reason
    );
    assert_eq!(
        substituted.calls.get(),
        0,
        "a substituted model was invoked"
    );
}

#[test]
fn reviewer_pin_validation_rejects_the_primary_orphan_models_and_malformed_models() {
    assert_eq!(reviewer_pin("codex", None, None).expect("automatic"), None);
    assert_eq!(
        reviewer_pin("codex", Some(Provider::Claude), Some("fable")).expect("a valid pin"),
        Some(pin(Provider::Claude, Some("fable")))
    );
    assert_eq!(
        reviewer_pin("codex", Some(Provider::Grok), None).expect("grok without a model"),
        Some(pin(Provider::Grok, None))
    );

    let same = reviewer_pin("codex", Some(Provider::Codex), None).expect_err("primary pin");
    assert!(same.to_string().contains("primary"), "{same}");
    let same = reviewer_pin("claude", Some(Provider::Claude), Some("fable"))
        .expect_err("primary pin with a model");
    assert!(same.to_string().contains("primary"), "{same}");

    let orphan = reviewer_pin("codex", None, Some("fable")).expect_err("model without provider");
    assert!(
        orphan
            .to_string()
            .contains("--model requires an explicit --provider"),
        "{orphan}"
    );

    let grok = reviewer_pin("codex", Some(Provider::Grok), Some("grok-4"))
        .expect_err("grok has no model selection");
    assert!(grok.to_string().contains("grok"), "{grok}");

    for malformed in ["", "  ", "fable opus", "--bg", "-p", "fa\nble"] {
        match reviewer_pin("codex", Some(Provider::Claude), Some(malformed)) {
            Err(error) => assert!(error.to_string().contains("model"), "{error}"),
            Ok(accepted) => panic!("model {malformed:?} was accepted as {accepted:?}"),
        }
    }
}

#[test]
fn a_failover_review_persists_fallback_from() {
    let codex = StubReviewer::failing_with("codex", "gpt", "rate limit exceeded");
    let grok = StubReviewer::successful("grok", "default", "grok review");

    let outcome = review_with_providers(&request("claude"), &[&codex, &grok], &PRIORITY)
        .expect("failover completes");

    let dir = tempfile::tempdir().expect("tempdir");
    let log = DecisionLog::open_at(&dir.path().join("router.db")).expect("opens");
    let outcome_json = serde_json::to_string(&outcome).ok();
    log.record_review(&ReviewEntry {
        exit_status: outcome.status.exit_status(),
        primary: &outcome.primary_provider,
        reviewer_provider: outcome.reviewer_provider.as_deref(),
        reviewer_model: outcome.reviewer_model.as_deref(),
        usage_provenance: "[]",
        rationale: &outcome.rationale,
        body_bytes: i64::try_from(outcome.result.as_deref().map_or(0, str::len)).unwrap(),
        dir: Path::new("/tmp"),
        outcome_json: outcome_json.as_deref(),
        reason: outcome.reason.as_deref(),
        fallback_from: outcome.fallback_from.as_deref(),
    })
    .expect("records the failover review");
    let row = &log.recent_reviews(1).expect("reads")[0];
    assert_eq!(row.exit_status, 0);
    assert_eq!(row.fallback_from.as_deref(), Some("codex"));
}

/// The Grok review lane against a scripted leader that behaves like the real one: a session that
/// still has `ask_user_question` calls it, and with no human attached the roster then reports the
/// session as `needs_input`. Reviews failed that way before the review session removed the tool.
#[cfg(target_os = "linux")]
mod grok_review_needs_input {
    use super::*;
    use agent_router_core::Context;
    use agent_router_core::adversarial_review::review_registered_pinned;
    use agent_router_core::binary::{Environment, GROK_BIN_ENV};
    use agent_router_core::config::Config;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    const SESSION: &str = "review-session";
    const REVIEW_BODY: &str = "No findings. Assumed the request meant the whole working tree.";

    #[derive(Default)]
    struct LeaderState {
        /// Whether the session will call `ask_user_question`, decided when it is created and seen
        /// by every later connection, since each roster poll arrives on a fresh one.
        asks: bool,
        session_new: Vec<Value>,
        deleted: bool,
        prompted: bool,
        lists_after_prompt: usize,
    }

    struct FakeLeader {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        state: Arc<Mutex<LeaderState>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FakeLeader {
        /// `honors_profile` false models a leader that ignores the review's agent profile, so the
        /// session asks its question whatever the request carried.
        fn start(grok_home: &Path, honors_profile: bool) -> FakeLeader {
            fs::create_dir_all(grok_home).expect("grok home");
            let socket = grok_home.join("leader.sock");
            fs::write(
                grok_home.join("leader.lock"),
                std::process::id().to_string(),
            )
            .expect("leader lock");
            let listener = UnixListener::bind(&socket).expect("leader socket");
            listener.set_nonblocking(true).expect("nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let state = Arc::new(Mutex::new(LeaderState {
                asks: !honors_profile,
                ..LeaderState::default()
            }));
            let (thread_stop, thread_state) = (Arc::clone(&stop), Arc::clone(&state));
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &thread_state),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            FakeLeader {
                socket,
                stop,
                state,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeLeader {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.socket);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn read_frame(stream: &mut UnixStream) -> Option<Value> {
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).ok()?;
        let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
        stream.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    fn write_frame(stream: &mut UnixStream, value: &Value) -> bool {
        let body = serde_json::to_vec(value).expect("frame");
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&body);
        stream.write_all(&bytes).is_ok()
    }

    /// Whether the real leader would leave `ask_user_question` in a session created with `params`.
    fn session_keeps_ask_tool(params: &Value) -> bool {
        params
            .pointer("/_meta/agentProfile/disallowedTools")
            .and_then(Value::as_array)
            .is_none_or(|tools| !tools.iter().any(|tool| tool == "ask_user_question"))
    }

    fn serve(mut stream: UnixStream, state: &Mutex<LeaderState>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        while let Some(outer) = read_frame(&mut stream) {
            let reply = match outer["type"].as_str() {
                Some("register") => json!({
                    "type": "registered",
                    "ready": true,
                    "leader_protocol_version": 1,
                    "leader_capabilities": {"control_v1": true},
                }),
                Some("control") => json!({
                    "type": "control_result",
                    "request_id": outer["request_id"],
                    "result": {"Ok": {"type": "leader_info"}},
                }),
                Some("acp") => {
                    let request: Value =
                        serde_json::from_str(outer["payload"].as_str().unwrap_or("")).unwrap();
                    let Some(id) = request.get("id").cloned() else {
                        continue;
                    };
                    let mut state = state.lock().expect("leader state");
                    let result = match request["method"].as_str().unwrap_or("") {
                        "session/new" => {
                            state.asks |= session_keeps_ask_tool(&request["params"]);
                            state.session_new.push(request["params"].clone());
                            json!({"sessionId": SESSION})
                        }
                        "session/prompt" => {
                            state.prompted = true;
                            continue;
                        }
                        "_x.ai/sessions/list" if state.prompted => {
                            state.lists_after_prompt += 1;
                            let activity = match state.lists_after_prompt {
                                1 => "working",
                                _ if state.asks => "needs_input",
                                _ => "idle",
                            };
                            json!({"result": {"sessions": [{
                                "sessionId": SESSION,
                                "cwd": "/review/target",
                                "activity": activity,
                                "resident": true,
                                "lastChangeUnixMs": 1_791_000_000_000_i64,
                            }]}})
                        }
                        "_x.ai/sessions/list" => json!({"result": {"sessions": []}}),
                        "_x.ai/session/delete" => {
                            state.deleted = true;
                            json!({"success": true})
                        }
                        _ => json!({}),
                    };
                    json!({
                        "type": "acp",
                        "payload": json!({"jsonrpc": "2.0", "id": id, "result": result})
                            .to_string(),
                    })
                }
                _ => return,
            };
            if !write_frame(&mut stream, &reply) {
                return;
            }
        }
    }

    /// The durable record the leader writes for a session: the summary the lifecycle reads and the
    /// chat history the review body is taken from.
    fn durable_session(grok_home: &Path) {
        let dir = grok_home.join("sessions/review-target").join(SESSION);
        fs::create_dir_all(&dir).expect("durable session");
        fs::write(
            dir.join("summary.json"),
            json!({
                "info": {"id": SESSION, "cwd": "/review/target"},
                "session_summary": "Review",
                "created_at": "2026-10-07T01:00:00.000Z",
                "updated_at": "2026-10-07T01:00:05.000Z",
            })
            .to_string(),
        )
        .expect("summary");
        fs::write(
            dir.join("chat_history.jsonl"),
            format!(
                "{}\n",
                json!({"type": "assistant", "content": REVIEW_BODY, "tool_calls": []})
            ),
        )
        .expect("history");
    }

    fn run_review(honors_profile: bool) -> (ReviewOutcome, Arc<Mutex<LeaderState>>) {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        let grok_home = home.join(".grok");
        let target = root.path().join("target");
        fs::create_dir_all(&target).expect("target");
        durable_session(&grok_home);
        let leader = FakeLeader::start(&grok_home, honors_profile);
        let environment = Environment::new(
            None,
            Some(home.clone()),
            BTreeMap::from([(GROK_BIN_ENV.to_string(), OsString::from("/bin/true"))]),
        );
        let ctx = Context::new(environment, home, Config::default())
            .with_claude_usage_cache(root.path().join("claude-usage.json"))
            .with_grok_usage_cache(root.path().join("grok-usage.json"));
        let request = ReviewRequest {
            primary_provider: "codex",
            body: "Before starting, use ask_user_question to ask which files to review.",
            dir: &target,
        };
        let outcome = review_registered_pinned(&request, &pin(Provider::Grok, None), &ctx);
        let state = Arc::clone(&leader.state);
        drop(leader);
        (outcome, state)
    }

    #[test]
    fn a_review_session_is_created_without_ask_user_question_and_completes() {
        let (outcome, state) = run_review(true);

        assert_eq!(outcome.status, ReviewStatus::Completed, "{outcome:?}");
        assert_eq!(outcome.result.as_deref(), Some(REVIEW_BODY));
        let state = state.lock().expect("leader state");
        assert_eq!(state.session_new.len(), 1);
        assert_eq!(
            state.session_new[0]["_meta"]["agentProfile"]["disallowedTools"],
            json!(["ask_user_question"])
        );
        assert_eq!(
            state.session_new[0]["_meta"]["agentProfile"]["promptMode"], "extend",
            "the profile must extend the default agent rather than replace its prompt"
        );
        assert!(state.deleted, "a completed review deletes its session");
    }

    #[test]
    fn a_review_session_that_still_needs_input_fails_and_is_cleaned_up() {
        let (outcome, state) = run_review(false);

        assert_eq!(outcome.status, ReviewStatus::Failed, "{outcome:?}");
        assert_eq!(outcome.result, None);
        let reason = outcome.reason.as_deref().unwrap_or_default();
        assert!(
            reason.contains(&format!("Grok review {SESSION} needs input")),
            "{reason}"
        );
        assert!(
            state.lock().expect("leader state").deleted,
            "a review parked on a question must not leave a live session behind"
        );
    }
}
