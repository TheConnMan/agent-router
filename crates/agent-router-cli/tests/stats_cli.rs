//! Feature 1 through the real binary: `agent-router stats` against a decision log this fixture
//! wrote itself, one dry run at a time.
//!
//! The whole file is unix only because the fixture stubs its provider binaries as shell scripts,
//! which is the idiom `run_json_tests.rs` already uses.
#![cfg(unix)]

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

/// The gates that move a task off the provider its verdict named, as the router defines them at
/// the end of stream 1. Restated here rather than imported so the reconciliation below is an
/// independent count rather than the implementation agreeing with itself.
const FLIP_GATES: [&str; 2] = ["flipped_on_exhaustion", "headroom_tiebreak"];

/// The codex rollout directory each invocation scans. Idle is empty, so codex reads as fail open
/// with nothing consumed; exhausted carries a weekly number past the hard ceiling.
const IDLE: &str = "idle codex sessions";
const EXHAUSTED: &str = "exhausted codex sessions";

const SCORED_LOW: &str = "SCORED LOW: rename one constant";
const SCORED_ULTRA: &str = "SCORED ULTRA: review the routing architecture";
const SCORED_MEDIUM: &str = "a normal well scoped implementation";
const CLASSIFIER_FAILS: &str = "CLASSIFIER FAILS: nothing can score this one";
const ON_EXHAUSTED_CODEX: &str = "a normal well scoped implementation, codex out of room";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-stats-cli-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// The envelope shape of `claude -p --output-format json`, carrying one classification answer.
fn envelope(complexity: &str) -> String {
    let answer = json!({
        "codex_ready": [true, true, true, true, true, true],
        "claude_signals": [false, false, false, false, false, false],
        "missing_connector": false,
        "verdict": "codex",
        "confidence": "high",
        "complexity": complexity,
        "rationale": "stats fixture",
    })
    .to_string();
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": answer,
    })
    .to_string()
}

/// A classifier that answers from a marker in the task, so every row in this fixture is chosen
/// rather than observed. The nonzero exit is how the real fallback path, and with it the
/// `classifier_failed` gate, is reached without a model.
fn write_fake_claude(bin: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         for argument in \"$@\"; do prompt=$argument; done\n\
         case \"$prompt\" in\n\
           *'CLASSIFIER FAILS'*) exit 3 ;;\n\
           *'SCORED LOW'*) printf '%s\\n' {} ;;\n\
           *'SCORED ULTRA'*) printf '%s\\n' {} ;;\n\
           *) printf '%s\\n' {} ;;\n\
         esac\n",
        shell_quote(&envelope("low")),
        shell_quote(&envelope("ultra")),
        shell_quote(&envelope("medium")),
    );
    let binary = bin.join("claude");
    fs::write(&binary, script).expect("write fake claude");
    let mut permissions = fs::metadata(&binary).expect("fake metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&binary, permissions).expect("make fake executable");
}

/// One rollout whose weekly window is past the hard ceiling, so a codex verdict read against it
/// fires a weekly gate rather than routing untouched.
fn write_exhausted_rollout(sessions: &Path) {
    let resets_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        + 3_600;
    let line = json!({
        "payload": {
            "rate_limits": {
                "primary": {
                    "window_minutes": 10080,
                    "used_percent": 99,
                    "resets_at": resets_at,
                },
                "secondary": null,
            }
        }
    })
    .to_string();
    fs::write(sessions.join("rollout.jsonl"), format!("{line}\n"))
        .expect("write the codex rollout");
}

struct StatsFixture {
    root: TempDir,
}

impl StatsFixture {
    fn new() -> Self {
        let root = TempDir::new();
        for child in ["home", "bin", "working directory", IDLE, EXHAUSTED] {
            fs::create_dir_all(root.path.join(child)).expect("create fixture directory");
        }
        write_fake_claude(&root.path.join("bin"));
        write_exhausted_rollout(&root.path.join(EXHAUSTED));
        Self { root }
    }

    /// The router binary against this fixture's fake PATH, home, and decision log. `sessions` is
    /// the codex rollout directory this invocation reads its codex headroom from.
    fn router(&self, sessions: &str) -> Command {
        let path = format!(
            "{}:{}",
            self.root.path.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .env("HOME", self.root.path.join("home"))
            .env("CODEX_SESSIONS_DIR", self.root.path.join(sessions))
            .env("PATH", path);
        command
    }

    /// One decided and logged task that dispatches nothing. `provider` is None for the auto path.
    fn dry_run(&self, task: &str, provider: Option<&str>, sessions: &str) {
        let mut command = self.router(sessions);
        command
            .arg("run")
            .arg(task)
            .arg("--dir")
            .arg(self.root.path.join("working directory"))
            .arg("--dry-run");
        if let Some(provider) = provider {
            command.arg("--provider").arg(provider);
        }
        let output = command.output().expect("run the router");
        assert!(
            output.status.success(),
            "dry run of {task:?} failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self
            .router(IDLE)
            .args(arguments)
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "{arguments:?} failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("router json")
    }

    fn stats(&self) -> Output {
        self.router(IDLE)
            .arg("stats")
            .output()
            .expect("run the router")
    }
}

fn text(row: &Value, key: &str) -> String {
    row[key]
        .as_str()
        .unwrap_or_else(|| panic!("a log row must carry a string {key}"))
        .to_string()
}

/// The tags on one log row. The column is a comma joined string, and an empty one is no gate.
fn gate_tags(row: &Value) -> Vec<String> {
    text(row, "gates")
        .split(',')
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// A row with no complexity was never scored, which is a value in the distribution rather than an
/// absence from it.
fn complexity_tag(row: &Value) -> String {
    match row["complexity"].as_str() {
        Some(complexity) => complexity.to_string(),
        None => "unscored".to_string(),
    }
}

fn tally(values: Vec<String>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts
}

fn object_counts(stats: &Value, key: &str) -> BTreeMap<String, usize> {
    stats[key]
        .as_object()
        .unwrap_or_else(|| panic!("stats --json must carry a {key} object"))
        .iter()
        .map(|(name, count)| {
            let count = count
                .as_u64()
                .unwrap_or_else(|| panic!("{key}.{name} must be a count"));
            (name.clone(), count as usize)
        })
        .collect()
}

/// The acceptance criterion, executed rather than checked by hand: every number `stats --json`
/// reports is recomputed here from the rows `log --json --limit 200` prints over the same window,
/// so the two commands must be summarizing the same decisions.
#[test]
fn stats_json_reconciles_with_the_log_json_it_summarises() {
    let fixture = StatsFixture::new();
    fixture.dry_run(SCORED_LOW, None, IDLE);
    fixture.dry_run(SCORED_ULTRA, None, IDLE);
    fixture.dry_run(SCORED_MEDIUM, None, IDLE);
    fixture.dry_run(CLASSIFIER_FAILS, None, IDLE);
    fixture.dry_run(ON_EXHAUSTED_CODEX, None, EXHAUSTED);
    fixture.dry_run("explicitly on claude", Some("claude"), IDLE);
    fixture.dry_run("explicitly on opencode", Some("opencode"), IDLE);

    let rows: Vec<Value> = fixture
        .json(&["log", "--json", "--limit", "200"])
        .as_array()
        .expect("log --json prints an array")
        .clone();
    let stats = fixture.json(&["stats", "--json"]);

    // The fixture chose this shape, so a silent change in what it produced is a failure rather
    // than a quietly weaker reconciliation: a report of all zeros would otherwise agree with a log
    // that never got written.
    assert_eq!(rows.len(), 7, "the fixture must have recorded seven rows");
    assert_eq!(stats["rows_considered"], 7);
    assert_eq!(stats["auto_routes"], 5);
    assert_eq!(stats["classifier_failure_rate"]["numerator"], 1);
    let gates = object_counts(&stats, "gates");
    assert_eq!(gates.get("explicit_provider").copied(), Some(2));
    assert_eq!(gates.get("classifier_failed").copied(), Some(1));

    // The one usage driven row: codex reads 99 percent weekly from this fixture's rollout, so
    // exactly one weekly gate must have fired on it. WHICH of the two it is depends on claude's
    // own weekly number, and that comes from a machine wide cache no fixture can set, so the flip
    // rate is not pinned to a number here; `tests/stats.rs` owns its semantics. What is pinned is
    // that the usage plumbing reached the decision at all.
    let exhausted = rows
        .iter()
        .find(|row| text(row, "task") == ON_EXHAUSTED_CODEX)
        .expect("the row decided against an exhausted codex");
    let fired = gate_tags(exhausted);
    assert_eq!(
        fired.len(),
        1,
        "gates on the exhausted codex row: {fired:?}"
    );
    assert!(
        fired[0] == "flipped_on_exhaustion" || fired[0] == "over_ceiling",
        "an exhausted codex must fire a weekly gate, got {fired:?}"
    );

    assert_eq!(
        object_counts(&stats, "routes"),
        tally(rows.iter().map(|row| text(row, "provider")).collect())
    );
    assert_eq!(gates, tally(rows.iter().flat_map(gate_tags).collect()));
    assert_eq!(
        object_counts(&stats, "complexity"),
        tally(rows.iter().map(complexity_tag).collect())
    );

    let auto: Vec<&Value> = rows
        .iter()
        .filter(|row| text(row, "requested") == "auto")
        .collect();
    assert_eq!(stats["auto_routes"], auto.len() as u64);

    let flipped = auto
        .iter()
        .filter(|row| {
            gate_tags(row)
                .iter()
                .any(|tag| FLIP_GATES.contains(&tag.as_str()))
        })
        .count();
    assert_eq!(stats["flip_rate"]["numerator"], flipped as u64);
    assert_eq!(stats["flip_rate"]["denominator"], auto.len() as u64);

    let unclassified = auto
        .iter()
        .filter(|row| gate_tags(row).iter().any(|tag| tag == "classifier_failed"))
        .count();
    assert_eq!(
        stats["classifier_failure_rate"]["numerator"],
        unclassified as u64
    );
    assert_eq!(
        stats["classifier_failure_rate"]["denominator"],
        auto.len() as u64
    );

    let dry_runs = rows.iter().filter(|row| row["dry_run"] == true).count();
    assert_eq!(stats["dry_run_share"]["numerator"], dry_runs as u64);
    assert_eq!(stats["dry_run_share"]["denominator"], rows.len() as u64);

    // The window itself, so the two commands are proved to be reading the same rows rather than
    // agreeing on totals by coincidence.
    let stamps: Vec<i64> = rows
        .iter()
        .map(|row| row["created_at_ms"].as_i64().expect("a row timestamp"))
        .collect();
    assert_eq!(
        stats["oldest_created_at_ms"].as_i64(),
        stamps.iter().min().copied()
    );
    assert_eq!(
        stats["newest_created_at_ms"].as_i64(),
        stamps.iter().max().copied()
    );
}

#[test]
fn stats_on_an_empty_database_exits_zero_and_reports_no_rows() {
    let fixture = StatsFixture::new();

    let output = fixture.stats();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        stdout.contains('0'),
        "the row count must be shown: {stdout}"
    );
    // An empty window has no denominator, so any percentage on the screen was invented, and a
    // share computed as zero over zero prints as NaN.
    assert!(
        !stdout.contains('%'),
        "an empty window rendered a rate: {stdout}"
    );
    assert!(
        !stdout.to_lowercase().contains("nan"),
        "an empty window rendered a NaN: {stdout}"
    );

    let stats = fixture.json(&["stats", "--json"]);
    assert_eq!(stats["rows_considered"], 0);
    assert_eq!(stats["auto_routes"], 0);
    assert_eq!(stats["oldest_created_at_ms"], Value::Null);
    assert_eq!(stats["newest_created_at_ms"], Value::Null);
    for rate in ["flip_rate", "classifier_failure_rate", "dry_run_share"] {
        assert_eq!(stats[rate]["numerator"], 0, "{rate} numerator");
        assert_eq!(stats[rate]["denominator"], 0, "{rate} denominator");
    }
}
