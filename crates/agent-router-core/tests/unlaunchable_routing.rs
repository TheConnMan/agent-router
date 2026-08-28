//! What `decide` does with a provider whose CLI could not be launched.
//!
//! Unlaunchability is an ELIGIBILITY input, not a re-route instruction. A provider the router
//! could not start is ineligible in exactly the same sense as one over its hard ceiling or one
//! carrying a weekly window nobody read, and the existing capacity machinery then decides what to
//! do about it. No new destination is invented: `decide.rs` states "Claude is a capability
//! destination only; automatic capacity routing chooses between Codex and Grok", and a bug fix does
//! not get to change that.
//!
//! The consequence is a real and deliberate limit, pinned below rather than left implicit: when
//! Grok is not eligible — which is its normal state on this box, because its weekly window is
//! usually unread — an unlaunchable Codex is NOT re-routed. It stays on Codex, the row carries the
//! diagnostic gate, and the dispatch fails loudly with a named launch error. That is the outcome
//! this ticket buys: the incident's defect was a SILENT loss with a LYING rationale, and a named,
//! diagnosable failure is the fix.

use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::Config;
use agent_router_core::decide::{Gate, decide, decide_explicit};
use agent_router_core::{Headroom, Provider, UsageSnapshot};

/// The instant every case decides at, in the same range as the recorded decisions.
const NOW: i64 = 1_785_400_000;
/// Half the weekly window: a reset this far out means the window is exactly 50 percent elapsed.
const HALF_WEEK: i64 = 302_400;

/// A provider window that was actually read.
fn window(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + HALF_WEEK,
        weekly_capacity_known: true,
        stale: false,
        ..Headroom::full()
    }
}

/// A provider whose weekly window nobody read. This is Grok's normal state on this box, which is
/// why the no-move row below is the COMMON case and not the corner case.
fn unread() -> Headroom {
    Headroom {
        weekly_capacity_known: false,
        ..Headroom::full()
    }
}

fn usage(claude: Headroom, codex: Headroom, grok: Headroom) -> UsageSnapshot {
    UsageSnapshot {
        claude,
        codex,
        grok,
    }
}

/// A failed classification that also recorded which CLI could not be launched. The evidence comes
/// from the launch attempt, never from the model.
fn unlaunchable(provider: Provider) -> Classification {
    Classification {
        unlaunchable: Some(provider),
        ..Classification::fallback("could not find the codex executable")
    }
}

/// An ordinary scored task with nothing pinned.
fn plain() -> Classification {
    Classification {
        orchestration: false,
        missing_connector: false,
        complexity: Complexity::High,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
        invokes_implement: false,
        unlaunchable: None,
    }
}

// ------------------------------------------------------------------ #16: the move that can happen

/// Plan test #16. An unlaunchable Codex is ineligible, so an eligible Grok takes the work through
/// the machinery that already existed.
///
/// The row carries BOTH gates: `classifier_unlaunchable` records that unlaunchability was applied,
/// and `flipped_on_exhaustion` records that the destination actually moved. They are separate facts
/// and the log needs both — which is also why the new gate stays out of `stats.rs`'s `FLIP_GATES`,
/// where it would be a no-op on this row and an over-report on the next one.
#[test]
fn an_unlaunchable_codex_is_ineligible_and_grok_takes_the_work() {
    let decision = decide(
        unlaunchable(Provider::Codex),
        usage(window(10.0), window(10.0), window(10.0)),
        NOW,
        &Config::default(),
    );

    assert_eq!(
        decision.provider,
        Provider::Grok,
        "Codex could not be launched and Grok is eligible: {:?}",
        decision.gates
    );
    assert!(
        decision.gates.contains(&Gate::ClassifierUnlaunchable),
        "the row must say why Codex was excluded: {:?}",
        decision.gates
    );
    assert!(
        decision.gates.contains(&Gate::FlippedOnExhaustion),
        "the move itself is still tagged by the gate that already tags moves: {:?}",
        decision.gates
    );
}

// ------------------------------------------------------------------ #17: the limit, stated

/// Plan test #17, and the load-bearing one. With Grok ineligible, an unlaunchable Codex stays on
/// Codex and DOES NOT BECOME CLAUDE.
///
/// This is the common production row, not a corner case: Grok is marked ineligible whenever its
/// weekly window is unread, which is its normal state. An earlier draft of this design added a
/// Codex-to-Claude swap here; it would have invented an automatic Claude destination the routing
/// policy explicitly forbids, and it would have sat after the usage match so it overrode
/// `over_ceiling` — producing a row tagged `over_ceiling` with a provider usage had just refused.
///
/// The `!= Claude` assertion is what pins that. A test asserting only `!= Codex` passes on the
/// wrong fix.
#[test]
fn an_unlaunchable_codex_with_no_eligible_grok_stays_on_codex_and_does_not_become_claude() {
    let decision = decide(
        unlaunchable(Provider::Codex),
        usage(window(10.0), window(10.0), unread()),
        NOW,
        &Config::default(),
    );

    assert_ne!(
        decision.provider,
        Provider::Claude,
        "automatic routing has no Claude destination, and a bug fix does not add one: {:?}",
        decision.gates
    );
    assert_eq!(
        decision.provider,
        Provider::Codex,
        "the task stays where it was and fails loudly, which is the diagnosable outcome: {:?}",
        decision.gates
    );
    assert!(
        decision.gates.contains(&Gate::ClassifierUnlaunchable),
        "an operator needs to see this row above all others: {:?}",
        decision.gates
    );
    assert!(
        decision.gates.contains(&Gate::OverCeiling),
        "neither workhorse was eligible, which is what over_ceiling already means: {:?}",
        decision.gates
    );
}

// ------------------------------------------------------------------ #18: the model recompute

/// Plan test #18. A provider moved for being unlaunchable gets the DESTINATION's model, never the
/// origin's.
///
/// `decide.rs`'s own comment says carrying the prior provider's model across "would hand a backend
/// a name it cannot resolve, since no model exists on both", and a commit exists solely to fix that
/// bug. Folding unlaunchability into `eligible()` keeps that fix applying for free; this is what
/// proves it still does, so a later refactor that re-routes after the recompute goes red here.
#[test]
fn a_provider_moved_for_being_unlaunchable_gets_that_providers_model() {
    let config = Config::default();
    let moved = decide(
        unlaunchable(Provider::Codex),
        usage(window(10.0), window(10.0), window(10.0)),
        NOW,
        &config,
    );
    assert_eq!(moved.provider, Provider::Grok);

    let stayed = decide(
        plain(),
        usage(window(10.0), window(10.0), unread()),
        NOW,
        &config,
    );
    assert_eq!(stayed.provider, Provider::Codex);

    assert_ne!(
        moved.model, stayed.model,
        "the moved row must not be carrying Codex's model name to a Grok backend"
    );
    assert_eq!(
        moved.model, None,
        "Grok resolves its own model, so the recomputed value is None rather than Codex's tier"
    );
    assert_eq!(
        moved.effort, None,
        "the effort is re-derived for the destination too, and Grok takes none"
    );
}

// ------------------------------------------------------------------ #19, #20: Claude is untouched

/// Plan test #19, and edge case E3. A capability pin outranks unlaunchability entirely.
///
/// Orchestration is a capability REQUIREMENT, not a preference: such a task "is not cheaper on
/// Codex, it is failed there". Under the eligibility design this holds structurally, because the
/// closure lives inside the `!capability_pin` branch and a pinned decision never evaluates it. The
/// test exists precisely to pin that structural property, so a refactor that hoists the closure out
/// of the branch is caught rather than shipped.
#[test]
fn a_capability_pin_survives_an_unlaunchable_classifier() {
    let pinned = Classification {
        orchestration: true,
        unlaunchable: Some(Provider::Claude),
        ..plain()
    };

    let decision = decide(
        pinned,
        usage(window(10.0), window(10.0), window(10.0)),
        NOW,
        &Config::default(),
    );

    assert_eq!(
        decision.provider,
        Provider::Claude,
        "the pin decides, and the task fails loudly at dispatch if Claude cannot start"
    );
    assert!(
        !decision.gates.contains(&Gate::ClassifierUnlaunchable),
        "nothing was excluded from an eligibility test that never ran: {:?}",
        decision.gates
    );
}

/// Plan test #20. An unlaunchable Claude changes automatic routing not at all, because Claude is
/// never an `eligible()` candidate.
///
/// Asserted against the same decision with no launch evidence at all, so the new conjunct cannot
/// have accidentally made Claude a candidate, and cannot have pushed a gate on a row where nothing
/// was excluded.
#[test]
fn an_unlaunchable_claude_does_not_disturb_automatic_routing() {
    let config = Config::default();
    let picture = usage(window(10.0), window(60.0), window(20.0));

    let with_evidence = decide(
        Classification {
            unlaunchable: Some(Provider::Claude),
            ..plain()
        },
        picture,
        NOW,
        &config,
    );
    let without = decide(plain(), picture, NOW, &config);

    assert_eq!(with_evidence.provider, without.provider);
    assert_eq!(with_evidence.model, without.model);
    assert_eq!(with_evidence.effort, without.effort);
    assert_eq!(with_evidence.gates, without.gates);
    assert_eq!(with_evidence.rationale, without.rationale);
    assert!(
        !with_evidence.gates.contains(&Gate::ClassifierUnlaunchable),
        "Claude is never asked about, so nothing was excluded: {:?}",
        with_evidence.gates
    );
}

// ------------------------------------------------------------------ #21: the incident string

/// Plan test #21, and the most direct encoding of the reported defect.
///
/// The production line was `claude requested explicitly: classifier failed (could not run codex:
/// ...), defaulting to codex` — a job that ran on Claude claiming it defaulted to the Codex that
/// had just failed to execute. The explicit path never consults the eligibility rules and must not
/// start; all that changes is that the rationale stops naming a destination nothing chose.
#[test]
fn an_explicitly_requested_provider_is_not_told_it_defaulted_elsewhere() {
    let decision = decide_explicit(
        Provider::Claude,
        None,
        None,
        Some(unlaunchable(Provider::Codex)),
        usage(window(10.0), window(10.0), window(10.0)),
        &Config::default(),
    );

    assert_eq!(decision.provider, Provider::Claude);
    assert!(
        decision.rationale.contains("claude requested explicitly"),
        "the real destination is named first, as it already was: {}",
        decision.rationale
    );
    assert!(
        !decision.rationale.contains("defaulting to"),
        "the fallback claims no destination, so no caller can be told a false one: {}",
        decision.rationale
    );
    assert!(
        !decision.gates.contains(&Gate::ClassifierUnlaunchable),
        "decide_explicit runs no eligibility rule and must not start: {:?}",
        decision.gates
    );
}

// ------------------------------------------------------------------ #22: the persisted tag

/// Plan test #22. Gate tags are persisted in the `gates` column and read back by `stats.rs`, so a
/// rename is a silent analysis break: old rows keep the old string and every aggregate spanning the
/// rename quietly splits in two.
#[test]
fn the_unlaunchable_gate_tag_is_stable() {
    assert_eq!(
        Gate::ClassifierUnlaunchable.tag(),
        "classifier_unlaunchable"
    );
    assert_eq!(
        serde_json::to_value(Gate::ClassifierUnlaunchable).expect("serializes"),
        serde_json::json!("classifier_unlaunchable"),
        "the serde form and the CLI-facing tag are two paths to the same string and must agree"
    );
}
