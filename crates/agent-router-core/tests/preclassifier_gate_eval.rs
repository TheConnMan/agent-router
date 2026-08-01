//! Evaluation of one candidate deterministic pre classifier rule against the labelled fixture.
//!
//! The rule under evaluation: if the task text mentions, as a whole word, any external system name
//! from a fixed vocabulary that is not present in the configured connector inventory, pin the task
//! to Claude and skip the classifier call. The point of this file is to measure that rule, not to
//! ship it: nothing here is wired into the routing path.
//!
//! The bar was fixed before measurement: zero wrong pins, and the rule must fire on at least 20
//! percent of the fixture. A wrong pin is a case where the rule fires but the classifier's own
//! recorded verdict is `codex`, which is a silent misroute with no log evidence that a heuristic
//! caused it.

use agent_router_core::Config;
use serde::Deserialize;

const FIXTURE: &str = include_str!("fixtures/preclassifier_cases.json");

/// The fixed vocabulary of external system names the rule looks for.
const VOCABULARY: [&str; 26] = [
    "airtable",
    "asana",
    "bigquery",
    "confluence",
    "datadog",
    "figma",
    "github",
    "gitlab",
    "hubspot",
    "intercom",
    "jira",
    "linear",
    "n8n",
    "notion",
    "okta",
    "pagerduty",
    "salesforce",
    "sentry",
    "shopify",
    "slack",
    "snowflake",
    "stripe",
    "trello",
    "twilio",
    "zapier",
    "zendesk",
];

#[derive(Debug, Deserialize)]
struct Case {
    log_id: i64,
    task: String,
    verdict: String,
    missing_connector: bool,
}

/// PURE: whether the candidate rule fires on `task` given `connectors`. It fires when the task
/// mentions, as a whole word and case insensitively, at least one vocabulary term that does not
/// appear as a case insensitive substring of any connector inventory entry.
fn rule_fires(task: &str, connectors: &[String]) -> bool {
    let inventory: Vec<String> = connectors.iter().map(|c| c.to_lowercase()).collect();
    let haystack = task.to_lowercase();
    VOCABULARY.iter().any(|term| {
        let available = inventory.iter().any(|entry| entry.contains(term));
        !available && contains_whole_word(&haystack, term)
    })
}

/// PURE: whether `needle` occurs in `haystack` bounded on both sides by a character that is not an
/// ASCII alphanumeric. Both arguments are expected to already be lowercase.
fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let needle_len = needle.len();
    haystack.match_indices(needle).any(|(start, _)| {
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let end = start + needle_len;
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        before_ok && after_ok
    })
}

#[test]
fn candidate_preclassifier_gate_measured_against_the_fixture() {
    let cases: Vec<Case> = serde_json::from_str(FIXTURE).expect("parse preclassifier fixture");
    let connectors = Config::default().connectors;

    let total = cases.len();
    let mut fired = 0usize;
    let mut agreed = 0usize;
    let mut wrong_pins: Vec<&Case> = Vec::new();

    for case in &cases {
        if rule_fires(&case.task, &connectors) {
            fired += 1;
            if case.verdict == "claude" {
                agreed += 1;
            } else {
                wrong_pins.push(case);
            }
        }
    }

    let oracle = cases.iter().filter(|c| c.missing_connector).count();
    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            n as f64 * 100.0 / d as f64
        }
    };

    println!("candidate pre classifier gate evaluation");
    println!("  total cases: {total}");
    println!("  fired: {fired}");
    println!("  fire rate: {:.1} percent", pct(fired, total));
    println!(
        "  agreement rate among fired: {:.1} percent ({agreed} of {fired})",
        pct(agreed, fired)
    );
    println!("  wrong pins: {}", wrong_pins.len());
    println!(
        "  oracle ceiling (missing_connector true): {oracle} ({:.1} percent of fixture)",
        pct(oracle, total)
    );

    if wrong_pins.is_empty() {
        println!("  wrong pin detail: none");
    } else {
        println!("  wrong pin detail:");
        for case in &wrong_pins {
            let prefix: String = case.task.chars().take(80).collect();
            println!("    log_id {}: {prefix}", case.log_id);
        }
    }

    // The bar, fixed before measurement: zero wrong pins, and a fire rate of at least 20 percent.
    //
    // Measured over 98 cases: the rule fires 20 times (20.4 percent), of which 13 agree with the
    // classifier and 7 disagree. So the fire rate bar is MET and the zero wrong pin bar is MISSED
    // by 7. This rule FAILS the bar and must not enter the routing path.
    //
    // The 7 wrong pins are all the same shape: the task names an out of inventory system but only
    // as a token in the codebase, not as a system to reach. Fixing a typo about n8n, deleting a
    // dead slack_webhook_url key, or grepping for mentions of Snowflake needs no connector at all,
    // and the classifier sent every one of them to Codex. The rule cannot tell a system that must
    // be reached from a system that is merely named, because a substring match carries no such
    // distinction.
    //
    // The oracle ceiling says this is not fixable by tuning the vocabulary. Only 13 of 98 cases
    // (13.3 percent) actually carry missing_connector true, so the largest a rule that never wrong
    // pins could ever fire on this fixture is 13.3 percent, which is already below the 20 percent
    // bar. The two bars cannot both be met by any rule of this family on this evidence: this
    // candidate only clears 20 percent by firing on 7 cases it gets wrong.
    //
    // These assertions pin the numbers as measured. They are a regression guard recording WHY this
    // rule is not in the routing path: changing the rule, the vocabulary, or the fixture moves
    // these numbers and fails this test loudly, forcing the ship decision to be made again rather
    // than drifting.
    assert_eq!(total, 98, "fixture size changed");
    assert_eq!(fired, 20, "fired count changed");
    assert_eq!(wrong_pins.len(), 7, "wrong pin count changed");
    assert_eq!(oracle, 13, "oracle ceiling changed");
}
