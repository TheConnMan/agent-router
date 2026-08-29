//! The retroactive backtest: captured decisions replayed through the workhorse policy.
//!
//! The corpus is a deterministic historical fixture, not a live database. It predates Grok weekly
//! telemetry, so these regressions exercise the fail-closed migration path rather than inventing
//! capacity from an absent reading.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::Config;
use agent_router_core::decide::{Decision, Gate, decide};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use serde::Deserialize;

const FIXTURE: &str = include_str!("fixtures/decision_history.json");

/// One logged decision, reduced to the fields the current replay and fixture-integrity checks use.
#[derive(Debug, Deserialize)]
struct HistoricalRow {
    id: i64,
    created_at_ms: i64,
    /// "auto", or the provider the caller named. Named-provider rows never reached `decide`.
    requested: String,
    complexity: Option<String>,
    /// The six historic rubric booleans as "010000", or null for an explicit row.
    claude_signals: Option<String>,
    missing_connector: Option<i64>,
    historical_gates: Option<String>,
    claude_weekly_pct: f64,
    claude_weekly_reset: i64,
    codex_weekly_pct: f64,
    codex_weekly_reset: i64,
    claude_five_hour_pct: f64,
    codex_five_hour_pct: f64,
}

impl HistoricalRow {
    fn usage(&self) -> UsageSnapshot {
        UsageSnapshot {
            claude: Headroom {
                weekly_pct: self.claude_weekly_pct,
                weekly_reset_epoch: self.claude_weekly_reset,
                weekly_capacity_known: self.claude_weekly_reset != 0,
                five_hour_pct: self.claude_five_hour_pct,
                ..Headroom::full()
            },
            codex: Headroom {
                weekly_pct: self.codex_weekly_pct,
                weekly_reset_epoch: self.codex_weekly_reset,
                weekly_capacity_known: self.codex_weekly_reset != 0,
                five_hour_pct: self.codex_five_hour_pct,
                ..Headroom::full()
            },
            grok: Headroom::closed(),
        }
    }

    fn now(&self) -> i64 {
        self.created_at_ms / 1000
    }

    fn classifier_failed(&self) -> bool {
        self.historical_gates
            .as_deref()
            .unwrap_or("")
            .split(',')
            .any(|tag| tag == "classifier_failed")
    }

    fn classification(&self) -> Classification {
        if self.classifier_failed() {
            return Classification::fallback("replayed classifier failure");
        }
        let signals = self
            .claude_signals
            .as_deref()
            .expect("an auto row carries its rubric scores");
        Classification {
            orchestration: signals.as_bytes()[1] == b'1',
            missing_connector: self.missing_connector.unwrap_or(0) != 0,
            complexity: complexity(self.complexity.as_deref()),
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "replayed".to_string(),
            classifier_failed: false,
            invokes_implement: false,
            unlaunchable: None,
        }
    }

    fn replay(&self, config: &Config) -> Decision {
        decide(self.classification(), self.usage(), self.now(), config)
    }
}

fn complexity(tag: Option<&str>) -> Complexity {
    match tag {
        Some("trivial") | Some("low") => Complexity::Low,
        Some("medium") => Complexity::Medium,
        Some("ultra") => Complexity::Ultra,
        None | Some("hard") | Some("high") => Complexity::High,
        Some(other) => panic!("unknown complexity {other} in the fixture"),
    }
}

fn corpus() -> Vec<HistoricalRow> {
    serde_json::from_str(FIXTURE).expect("parse the decision history fixture")
}

/// Explicit rows have no classifier answer because they never reached automatic routing. This
/// keeps the legacy fixture readable while preventing a future fixture edit from silently removing
/// auto rows from replay.
#[test]
fn only_explicitly_routed_rows_are_outside_the_replay() {
    let corpus = corpus();
    assert_eq!(corpus.len(), 108, "the fixture is the full recorded corpus");

    let unscored: Vec<i64> = corpus
        .iter()
        .filter(|row| row.claude_signals.is_none())
        .map(|row| row.id)
        .collect();
    let explicit: Vec<i64> = corpus
        .iter()
        .filter(|row| row.requested != "auto")
        .map(|row| row.id)
        .collect();
    assert_eq!(unscored, explicit);
    assert_eq!(explicit, vec![9, 10, 11, 24, 26, 27, 105, 106]);
    assert_eq!(corpus.len() - explicit.len(), 100);
}

/// Failed classification is not a Claude capability pin. With the corpus's unavailable Grok
/// telemetry, its deterministic automatic fallback remains Codex and still records the failure.
#[test]
fn every_classifier_failure_row_replays_as_a_failure_and_still_routes() {
    let config = Config::default();
    let failures: Vec<HistoricalRow> = corpus()
        .into_iter()
        .filter(|row| row.requested == "auto" && row.classifier_failed())
        .collect();
    assert_eq!(failures.len(), 9, "the corpus records nine failed scores");

    for row in failures {
        let decision = row.replay(&config);
        assert!(
            decision.gates.contains(&Gate::ClassifierFailed),
            "row {}",
            row.id
        );
        assert_eq!(decision.provider, Provider::Codex, "row {}", row.id);
    }
}

/// Every historical auto row lacks a known Grok weekly window. That must never manufacture a Grok
/// route: orchestration still pins Claude, ordinary rows deterministically fall back to Codex, and
/// a recorded connector miss now preserves its observation while blocking dispatch.
#[test]
fn historical_rows_without_known_grok_capacity_never_fabricate_a_grok_auto_route() {
    let config = Config::default();
    let auto: Vec<HistoricalRow> = corpus()
        .into_iter()
        .filter(|row| row.requested == "auto")
        .collect();
    assert_eq!(auto.len(), 100);

    for row in auto {
        let classification = row.classification();
        let orchestration_pinned = classification.orchestration;
        let missing_connector = classification.missing_connector;
        assert!(
            !row.usage().grok.weekly_known(),
            "row {} unexpectedly has Grok capacity",
            row.id
        );

        let decision = decide(classification, row.usage(), row.now(), &config);
        assert_ne!(
            decision.provider,
            Provider::Grok,
            "row {} fabricated Grok capacity",
            row.id
        );
        assert_eq!(
            decision.capability_blocked, missing_connector,
            "row {}",
            row.id
        );
        assert_eq!(
            decision.provider,
            if orchestration_pinned {
                Provider::Claude
            } else {
                Provider::Codex
            },
            "row {}",
            row.id
        );
    }
}
