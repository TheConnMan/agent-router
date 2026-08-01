//! Integrity checks for the labelled pre classifier evidence set.
//!
//! `tests/fixtures/preclassifier_cases.json` is the ground truth used to evaluate whether a cheap
//! deterministic pre classifier rule could pin some tasks to Claude without paying for the
//! classifier call. Every case carries the decision log row id it came from, so each label is
//! traceable back to a real recorded routing decision.
//!
//! Where the labels came from: each case is a `--provider auto` decision log row that carries a
//! real classifier verdict. Rows whose classifier call failed and fell back are excluded, because
//! a fallback verdict records the configured default rather than any classifier judgment. Cases
//! marked `observed_traffic` are decisions from real routing runs. Cases marked `authored_probe`
//! have hand authored task text, written to cover the near miss cases around connector inventory,
//! and were then labelled by running the live classifier on them on 2026-08-01.
//!
//! A label is therefore the classifier's own recorded verdict, not a human judgment about where
//! the task should have gone. A candidate gate is measured against what the classifier actually
//! said, so disagreement means the gate and the classifier differ, not that the gate is wrong in
//! some absolute sense.
//!
//! These assertions exist so the evidence cannot silently rot: a truncated, relabelled, or one
//! sided fixture would otherwise still parse and still produce a confident looking evaluation.
//!
//! What is actually enforced here is the exact composition of the fixture, not merely that it is
//! non empty: the total case count, the split by verdict, the split by missing_connector, and the
//! split by origin are each pinned to the measured value. Relabelling a single case, dropping a
//! single case, or appending a new one fails this test. That strictness is deliberate, because the
//! measured numbers recorded in `preclassifier_gate_eval.rs` describe this exact set of cases and
//! stop being true of any other one. Alongside the counts, the structural checks require every
//! log_id to be positive and unique and every task text to be non empty.

use serde::Deserialize;

const FIXTURE: &str = include_str!("fixtures/preclassifier_cases.json");

#[derive(Debug, Deserialize)]
struct Case {
    log_id: i64,
    origin: String,
    task: String,
    verdict: String,
    missing_connector: bool,
}

#[test]
fn preclassifier_fixture_is_a_usable_evidence_set() {
    let cases: Vec<Case> = serde_json::from_str(FIXTURE).expect("parse preclassifier fixture");

    assert_eq!(
        cases.len(),
        98,
        "fixture composition changed: total case count, so the evaluation must be run again"
    );

    let mut seen = std::collections::HashSet::new();
    for case in &cases {
        assert!(case.log_id > 0, "log_id must be positive: {}", case.log_id);
        assert!(
            seen.insert(case.log_id),
            "duplicate log_id: {}",
            case.log_id
        );
        assert!(
            !case.task.trim().is_empty(),
            "task must be non empty for log_id {}",
            case.log_id
        );
    }

    let claude = cases.iter().filter(|c| c.verdict == "claude").count();
    let codex = cases.iter().filter(|c| c.verdict == "codex").count();
    assert_eq!(
        claude + codex,
        cases.len(),
        "every verdict must be claude or codex"
    );
    assert_eq!(
        claude, 57,
        "fixture composition changed: claude verdict count, so the evaluation must be run again"
    );
    assert_eq!(
        codex, 41,
        "fixture composition changed: codex verdict count, so the evaluation must be run again"
    );

    let missing = cases.iter().filter(|c| c.missing_connector).count();
    let present = cases.len() - missing;
    assert_eq!(
        missing, 13,
        "fixture composition changed: missing_connector true count, so the evaluation must be run \
         again"
    );
    assert_eq!(
        present, 85,
        "fixture composition changed: missing_connector false count, so the evaluation must be \
         run again"
    );

    let observed = cases
        .iter()
        .filter(|c| c.origin == "observed_traffic")
        .count();
    let authored = cases
        .iter()
        .filter(|c| c.origin == "authored_probe")
        .count();
    assert_eq!(
        observed + authored,
        cases.len(),
        "every origin must be observed_traffic or authored_probe"
    );
    assert_eq!(
        observed, 79,
        "fixture composition changed: observed_traffic count, so the evaluation must be run again"
    );
    assert_eq!(
        authored, 19,
        "fixture composition changed: authored_probe count, so the evaluation must be run again"
    );

    println!("preclassifier fixture composition");
    println!("  total: {}", cases.len());
    println!("  verdict claude: {claude}");
    println!("  verdict codex: {codex}");
    println!("  missing_connector true: {missing}");
    println!("  missing_connector false: {present}");
    println!("  origin observed_traffic: {observed}");
    println!("  origin authored_probe: {authored}");
}
