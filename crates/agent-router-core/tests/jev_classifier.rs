//! Composition of TypeSafe answers into a Classification. Canned JSON, no network.

use agent_router_core::binary::Environment;
use agent_router_core::classify::{
    Classification, Complexity, SystemOneTransport, TaskContextHorizon,
    classify_jev_with_transport, compose_jev, jev_questions, job_name, questions_carry_anti_halo,
};
use agent_router_core::config::{ClassifierEngine, Config};
use agent_router_core::context::Context;
use serde_json::{Value, json};
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
fn answers(
    nothing: f64,
    several: f64,
    exchange: f64,
    changes: f64,
    reach: f64,
    named: &str,
    complexity: [&str; 4],
    horizon: &str,
) -> Value {
    let [low, medium, high, ultra] = complexity.map(|s| s.parse::<f64>().unwrap());
    json!({
        "nothing_to_score": {"type": "noul", "noul": nothing},
        "several_agents": {"type": "noul", "noul": several},
        "mid_run_exchange": {"type": "noul", "noul": exchange},
        "findings_change_next": {"type": "noul", "noul": changes},
        "must_now_reach": {"type": "noul", "noul": reach},
        "named_system": {
            "type": "choice",
            "choice": named,
            "confidence": 0.9,
            "probabilities": {"other": 0.5, "none": 0.5, "local shell": 0.0, "Slack": 0.0}
        },
        "complexity": {
            "type": "score",
            "score": 0.0,
            "confidence": 0.8,
            "probabilities": {"0": low, "1": medium, "2": high, "3": ultra}
        },
        "task_context_horizon": {
            "type": "choice",
            "choice": horizon,
            "confidence": 1.0,
            "probabilities": {"ordinary": 1.0, "extended": 0.0}
        }
    })
}

fn compose(v: &Value) -> Classification {
    compose_jev(v).expect("composes")
}

#[test]
fn empty_and_greeting_do_not_pin() {
    for nothing in [0.9, 0.5] {
        let got = compose(&answers(
            nothing,
            0.9,
            0.9,
            0.9,
            0.9,
            "other",
            ["1", "0", "0", "0"],
            "ordinary",
        ));
        assert!(!got.orchestration);
        assert!(!got.missing_connector);
        assert_eq!(got.complexity, Complexity::Low);
        assert_eq!(got.task_context_horizon, TaskContextHorizon::Ordinary);
    }
}

#[test]
fn mention_of_agents_is_not_orchestration() {
    let got = compose(&answers(
        0.05,
        0.17,
        0.10,
        0.08,
        0.05,
        "none",
        ["0", "1", "0", "0"],
        "ordinary",
    ));
    assert!(!got.orchestration);
}

#[test]
fn true_orchestration_requires_all_three_conjuncts() {
    let got = compose(&answers(
        0.04,
        0.94,
        0.91,
        0.88,
        0.11,
        "none",
        ["0", "0", "1", "0"],
        "ordinary",
    ));
    assert!(got.orchestration);
}

#[test]
fn orchestration_noul_point_five_one_does_not_pin() {
    let got = compose(&answers(
        0.04,
        0.51,
        0.94,
        0.90,
        0.10,
        "none",
        ["0", "0", "1", "0"],
        "ordinary",
    ));
    assert!(!got.orchestration, "uncertain conjunct must not pin Claude");
}

#[test]
fn slack_now_missing_when_only_local_shell() {
    let got = compose(&answers(
        0.05,
        0.06,
        0.04,
        0.04,
        0.70,
        "other",
        ["1", "0", "0", "0"],
        "ordinary",
    ));
    assert!(got.missing_connector);
}

#[test]
fn slack_now_present_when_named_system_is_slack() {
    let got = compose(&answers(
        0.05,
        0.06,
        0.04,
        0.04,
        0.81,
        "Slack",
        ["1", "0", "0", "0"],
        "ordinary",
    ));
    assert!(!got.missing_connector);
}

#[test]
fn n8n_readme_is_not_a_miss() {
    let got = compose(&answers(
        0.04,
        0.03,
        0.03,
        0.03,
        0.04,
        "none",
        ["1", "0", "0", "0"],
        "ordinary",
    ));
    assert!(!got.missing_connector);
}

#[test]
fn mixed_required_systems_canned_other_is_a_miss() {
    // Canned TypeSafe answers JSON, not a helper injection of named_system into a second function.
    let body = answers(
        0.05,
        0.05,
        0.04,
        0.04,
        0.78,
        "other",
        ["0", "1", "0", "0"],
        "ordinary",
    );
    assert_eq!(body["named_system"]["choice"], "other");
    assert!(compose(&body).missing_connector);
}

#[test]
fn mixed_later_granola_canned_slack_is_not_a_miss() {
    let body = answers(
        0.05,
        0.05,
        0.04,
        0.04,
        0.72,
        "Slack",
        ["0", "1", "0", "0"],
        "ordinary",
    );
    assert_eq!(body["named_system"]["choice"], "Slack");
    assert!(!compose(&body).missing_connector);
}

#[test]
fn torn_ultra_reads_as_high() {
    let got = compose(&answers(
        0.04,
        0.10,
        0.10,
        0.10,
        0.10,
        "none",
        ["0", "0", "0.48", "0.52"],
        "ordinary",
    ));
    assert_eq!(got.complexity, Complexity::High);
}

#[test]
fn missing_horizon_is_unusable() {
    let mut body = answers(
        0.04,
        0.1,
        0.1,
        0.1,
        0.1,
        "none",
        ["0", "1", "0", "0"],
        "ordinary",
    );
    body.as_object_mut().unwrap().remove("task_context_horizon");
    assert!(compose_jev(&body).is_none());
}

#[test]
fn noul_out_of_range_is_unusable() {
    let body = answers(
        0.04,
        1.2,
        0.1,
        0.1,
        0.1,
        "none",
        ["0", "1", "0", "0"],
        "ordinary",
    );
    assert!(compose_jev(&body).is_none());
}

#[test]
fn questions_pin_anti_halo_and_other_precedence() {
    assert!(questions_carry_anti_halo(&["local shell".to_string()]));
    let q = jev_questions(&["local shell".to_string(), "Slack".to_string()]);
    assert!(
        q["named_system"]["criteria"]
            .as_object()
            .unwrap()
            .contains_key("Slack")
    );
    assert!(
        q["named_system"]["criteria"]
            .as_object()
            .unwrap()
            .contains_key("other")
    );
}

#[test]
fn fallback_does_not_name_a_destination() {
    let got = Classification::fallback("typesafe http 500");
    assert!(!got.orchestration);
    assert!(!got.missing_connector);
    assert!(got.classifier_failed);
    assert!(got.unlaunchable.is_none());
    assert!(!got.rationale.contains("defaulting to"));
}

#[test]
fn nothing_to_score_alone_is_unusable() {
    let body = json!({"nothing_to_score": {"type": "noul", "noul": 0.9}});
    assert!(compose_jev(&body).is_none());
}

struct FakeTransport {
    result: Result<Value, String>,
}

impl SystemOneTransport for FakeTransport {
    fn evaluate(&self, _body: &Value, _timeout: Duration, _key: &str) -> Result<Value, String> {
        self.result.clone()
    }
}

fn jev_ctx(root: &std::path::Path) -> Context {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let mut config = Config::default();
    config.classifier.engine = ClassifierEngine::Jev;
    Context::new(
        Environment::new(None, Some(home.clone()), Default::default()),
        home,
        config,
    )
}

#[test]
fn missing_key_fail_opens_through_the_entry_point() {
    let root = tempfile::tempdir().expect("tempdir");
    let ctx = jev_ctx(root.path());
    let scored = classify_jev_with_transport(
        &ctx,
        "Fix a typo in README.md",
        None,
        &FakeTransport {
            result: Ok(json!({})),
        },
    );
    assert!(scored.classification.classifier_failed);
    assert!(scored.classification.unlaunchable.is_none());
    assert!(!scored.classification.orchestration);
    assert!(
        scored
            .classification
            .rationale
            .contains("missing typesafe api key")
    );
}

#[test]
fn http_500_fail_opens_through_the_entry_point() {
    let root = tempfile::tempdir().expect("tempdir");
    let ctx = jev_ctx(root.path());
    let scored = classify_jev_with_transport(
        &ctx,
        "Fix a typo in README.md",
        Some("test-key"),
        &FakeTransport {
            result: Err("typesafe http 500".to_string()),
        },
    );
    assert!(scored.classification.classifier_failed);
    assert!(scored.classification.unlaunchable.is_none());
    assert_eq!(scored.classification.complexity, Complexity::High);
    assert!(
        scored
            .classification
            .rationale
            .contains("typesafe http 500")
    );
}

#[test]
fn canned_success_goes_through_the_entry_point() {
    let root = tempfile::tempdir().expect("tempdir");
    let ctx = jev_ctx(root.path());
    let payload = json!({"answers": answers(
        0.05, 0.06, 0.04, 0.04, 0.05, "none", ["1", "0", "0", "0"], "ordinary",
    )});
    let scored = classify_jev_with_transport(
        &ctx,
        "Fix a typo in README.md",
        Some("test-key"),
        &FakeTransport {
            result: Ok(payload),
        },
    );
    assert!(!scored.classification.classifier_failed);
    assert!(!scored.classification.orchestration);
    assert!(!scored.classification.missing_connector);
    assert_eq!(scored.classification.complexity, Complexity::Low);
    assert!(scored.job_name.is_some());
}

#[test]
fn jev_job_name_is_heuristic_without_a_cli() {
    let root = tempfile::tempdir().expect("tempdir");
    let ctx = jev_ctx(root.path());
    let name = job_name(&ctx, "audit the scheduler").expect("named");
    assert_eq!(name, "Audit The Scheduler");
}
