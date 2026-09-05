use agent_router_core::adversarial_review::{
    ReviewProvider, ReviewRequest, ReviewStatus, ReviewerPin, review_pinned_with_providers,
    review_with_claude_usage_reserve, review_with_providers, reviewer_pin,
};
use agent_router_core::{Error, Headroom, Provider, Result};
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

fn pin(provider: Provider, model: Option<&str>) -> ReviewerPin {
    ReviewerPin {
        provider,
        model: model.map(str::to_string),
    }
}

#[test]
fn automatic_selection_records_no_requested_reviewer() {
    let primary = StubReviewer::successful("codex", "primary", Some(fresh(1.0)), "wrong");
    let alternative = StubReviewer::successful("claude", "opus[1m]", Some(fresh(35.0)), "ok");

    let outcome = review_with_providers(&request("codex"), &[&primary, &alternative])
        .expect("eligible alternative completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.requested_provider, None);
    assert_eq!(outcome.requested_model, None);
    assert_eq!(outcome.reviewer_model.as_deref(), Some("opus[1m]"));
}

#[test]
fn explicit_pin_selects_the_requested_provider_and_records_the_pin() {
    let primary = StubReviewer::successful("codex", "primary", Some(fresh(1.0)), "wrong");
    let cheaper = StubReviewer::successful("grok", "default", Some(fresh(5.0)), "wrong");
    let pinned = StubReviewer::successful("claude", "fable", Some(fresh(35.0)), "fable review");

    let outcome = review_pinned_with_providers(
        &request("codex"),
        &[&primary, &cheaper, &pinned],
        &pin(Provider::Claude, Some("fable")),
        25.0,
    )
    .expect("an eligible pinned reviewer completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.primary_provider, "codex");
    assert_eq!(outcome.requested_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.requested_model.as_deref(), Some("fable"));
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.reviewer_model.as_deref(), Some("fable"));
    assert_eq!(outcome.result.as_deref(), Some("fable review"));
    assert_eq!(outcome.usage.map(|usage| usage.weekly_pct), Some(35.0));
    assert!(outcome.rationale.contains("requested explicitly"));
    assert!(outcome.rationale.contains("25.0 point reserve"));
    // The automatic policy would have picked the 5 percent Grok reviewer. The pin overrides the
    // headroom ordering, never the eligibility gates.
    assert_eq!(
        cheaper.calls.get(),
        0,
        "the cheaper alternative was invoked"
    );
    assert_eq!(primary.calls.get(), 0);
    assert_eq!(pinned.calls.get(), 1);
    assert_eq!(outcome.usage_provenance.len(), 1);
    assert_eq!(outcome.usage_provenance[0].provider, "claude");
    assert!(outcome.usage_provenance[0].eligible);
}

#[test]
fn explicit_pin_without_a_model_records_the_registered_model_as_the_actual_one() {
    let pinned = StubReviewer::successful("claude", "opus[1m]", Some(fresh(35.0)), "review");

    let outcome = review_pinned_with_providers(
        &request("codex"),
        &[&pinned],
        &pin(Provider::Claude, None),
        25.0,
    )
    .expect("completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.requested_provider.as_deref(), Some("claude"));
    assert_eq!(outcome.requested_model, None);
    assert_eq!(outcome.reviewer_model.as_deref(), Some("opus[1m]"));
}

#[test]
fn an_ineligible_pin_is_a_skip_and_never_falls_back_to_an_eligible_alternative() {
    for (label, usage, expected) in [
        ("ceiling", Some(fresh(90.0)), "90"),
        (
            "stale",
            Some(Headroom {
                stale: true,
                ..fresh(10.0)
            }),
            "stale",
        ),
        (
            "unknown",
            Some(Headroom {
                weekly_capacity_known: false,
                ..fresh(0.0)
            }),
            "unknown",
        ),
        ("unavailable", None, "unavailable"),
    ] {
        let primary = StubReviewer::successful("codex", "primary", Some(fresh(1.0)), "wrong");
        let fallback = StubReviewer::successful("grok", "default", Some(fresh(5.0)), "wrong");
        let pinned = StubReviewer::successful("claude", "fable", usage, "must not run");

        let outcome = review_pinned_with_providers(
            &request("codex"),
            &[&primary, &fallback, &pinned],
            &pin(Provider::Claude, Some("fable")),
            25.0,
        )
        .expect("an ineligible pin is a skip rather than an infrastructure failure");

        assert_eq!(outcome.status, ReviewStatus::Skipped, "{label}");
        assert_eq!(outcome.requested_provider.as_deref(), Some("claude"));
        assert_eq!(outcome.requested_model.as_deref(), Some("fable"));
        assert_eq!(outcome.reviewer_provider, None, "{label}");
        assert_eq!(outcome.reviewer_model, None, "{label}");
        assert_eq!(outcome.result, None, "{label}");
        let reason = outcome.reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("requested reviewer claude is not eligible: "),
            "{label}: {reason}"
        );
        assert!(reason.contains(expected), "{label}: {reason}");
        assert!(
            outcome.rationale.contains(expected),
            "{label}: {}",
            outcome.rationale
        );
        assert_eq!(
            fallback.calls.get(),
            0,
            "{label}: the pin fell back to grok"
        );
        assert_eq!(pinned.calls.get(), 0, "{label}");
        assert_eq!(primary.calls.get(), 0, "{label}");
        let claude = outcome
            .usage_provenance
            .iter()
            .find(|candidate| candidate.provider == "claude")
            .expect("the pinned candidate is in the provenance");
        assert!(!claude.eligible);
        assert!(claude.rejection_reason.is_some());
    }
}

#[test]
fn a_pinned_claude_reviewer_is_refused_once_usage_plus_the_reserve_reaches_the_ceiling() {
    let at_seventy = StubReviewer::successful("claude", "fable", Some(fresh(70.0)), "review");

    let refused = review_pinned_with_providers(
        &request("codex"),
        &[&at_seventy],
        &pin(Provider::Claude, Some("fable")),
        25.0,
    )
    .expect("a reserve refusal is a skip");
    assert_eq!(refused.status, ReviewStatus::Skipped);
    assert!(
        refused
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("reserve") && reason.contains("70.0")),
        "{:?}",
        refused.reason
    );
    assert!(
        refused.rationale.contains("reserve"),
        "{}",
        refused.rationale
    );
    assert!(refused.rationale.contains("70.0"), "{}", refused.rationale);
    assert_eq!(at_seventy.calls.get(), 0);
    let claude = &refused.usage_provenance[0];
    assert_eq!(claude.weekly_pct, Some(70.0));
    assert!(!claude.eligible);
    assert!(
        claude
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("reserve"))
    );

    // Mutation check: the same reading passes with no reserve, so the refusal above is the
    // reserve floor and not the raw ceiling.
    let allowed = review_pinned_with_providers(
        &request("codex"),
        &[&at_seventy],
        &pin(Provider::Claude, Some("fable")),
        0.0,
    )
    .expect("completes without a reserve");
    assert_eq!(allowed.status, ReviewStatus::Completed);
    assert_eq!(at_seventy.calls.get(), 1);

    // The reserve is Claude's alone: a pinned codex reviewer at the same reading is unaffected.
    let codex = StubReviewer::successful("codex", "gpt-5.6-sol", Some(fresh(70.0)), "review");
    let outcome = review_pinned_with_providers(
        &request("claude"),
        &[&codex],
        &pin(Provider::Codex, Some("gpt-5.6-sol")),
        25.0,
    )
    .expect("completes");
    assert_eq!(outcome.status, ReviewStatus::Completed);
}

#[test]
fn a_pin_naming_an_unregistered_reviewer_fails_without_invocation() {
    let only = StubReviewer::successful("claude", "fable", Some(fresh(10.0)), "wrong");

    let outcome = review_pinned_with_providers(
        &request("codex"),
        &[&only],
        &pin(Provider::Grok, None),
        25.0,
    )
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
    let substituted = StubReviewer::successful("claude", "opus[1m]", Some(fresh(10.0)), "wrong");

    let outcome = review_pinned_with_providers(
        &request("codex"),
        &[&substituted],
        &pin(Provider::Claude, Some("fable")),
        0.0,
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
fn a_pinned_reviewer_failure_is_an_invocation_error_and_names_the_pin() {
    let pinned = StubReviewer::failing("claude", "fable", Some(fresh(10.0)));
    let fallback = StubReviewer::successful("grok", "default", Some(fresh(1.0)), "must not run");

    let error = review_pinned_with_providers(
        &request("codex"),
        &[&fallback, &pinned],
        &pin(Provider::Claude, Some("fable")),
        0.0,
    )
    .expect_err("the pinned invocation failure is reported");

    assert!(error.to_string().contains("review invocation failed"));
    assert_eq!(pinned.calls.get(), 1);
    assert_eq!(fallback.calls.get(), 0);
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
