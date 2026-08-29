use agent_router_core::adversarial_review::{
    ReviewProvider, ReviewRequest, ReviewStatus, review_with_providers,
};
use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::Config;
use agent_router_core::decide::{Gate, decide, decide_explicit};
use agent_router_core::dispatch::grok::dispatch_with_lifecycle;
use agent_router_core::run::parse_provider;
use agent_router_core::usage::{Headroom, UsageSnapshot, grok_headroom_in};
use agent_router_core::{Provider, Result};
use agent_viewer_core::SpawnResult;
use std::path::Path;
use std::{fs, io::Write as _};

const NOW: i64 = 1_787_313_600;
const RESET: i64 = 1_787_356_800;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn known(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: RESET,
        weekly_capacity_known: true,
        stale: false,
        ..Headroom::full()
    }
}

fn plain_task() -> Classification {
    Classification {
        orchestration: false,
        missing_connector: false,
        complexity: Complexity::High,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "portable fixture".to_string(),
        classifier_failed: false,
        invokes_implement: false,
        unlaunchable: None,
    }
}

struct EligibleGrokReviewer;

impl ReviewProvider for EligibleGrokReviewer {
    fn provider_name(&self) -> &str {
        "grok"
    }

    fn reviewer_model(&self) -> &str {
        "grok-review"
    }

    fn usage(&self) -> Option<Headroom> {
        Some(known(20.0))
    }

    fn review(&self, _: &ReviewRequest<'_>) -> Result<String> {
        Ok("completed Grok adversarial review".to_string())
    }
}

#[test]
fn explicit_grok_provider_parses_to_its_own_backend() {
    let provider = parse_provider("grok").expect("grok is a supported explicit provider");

    assert_eq!(provider, Some(Provider::Grok));
    assert_eq!(Provider::Grok.name(), "grok");
    assert_eq!(
        serde_json::to_string(&Provider::Grok).expect("serialize provider"),
        r#""grok""#
    );
}

#[test]
fn grok_dispatch_returns_the_exact_official_lifecycle_identity() {
    let official_identity = "8e058c03-1058-4d53-a10f-635abc3460a9";
    let dispatch = dispatch_with_lifecycle(
        Path::new("/tmp"),
        "Review the router",
        "Exact Grok Identity",
        Some("grok-4"),
        |cwd, task, model| {
            assert_eq!(cwd, Path::new("/tmp"));
            assert_eq!(task, "Review the router");
            assert_eq!(model, Some("grok-4"));
            Ok(SpawnResult {
                pid: None,
                session_id: Some(official_identity.to_string()),
            })
        },
    )
    .expect("Grok lifecycle dispatch");

    assert_eq!(dispatch.job_id.as_deref(), Some(official_identity));
    assert_eq!(dispatch.job_name, "Exact Grok Identity");
    assert_eq!(dispatch.effective_effort, None);
}

#[test]
fn official_grok_billing_log_reads_the_newest_plus_weekly_usage() {
    let headroom = grok_headroom_in(&fixture("grok-billing-known.jsonl"), NOW);

    assert_eq!(headroom.weekly_pct, 37.5);
    assert_eq!(headroom.weekly_reset_epoch, RESET);
    assert!(headroom.weekly_capacity_known);
    assert!(!headroom.stale);
    assert_eq!(headroom.five_hour_reset_epoch, 0);
}

#[test]
fn grok_billing_finds_event_before_a_suffix_larger_than_one_mib() {
    let directory = tempfile::tempdir().expect("temporary Grok log");
    let path = directory.path().join("unified.jsonl");
    let mut log = fs::File::create(&path).expect("create Grok log");
    writeln!(
        log,
        "{}",
        fs::read_to_string(fixture("grok-billing-known.jsonl"))
            .expect("read billing fixture")
            .lines()
            .last()
            .expect("billing fixture event")
    )
    .expect("write old billing event");
    log.write_all(&vec![b'x'; 1_048_577])
        .expect("write oversized newer suffix");
    writeln!(log).expect("terminate oversized suffix");
    drop(log);

    let headroom = grok_headroom_in(&path, NOW);

    assert_eq!(headroom.weekly_pct, 37.5);
    assert_eq!(headroom.weekly_reset_epoch, RESET);
    assert!(headroom.weekly_capacity_known);
    assert!(!headroom.stale);
}

#[test]
fn missing_grok_percentage_keeps_grok_out_of_automatic_candidates() {
    let grok = grok_headroom_in(&fixture("grok-billing-missing-usage.jsonl"), NOW);
    assert_eq!(
        grok.weekly_reset_epoch, RESET,
        "the weekly reset is still known"
    );
    assert!(
        !grok.weekly_capacity_known,
        "missing creditUsagePercent is unknown capacity, not zero usage"
    );

    let decision = decide(
        plain_task(),
        UsageSnapshot {
            claude: known(99.0),
            codex: known(80.0),
            grok,
        },
        NOW,
        &Config::default(),
    );

    assert_eq!(
        decision.provider,
        Provider::Codex,
        "Codex is the only eligible provider; unknown Grok must not look empty"
    );

    let explicit = decide_explicit(
        Provider::Grok,
        None,
        None,
        Some(plain_task()),
        UsageSnapshot {
            claude: known(99.0),
            codex: known(80.0),
            grok,
        },
        &Config::default(),
    );

    assert_eq!(explicit.provider, Provider::Grok);
    assert!(explicit.gates.contains(&Gate::GrokUnavailable));
}

#[test]
fn automatic_routing_balances_known_workhorse_weekly_headroom() {
    let unknown_grok = Headroom {
        weekly_pct: 0.0,
        weekly_reset_epoch: RESET,
        weekly_capacity_known: false,
        stale: false,
        ..Headroom::full()
    };
    let scenarios = [
        (known(99.0), known(80.0), known(10.0), Provider::Grok),
        (known(99.0), known(10.0), known(80.0), Provider::Codex),
        (known(99.0), known(10.0), known(10.0), Provider::Codex),
        (known(99.0), known(80.0), unknown_grok, Provider::Codex),
        (
            known(99.0),
            known(80.0),
            known(Config::default().hard_ceiling_pct),
            Provider::Codex,
        ),
    ];

    for (claude, codex, grok, expected) in scenarios {
        let decision = decide(
            plain_task(),
            UsageSnapshot {
                claude,
                codex,
                grok,
            },
            NOW,
            &Config::default(),
        );

        assert_eq!(decision.provider, expected);
    }
}

#[test]
fn grok_can_be_an_eligible_adversarial_reviewer() {
    let grok = EligibleGrokReviewer;
    let request = ReviewRequest {
        primary_provider: "codex",
        body: "Review this change for regressions",
        dir: Path::new("/tmp"),
    };

    let outcome = review_with_providers(&request, &[&grok]).expect("eligible reviewer completes");

    assert_eq!(outcome.status, ReviewStatus::Completed);
    assert_eq!(outcome.reviewer_provider.as_deref(), Some("grok"));
    assert_eq!(outcome.reviewer_model.as_deref(), Some("grok-review"));
    assert_eq!(
        outcome.result.as_deref(),
        Some("completed Grok adversarial review")
    );
    assert_eq!(outcome.usage_provenance.len(), 1);
    assert_eq!(outcome.usage_provenance[0].provider, "grok");
    assert!(outcome.usage_provenance[0].eligible);
}
