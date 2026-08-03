//! Feature 1 through the real binary: `agent-router stats` against a decision log this fixture
//! wrote itself, one dry run at a time.
//!
//! The whole file is unix only because the fixture stubs its provider binaries as shell scripts,
//! which is the idiom `run_json_tests.rs` already uses.
#![cfg(unix)]

use agent_router_core::stats::Stats;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// One copy of the stub helper, included by path from the core crate's tests. A workspace
// `test-support` dev-dependency crate is the intended follow-up; this shape keeps the helper single
// sourced without editing Cargo.toml while another stream owns it.
#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

/// The gates that move a task off the provider it started on, as the router defines them.
/// Restated here rather than imported so the reconciliation below is an independent count rather
/// than the implementation agreeing with itself. `headroom_tiebreak` is retired but still counted,
/// because rows carrying it remain in the log.
const FLIP_GATES: [&str; 4] = [
    "flipped_on_exhaustion",
    "headroom_tiebreak",
    "pace_flip",
    "five_hour_pacing",
];

/// Every metric `stats --json` is contracted to publish. The CLI writes that object field by
/// field, so a metric added to the report and not surfaced there is otherwise invisible: it
/// compiles, it lints, and every assertion below still passes. `expected_metrics` binds this list
/// to the report's own fields, and the test asserts the whole key set rather than the presence of
/// the keys this file happens to read, so neither half can drift alone.
const METRICS: [&str; 17] = [
    "rows_considered",
    "oldest_created_at_ms",
    "newest_created_at_ms",
    "routes",
    "gates",
    "complexity",
    "router_versions",
    "auto_routes",
    "flip_rate",
    "classifier_failure_rate",
    "dry_run_share",
    "bad_rate_by_gate",
    "bad_rate_by_provider",
    "bad_rate_by_complexity",
    "failure_rate_by_gate",
    "failure_rate_by_provider",
    "failure_rate_by_complexity",
];

/// The six breakdowns commit 4 publishes, in the two families they belong to. Every one of them is
/// a map from a key to a rate carrying its own numerator and denominator.
const BAD_RATES: [&str; 3] = [
    "bad_rate_by_gate",
    "bad_rate_by_provider",
    "bad_rate_by_complexity",
];
const FAILURE_RATES: [&str; 3] = [
    "failure_rate_by_gate",
    "failure_rate_by_provider",
    "failure_rate_by_complexity",
];

/// The codex rollout directory each invocation scans. Idle is empty, so codex reads as fail open
/// with nothing consumed; exhausted carries a weekly number past the hard ceiling.
const IDLE: &str = "idle codex sessions";
const EXHAUSTED: &str = "exhausted codex sessions";

const SCORED_LOW: &str = "SCORED LOW: rename one constant";
const SCORED_ULTRA: &str = "SCORED ULTRA: review the routing architecture";
const SCORED_MEDIUM: &str = "a normal well scoped implementation";
const CLASSIFIER_FAILS: &str = "CLASSIFIER FAILS: nothing can score this one";
const ON_EXHAUSTED_CODEX: &str = "a normal well scoped implementation, codex out of room";

/// Makes every temp directory this file creates distinct from every other one, whatever the clock
/// does. `fs::create_dir_all` succeeds silently on a path that already exists, so two tests deriving
/// the same path would share one HOME and therefore one `router.db`, and one fixture's `Drop` would
/// delete a live sibling's directories mid run.
static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-router-stats-cli-{}-{serial}-{label}-{unique}",
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
        "orchestration": false,
        "missing_connector": false,
        "complexity": complexity,
        "task_context_horizon": "ordinary",
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
    // No interpreter line: the helper supplies exactly one, and its probe guard is emitted ahead of
    // this body. The ordering is load bearing rather than incidental. The `case` below reads the
    // LAST argument and its `*)` arm matches anything at all, so a probe that reached it would be
    // answered with a `medium` envelope instead of exiting; the guard is what keeps the catch-all
    // out of reach. Do not restructure this so the case can see the probe argument.
    let body = format!(
        "for argument in \"$@\"; do prompt=$argument; done\n\
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
    common::write_stub(&bin.join("claude"), &body);
}

/// One rollout whose weekly window is past the hard ceiling, so a codex route read against it
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
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
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

    /// One human judgement on one row, through the same `log --mark` path an operator uses, so the
    /// value under test reached the column the way a real one does.
    fn mark(&self, id: i64, mark: &str) {
        let output = self
            .router(IDLE)
            .args(["log", "--mark", &id.to_string(), mark])
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "marking #{id} {mark} failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

/// One published breakdown, as key to numerator and denominator. The share is deliberately not
/// read here: the two counts are what a reader checks by hand, and the share is checked against
/// them separately.
fn rate_map(stats: &Value, key: &str) -> BTreeMap<String, (u64, u64)> {
    stats[key]
        .as_object()
        .unwrap_or_else(|| panic!("stats --json must carry a {key} object"))
        .iter()
        .map(|(name, rate)| {
            let counts = (
                rate["numerator"]
                    .as_u64()
                    .unwrap_or_else(|| panic!("{key}.{name} must carry a numerator")),
                rate["denominator"]
                    .as_u64()
                    .unwrap_or_else(|| panic!("{key}.{name} must carry a denominator")),
            );
            (name.clone(), counts)
        })
        .collect()
}

/// The id the log gave the row that logged this task.
fn row_id(rows: &[Value], task: &str) -> i64 {
    rows.iter()
        .find(|row| text(row, "task") == task)
        .unwrap_or_else(|| panic!("no logged row for {task:?}"))["id"]
        .as_i64()
        .expect("a log row must carry an id")
}

/// The key set `stats --json` must print, derived by destructuring the report exhaustively. The
/// pattern carries no `..`, so a field added to `Stats` stops this file from compiling until the
/// metric is named here, and the assertion in the test then requires the CLI to print it too.
fn expected_metrics() -> BTreeSet<String> {
    let Stats {
        rows_considered: _,
        oldest_created_at_ms: _,
        newest_created_at_ms: _,
        routes: _,
        gates: _,
        complexity: _,
        router_versions: _,
        auto_routes: _,
        flip_rate: _,
        classifier_failure_rate: _,
        dry_run_share: _,
        bad_rate_by_gate: _,
        bad_rate_by_provider: _,
        bad_rate_by_complexity: _,
        failure_rate_by_gate: _,
        failure_rate_by_provider: _,
        failure_rate_by_complexity: _,
    } = Stats::default();
    METRICS.iter().map(|key| key.to_string()).collect()
}

fn top_level_keys(stats: &Value) -> BTreeSet<String> {
    stats
        .as_object()
        .expect("stats --json must print an object")
        .keys()
        .cloned()
        .collect()
}

/// The acceptance criterion, executed rather than checked by hand: every number `stats --json`
/// reports is recomputed here from the rows `log --json --limit 200` prints over the same window,
/// so the two commands must be summarizing the same decisions.
#[test]
fn stats_json_reconciles_with_the_log_json_it_summarises() {
    let fixture = StatsFixture::new("log-reconciliation");
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

    // Equality, not containment: a metric the report gained and the CLI never wrote would satisfy
    // every other assertion in this file, so completeness is the only thing that catches it.
    assert_eq!(
        top_level_keys(&stats),
        expected_metrics(),
        "stats --json must publish every metric and nothing else"
    );

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

/// Commit 4's acceptance criterion through the real binary: the bad rates `stats --json` publishes
/// reconcile against the rows `log --json` prints, over a window this fixture marked itself with
/// `log --mark`.
///
/// Six rows are logged and four are judged: the explicit claude route `bad`, the explicit opencode
/// route `good`, the unclassifiable route `bad`, and the low complexity route `good`. The medium
/// and ultra routes are left unmarked on purpose, because an unjudged row is the case the whole
/// denominator rule exists for.
#[test]
fn stats_json_emits_a_bad_rate_by_gate_that_reconciles_against_the_marked_rows() {
    let fixture = StatsFixture::new("bad-rate-by-gate");
    fixture.dry_run(SCORED_LOW, None, IDLE);
    fixture.dry_run(SCORED_ULTRA, None, IDLE);
    fixture.dry_run(SCORED_MEDIUM, None, IDLE);
    fixture.dry_run(CLASSIFIER_FAILS, None, IDLE);
    fixture.dry_run("explicitly on claude", Some("claude"), IDLE);
    fixture.dry_run("explicitly on opencode", Some("opencode"), IDLE);

    let logged: Vec<Value> = fixture
        .json(&["log", "--json", "--limit", "200"])
        .as_array()
        .expect("log --json prints an array")
        .clone();
    assert_eq!(logged.len(), 6, "the fixture must have recorded six rows");

    fixture.mark(row_id(&logged, "explicitly on claude"), "bad");
    fixture.mark(row_id(&logged, "explicitly on opencode"), "good");
    fixture.mark(row_id(&logged, CLASSIFIER_FAILS), "bad");
    fixture.mark(row_id(&logged, SCORED_LOW), "good");

    let rows: Vec<Value> = fixture
        .json(&["log", "--json", "--limit", "200"])
        .as_array()
        .expect("log --json prints an array")
        .clone();
    let stats = fixture.json(&["stats", "--json"]);

    assert_eq!(
        top_level_keys(&stats),
        expected_metrics(),
        "stats --json must publish every metric and nothing else"
    );

    // Four marks landed and two of them are bad, so a report reading anything else is summarizing
    // a different log than the one `log --json` just printed.
    let marked = rows.iter().filter(|row| !row["mark"].is_null()).count();
    let bad = rows.iter().filter(|row| row["mark"] == "bad").count();
    assert_eq!((bad, marked), (2, 4));

    // The counts a reader can check by hand. Two rows named their provider, so both carry
    // `explicit_provider` and both are judged, one bad: 1 of 2. One row could not be classified,
    // it is judged bad, and no other row carries that tag: 1 of 1. Complexity is the classifier's
    // own answer, so the two explicit rows are unscored and the unclassifiable one reads high.
    let by_gate = rate_map(&stats, "bad_rate_by_gate");
    assert_eq!(by_gate.get("explicit_provider").copied(), Some((1, 2)));
    assert_eq!(by_gate.get("classifier_failed").copied(), Some((1, 1)));
    let by_complexity = rate_map(&stats, "bad_rate_by_complexity");
    assert_eq!(by_complexity.get("unscored").copied(), Some((1, 2)));
    assert_eq!(by_complexity.get("high").copied(), Some((1, 1)));
    assert_eq!(by_complexity.get("low").copied(), Some((0, 1)));
    // The ultra and medium rows are the only ones at their tier and neither is judged, so neither
    // tier has a bad rate at all. Counting an unjudged row as good would report 0 of 1 here.
    assert_eq!(by_complexity.get("ultra").copied(), Some((0, 0)));
    assert_eq!(by_complexity.get("medium").copied(), Some((0, 0)));
    assert_eq!(
        stats["bad_rate_by_complexity"]["ultra"]["share"],
        Value::Null
    );
    assert_eq!(stats["bad_rate_by_gate"]["explicit_provider"]["share"], 0.5);
    // Only an explicit route reaches opencode, and that one is judged good.
    assert_eq!(
        rate_map(&stats, "bad_rate_by_provider")
            .get("opencode")
            .copied(),
        Some((0, 1))
    );

    // The whole of every bad rate, recomputed from the rows the log printed rather than read back
    // out of the report. Which provider each auto row landed on depends on this box's own usage
    // numbers, so the provider map is reconciled rather than pinned.
    let mut want_gate: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut want_provider: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut want_complexity: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for row in &rows {
        let judged = !row["mark"].is_null();
        let is_bad = row["mark"] == "bad";
        let count = |into: &mut BTreeMap<String, (u64, u64)>, key: String| {
            let counts = into.entry(key).or_insert((0, 0));
            if judged {
                counts.1 += 1;
                if is_bad {
                    counts.0 += 1;
                }
            }
        };
        for tag in gate_tags(row) {
            count(&mut want_gate, tag);
        }
        count(&mut want_provider, text(row, "provider"));
        count(&mut want_complexity, complexity_tag(row));
    }
    assert_eq!(by_gate, want_gate);
    assert_eq!(rate_map(&stats, "bad_rate_by_provider"), want_provider);
    assert_eq!(by_complexity, want_complexity);

    // Every key of every breakdown is a key of the distribution it breaks down, so a rate nobody
    // has judged is visible as a null share rather than as a missing key.
    for metric in BAD_RATES.iter().chain(&FAILURE_RATES) {
        let distribution = match *metric {
            name if name.ends_with("_by_gate") => "gates",
            name if name.ends_with("_by_provider") => "routes",
            _ => "complexity",
        };
        assert_eq!(
            rate_map(&stats, metric).keys().collect::<Vec<_>>(),
            object_counts(&stats, distribution)
                .keys()
                .collect::<Vec<_>>(),
            "{metric} must cover every key of {distribution}"
        );
    }

    // Every row here is a dry run, so nothing dispatched and nothing could be reconciled. A
    // failure rate over rows that never ran is the reading this pins out.
    for metric in FAILURE_RATES {
        for (key, (numerator, denominator)) in rate_map(&stats, metric) {
            assert_eq!(
                (numerator, denominator),
                (0, 0),
                "{metric}.{key} counted a dry run"
            );
            assert_eq!(stats[metric][&key]["share"], Value::Null, "{metric}.{key}");
        }
    }
}

/// A rate nobody has judged has no percentage to print. Every row here named its provider and every
/// one is a dry run, so the only rate in the report with a denominator at all is the dry run share:
/// one percentage on the screen, and a dash everywhere else. Any other percentage was invented, and
/// a zero over zero rendered as a number is how NaN reaches a terminal.
#[test]
fn the_human_report_prints_a_dash_for_a_rate_nobody_has_judged() {
    let fixture = StatsFixture::new("dash-for-unjudged");
    fixture.dry_run("explicitly on claude", Some("claude"), IDLE);
    fixture.dry_run("a second explicit claude route", Some("claude"), IDLE);
    fixture.dry_run("explicitly on opencode", Some("opencode"), IDLE);

    let output = fixture.stats();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    assert!(
        stdout.contains("explicit_provider"),
        "the gate distribution must be shown: {stdout}"
    );
    assert_eq!(
        stdout.matches('%').count(),
        1,
        "only the dry run share has a denominator: {stdout}"
    );
    assert!(
        !stdout.to_lowercase().contains("nan"),
        "a rate with no denominator rendered a NaN: {stdout}"
    );
}

/// The pooling regression through the real binary, in both renderings of the report.
///
/// On 2026-08-01 a routing quality review pooled 114 rows written by four incompatible routers and
/// read them as one population. The reader of that report was a human looking at the terminal, so a
/// version breakdown that only reached the JSON would have left the same reader with the same
/// undifferentiated totals: the human line is the one that had to exist, and it is asserted here.
///
/// This fixture runs one binary, so every row it writes must land under that binary's own version.
/// The reconciliation against `log --json` is what proves the report is counting the rows it claims
/// to be summarizing, and the pinned key is what stops both halves from agreeing on nothing: a
/// report and a log that both published null would reconcile perfectly.
#[test]
fn stats_reports_the_router_version_every_row_it_pooled_was_written_by() {
    let fixture = StatsFixture::new("router-versions");
    fixture.dry_run(SCORED_LOW, None, IDLE);
    fixture.dry_run(SCORED_MEDIUM, None, IDLE);
    fixture.dry_run("explicitly on claude", Some("claude"), IDLE);

    let rows: Vec<Value> = fixture
        .json(&["log", "--json", "--limit", "200"])
        .as_array()
        .expect("log --json prints an array")
        .clone();
    assert_eq!(rows.len(), 3, "the fixture must have recorded three rows");
    let stats = fixture.json(&["stats", "--json"]);

    assert_eq!(
        top_level_keys(&stats),
        expected_metrics(),
        "stats --json must publish every metric and nothing else"
    );

    // Both crates in this workspace ship at one version, so the version stamped into a row is the
    // version of the binary that wrote it. Equality with the version, not merely a present key: a
    // row labelled with the wrong build misattributes history exactly as an unlabelled one pools it.
    let version = env!("CARGO_PKG_VERSION").to_string();
    for row in &rows {
        assert_eq!(
            text(row, "router_version"),
            version,
            "every logged row must name the build that wrote it: {row}"
        );
    }
    let published = object_counts(&stats, "router_versions");
    assert_eq!(
        published,
        tally(rows.iter().map(|row| text(row, "router_version")).collect()),
        "the report must attribute the rows the log printed"
    );

    // The human reading the terminal is who pooled the versions in the first place, so the same
    // breakdown has to be on the screen, not only in the JSON a script reads.
    let output = fixture.stats();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let line = stdout
        .lines()
        .find(|line| line.starts_with("router versions:"))
        .unwrap_or_else(|| panic!("the human report must show the router versions: {stdout}"));
    assert_eq!(line, format!("router versions: {version} 3"));
}

#[test]
fn stats_on_an_empty_database_exits_zero_and_reports_no_rows() {
    let fixture = StatsFixture::new("empty-db");

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
    // A window with no rows has no keys to break down, so each of the six maps is an empty object
    // rather than a missing key or a null.
    for metric in BAD_RATES.iter().chain(&FAILURE_RATES) {
        assert_eq!(
            rate_map(&stats, metric),
            BTreeMap::new(),
            "{metric} over an empty window"
        );
    }
}
