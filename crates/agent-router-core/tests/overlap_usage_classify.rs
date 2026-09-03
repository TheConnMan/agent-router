//! The usage snapshot and the classifier call are independent inputs to `decide`. They used to
//! run in series on every auto route, so the usage read's 20-600ms (and up to ~18s on HTTP
//! timeout) sat on top of the 3.4-8s classifier. This file is the test that fails if they
//! serialize again.

#![cfg(unix)]

mod common;

use agent_router_core::Context;
use agent_router_core::binary::{CLAUDE_BIN_ENV, Environment};
use agent_router_core::config::{ClassifierEngine, Config};
use agent_router_core::log::DecisionLog;
use agent_router_core::run::{Request, run_with};
use agent_router_core::{Headroom, UsageSnapshot};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const USAGE_DELAY: Duration = Duration::from_millis(500);
const CLASSIFIER_DELAY_SECS: &str = "0.5";
const SERIAL_BUDGET: Duration = Duration::from_millis(850);

fn envelope(result: &str) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": result,
    })
    .to_string()
}

fn distinctive_usage() -> UsageSnapshot {
    UsageSnapshot {
        claude: Headroom {
            five_hour_pct: 1.25,
            five_hour_reset_epoch: 1_800_000_100,
            weekly_pct: 11.11,
            weekly_reset_epoch: 1_800_000_000,
            weekly_capacity_known: true,
            stale: false,
        },
        codex: Headroom {
            five_hour_pct: 2.25,
            five_hour_reset_epoch: 1_800_000_200,
            weekly_pct: 22.22,
            weekly_reset_epoch: 1_800_000_000,
            weekly_capacity_known: true,
            stale: false,
        },
        grok: Headroom {
            five_hour_pct: 3.25,
            five_hour_reset_epoch: 1_800_000_300,
            weekly_pct: 33.33,
            weekly_reset_epoch: 1_800_000_000,
            weekly_capacity_known: true,
            stale: false,
        },
    }
}

fn slow_classifier_environment(root: &Path) -> Environment {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create the stub directory");
    let answer_path = root.join("classifier.answer");
    let result = json!({
        "orchestration": false,
        "missing_connector": false,
        "complexity": "low",
        "task_context_horizon": "ordinary",
        "rationale": "fixture overlap",
        "job_name": "Overlap Usage Classify",
    })
    .to_string();
    fs::write(&answer_path, envelope(&result)).expect("write the classifier answer");
    let stub = bin.join("claude");
    common::write_stub(
        &stub,
        &format!(
            "sleep {CLASSIFIER_DELAY_SECS}\ncat '{}'\nexit 0\n",
            answer_path.display()
        ),
    );
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create the empty HOME");
    Environment::new(
        None,
        Some(home),
        BTreeMap::from([(CLAUDE_BIN_ENV.to_string(), OsString::from(stub))]),
    )
}

/// Both the usage reader and the classifier sleep ~500ms. If `run_with` still calls them in
/// series, wall time is ~1s and this fails. Overlap keeps it under the serial sum, and the
/// injected snapshot still has to survive into the Decision.
#[test]
fn usage_read_overlaps_classification_on_the_auto_route() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd = root.path().join("workdir");
    fs::create_dir_all(&cwd).expect("create the target directory");
    let db_path = root.path().join("router.db");
    let environment = slow_classifier_environment(root.path());
    let mut config = Config::default();
    config.classifier.engine = ClassifierEngine::Claude;
    let ctx = Context::new(environment, root.path().join("home"), config);
    let injected = distinctive_usage();
    let request = Request {
        task: "summarize the weekly usage report",
        dir: &cwd,
        provider: None,
        model: None,
        effort: None,
        name: Some("Overlap Usage Classify".to_string()),
        dry_run: true,
        mcp_configs: &[],
        strict_mcp_config: false,
    };

    let started = Instant::now();
    let outcome = run_with(
        &request,
        &ctx,
        || {
            std::thread::sleep(USAGE_DELAY);
            injected
        },
        || DecisionLog::open_at(&db_path),
    )
    .expect("auto dry-run routes");
    let elapsed = started.elapsed();

    assert!(
        elapsed < SERIAL_BUDGET,
        "usage read and classification must overlap: elapsed {elapsed:?} is not well under the 1s serial sum"
    );
    assert_eq!(
        outcome.decision.usage, injected,
        "the injected snapshot must still be the Decision's usage"
    );
    let classification = outcome
        .decision
        .classification
        .as_ref()
        .expect("an automatic route always classifies");
    assert!(
        !classification.classifier_failed,
        "the stub classifier must have answered: {}",
        classification.rationale
    );
}
