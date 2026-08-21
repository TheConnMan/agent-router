use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::Config;
use agent_router_core::decide::{Gate, decide};
use agent_router_core::run::parse_provider;
use agent_router_core::usage::{Headroom, UsageSnapshot, grok_headroom_in};
use agent_router_core::Provider;
use std::path::Path;

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
fn official_grok_billing_log_reads_the_newest_plus_weekly_usage() {
    let headroom = grok_headroom_in(&fixture("grok-billing-known.jsonl"), NOW);

    assert_eq!(headroom.weekly_pct, 37.5);
    assert_eq!(headroom.weekly_reset_epoch, RESET);
    assert!(headroom.weekly_capacity_known);
    assert!(!headroom.stale);
    assert_eq!(headroom.five_hour_reset_epoch, 0);
}

#[test]
fn missing_grok_percentage_keeps_grok_out_of_automatic_candidates() {
    let grok = grok_headroom_in(&fixture("grok-billing-missing-usage.jsonl"), NOW);
    assert_eq!(grok.weekly_reset_epoch, RESET, "the weekly reset is still known");
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
    assert!(decision.gates.contains(&Gate::GrokUnavailable));
}
