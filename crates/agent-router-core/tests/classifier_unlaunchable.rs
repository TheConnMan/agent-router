//! The classifier's discrimination: a CLI the router could not LAUNCH, against a CLI that ran
//! and answered badly. A fallback names no destination; only a launch failure sets
//! `unlaunchable`. See docs/decisions/0005-launch-error-and-binary-resolver.md.
//!
//! Every case drives `classify` / `job_name` with an explicit `Context`. No
//! process-environment mutation and no user-namespace isolation: see `binary_resolution.rs`.

#![cfg(unix)]

use agent_router_core::Context;
use agent_router_core::Provider;
use agent_router_core::binary::{CLAUDE_BIN_ENV, CODEX_BIN_ENV, Environment};
use agent_router_core::classify::{
    Classification, Complexity, TaskContextHorizon, classify, classify_with_name, job_name,
    parse_classification, parse_classifier_output_with_name,
};
use agent_router_core::config::{ClassifierEngine, Config};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

mod common;

/// A well-formed classifier answer, matching the fixture `classify.rs`'s own tests use.
const GOOD: &str = r#"{"orchestration":false,"missing_connector":false,
    "task_context_horizon":"ordinary","rationale":"explicit outcome, mechanical verification"}"#;

/// The claude envelope: one JSON object whose `result` field carries the model's text.
fn envelope(result: &str) -> String {
    serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": result,
    })
    .to_string()
}

/// A `HOME` and `PATH` that resolve no provider CLI at all.
///
/// The shared, drift-proof fixture lives in `tests/common`; see its doc comment for why the empty
/// system fallback list is load-bearing.
fn stripped(root: &Path) -> Environment {
    common::stripped_environment(Some(root))
}

/// An environment in which `binary` is exactly the stub written at `stub`, pinned by override so
/// nothing on the real box can be reached.
fn pinned(root: &Path, override_env: &str, name: &str, body: &str) -> Environment {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create the stub directory");
    let stub = bin.join(name);
    common::write_stub(&stub, body);
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create the empty HOME");
    Environment::new(
        None,
        Some(home),
        BTreeMap::from([(override_env.to_string(), OsString::from(stub))]),
    )
}

fn config_on(engine: ClassifierEngine) -> Config {
    let mut config = Config::default();
    config.classifier.engine = engine;
    config
}

fn ctx(root: &Path, environment: Environment, config: Config) -> Context {
    Context::new(environment, root.join("home"), config)
}

// ------------------------------------------------------------------ #10: could not launch

/// Plan test #10. When the classifier's own CLI cannot be launched, the fallback must record WHICH
/// provider that was, and its rationale must carry the resolver's named message.
///
/// Without `unlaunchable`, `decide` has no way to tell "codex could not start" from "codex scored
/// this badly", and the router keeps sending work to a provider that cannot run it.
///
/// Deliberately NOT asserted here: `!rationale.contains("os error 2")`. Per edge case E17 the
/// post-resolve spawn path keeps `capture`'s `could not run {engine}: {e}` byte-identical, and for
/// a TOCTOU ENOENT that `{e}` legitimately renders as the production string. A blanket negative
/// would fail on correct code, which is worse than no assertion. The positive substrings below are
/// what scope this to the resolver's own message.
#[test]
fn a_classifier_whose_cli_cannot_be_launched_falls_back_with_the_named_message() {
    let root = tempfile::tempdir().expect("tempdir");
    let environment = stripped(root.path());

    for (engine, provider, binary, override_env) in [
        (
            ClassifierEngine::Codex,
            Provider::Codex,
            "codex",
            CODEX_BIN_ENV,
        ),
        (
            ClassifierEngine::Claude,
            Provider::Claude,
            "claude",
            CLAUDE_BIN_ENV,
        ),
    ] {
        let classification = classify(
            &ctx(root.path(), environment.clone(), config_on(engine)),
            "audit the airtable records",
        );

        assert!(
            classification.classifier_failed,
            "{binary}: a CLI that never started did not score anything"
        );
        assert_eq!(
            classification.unlaunchable,
            Some(provider),
            "{binary}: the fallback must record which CLI could not be launched"
        );
        assert!(
            classification
                .rationale
                .contains(&format!("could not find the {binary} executable")),
            "{binary}: the rationale carries the resolver's named message: {}",
            classification.rationale
        );
        assert!(
            classification.rationale.contains(override_env),
            "{binary}: the rationale names the override that would fix it: {}",
            classification.rationale
        );
    }
}

// ------------------------------------------------------------------ #11: ran and failed

/// Plan test #11, and what makes #10 mean anything. A classifier that RAN and failed is not
/// unlaunchable.
///
/// Stamping `Some(engine_provider)` unconditionally would pass #10 while destroying the routing
/// signal: every timeout on this box would start marking Codex ineligible, and the router would
/// drain into Grok — or fail loudly on the common no-Grok row — for a provider that is working.
#[test]
fn a_classifier_that_ran_and_failed_is_not_marked_unlaunchable() {
    for (label, body) in [
        ("nonzero exit", "exit 3\n"),
        ("unparseable json", "printf 'not json at all\\n'\nexit 0\n"),
        (
            "an envelope carrying no answer",
            "printf '%s' '{\"type\":\"result\"}'\nexit 0\n",
        ),
        ("timeout", "sleep 30\n"),
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let environment = pinned(root.path(), CLAUDE_BIN_ENV, "claude", body);
        let mut config = config_on(ClassifierEngine::Claude);
        // Keep the timeout case bounded: the deadline is the behaviour under test, not the wait.
        config.classifier_timeout_secs = 1;

        let classification = classify(
            &ctx(root.path(), environment, config),
            "audit the airtable records",
        );

        assert!(
            classification.classifier_failed,
            "{label}: this is still a classifier failure"
        );
        assert_eq!(
            classification.unlaunchable, None,
            "{label}: the CLI started, so nothing about it was unlaunchable — rationale {}",
            classification.rationale
        );
    }
}

// ------------------------------------------------------------------ #12: the spoofing guard

/// Plan test #12, and the C17 guard. A model that echoes `unlaunchable` must not get to set it.
///
/// This is a security-shaped bug, not a tidiness point. Without the stamp, a classifier answer
/// containing `"unlaunchable":"codex"` would make a SUCCESSFUL score mark Codex ineligible and
/// push a gate asserting a launch failure that never happened — model output steering the router
/// past its own usage rules. The repo already fixed exactly this class once for `classifier_failed`
/// (`a_model_claiming_classifier_failed_does_not_get_to_set_it`), and both parse entry points are
/// covered here because C17 stamps in two places and a single-site test would let the other rot.
#[test]
fn a_model_claiming_a_provider_is_unlaunchable_does_not_get_to_set_it() {
    let spoofed = GOOD.replace(
        "\"missing_connector\":false",
        "\"missing_connector\":false,\"unlaunchable\":\"codex\"",
    );

    let direct = parse_classification(&spoofed).expect("the answer still parses");
    assert_eq!(
        direct.unlaunchable, None,
        "parse_classification must stamp the field the model tried to set"
    );

    let (enveloped, _) =
        parse_classifier_output_with_name(&envelope(&spoofed), ClassifierEngine::Claude)
            .expect("the answer still parses out of the envelope");
    assert_eq!(
        enveloped.unlaunchable, None,
        "parse_classifier_output_with_name must stamp it too, or one entry point rots"
    );
}

// ------------------------------------------------------------------ #14: job naming

/// Plan test #14, and the C19 guard. `job_name` is a THIRD classifier-command caller, and naming
/// is optional by design: an unlaunchable CLI must return None rather than panicking or
/// propagating an error into a path documented as never failing.
#[test]
fn job_name_returns_none_when_the_classifier_cannot_be_launched() {
    let root = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        job_name(
            &ctx(
                root.path(),
                stripped(root.path()),
                config_on(ClassifierEngine::Codex),
            ),
            "GH-123 audit the airtable records",
        ),
        None,
        "a job that cannot be named still dispatches"
    );
}

// ------------------------------------------------------------------ #15: the serde default

/// Plan test #15, scoped honestly. `#[serde(default)]` on `unlaunchable` buys exactly two things:
/// a model answer that omits the key still parses, and a `Classification` survives a JSON round
/// trip.
///
/// It buys NOTHING for decision-log compatibility — `log.rs` writes named SQLite columns with no
/// `Classification` blob and no `unlaunchable` column, so the field never reaches disk at all. The
/// on-disk claim is `decision_log_launch_outcome.rs`'s, not this test's.
#[test]
fn an_answer_without_unlaunchable_still_parses_and_round_trips() {
    assert!(
        !GOOD.contains("unlaunchable"),
        "the fixture omits the field, which is the whole point"
    );
    let parsed = parse_classification(GOOD).expect("an answer omitting the field parses");
    assert_eq!(parsed.unlaunchable, None);

    let scored = Classification {
        orchestration: false,
        missing_connector: false,
        complexity: Complexity::Medium,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
        invokes_implement: false,
        unlaunchable: Some(Provider::Codex),
    };
    let text = serde_json::to_string(&scored).expect("serializes");
    let back: Classification = serde_json::from_str(&text).expect("deserializes");
    assert_eq!(
        back.unlaunchable,
        Some(Provider::Codex),
        "the field survives its own round trip, so a replayed row keeps the evidence"
    );
}

// ------------------------------------------------------------------ #13: no invented destination

/// A fallback chooses nothing, so it must claim nothing. See
/// docs/decisions/0007-claude-capability-only.md.
#[test]
fn the_fallback_claims_no_destination_and_still_says_why() {
    let got = Classification::fallback("timed out after 30s");

    assert!(
        got.rationale.contains("timed out after 30s"),
        "the reason survives, because it is what lands in the decision log: {}",
        got.rationale
    );
    assert!(
        !got.rationale.contains("defaulting to"),
        "a fallback that picks no provider must not name one: {}",
        got.rationale
    );
    assert!(got.classifier_failed);
    assert_eq!(
        got.unlaunchable, None,
        "the bare fallback records no launch evidence; only the launch path sets it"
    );
}

/// The classified-task wrapper carries the same evidence, so a caller reading `ClassifiedTask`
/// rather than `Classification` is not looking at a different answer.
#[test]
fn the_named_classify_path_reports_the_same_unlaunchable_evidence() {
    let root = tempfile::tempdir().expect("tempdir");

    let scored = classify_with_name(
        &ctx(
            root.path(),
            stripped(root.path()),
            config_on(ClassifierEngine::Codex),
        ),
        "audit the airtable records",
    );

    assert_eq!(scored.classification.unlaunchable, Some(Provider::Codex));
    assert_eq!(
        scored.job_name, None,
        "a CLI that never ran produced no title either"
    );
}
