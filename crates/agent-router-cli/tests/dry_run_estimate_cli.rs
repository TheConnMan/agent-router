//! Feature 3 through the real binary: what `agent-router run --dry-run` says a job will draw.
//!
//! Every row this fixture reads was written by an earlier invocation of the same binary, and every
//! percentage in it comes from a codex rollout the fixture wrote, so nothing here depends on this
//! box's real usage. Claude is deliberately never the provider under test: `claude_headroom()`
//! reads the machine wide `/tmp/claude-usage-cache.json`, which a temp HOME does not isolate.
//! `CLAUDE_USAGE_CACHE` now moves that path, so a fixture here could fix Claude's read too; nothing
//! in this file has been rewritten to depend on it.
//!
//! The whole file is unix only because the fixture stubs its provider binaries as shell scripts,
//! which is the idiom `run_json_tests.rs` and `stats_cli.rs` already use.
#![cfg(unix)]

use serde_json::{Value, json};
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

/// The weekly percentages the seeded jobs are decided against, oldest first. The steps between
/// them are all 10.0, so the median gap is 10.0 whichever way the sample is counted.
const SEEDED_WEEKLY_PCTS: [i64; 5] = [10, 20, 30, 40, 50];
/// The median gap those percentages produce.
const SEEDED_MEDIAN: f64 = 10.0;
/// How many comparable observations the projection needs before it will answer with a number.
const NEEDED: u64 = 3;

const SEED_TASK: &str = "a seeded job on the codex tier";
const DRY_RUN_TASK: &str = "the task being priced";

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
            "agent-router-estimate-cli-{}-{serial}-{label}-{unique}",
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

/// A stub that refuses every invocation. Both providers get one: claude is never asked to classify
/// here (every invocation names its provider), and codex must fail its dispatch fast rather than
/// reach a real app-server daemon on this box.
fn write_refusing_stub(bin: &Path, name: &str) {
    common::write_stub(&bin.join(name), "exit 1\n");
}

struct EstimateFixture {
    root: TempDir,
}

impl EstimateFixture {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        for child in ["home", "bin", "working directory"] {
            fs::create_dir_all(root.path.join(child)).expect("create fixture directory");
        }
        write_refusing_stub(&root.path.join("bin"), "claude");
        write_refusing_stub(&root.path.join("bin"), "codex");
        Self { root }
    }

    /// A codex rollout directory whose weekly window reads `used_percent`, so every invocation
    /// against it records a chosen codex weekly percentage rather than an observed one.
    fn sessions(&self, used_percent: i64) -> PathBuf {
        let directory = self
            .root
            .path
            .join(format!("codex sessions {used_percent}"));
        fs::create_dir_all(&directory).expect("create the rollout directory");
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
                        "used_percent": used_percent,
                        "resets_at": resets_at,
                    },
                    "secondary": null,
                }
            }
        })
        .to_string();
        fs::write(directory.join("rollout.jsonl"), format!("{line}\n"))
            .expect("write the codex rollout");
        directory
    }

    /// The router binary against this fixture's fake PATH, home, and decision log.
    fn router(&self, used_percent: i64) -> Command {
        let path = format!(
            "{}:{}",
            self.root.path.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .env("HOME", self.root.path.join("home"))
            .env("GROK_HOME", self.root.path.join("grok-home"))
            .env("CODEX_SESSIONS_DIR", self.sessions(used_percent))
            .env("PATH", path);
        command
    }

    /// One non dry run decision on the codex tier, logged at the given weekly percentage. The
    /// dispatch itself fails against the stub, which is fine and is the point: `run` records the
    /// row before it reports the failure, and it is the row the projection reads.
    ///
    /// So the seed is checked against the reason it failed, never against its exit status: every
    /// way this can go wrong exits 1, including the ways that die before the row is written. The
    /// check is a proxy. It reads which failure occurred, not directly that a row landed, because
    /// reading the log here would cost every seed a second router invocation.
    ///
    /// Every failure that reaches the log names the codex dispatch, and there are more of them
    /// than the obvious one. `... daemon start` exited is the intended path: the stub ran and
    /// refused. Under load the same path instead reports `... daemon start` stdout was
    /// unreadable, because `run_reporting_failure` waits only 250 ms on the stdout reader thread
    /// and does that before it looks at the exit status; it can also report `... daemon start`
    /// timed out after ..., or could not wait for `... daemon start`. All of those record the row
    /// just the same, so naming the codex dispatch is what the assertion asks for, not one
    /// particular wording of the refusal.
    ///
    /// One failure names the codex dispatch and must still fail loudly, so it is excluded by
    /// name: could not run `codex app-server daemon start` means the stub could not be executed
    /// at all, which is the ETXTBSY or ENOENT shape this fixture exists to catch. The exclusion
    /// is anchored to that exact literal, the one `run_reporting_failure` emits when `spawn`
    /// itself fails for this command, rather than a bare "could not run" that also appears in
    /// unrelated production prose elsewhere in the binary; an unanchored substring could reject a
    /// seed over a message that has nothing to do with this dispatch. The failures that record
    /// nothing say something else entirely and name no dispatch at all (`target directory does
    /// not exist`, or a bare os error from the log file being unopenable). Without this the first
    /// sign of a seed that recorded nothing is the row set assertion 140 lines below, which names
    /// the projection rather than the fixture.
    fn seed_job(&self, used_percent: i64) {
        let output = self
            .router(used_percent)
            .arg("run")
            .arg(SEED_TASK)
            .arg("--dir")
            .arg(self.root.path.join("working directory"))
            .arg("--provider")
            .arg("codex")
            .output()
            .expect("run the router");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("codex app-server daemon")
                && !stderr.contains("could not run `codex app-server daemon start`"),
            "the seed at {used_percent} percent did not fail the way a refused dispatch fails, so \
             it may have recorded no row at all, status: {:?}, stderr: {stderr}",
            output.status
        );
    }

    fn seed_comparable_jobs(&self) {
        for used_percent in SEEDED_WEEKLY_PCTS {
            self.seed_job(used_percent);
        }
    }

    fn dry_run(&self, json: bool) -> Output {
        let mut command = self.router(SEEDED_WEEKLY_PCTS[SEEDED_WEEKLY_PCTS.len() - 1]);
        command
            .arg("run")
            .arg(DRY_RUN_TASK)
            .arg("--dir")
            .arg(self.root.path.join("working directory"))
            .arg("--provider")
            .arg("codex")
            .arg("--dry-run");
        if json {
            command.arg("--json");
        }
        command.output().expect("run the router")
    }

    /// The dry run's human output, with its exit code checked: a dry run dispatches nothing, so it
    /// has nothing to fail at.
    fn dry_run_stdout(&self) -> String {
        let output = self.dry_run(false);
        assert!(
            output.status.success(),
            "the dry run failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 stdout")
    }

    fn dry_run_json(&self) -> Value {
        let output = self.dry_run(true);
        assert!(
            output.status.success(),
            "the dry run failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("router json")
    }

    fn log_rows(&self) -> Vec<Value> {
        let output = self
            .router(SEEDED_WEEKLY_PCTS[0])
            .args(["log", "--json", "--limit", "200"])
            .output()
            .expect("run the router");
        assert!(
            output.status.success(),
            "log --json failed, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: Value = serde_json::from_slice(&output.stdout).expect("router json");
        rows.as_array().expect("log --json prints an array").clone()
    }
}

/// The one line the estimate is rendered on, so an assertion about what it does not say cannot be
/// satisfied by some other line of the output.
fn estimate_line(stdout: &str) -> String {
    stdout
        .lines()
        .find(|line| line.starts_with("estimate:"))
        .unwrap_or_else(|| panic!("a dry run must print an estimate line, got:\n{stdout}"))
        .to_string()
}

fn between<'a>(line: &'a str, start: &str, end: &str) -> &'a str {
    let rest = line
        .split_once(start)
        .unwrap_or_else(|| panic!("the estimate line must contain {start:?}: {line}"))
        .1;
    rest.split_once(end)
        .unwrap_or_else(|| panic!("the estimate line must contain {end:?} after {start:?}: {line}"))
        .0
}

/// The acceptance criterion: with nothing to derive a projection from, the dry run says so and
/// prints no number at all.
#[test]
fn a_dry_run_prints_an_explicit_insufficient_data_line_on_an_empty_log() {
    let fixture = EstimateFixture::new("empty-log");

    let stdout = fixture.dry_run_stdout();
    let line = estimate_line(&stdout);
    assert!(
        !line.contains('%'),
        "an empty log produced a percentage, which can only have been invented: {line}"
    );
    assert_eq!(
        line, "estimate: insufficient data (0 comparable jobs, 3 needed)",
        "full output:\n{stdout}"
    );

    let estimate = &fixture.dry_run_json()["estimate"];
    assert_eq!(
        estimate["weekly_pct"],
        Value::Null,
        "an empty log must carry no number in the JSON either: {estimate}"
    );
    assert_eq!(estimate["samples"], 0);
    assert_eq!(estimate["needed"], NEEDED);
}

#[test]
fn a_dry_run_projects_a_draw_once_the_log_carries_comparable_jobs() {
    let fixture = EstimateFixture::new("comparable-jobs");
    fixture.seed_comparable_jobs();

    // The projection reads rows, so a fixture that quietly recorded none, or that recorded them
    // all against one observed percentage, would make every assertion below meaningless.
    let rows = fixture.log_rows();
    let mut recorded: Vec<f64> = rows
        .iter()
        .filter(|row| row["dry_run"] == false && row["provider"] == "codex")
        .map(|row| {
            row["codex_weekly_pct"]
                .as_f64()
                .expect("a row must carry a codex weekly percentage")
        })
        .collect();
    recorded.sort_by(|left, right| left.partial_cmp(right).expect("no NaN"));
    let wanted: Vec<f64> = SEEDED_WEEKLY_PCTS
        .iter()
        .map(|percent| *percent as f64)
        .collect();
    assert_eq!(
        recorded, wanted,
        "the fixture must have recorded one row per seeded job, at the seeded percentages"
    );

    let stdout = fixture.dry_run_stdout();
    let line = estimate_line(&stdout);
    let percent: f64 = between(&line, "up to ", "% of")
        .parse()
        .unwrap_or_else(|_| panic!("the projected percentage must be a number: {line}"));
    assert!(
        (percent - SEEDED_MEDIAN).abs() < 0.05,
        "the seeded gaps are all {SEEDED_MEDIAN}, so the median is too: {line}"
    );
    assert!(
        line.contains("of the codex weekly window"),
        "the projection must name the window it is a share of: {line}"
    );
    let reported_jobs: u64 = between(&line, "median gap over ", " comparable jobs")
        .parse()
        .unwrap_or_else(|_| panic!("the sample count must be a number: {line}"));

    let estimate = &fixture.dry_run_json()["estimate"];
    assert_eq!(estimate["provider"], "codex");
    assert!(
        estimate["model"].as_str().is_some_and(|m| !m.is_empty()),
        "the sample key is the tier, so the JSON must name it: {estimate}"
    );
    let weekly_pct = estimate["weekly_pct"].as_f64().unwrap_or_else(|| {
        panic!("weekly_pct must be a number once the log supports one: {estimate}")
    });
    assert!(
        (weekly_pct - SEEDED_MEDIAN).abs() < 0.05,
        "the JSON projection is {weekly_pct}, want {SEEDED_MEDIAN}: {estimate}"
    );
    assert_eq!(estimate["needed"], NEEDED);
    let samples = estimate["samples"]
        .as_u64()
        .unwrap_or_else(|| panic!("samples must be a count: {estimate}"));
    assert!(
        samples >= NEEDED,
        "a number was printed off {samples} samples with {NEEDED} needed: {estimate}"
    );
    assert_eq!(
        samples, reported_jobs,
        "the human line and the JSON must report the same sample count: {line}"
    );
}

/// The step between two consecutive rows is the provider's total weekly movement over that
/// interval, so it carries every other job and every interactive session in the same period. The
/// wording is the only thing keeping the number from being read as this job's own cost, and an
/// edit that shortens the line back to a bare percentage is exactly the regression this pins.
#[test]
fn a_projected_draw_is_labelled_as_an_upper_bound() {
    let fixture = EstimateFixture::new("upper-bound");
    fixture.seed_comparable_jobs();

    let stdout = fixture.dry_run_stdout();
    let line = estimate_line(&stdout);
    assert!(
        line.starts_with("estimate: up to "),
        "a projection stated as a flat figure claims to be the job's own draw: {line}"
    );
    assert!(
        line.contains("includes other usage in the same period"),
        "the projection must say what else is inside the number: {line}"
    );

    let percent = between(&line, "up to ", "% of");
    let jobs = between(&line, "median gap over ", " comparable jobs");
    assert_eq!(
        line,
        format!(
            "estimate: up to {percent}% of the codex weekly window (median gap over {jobs} \
             comparable jobs, includes other usage in the same period)"
        ),
        "the wording is what makes the number honest, so it is pinned word for word"
    );
}
