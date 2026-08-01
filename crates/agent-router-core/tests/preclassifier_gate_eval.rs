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
    origin: String,
    task: String,
    verdict: String,
    missing_connector: bool,
}

/// Measures for one slice of the fixture.
struct Measures {
    cases: usize,
    fired: usize,
    agreed: usize,
    wrong_pins: usize,
    oracle: usize,
}

/// PURE: evaluates the candidate rule over `cases` and counts how often it fires, how often it
/// agrees with the recorded classifier verdict, how often it wrong pins, and how many cases carry
/// missing_connector true (the ceiling for any rule keyed on that criterion).
fn measure(cases: &[&Case], connectors: &[String]) -> Measures {
    let mut m = Measures {
        cases: cases.len(),
        fired: 0,
        agreed: 0,
        wrong_pins: 0,
        oracle: cases.iter().filter(|c| c.missing_connector).count(),
    };
    for case in cases {
        if rule_fires(&case.task, connectors) {
            m.fired += 1;
            if case.verdict == "claude" {
                m.agreed += 1;
            } else {
                m.wrong_pins += 1;
            }
        }
    }
    m
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

    // The rule reads the live connector inventory, so every number measured below is a statement
    // about one specific inventory. Check it before measuring anything, so that a change to the
    // configured connectors fails here naming its own cause rather than surfacing further down as
    // an unexplained change in a fired count.
    assert_eq!(
        connectors,
        vec!["local shell", "git", "gh (github)", "airtable"],
        "the connector inventory in Config::default() changed, so the measured numbers below no \
         longer describe the current configuration and the evaluation must be run again"
    );

    let all: Vec<&Case> = cases.iter().collect();
    let observed: Vec<&Case> = cases
        .iter()
        .filter(|c| c.origin == "observed_traffic")
        .collect();
    let authored: Vec<&Case> = cases
        .iter()
        .filter(|c| c.origin == "authored_probe")
        .collect();

    let blended = measure(&all, &connectors);
    let observed_m = measure(&observed, &connectors);
    let authored_m = measure(&authored, &connectors);

    let total = blended.cases;
    let fired = blended.fired;
    let agreed = blended.agreed;
    let oracle = blended.oracle;
    let wrong_pins: Vec<&Case> = cases
        .iter()
        .filter(|c| rule_fires(&c.task, &connectors) && c.verdict != "claude")
        .collect();

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

    // The two halves of the evidence behave completely differently, so the blended line above hides
    // the finding rather than showing it. Report each origin on its own.
    for (label, m) in [
        ("observed_traffic", &observed_m),
        ("authored_probe", &authored_m),
    ] {
        println!("  by origin {label}:");
        println!("    cases: {}", m.cases);
        println!(
            "    fired: {} ({:.1} percent)",
            m.fired,
            pct(m.fired, m.cases)
        );
        println!(
            "    agreement rate among fired: {:.1} percent ({} of {})",
            pct(m.agreed, m.fired),
            m.agreed,
            m.fired
        );
        println!("    wrong pins: {}", m.wrong_pins);
        println!(
            "    oracle ceiling (missing_connector true): {} ({:.1} percent)",
            m.oracle,
            pct(m.oracle, m.cases)
        );
    }

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
    // classifier and 7 disagree. The zero wrong pin bar is MISSED by 7. The fire rate bar is met
    // only on the blended number, and only by one case, for the reasons set out below: on observed
    // traffic alone the rule fires on 5 of 79 cases, 6.3 percent, so it misses that bar too. This
    // rule FAILS the bar and must not enter the routing path.
    //
    // The 7 wrong pins are all the same shape: the task names an out of inventory system but only
    // as a token in the codebase, not as a system to reach. Fixing a typo about n8n, deleting a
    // dead slack_webhook_url key, or grepping for mentions of Snowflake needs no connector at all,
    // and the classifier sent every one of them to Codex. The rule cannot tell a system that must
    // be reached from a system that is merely named, because a substring match carries no such
    // distinction.
    //
    // The oracle ceiling says this is not fixable by tuning the vocabulary, for the family of rule
    // measured here. Only 13 of 98 cases (13.3 percent) carry missing_connector true, so the most
    // often a rule keyed on the missing connector criterion could fire on this fixture without
    // wrong pinning is 13.3 percent, already below the 20 percent bar. That ceiling binds this
    // family only. It is not a claim that no rule can do better: 57 of 98 cases carry verdict
    // claude, so a rule keyed on something else entirely could in principle fire more often and
    // still never wrong pin. Nothing here measures such a rule.
    //
    // Splitting by origin is what makes the result readable, because the two halves behave
    // completely differently:
    //
    //   observed_traffic: 79 cases, fires on 5 (6.3 percent), 0 wrong pins, ceiling 5
    //   authored_probe:   19 cases, fires on 15 (78.9 percent), 7 wrong pins, ceiling 8
    //
    // On real traffic the rule fires on 5 of 79 cases, 6.3 percent. That is the traffic grounded
    // reason it misses the 20 percent fire rate bar: on the only half of the evidence drawn from
    // production, it almost never fires at all.
    //
    // The blended 20.4 percent clears the bar by exactly one case, and it clears it only because
    // the authored probe set was deliberately written dense in connector cases to probe the near
    // misses. That density is an artifact of how the probes were authored, not an estimate of
    // anything, so the blended fire rate should not be read as a production fire rate.
    //
    // All 7 wrong pins are authored cases. They prove the failure mode is real and easy to hit with
    // ordinary task text, since the probes are ordinary tasks rather than adversarial ones. They do
    // not establish how often it would happen in production, because authored cases carry no
    // traffic frequency at all. The observed half records 0 wrong pins in 79 cases, which bounds
    // the rate loosely and is far too little evidence to call it zero.
    //
    // These assertions pin the measured numbers. They do not guard a behavior, they record a
    // decision: any change to the rule, to the vocabulary, or to the fixture moves these numbers
    // and fails this test loudly, which forces the ship decision to be made again rather than
    // letting the evidence drift out from under a conclusion already drawn.
    assert_eq!(total, 98, "fixture size changed");
    assert_eq!(fired, 20, "fired count changed");
    assert_eq!(wrong_pins.len(), 7, "wrong pin count changed");
    assert_eq!(oracle, 13, "oracle ceiling changed");

    assert_eq!(observed_m.cases, 79, "observed_traffic case count changed");
    assert_eq!(observed_m.fired, 5, "observed_traffic fired count changed");
    assert_eq!(
        observed_m.wrong_pins, 0,
        "observed_traffic wrong pin count changed"
    );
    assert_eq!(
        observed_m.oracle, 5,
        "observed_traffic oracle ceiling changed"
    );

    assert_eq!(authored_m.cases, 19, "authored_probe case count changed");
    assert_eq!(authored_m.fired, 15, "authored_probe fired count changed");
    assert_eq!(
        authored_m.wrong_pins, 7,
        "authored_probe wrong pin count changed"
    );
    assert_eq!(
        authored_m.oracle, 8,
        "authored_probe oracle ceiling changed"
    );
}
