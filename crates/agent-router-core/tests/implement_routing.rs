//! `/implement` runs never land on Grok, and Codex runs them at its own default effort.
//!
//! Automatic routing drops Grok from the priority candidates for an implement task and records
//! `implement_excludes_grok` when that removed a capable candidate. An explicit `--provider grok`
//! with an implement task is refused before anything is classified, logged, or dispatched. A Codex
//! implement run carries no derived effort, so the backend's own default applies, while an explicit
//! `--effort` still wins and every other route keeps the complexity ladder.
//!
//! Every refusal below sits next to the valid input that must still work, so a rule that refused
//! everything (or nothing) cannot pass.

#![cfg(unix)]

mod common;

use agent_router_core::Surface;
use agent_router_core::binary::{CLAUDE_BIN_ENV, Environment};
use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::config::{ClassifierEngine, Config, Routing};
use agent_router_core::decide::{Decision, Gate, decide, decide_explicit, decide_with_task};
use agent_router_core::log::DecisionLog;
use agent_router_core::run::{Request, run_with};
use agent_router_core::{Context, Headroom, Provider, UsageSnapshot};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const NOW: i64 = 1_785_400_000;
/// Half the weekly window: elapsed is exactly 0.5, so draw = 2 x percent.
const HALF_WEEK: i64 = 302_400;

const IMPLEMENT_TASK: &str = "/implement RS-1\nBACKGROUND_RUN=1";

fn window(weekly_pct: f64) -> Headroom {
    Headroom {
        weekly_pct,
        weekly_reset_epoch: NOW + HALF_WEEK,
        weekly_capacity_known: true,
        ..Headroom::full()
    }
}

fn usage(claude: f64, codex: f64, grok: f64) -> UsageSnapshot {
    UsageSnapshot {
        claude: window(claude),
        codex: window(codex),
        grok: window(grok),
    }
}

fn scored(complexity: Complexity, invokes_implement: bool) -> Classification {
    Classification {
        orchestration: false,
        missing_connector: false,
        complexity,
        task_context_horizon: TaskContextHorizon::Ordinary,
        rationale: "fixture".to_string(),
        classifier_failed: false,
        invokes_implement,
        unlaunchable: None,
    }
}

fn implement(complexity: Complexity) -> Classification {
    scored(complexity, true)
}

fn ordinary(complexity: Complexity) -> Classification {
    scored(complexity, false)
}

fn prioritized(priority: Vec<Provider>) -> Config {
    Config {
        routing: Routing {
            priority,
            priority_margin_pct: 0.0,
        },
        ..Config::default()
    }
}

fn has(decision: &Decision, gate: Gate) -> bool {
    decision.gates.contains(&gate)
}

// ------------------------------------------------------------------ automatic routing (AC1)

/// Plan item 1. Grok leads the priority and is the idle provider, so ordinary work goes there; the
/// identical picture for an implement task must go to Codex and say why.
#[test]
fn an_implement_task_skips_a_leading_idle_grok_that_ordinary_work_takes() {
    let config = prioritized(vec![Provider::Grok, Provider::Codex]);
    let picture = usage(50.0, 60.0, 10.0);

    let neighbor = decide(ordinary(Complexity::Low), picture, NOW, &config);
    assert_eq!(
        neighbor.provider,
        Provider::Grok,
        "ordinary work must still take the leading idle grok: {:?}",
        neighbor.gates
    );
    assert!(
        !has(&neighbor, Gate::ImplementExcludesGrok),
        "a non-implement task must not record the exclusion: {:?}",
        neighbor.gates
    );

    let decision = decide(implement(Complexity::Low), picture, NOW, &config);
    assert_eq!(decision.provider, Provider::Codex, "{:?}", decision.gates);
    assert!(
        has(&decision, Gate::ImplementExcludesGrok),
        "the exclusion must be recorded when it removed a capable grok: {:?}",
        decision.gates
    );
    assert!(
        !has(&decision, Gate::FlippedOnExhaustion)
            && !has(&decision, Gate::PriorityOverriddenByUsage),
        "excluding grok is not a usage flip: {:?}",
        decision.gates
    );
    assert!(
        decision.gate_tags().contains(&"implement_excludes_grok"),
        "the log tag is implement_excludes_grok: {:?}",
        decision.gate_tags()
    );
    assert_eq!(
        decision.effort, None,
        "auto implement on codex derives no effort"
    );
}

/// Plan item 2. Default priority with Codex over its hard ceiling: ordinary work flips to the idle
/// Grok, an implement task does not, and the capacity verdict is still recorded.
#[test]
fn an_exhausted_codex_flips_ordinary_work_to_grok_but_never_an_implement_task() {
    let config = Config::default();
    assert_eq!(
        config.routing.priority,
        vec![Provider::Codex, Provider::Grok],
        "this case is about the shipped default priority"
    );
    let picture = usage(50.0, 99.0, 10.0);

    let neighbor = decide(ordinary(Complexity::Medium), picture, NOW, &config);
    assert_eq!(neighbor.provider, Provider::Grok, "{:?}", neighbor.gates);
    assert!(
        has(&neighbor, Gate::FlippedOnExhaustion),
        "{:?}",
        neighbor.gates
    );

    let decision = decide(implement(Complexity::Medium), picture, NOW, &config);
    assert_ne!(
        decision.provider,
        Provider::Grok,
        "an implement task must not flip onto grok: {:?}",
        decision.gates
    );
    assert!(
        has(&decision, Gate::OverCeiling),
        "the only candidate left is over its ceiling: {:?}",
        decision.gates
    );
    assert!(
        has(&decision, Gate::ImplementExcludesGrok),
        "{:?}",
        decision.gates
    );
    assert!(
        !has(&decision, Gate::FlippedOnExhaustion),
        "{:?}",
        decision.gates
    );
}

/// Plan item 3. A priority naming only Grok cannot route an implement task to Grok through the
/// no-candidate fallback either.
#[test]
fn a_grok_only_priority_still_keeps_an_implement_task_off_grok() {
    let config = prioritized(vec![Provider::Grok]);
    let picture = usage(10.0, 10.0, 10.0);

    let neighbor = decide(ordinary(Complexity::Low), picture, NOW, &config);
    assert_eq!(neighbor.provider, Provider::Grok, "{:?}", neighbor.gates);

    for complexity in [Complexity::Low, Complexity::Medium] {
        let decision = decide(implement(complexity), picture, NOW, &config);
        assert_ne!(
            decision.provider,
            Provider::Grok,
            "{complexity:?} implement task routed to grok: {:?}",
            decision.gates
        );
        assert!(
            has(&decision, Gate::ImplementExcludesGrok),
            "{complexity:?}: {:?}",
            decision.gates
        );
    }
}

/// Plan item 4. With no Grok in the priority there is nothing to exclude, so the gate is absent and
/// routing is unchanged.
#[test]
fn a_priority_without_grok_records_no_exclusion() {
    for priority in [
        vec![Provider::Codex],
        vec![Provider::Codex, Provider::Claude],
        vec![Provider::Claude, Provider::Codex],
    ] {
        let config = prioritized(priority.clone());
        for complexity in [Complexity::Low, Complexity::Medium, Complexity::High] {
            let decision = decide(implement(complexity), usage(10.0, 10.0, 10.0), NOW, &config);
            assert!(
                !has(&decision, Gate::ImplementExcludesGrok),
                "{priority:?} {complexity:?}: {:?}",
                decision.gates
            );
            assert_ne!(decision.provider, Provider::Grok);
        }
    }
}

/// Edge case from the plan: an implement task whose required capability only Grok holds is
/// blocked rather than dispatched to Grok. The ordinary task with the same capability still goes
/// to Grok.
#[test]
fn an_implement_task_needing_a_grok_only_capability_is_blocked_not_sent_to_grok() {
    let config = Config {
        provider_capabilities: BTreeMap::from([("grok".to_string(), vec!["Slack".to_string()])]),
        ..Config::default()
    };
    let task = "Post the summary to the client Slack channel.";
    let picture = usage(10.0, 10.0, 10.0);

    let neighbor = decide_with_task(task, ordinary(Complexity::Low), picture, NOW, &config);
    assert_eq!(neighbor.provider, Provider::Grok, "{:?}", neighbor.gates);
    assert!(!neighbor.capability_blocked);

    let decision = decide_with_task(task, implement(Complexity::Low), picture, NOW, &config);
    assert!(
        decision.capability_blocked,
        "nothing but grok can serve it, so it must not dispatch: {:?}",
        decision.gates
    );
    assert!(
        has(&decision, Gate::CapabilityBlocked),
        "{:?}",
        decision.gates
    );
}

// ------------------------------------------------------------------ effort (AC3)

/// Plan item 6, decide level. A Codex implement run carries no derived effort; a pinned effort
/// still wins, and every other provider and task keeps the ladder.
#[test]
fn a_codex_implement_run_derives_no_effort_and_everything_else_keeps_the_ladder() {
    let config = Config::default();
    let picture = usage(10.0, 10.0, 10.0);
    let explicit = |provider, model: Option<&str>, effort: Option<&str>, classification| {
        decide_explicit(
            provider,
            model.map(str::to_string),
            effort.map(str::to_string),
            Some(classification),
            picture,
            &config,
        )
        .effort
    };

    assert_eq!(
        explicit(Provider::Codex, None, None, implement(Complexity::High)),
        None,
        "a codex implement run must leave effort to the backend"
    );
    assert_eq!(
        explicit(
            Provider::Codex,
            Some("gpt-6-astra"),
            Some("high"),
            implement(Complexity::High)
        )
        .as_deref(),
        Some("high"),
        "an explicit --effort still wins"
    );
    assert_eq!(
        explicit(Provider::Codex, None, None, ordinary(Complexity::High)).as_deref(),
        Some("low"),
        "non-implement codex keeps the ladder"
    );
    assert_eq!(
        explicit(Provider::Claude, None, None, implement(Complexity::High)).as_deref(),
        Some("medium"),
        "claude implement keeps its held medium"
    );

    let auto = decide(implement(Complexity::Low), picture, NOW, &config);
    assert_eq!(auto.provider, Provider::Codex, "{:?}", auto.gates);
    assert_eq!(
        auto.effort, None,
        "auto implement on codex derives no effort"
    );
    let auto_ordinary = decide(ordinary(Complexity::Low), picture, NOW, &config);
    assert_eq!(auto_ordinary.provider, Provider::Codex);
    assert_eq!(auto_ordinary.effort.as_deref(), Some("high"));
}

// ------------------------------------------------------------------ the run pipeline

/// A context whose classifier is a stub `claude -p` answering `complexity`, and which resolves no
/// other provider CLI, so nothing real can be dispatched from these tests.
fn context(root: &Path, complexity: &str) -> Context {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create the stub directory");
    let answer = root.join("classifier.answer");
    let result = json!({
        "orchestration": false,
        "missing_connector": false,
        "complexity": complexity,
        "task_context_horizon": "ordinary",
        "rationale": "fixture implement routing",
        "job_name": "Implement Routing Fixture",
    })
    .to_string();
    fs::write(
        &answer,
        json!({"type": "result", "subtype": "success", "is_error": false, "result": result})
            .to_string(),
    )
    .expect("write the classifier answer");
    let stub = bin.join("claude");
    common::write_stub(&stub, &format!("cat '{}'\nexit 0\n", answer.display()));
    let home = root.join("home");
    let empty = root.join("empty-path-dir");
    fs::create_dir_all(&home).expect("create HOME");
    fs::create_dir_all(&empty).expect("create the empty PATH directory");
    let no_system_fallbacks: [PathBuf; 0] = [];
    let environment = Environment::new(
        Some(std::env::join_paths([&empty]).expect("join PATH")),
        Some(home.clone()),
        BTreeMap::from([(CLAUDE_BIN_ENV.to_string(), OsString::from(&stub))]),
    )
    .with_system_fallbacks(no_system_fallbacks);
    let mut config = Config::default();
    config.classifier.engine = ClassifierEngine::Claude;
    Context::new(environment, home, config)
}

fn request<'a>(task: &'a str, dir: &'a Path, provider: Provider, dry_run: bool) -> Request<'a> {
    Request {
        task,
        dir,
        provider: Some(provider),
        model: None,
        effort: None,
        name: Some("Implement Routing".to_string()),
        dry_run,
        mcp_configs: &[],
        strict_mcp_config: false,
        surface: Surface::Background,
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    dir: PathBuf,
    db: PathBuf,
    ctx: Context,
}

fn fixture(complexity: &str) -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("work");
    fs::create_dir_all(&dir).expect("create the launch dir");
    let db = root.path().join("router.db");
    let ctx = context(root.path(), complexity);
    Fixture {
        dir,
        db,
        ctx,
        _root: root,
    }
}

impl Fixture {
    fn run(
        &self,
        task: &str,
        provider: Provider,
        dry_run: bool,
    ) -> agent_router_core::Result<agent_router_core::run::Outcome> {
        run_with(
            &request(task, &self.dir, provider, dry_run),
            &self.ctx,
            || usage(10.0, 10.0, 10.0),
            || DecisionLog::open_at(&self.db),
        )
    }

    fn rows(&self) -> Vec<agent_router_core::log::Row> {
        DecisionLog::open_at(&self.db)
            .expect("open the decision log")
            .recent(10)
            .expect("read the decision log")
    }
}

/// Plan item 5. An explicit grok with an implement task is refused, dry run or not, with an error
/// naming grok and /implement, and nothing is written to the decision log. The leading
/// `BACKGROUND_RUN=1` shape is the same task to the detector and is refused too.
#[test]
fn an_explicit_grok_implement_run_is_refused_and_logs_nothing() {
    for task in [IMPLEMENT_TASK, "BACKGROUND_RUN=1\n/implement X"] {
        for dry_run in [true, false] {
            let fixture = fixture("low");
            let result = fixture.run(task, Provider::Grok, dry_run);
            let error = match result {
                Ok(outcome) => panic!(
                    "explicit grok accepted {task:?} (dry_run {dry_run}): provider {:?}, dispatch {:?}",
                    outcome.decision.provider, outcome.dispatch
                ),
                Err(error) => error.to_string(),
            };
            let lowered = error.to_lowercase();
            assert!(
                lowered.contains("grok") && error.contains("/implement"),
                "the refusal must name grok and /implement: {error}"
            );
            assert!(
                fixture.rows().is_empty(),
                "a refused run must write no decision row ({task:?}, dry_run {dry_run})"
            );
        }
    }
}

/// Plan item 5, neighbor. A grok task that only mentions /implement after its first line is not an
/// implement run and still dry-runs on grok, writing its row.
#[test]
fn an_explicit_grok_task_that_only_mentions_implement_still_dry_runs() {
    let fixture = fixture("low");
    let task = "audit how the router handles\n/implement RS-1 style tasks";

    let outcome = fixture
        .run(task, Provider::Grok, true)
        .expect("a mid-text mention is not an implement run");

    assert_eq!(outcome.decision.provider, Provider::Grok);
    assert!(outcome.dispatch.is_none());
    let rows = fixture.rows();
    assert_eq!(rows.len(), 1, "the dry run writes exactly its own row");
    assert_eq!(rows[0].provider, "grok");
    assert_eq!(rows[0].outcome, "dry-run");
    assert_eq!(rows[0].task, task, "the task is recorded verbatim");
}

/// Plan item 6, run level. A Codex implement dry run records a NULL effort; the ordinary Codex dry
/// run beside it records the ladder's value, so a NULL here cannot be a missing column.
#[test]
fn a_codex_implement_dry_run_logs_no_effort_and_an_ordinary_one_does() {
    let fixture = fixture("high");

    let ordinary = fixture
        .run("redesign the router architecture", Provider::Codex, true)
        .expect("ordinary codex dry run");
    assert_eq!(ordinary.decision.effort.as_deref(), Some("low"));

    let implement = fixture
        .run(IMPLEMENT_TASK, Provider::Codex, true)
        .expect("codex implement dry run");
    assert_eq!(implement.decision.provider, Provider::Codex);
    assert!(
        implement
            .decision
            .classification
            .as_ref()
            .is_some_and(|classification| classification.invokes_implement),
        "the classifier path must stamp the implement detector"
    );
    assert_eq!(implement.decision.effort, None);

    let rows = fixture.rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].task, IMPLEMENT_TASK);
    assert_eq!(
        rows[0].effort, None,
        "newest row: codex implement, no effort"
    );
    assert_eq!(rows[1].effort.as_deref(), Some("low"));
}
