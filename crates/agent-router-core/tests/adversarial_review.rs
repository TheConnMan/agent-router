use agent_router_core::adversarial_review::{
    ReviewProvider, ReviewRequest, ReviewStatus, review_with_claude_usage_reserve,
    review_with_providers,
};
use agent_router_core::{Error, Headroom, Result};
use std::cell::Cell;
use std::path::Path;

struct StubReviewer<'a> {
    provider: &'a str,
    model: &'a str,
    usage: Option<Headroom>,
    result: Result<&'a str>,
    calls: Cell<usize>,
}

impl<'a> StubReviewer<'a> {
    fn successful(
        provider: &'a str,
        model: &'a str,
        usage: Option<Headroom>,
        result: &'a str,
    ) -> Self {
        Self {
            provider,
            model,
            usage,
            result: Ok(result),
            calls: Cell::new(0),
        }
    }

    fn failing(provider: &'a str, model: &'a str, usage: Option<Headroom>) -> Self {
        Self {
            provider,
            model,
            usage,
            result: Err(Error::Command("review invocation failed".to_string())),
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

    fn usage(&self) -> Option<Headroom> {
        self.usage
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

fn fresh(weekly_pct: f64) -> Headroom {
    Headroom {
        five_hour_pct: 12.0,
        five_hour_reset_epoch: 1_800_000_000,
        weekly_pct,
        weekly_reset_epoch: 1_800_500_000,
        weekly_capacity_known: true,
        stale: false,
    }
}

fn request<'a>(primary_provider: &'a str) -> ReviewRequest<'a> {
    ReviewRequest {
        primary_provider,
        body: "Review this working tree for regressions",
        dir: Path::new("/tmp/review target"),
    }
}

#[test]
fn primary_provider_is_a_hard_exclusion_and_is_never_invoked() {
    let primary =
        StubReviewer::successful("codex", "primary model", Some(fresh(1.0)), "wrong review");
    let alternative = StubReviewer::successful(
        "claude",
        "alternative model",
        Some(fresh(35.0)),
        "completed adversarial review",
    );

    let outcome = review_with_providers(&request("codex"), &[&primary, &alternative])
        .expect("eligible alternative completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.primary_provider, "codex");
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.reviewer_model.as_deref(), Some("alternative model"));
    assert_eq!(
        outcome.result.as_deref(),
        Some("completed adversarial review")
    );
    assert_eq!(primary.calls.get(), 0, "the primary provider was invoked");
    assert_eq!(alternative.calls.get(), 1);

    // Mutation check: removing the primary exclusion makes the one percent primary win the
    // headroom ordering. This assertion then fails because its call count becomes one.
    assert_eq!(primary.calls.get(), 0);
}

#[test]
fn stale_unknown_unavailable_and_ceiling_candidates_are_ineligible() {
    let mut stale_usage = fresh(5.0);
    stale_usage.stale = true;
    let mut unknown_usage = fresh(6.0);
    unknown_usage.weekly_capacity_known = false;
    let primary = StubReviewer::successful("codex", "primary", Some(fresh(2.0)), "wrong");
    let stale = StubReviewer::successful("claude", "stale", Some(stale_usage), "wrong");
    let unknown = StubReviewer::successful("openrouter", "unknown", Some(unknown_usage), "wrong");
    let unavailable = StubReviewer::successful("grok", "unavailable", None, "wrong");
    let at_ceiling = StubReviewer::successful("local", "ceiling", Some(fresh(90.0)), "wrong");
    let eligible =
        StubReviewer::successful("future", "eligible", Some(fresh(41.0)), "right review");

    let outcome = review_with_providers(
        &request("codex"),
        &[
            &stale,
            &unknown,
            &unavailable,
            &at_ceiling,
            &eligible,
            &primary,
        ],
    )
    .expect("one registered alternative remains eligible");

    assert_eq!(outcome.reviewer_provider.as_deref(), Some("future"));
    assert_eq!(outcome.usage, Some(fresh(41.0)));
    assert_eq!(outcome.result.as_deref(), Some("right review"));
    for rejected in [&primary, &stale, &unknown, &unavailable, &at_ceiling] {
        assert_eq!(rejected.calls.get(), 0, "{} was invoked", rejected.provider);
    }
    assert_eq!(eligible.calls.get(), 1);
    assert!(outcome.rationale.contains("future"));
    assert!(outcome.rationale.contains("41"));
}

#[test]
fn unknown_grok_capacity_falls_back_to_claude_without_misreporting_it_as_full() {
    let primary = StubReviewer::successful("codex", "primary", Some(fresh(2.0)), "wrong");
    let grok = StubReviewer::successful("grok", "grok", Some(Headroom::closed()), "wrong");
    let claude = StubReviewer::successful("claude", "claude", Some(fresh(12.0)), "claude review");

    let outcome = review_with_providers(&request("codex"), &[&primary, &grok, &claude])
        .expect("low-usage Claude remains the eligible alternative");

    assert_eq!(outcome.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(claude.calls.get(), 1);
    assert_eq!(
        grok.calls.get(),
        0,
        "unknown Grok capacity must never be invoked"
    );
    assert!(
        outcome.rationale.contains("no billing data available"),
        "the fail-closed sentinel is no data, not measured utilization: {}",
        outcome.rationale
    );
    assert!(
        !outcome
            .rationale
            .contains("stale at 100.0 percent weekly usage"),
        "the sentinel must not be rendered as real billing: {}",
        outcome.rationale
    );
    let grok_provenance = outcome
        .usage_provenance
        .iter()
        .find(|candidate| candidate.provider == "grok")
        .expect("Grok rejection provenance");
    assert_eq!(grok_provenance.weekly_pct, None);
    assert!(
        grok_provenance
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no billing data available"))
    );
}

#[test]
fn equal_capacity_selection_is_deterministic_across_registration_order() {
    let alpha_first = StubReviewer::successful("alpha", "a", Some(fresh(20.0)), "alpha review");
    let zeta_first = StubReviewer::successful("zeta", "z", Some(fresh(20.0)), "zeta review");
    let first = review_with_providers(&request("codex"), &[&zeta_first, &alpha_first])
        .expect("first ordering completes");

    let alpha_second = StubReviewer::successful("alpha", "a", Some(fresh(20.0)), "alpha review");
    let zeta_second = StubReviewer::successful("zeta", "z", Some(fresh(20.0)), "zeta review");
    let second = review_with_providers(&request("codex"), &[&alpha_second, &zeta_second])
        .expect("second ordering completes");

    assert_eq!(first.reviewer_provider.as_deref(), Some("alpha"));
    assert_eq!(second.reviewer_provider.as_deref(), Some("alpha"));
    assert_eq!(first.result, second.result);
    assert_eq!(zeta_first.calls.get(), 0);
    assert_eq!(zeta_second.calls.get(), 0);
}

#[test]
fn claude_reserve_preserves_premium_capacity_without_changing_eligibility() {
    let claude = StubReviewer::successful("claude", "claude", Some(fresh(0.0)), "claude review");
    let grok = StubReviewer::successful("grok", "grok", Some(fresh(20.0)), "grok review");

    let outcome = review_with_claude_usage_reserve(&request("codex"), &[&claude, &grok], 25.0)
        .expect("Grok wins while Claude's reserve remains larger than the usage gap");

    assert_eq!(outcome.reviewer_provider.as_deref(), Some("grok"));
    assert_eq!(grok.calls.get(), 1);
    assert_eq!(claude.calls.get(), 0);
    assert!(outcome.rationale.contains("25.0 point reserve"));

    // Mutation check: without the reserve, the raw 0 percent Claude reading wins.
    let raw_claude =
        StubReviewer::successful("claude", "claude", Some(fresh(0.0)), "claude review");
    let raw_grok = StubReviewer::successful("grok", "grok", Some(fresh(20.0)), "grok review");
    let raw = review_with_providers(&request("codex"), &[&raw_claude, &raw_grok])
        .expect("the raw-usage selection completes");
    assert_eq!(raw.reviewer_provider.as_deref(), Some("claude"));
}

#[test]
fn no_eligible_alternative_returns_a_rationalized_skip_without_invocation() {
    let primary = StubReviewer::successful("codex", "primary", Some(fresh(1.0)), "wrong");
    let at_ceiling = StubReviewer::successful("claude", "full", Some(fresh(90.0)), "wrong");

    let outcome = review_with_providers(&request("codex"), &[&primary, &at_ceiling])
        .expect("no capacity is a skip rather than infrastructure failure");

    assert_eq!(outcome.status, ReviewStatus::Skipped);
    assert_eq!(outcome.primary_provider, "codex");
    assert_eq!(outcome.reviewer_provider, None);
    assert_eq!(outcome.reviewer_model, None);
    assert_eq!(outcome.usage, None);
    assert_eq!(outcome.result, None);
    assert_eq!(
        outcome.reason.as_deref(),
        Some("no eligible alternative provider")
    );
    assert!(outcome.rationale.contains("codex"));
    assert!(outcome.rationale.contains("claude"));
    assert!(outcome.rationale.contains("90"));
    assert_eq!(primary.calls.get(), 0);
    assert_eq!(at_ceiling.calls.get(), 0);
}

#[test]
fn selected_provider_failure_is_an_invocation_error_and_does_not_fall_through() {
    let selected = StubReviewer::failing("alpha", "a", Some(fresh(10.0)));
    let fallback = StubReviewer::successful("zeta", "z", Some(fresh(20.0)), "must not run");

    let error = review_with_providers(&request("codex"), &[&fallback, &selected])
        .expect_err("selected invocation failure must be reported");

    assert!(error.to_string().contains("review invocation failed"));
    assert_eq!(selected.calls.get(), 1);
    assert_eq!(fallback.calls.get(), 0);
}
