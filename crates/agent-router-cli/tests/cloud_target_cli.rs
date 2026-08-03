//! The execution target end to end, through the built binary.
//!
//! The cloud-eligible fixture reaches a cloud decision through pure filesystem state: an
//! allowlisted repository, a branch whose remote-tracking ref matches its clean working tree, and a
//! seeded `cloud-environments.db` under the fixture's own `HOME`. That database is not a test
//! affordance. It is the same path, the same schema, and the same production code path a real run
//! takes on its fast path, so a test that seeds it exercises what production does rather than a
//! parallel path built to be tested.
//!
//! The fixture deliberately contains NO `.codex/auth.json` in most cases. Its absence is what
//! proves the cache is consulted before any credential is read, and it is why those tests need no
//! secret and never contact the environments endpoint.
//!
//! The credential-confinement case is the exception, and it is the exception on purpose. A seeded
//! cache returns at resolution step 6, so `resolve_environment` never runs and the whole credential
//! path goes unexecuted; a leak added inside it would ship green. That case therefore starts with a
//! COLD cache and points `$AGENT_ROUTER_ENVIRONMENTS_URL` at a `TcpListener` bound on loopback
//! inside this process, so the real credential read, the real request, and the real response parse
//! all run. Nothing here reaches the network: the listener is on `127.0.0.1` and the credential it
//! is handed is a sentinel this file invented.
//!
//! Every environment variable is set on the CHILD process. These tests run in parallel threads
//! inside one process, so `env::set_var` here would race with every other test in the binary.
#![cfg(unix)]

use agent_router_core::UsageSnapshot;
use agent_router_core::classify::{Classification, Complexity, TaskContextHorizon};
use agent_router_core::cloud::{CloudEligibility, CloudIneligible, CloudTarget};
use agent_router_core::config::Config;
use agent_router_core::decide::{Decision, decide};
use agent_router_core::log::{DecisionLog, Entry};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// One copy of the stub helper, the endpoint stub, and the fixture repository builder, included by
// path from the core crate's tests, exactly as `run_json_tests.rs` does. The core crate's
// `cloud_eligibility.rs` uses the same three. The probe guard is load bearing here: a probe that
// reached a stub's body would create the argv log whose non-existence this file asserts.
#[path = "../../agent-router-core/tests/common/mod.rs"]
mod common;

use common::{EnvironmentsStub, git};

/// The fixture repository's slug, in the case its remote states it.
const OWNER_REPO: &str = "fixture-owner/fixture-repo";
/// The remote-tracking ref of the fixture's branch, which is what a cloud task would really run.
const REMOTE_REF: &str = "refs/remotes/origin/task/fixture";
/// The environment id the seeded cache resolves that slug to.
const ENVIRONMENT_ID: &str = "env-fixture";

/// The id the stub endpoint answers with. Distinct from `ENVIRONMENT_ID` on purpose: a run that
/// reached this value can only have got it out of a response body, never out of a seeded cache.
const SERVED_ENVIRONMENT_ID: &str = "69a1b2c3d4e5f60718293a4b5c6d7e8f";

/// The body the stub endpoint answers a connected repository with.
///
/// The shape is OBSERVED, not invented. Measured against the live endpoint on 2026-08-03: a
/// connected repository answers HTTP 200 with a TOP-LEVEL JSON ARRAY whose first element carries
/// `id`, a 32 character lowercase hex string with no prefix; an unconnected one answers HTTP 200
/// with exactly `[]`. The sibling keys are kept rather than trimmed to `{"id":...}`, because a
/// one-key stub would keep passing against a parser that reads the sole key of the sole element
/// while the real multi-key payload resolved to nothing. Transcribed with its values scrubbed from
/// `crates/agent-router-core/tests/cloud_eligibility.rs`, which pins the same measurement.
const SERVED_BODY: &str = r#"[{"id":"69a1b2c3d4e5f60718293a4b5c6d7e8f","label":"example","repos":["github-1105248136"],"created_at":1773140014.26244,"is_pinned":false}]"#;

/// Makes every temp directory distinct whatever the clock does, so two tests can never share one
/// `HOME` and therefore one `router.db`. Same reason and same shape as `run_json_tests.rs`.
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
            "agent-router-cloud-{}-{serial}-{label}-{unique}",
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

struct CliFixture {
    root: TempDir,
    /// Set only by the case that drives the real credential path, and passed to the CHILD rather
    /// than to this process, so it cannot leak into a sibling test running on another thread.
    environments_url: Option<String>,
}

impl CliFixture {
    /// A fake `HOME` with an allowlisting config, a git repository with a GitHub `origin`, a plain
    /// non-repository directory beside it, and `claude` and `codex` stubs on `PATH`.
    ///
    /// The allowlist is populated in every case, including the ones that must route local. A config
    /// with an empty allowlist short-circuits before the git reads, so a local assertion under one
    /// would pass without ever proving anything about the repository.
    fn new(label: &str) -> CliFixture {
        let root = TempDir::new(label);
        let home = root.path.join("home");
        let bin = root.path.join("bin");
        let repo = root.path.join("repo");
        let plain = root.path.join("plain");
        for directory in [&home, &bin, &repo, &plain] {
            fs::create_dir_all(directory).expect("create a fixture directory");
        }

        fs::create_dir_all(home.join(".config/agent-router")).expect("create the config directory");
        fs::write(
            home.join(".config/agent-router/config.toml"),
            format!("config_version = 2\ncloud_repos = [\"{OWNER_REPO}\"]\n"),
        )
        .expect("write the config");

        git(&repo, &["init", "-b", "task/fixture"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:fixture-owner/fixture-repo.git",
            ],
        );
        fs::write(repo.join("README"), "fixture\n").expect("write a file to commit");
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--message", "fixture"]);
        // The remote-tracking ref a fetch would have left behind. Eligibility proves the remote ref
        // would run the source the operator is looking at, because `codex cloud exec --branch` runs
        // that ref rather than the working tree, so without this every cloud case here would stop at
        // `BranchNotOnRemote`. Written with `update-ref` because nothing in this file may touch a
        // network, and that ref is the only thing resolution reads.
        git(&repo, &["update-ref", REMOTE_REF, "HEAD"]);

        let fixture = CliFixture {
            root,
            environments_url: None,
        };
        fixture.write_claude_stub();
        fixture.write_codex_stub();
        fixture
    }

    fn home(&self) -> PathBuf {
        self.root.path.join("home")
    }

    fn repo(&self) -> PathBuf {
        self.root.path.join("repo")
    }

    /// A directory that is not a git repository at all, which is the other half of AC 3.
    fn plain(&self) -> PathBuf {
        self.root.path.join("plain")
    }

    /// Where the codex stub records having been invoked. Asserted NON-EXISTENT by the dry-run case:
    /// a file that is absent is a stronger claim than a file that is empty, and the probe guard in
    /// `write_stub` is what makes it reachable, since the probe exits before the body's append.
    fn codex_argv(&self) -> PathBuf {
        self.root.path.join("codex.argv")
    }

    fn claude_argv(&self) -> PathBuf {
        self.root.path.join("claude.argv")
    }

    /// The classifier stub, plus the `agents` listing a real claude dispatch resolves its short id
    /// through. Anything else it is invoked with is a dispatch, and is appended to the argv log.
    fn write_claude_stub(&self) {
        let agents = serde_json::to_string(&json!([{
            "id": "claude exact id",
            "sessionId": "claude full id",
            "cwd": self.plain(),
            "name": "a routed job",
            "startedAt": i64::MAX,
            "kind": "background",
            "state": "working"
        }]))
        .expect("agents json");
        let classifier_result = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": json!({
                "orchestration": false,
                "missing_connector": false,
                "complexity": "high",
                "task_context_horizon": "ordinary",
                "rationale": "fixture context",
            })
            .to_string(),
        })
        .to_string();
        // No interpreter line: the helper supplies exactly one, ahead of the probe guard.
        let body = format!(
            "if [ \"$1\" = \"agents\" ]; then\n\
               printf '%s\\n' {}\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"-p\" ]; then\n\
               printf '%s\\n' {}\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&agents),
            shell_quote(&classifier_result),
            shell_quote(&self.claude_argv().to_string_lossy())
        );
        common::write_stub(&self.root.path.join("bin/claude"), &body);
    }

    /// A codex stub whose every invocation, with any argument at all, creates the argv log. It
    /// branches on nothing, so no path through it can run without leaving the evidence behind.
    fn write_codex_stub(&self) {
        let body = format!(
            "printf '%s\\n' \"$@\" > {}\n",
            shell_quote(&self.codex_argv().to_string_lossy())
        );
        common::write_stub(&self.root.path.join("bin/codex"), &body);
    }

    /// The resolved-environment cache, in the schema and at the path production reads: one row in
    /// the `cloud_environments` table of its own database beside `router.db`.
    fn seed_environment_cache(&self) {
        let state = self.home().join(".local/state/agent-router");
        fs::create_dir_all(&state).expect("create the state directory");
        let conn = rusqlite::Connection::open(self.environment_cache_path())
            .expect("open the environment cache database");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cloud_environments (
                 slug           TEXT PRIMARY KEY,
                 environment_id TEXT NOT NULL
             );",
        )
        .expect("create the cache table");
        conn.execute(
            "INSERT INTO cloud_environments (slug, environment_id) VALUES (?1, ?2)",
            rusqlite::params![OWNER_REPO, ENVIRONMENT_ID],
        )
        .expect("seed the environment cache");
    }

    fn environment_cache_path(&self) -> PathBuf {
        self.home()
            .join(".local/state/agent-router/cloud-environments.db")
    }

    /// Send this fixture's runs at a stub endpoint instead of the real one.
    fn point_environments_at(&mut self, stub: &EnvironmentsStub) {
        self.environments_url = Some(stub.base_url().to_string());
    }

    /// Delete the cache a successful resolution just wrote, so the NEXT run resolves from scratch
    /// rather than short-circuiting at step 6.
    ///
    /// The precondition is asserted rather than assumed, which makes this the CLI-level proof that
    /// a successful resolution writes the cache at all: if the file is not there, the run either
    /// never reached step 9 or never reached the endpoint, and either way the run that follows was
    /// never going to exercise the credential path.
    fn clear_environment_cache(&self) {
        let path = self.environment_cache_path();
        assert!(
            path.exists(),
            "a successful cloud resolution must have written {}, so its absence means the run \
             resolved some other way and the next run cannot be assumed to reach the credential \
             path",
            path.display()
        );
        fs::remove_file(&path).expect("remove the environment cache");
    }

    /// A credential file carrying values nothing may ever echo. Only the security case writes one:
    /// every other case relies on its absence to prove the cache is read first.
    fn write_codex_auth(&self, token: &str, account_id: &str) {
        let codex_home = self.home().join(".codex");
        fs::create_dir_all(&codex_home).expect("create the codex home");
        fs::write(
            codex_home.join("auth.json"),
            json!({
                "tokens": {"access_token": token, "account_id": account_id},
                "access_token": token,
                "account_id": account_id,
            })
            .to_string(),
        )
        .expect("write the fake credential");
    }

    fn db_path(&self) -> PathBuf {
        self.home().join(".local/state/agent-router/router.db")
    }

    /// One row written into the fixture's own decision log, through the same writer a dispatch
    /// uses, so the row the tool then reads is shaped exactly like a real one. Seeding is what lets
    /// the read-back cases assert on a cloud row without a submit, a credential, or an endpoint.
    fn seed_row(
        &self,
        task: &str,
        decision: &Decision,
        job_id: &str,
        cloud_task_url: Option<&str>,
    ) {
        let log = DecisionLog::open_at(&self.db_path()).expect("open the fixture log");
        log.record(&Entry {
            task,
            dir: Path::new("/tmp"),
            requested: "auto",
            decision,
            dry_run: false,
            job_id: Some(job_id),
            job_name: None,
            outcome: "dispatched",
            effective_effort: None,
            cloud_task_url,
        })
        .expect("seed a decision row");
    }

    fn assert_no_codex_auth(&self) {
        assert!(
            !self.home().join(".codex/auth.json").exists(),
            "this case proves the cache is consulted before any credential read, so the fixture \
             must genuinely have no credential"
        );
    }

    fn router(&self) -> Command {
        let sessions = self.root.path.join("empty codex sessions");
        fs::create_dir_all(&sessions).expect("create the sessions directory");
        let path = format!(
            "{}:{}",
            self.root.path.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
        command
            .env("HOME", self.home())
            .env("CODEX_SESSIONS_DIR", sessions)
            .env("PATH", path);
        if let Some(url) = &self.environments_url {
            command.env("AGENT_ROUTER_ENVIRONMENTS_URL", url);
        }
        command
    }

    fn run(&self, dir: &Path, extra: &[&str]) -> Output {
        let mut command = self.router();
        command
            .arg("run")
            .arg("a routed job")
            .arg("--dir")
            .arg(dir)
            .arg("--provider")
            .arg("auto")
            .args(extra);
        command.output().expect("run the router")
    }
}

/// A decision built at a fixed instant with no usage rule in play, so a seeded row differs from its
/// neighbour in the execution target and in nothing else.
fn decision(cloud: CloudEligibility) -> Decision {
    decide(
        Classification {
            orchestration: false,
            missing_connector: false,
            complexity: Complexity::High,
            task_context_horizon: TaskContextHorizon::Ordinary,
            rationale: "fixture".to_string(),
            classifier_failed: false,
        },
        UsageSnapshot::full(),
        1_785_400_000,
        cloud,
        &Config::default(),
    )
}

fn cloud_decision() -> Decision {
    decision(CloudEligibility::Eligible(CloudTarget::resolved(
        ENVIRONMENT_ID.to_string(),
        "task/fixture".to_string(),
    )))
}

fn local_decision() -> Decision {
    decision(CloudEligibility::Ineligible(
        CloudIneligible::NotAllowlisted,
    ))
}

fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "the router failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

/// AC 1e and AC 3, both halves in one invocation: the projection says cloud, and nothing ran.
///
/// The non-existence assertion is the strong one. An empty argv log would also be produced by a
/// stub that ran and wrote nothing, so absence is what proves no `codex cloud exec` process was
/// created at all. `run` returns before `dispatch` on the dry-run path, which is the property that
/// makes a cloud dry run a projection rather than a submission.
#[test]
fn a_dry_run_in_a_cloud_eligible_repo_prints_cloud_and_executes_nothing() {
    let fixture = CliFixture::new("cloud-dry-run");
    fixture.seed_environment_cache();
    fixture.assert_no_codex_auth();

    let stdout = stdout_of(&fixture.run(&fixture.repo(), &["--dry-run"]));

    assert!(
        stdout.contains("execution_target=cloud"),
        "the dry run must project a cloud dispatch: {stdout}"
    );
    assert!(
        !fixture.codex_argv().exists(),
        "a dry run created a codex process: {}",
        fs::read_to_string(fixture.codex_argv()).unwrap_or_default()
    );
}

/// AC 3, the other half. The same invocation from a directory that is not a repository routes
/// local, and the allowlist is non-empty in this fixture so the local answer is reached by the git
/// check rather than by the empty-allowlist short circuit.
#[test]
fn a_dry_run_outside_a_git_repo_prints_local() {
    let fixture = CliFixture::new("local-dry-run");
    fixture.seed_environment_cache();

    let stdout = stdout_of(&fixture.run(&fixture.plain(), &["--dry-run"]));

    assert!(
        stdout.contains("execution_target=local"),
        "a directory that is not a repository has no cloud target: {stdout}"
    );
}

/// AC 1e, the machine output. The key is emitted on every run, so its absence never has to be
/// interpreted and a consumer can read it without first checking whether it is there.
#[test]
fn run_json_carries_the_execution_target() {
    let fixture = CliFixture::new("json-local");

    let stdout = stdout_of(&fixture.run(&fixture.plain(), &["--dry-run", "--json"]));
    let value: Value = serde_json::from_str(&stdout).expect("router json");

    assert_eq!(value["execution_target"], "local");
}

/// The human line and the JSON object are two separate call sites, and Behavior sites lists them as
/// a parity pair. Without this case the cloud value is asserted on one of the two only, so a
/// `outcome_json` emitting a hardcoded "local", or dropping the key entirely, would ship green
/// behind the human-output test above.
#[test]
fn run_json_carries_the_cloud_execution_target() {
    let fixture = CliFixture::new("json-cloud");
    fixture.seed_environment_cache();
    fixture.assert_no_codex_auth();

    let stdout = stdout_of(&fixture.run(&fixture.repo(), &["--dry-run", "--json"]));
    let value: Value = serde_json::from_str(&stdout).expect("router json");

    assert_eq!(value["execution_target"], "cloud");
    assert_eq!(
        value["provider"], "codex",
        "a cloud job is still Codex work, and the provider column is what every usage reader keys \
         off"
    );
}

/// The ordinary path, dispatched for real rather than projected, so the added token is proved on
/// the dispatch branch of the output as well as on the dry-run branch. An explicit provider never
/// reaches the cloud, and this is what would fail if the token stopped being printed on the path
/// that actually runs jobs.
#[test]
fn an_existing_local_run_still_prints_local() {
    let fixture = CliFixture::new("local-dispatch");
    fixture.seed_environment_cache();

    let mut command = fixture.router();
    let output = command
        .arg("run")
        .arg("a routed job")
        .arg("--dir")
        .arg(fixture.plain())
        .arg("--provider")
        .arg("claude")
        .output()
        .expect("run the router");
    let stdout = stdout_of(&output);

    assert!(
        stdout.contains("execution_target=local"),
        "an explicit claude dispatch runs locally and must say so: {stdout}"
    );
    assert!(
        stdout.contains("claude exact id"),
        "the fixture must have really dispatched, or this asserts about a job that never ran: \
         {stdout}"
    );
}

/// Nothing read out of `auth.json` may reach an output. The token and the account id are read into
/// locals inside `resolve_environment`, used once as header values, and dropped: they are not
/// fields on the resolved target, so they cannot ride out through the serialized decision, through
/// `--json`, through the printed line, or into the decision log.
///
/// **This case only means anything if `resolve_environment` actually runs.** The version this
/// replaced seeded the environment cache first, so resolution returned at step 6 and the one
/// function in the tree that opens `auth.json` was never called: the sentinels it planted were
/// searched for in the output of a run that had never touched a credential, and a leak added inside
/// `resolve_environment` would have shipped green. So the cache starts COLD here, the endpoint is a
/// loopback stub, and the run takes steps 7, 8, and 9 for real.
///
/// The received-request assertion is the one the old case could not make and is the whole point.
/// Asserting that the request carried the sentinel token and account id proves the credential path
/// executed; without it this can silently short-circuit back into vacuity and every negative below
/// keeps passing while proving nothing. Do not drop it, and do not replace the cold cache with a
/// seeded one to make the case faster.
///
/// The sentinels are searched for across the whole of each output rather than against named keys,
/// because a leak would arrive through whichever key nobody thought to check. The raw database file
/// is read as bytes for the same reason: `log --json` prints the columns the CLI chose to print,
/// and the question here is what landed on disk.
///
/// `execution_target=cloud` and the served environment id are the controls that keep the negatives
/// from being vacuous a second way: the first proves the run reached cloud at all, and the second
/// proves the id came out of the stub's response body rather than out of a cache entry.
#[test]
fn a_cloud_run_never_prints_or_records_credential_material() {
    const TOKEN: &str = "sk-sentinel-access-token-must-never-appear";
    const ACCOUNT_ID: &str = "sentinel-account-id-must-never-appear";

    let stub = EnvironmentsStub::serving(SERVED_BODY);
    let mut fixture = CliFixture::new("credential-leak");
    fixture.point_environments_at(&stub);
    fixture.write_codex_auth(TOKEN, ACCOUNT_ID);
    let auth = fs::read_to_string(fixture.home().join(".codex/auth.json"))
        .expect("read the fake credential back");
    assert!(
        auth.contains(TOKEN) && auth.contains(ACCOUNT_ID),
        "the fixture must genuinely hold the sentinels, or every assertion below is vacuous"
    );
    assert!(
        !fixture.environment_cache_path().exists(),
        "the cache must start cold, or resolution returns at step 6 and the credential path this \
         case exists to exercise never runs"
    );

    let human = stdout_of(&fixture.run(&fixture.repo(), &["--dry-run"]));
    // The first run wrote the cache, so the second would resolve from it and skip the credential
    // read. Clearing it is what keeps BOTH outputs the product of a real resolution.
    fixture.clear_environment_cache();
    let machine = stdout_of(&fixture.run(&fixture.repo(), &["--dry-run", "--json"]));

    assert!(
        human.contains("execution_target=cloud"),
        "the cloud path must have run, or nothing was exercised: {human}"
    );
    assert!(
        human.contains(SERVED_ENVIRONMENT_ID),
        "the printed environment id must be the one the endpoint answered with, or the run \
         resolved from somewhere other than the response body: {human}"
    );
    let value: Value = serde_json::from_str(&machine).expect("router json");
    assert_eq!(
        value["execution_target"], "cloud",
        "the second run must have reached cloud too, or its output is a local run's: {machine}"
    );

    // The proof that the credential path executed. Everything below is a negative, and a negative
    // over an unexecuted path is worth nothing.
    let requests = stub.requests();
    assert_eq!(
        requests.len(),
        2,
        "both runs must have reached the environments endpoint. A count of 0 means resolution \
         short-circuited before step 8 and this test has regressed to the vacuous version it \
         replaced: the sentinel searches below would then be scanning output from a run that never \
         opened auth.json, and a leak inside resolve_environment would pass unnoticed. Received: \
         {requests:?}"
    );
    let expected_authorization = format!("Bearer {TOKEN}");
    for request in &requests {
        assert_eq!(
            request.target, "/fixture-owner/fixture-repo",
            "the request must name the repository being resolved: {request:?}"
        );
        assert_eq!(
            request.header("authorization"),
            Some(expected_authorization.as_str()),
            "the environments request carried no Authorization header built from the sentinel \
             token, so resolve_environment did not run with this fixture's auth.json and the \
             credential-confinement assertions below are vacuous: {request:?}"
        );
        assert_eq!(
            request.header("chatgpt-account-id"),
            Some(ACCOUNT_ID),
            "the environments request carried no account id read from the sentinel auth.json, so \
             the credential path did not run and the assertions below are vacuous: {request:?}"
        );
    }

    let logged = stdout_of(
        &fixture
            .router()
            .arg("log")
            .arg("--limit")
            .arg("10")
            .arg("--json")
            .output()
            .expect("read the decision log"),
    );
    let database = fs::read(fixture.home().join(".local/state/agent-router/router.db"))
        .expect("read the decision log database");

    for (label, haystack) in [
        ("the human output", human.as_bytes()),
        ("the json output", machine.as_bytes()),
        ("the decision log listing", logged.as_bytes()),
        ("the decision log database", database.as_slice()),
    ] {
        for sentinel in [TOKEN, ACCOUNT_ID] {
            assert!(
                !haystack
                    .windows(sentinel.len())
                    .any(|window| window == sentinel.as_bytes()),
                "{label} carries credential material read from auth.json: {sentinel}"
            );
        }
    }
}

/// The columns are readable back through the tool, not only through `sqlite3`.
///
/// This is load bearing for the cache design rather than cosmetic. The environment cache has no TTL
/// deliberately, and the justification for that is that a stale environment id corrects itself when
/// `codex cloud exec` fails on it and the failure lands in that row's `outcome`, "visible in
/// `agent-router log`". If `log` cannot show which rows were cloud rows or where their tasks went,
/// that recovery path does not exist and the justification is false.
///
/// The key must be present on every row, cloud and local alike, so a consumer never has to read an
/// absent key as a value.
#[test]
fn log_json_carries_the_execution_target_and_the_cloud_task_url() {
    const TASK_URL: &str = "https://chatgpt.com/codex/tasks/task-123";
    let fixture = CliFixture::new("log-json-target");
    fixture.seed_row(
        "a task run right here",
        &local_decision(),
        "thread-abc",
        None,
    );
    fixture.seed_row(
        "a task submitted upstream",
        &cloud_decision(),
        "task-123",
        Some(TASK_URL),
    );

    let stdout = stdout_of(
        &fixture
            .router()
            .arg("log")
            .arg("--limit")
            .arg("10")
            .arg("--json")
            .output()
            .expect("read the decision log"),
    );
    let rows: Value = serde_json::from_str(&stdout).expect("log json");

    let cloud = rows[0].as_object().expect("the newest row is an object");
    assert_eq!(cloud["task"], "a task submitted upstream", "newest first");
    assert_eq!(cloud["execution_target"], "cloud");
    assert_eq!(cloud["cloud_task_url"], TASK_URL);
    assert_eq!(
        cloud["job_id"], "task-123",
        "the cloud task id is the job identity, so `log` already shows it"
    );

    let local = rows[1].as_object().expect("the older row is an object");
    assert!(
        local.contains_key("execution_target") && local.contains_key("cloud_task_url"),
        "a local row must carry both keys, or null is unreadable from absent: {local:?}"
    );
    assert_eq!(local["execution_target"], "local");
    assert_eq!(
        local["cloud_task_url"],
        Value::Null,
        "a local dispatch submitted no cloud task, so there is no url rather than an unread one"
    );
}

/// The human listing is the surface an operator actually looks at when a cloud dispatch failed, so
/// the target and the task url reach it too. A value emitted only under `--json` would be a column
/// written and never read back, which is exactly what the plan objected to when it deferred the
/// stats column.
///
/// Only the presence of the tag and the url is pinned, not the token layout: the exact format is
/// the implementer's to choose, and asserting one here would freeze a line nobody has designed yet.
#[test]
fn the_log_line_shows_the_execution_target_and_the_cloud_task_url() {
    const TASK_URL: &str = "https://chatgpt.com/codex/tasks/task-123";
    let fixture = CliFixture::new("log-line-target");
    let (local_task, cloud_task) = ("a task run right here", "a task submitted upstream");
    for task in [local_task, cloud_task] {
        assert!(
            !task.contains("cloud") && !task.contains("local"),
            "the seeded task text must not supply the words this test searches for: {task}"
        );
    }
    fixture.seed_row(local_task, &local_decision(), "thread-abc", None);
    fixture.seed_row(cloud_task, &cloud_decision(), "task-123", Some(TASK_URL));

    let stdout = stdout_of(
        &fixture
            .router()
            .arg("log")
            .arg("--limit")
            .arg("10")
            .output()
            .expect("read the decision log"),
    );

    assert!(
        stdout.contains(TASK_URL),
        "the listing must show where the cloud task went: {stdout}"
    );
    assert!(
        stdout.contains("cloud"),
        "the listing must name the cloud row's target: {stdout}"
    );
    assert!(
        stdout.contains("local"),
        "the listing must name the local row's target too, or absence has to be interpreted: \
         {stdout}"
    );
}
