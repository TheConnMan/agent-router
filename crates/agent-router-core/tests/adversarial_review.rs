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

    let result = review_with_providers(&request("codex"), &[&codex], &PRIORITY);

    assert_not_completed(&result);
    assert_eq!(codex.calls.get(), 0);
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
