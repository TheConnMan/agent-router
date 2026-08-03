//! Whether a repository may run its tasks as Codex cloud tasks, and the reason when it may not.
//!
//! The reason is the whole point of the type. A bool would answer "not eligible" identically for a
//! repo the operator never allowlisted and a repo whose Codex login has expired, so deleting the
//! allowlist guard would still produce "not eligible" and every test over it would stay green. Each
//! case below therefore asserts a NAMED reason, and
//! `a_repo_outside_the_allowlist_is_ineligible_for_that_reason` is the one that fails, with
//! `left: Ineligible(NoCodexAuth)`, when the guard is deleted.
//!
//! Nothing here contacts the REAL environments endpoint, and nothing here holds a real credential.
//! Most paths short-circuit before the endpoint, hit a seeded cache, or stop at the credential
//! read, so they are offline by construction rather than by a skip that would pass while exercising
//! nothing. The `parse_environment_id` cases at the end of this file read RECORDED bodies instead:
//! the endpoint was measured live on 2026-08-03 and its answers were transcribed here with their
//! values scrubbed, so the parser is pinned to a shape somebody observed rather than to one this
//! suite invented. What no offline test can prove is that the endpoint still answers that way
//! today.
//!
//! The three cases in "the environments endpoint" section below go further and execute steps 7
//! through 9 for real, against a `TcpListener` bound on loopback in this process and a sentinel
//! `auth.json`. They exist because the cache write and the negative-result path are otherwise
//! unreachable: the only offline route to `Eligible` used to be a seeded cache, which returns
//! before the write is attempted, so "an unwritable cache is a silent no-op" and "a negative result
//! is not cached" were both asserted at a point the code never got to.
//!
//! Resolution is reached through `eligibility_in`, which takes the codex home, the cache path, and
//! the environments base URL as arguments, rather than through `eligibility`, which reads all three
//! from the environment. These tests run in parallel threads inside one process, so an
//! `env::set_var` here would race with every concurrent `getenv` in the binary, which is exactly
//! what Rust 2024 made `set_var` unsafe over. There is no write to process environment anywhere in
//! this file and no `unsafe` block: the stub's URL is an argument like the other two.
//!
//! Every case passes the stub's URL, including the ones that stop at the allowlist or the credential
//! read and never open a socket. One shared stub serves them all, routing on the request path, which
//! is the thing production varies anyway. Passing it unconditionally is the cheap insurance: a case
//! that unexpectedly travels as far as step 8 reaches loopback and is caught by that path's request
//! count, rather than reaching the real endpoint.
//!
//! The cache is a SQLite table and the cases seed it through `rusqlite` with production's own
//! schema. Nothing here reaches for a test-only seeding function on the crate: the whole value of a
//! seeded cache is that it is the state production reads, and an accessor built for these tests
//! would be a second way in that could keep working while the real one broke.
#![cfg(unix)]

use agent_router_core::cloud::{
    CloudEligibility, CloudIneligible, eligibility_in, parse_environment_id, parse_github_remote,
};
use agent_router_core::config::Config;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// The endpoint stub and the fixture repository builder, shared with `cloud_target_cli.rs` in the
// other crate, which reaches this same file by `#[path]` include.
mod common;

use common::{EnvironmentsStub, git};

/// The allowlisted fixture's own slug, in the case the remote states it.
const OWNER_REPO: &str = "fixture-owner/fixture-repo";
/// The branch every fixture repository is created on, so a resolved target's branch is assertable.
const BRANCH: &str = "task/fixture";
/// The remote-tracking ref of that branch, which is what a cloud task would really run.
const REMOTE_REF: &str = "refs/remotes/origin/task/fixture";

/// The cache table, restated here rather than exported from the crate.
///
/// A test-only accessor on the production side would be a second way in to the storage, and the one
/// thing that must stay true is that these tests seed the cache production reads. The restatement is
/// self-policing: a schema this drifts from makes every insert below fail loudly on the next run
/// rather than quietly seeding a table nothing reads.
const CACHE_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS cloud_environments (
    slug           TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL
);
";

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// One throwaway repository with a GitHub `origin`, a branch, a commit, and a remote-tracking ref
/// pointing at that commit, plus a codex home that holds NO `auth.json` and a cache path that does
/// not exist yet.
///
/// The missing `auth.json` is load bearing in most cases below rather than incidental: it is what
/// makes `NoCodexAuth` the reason resolution reaches once every earlier guard has passed, so a
/// deleted guard is visible as a changed reason rather than as the same "not eligible".
///
/// The remote-tracking ref is the fixture's healthy shape, and it is built with `update-ref` rather
/// than a push because nothing here may touch a network. `refs/remotes/origin/<branch>` is exactly
/// what a fetch would have left behind, and it is the only thing resolution reads, so writing it
/// directly reproduces the state without inventing one. The three branch-state cases below each
/// break one property of this shape, and every other case depends on it holding, or they would all
/// stop at a branch-state reason instead of at the guard they are about.
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// A repository whose `origin` is `remote`. The remote is a parameter because which owner and
    /// repo the URL names is exactly what the allowlist cases vary.
    fn with_remote(remote: &str) -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).expect("create the repository directory");
        fs::create_dir_all(root.path().join("codex-home")).expect("create the codex home");
        git(&repo, &["init", "-b", BRANCH]);
        git(&repo, &["remote", "add", "origin", remote]);
        fs::write(repo.join("README"), "fixture\n").expect("write a file to commit");
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--message", "fixture"]);
        git(&repo, &["update-ref", REMOTE_REF, "HEAD"]);
        Fixture { root }
    }

    /// The allowlisted shape: a GitHub remote naming `OWNER_REPO`.
    fn new() -> Fixture {
        Fixture::with_remote("git@github.com:fixture-owner/fixture-repo.git")
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    /// A codex home which starts with no `auth.json`. Only the endpoint cases write one, and what
    /// they write is a sentinel: a test carrying a real credential would be a test that could
    /// contact the real endpoint, and every case here is aimed at loopback.
    fn codex_home(&self) -> PathBuf {
        self.root.path().join("codex-home")
    }

    /// A credential file in the shape `resolve_environment` reads, carrying sentinel values.
    ///
    /// Writing this is what lets a case travel past step 7. It is also what makes the received
    /// request assertable: the stub endpoint records the headers it was sent, so a case can prove
    /// the credential path ran rather than assuming it did.
    fn write_codex_auth(&self) {
        fs::write(
            self.codex_home().join("auth.json"),
            json!({"tokens": {"access_token": SENTINEL_TOKEN, "account_id": SENTINEL_ACCOUNT_ID}})
                .to_string(),
        )
        .expect("write the sentinel credential");
    }

    fn cache_path(&self) -> PathBuf {
        self.root.path().join("state/cloud-environments.db")
    }

    /// The cache database, created with production's own schema, so a seeded row is the row
    /// production reads rather than one shaped to be readable.
    fn open_cache(&self) -> Connection {
        let path = self.cache_path();
        fs::create_dir_all(path.parent().expect("the cache has a parent"))
            .expect("create the cache directory");
        let conn = Connection::open(&path).expect("open the cache database");
        conn.execute_batch(CACHE_SCHEMA)
            .expect("create the cache table");
        conn
    }

    fn seed_cache(&self, slug: &str, environment_id: &str) {
        self.open_cache()
            .execute(
                "INSERT INTO cloud_environments (slug, environment_id) VALUES (?1, ?2)",
                rusqlite::params![slug, environment_id],
            )
            .expect("seed the cache");
    }

    /// Every row in the cache, so a case can assert on the whole table rather than on one lookup and
    /// therefore catch a write that also left something else behind.
    fn cache_rows(&self) -> BTreeMap<String, String> {
        let conn = Connection::open(self.cache_path()).expect("open the cache database");
        let mut statement = conn
            .prepare("SELECT slug, environment_id FROM cloud_environments")
            .expect("prepare the cache read");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("read the cache")
            .collect::<rusqlite::Result<BTreeMap<String, String>>>()
            .expect("collect the cache rows")
    }

    fn resolve(&self, config: &Config) -> CloudEligibility {
        eligibility_in(
            &self.repo(),
            config,
            &self.codex_home(),
            &self.cache_path(),
            environments().base_url(),
        )
    }
}

/// A config whose allowlist is exactly `repos` and which is otherwise the shipped default, so a
/// case varies one input and nothing else.
fn config(repos: &[&str]) -> Config {
    Config {
        cloud_repos: repos.iter().map(|repo| (*repo).to_string()).collect(),
        ..Config::default()
    }
}

// ------------------------------------------------------------------ the allowlist

/// An operator who has configured no cloud repositories pays nothing for the feature: resolution
/// stops at the empty allowlist before any subprocess or socket, and cloud is never inferred from a
/// repository merely happening to be connected upstream.
///
/// The fixture's own premise is asserted first. Without it this passes just as well against a
/// broken fixture whose remote does not parse, which would be a test proving nothing about the
/// allowlist at all.
#[test]
fn an_empty_allowlist_short_circuits_before_any_git_call() {
    let fixture = Fixture::new();
    let remote = git_stdout(&fixture.repo(), &["remote", "get-url", "origin"]);
    assert_eq!(
        parse_github_remote(&remote),
        Some(("fixture-owner".to_string(), "fixture-repo".to_string())),
        "the fixture must have a resolvable GitHub remote, or this proves nothing about the \
         allowlist"
    );

    assert_eq!(
        fixture.resolve(&config(&[])),
        CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted)
    );
}

/// AC 5, the named mutation check. Delete the allowlist membership guard from
/// `cloud::eligibility_in` and control falls through to the credential read, which this fixture's
/// codex home cannot satisfy, so the result becomes `Ineligible(NoCodexAuth)` and this assertion
/// fails with `left: Ineligible(NoCodexAuth)`, `right: Ineligible(NotAllowlisted)`.
///
/// That only works because the reason is an enum. Against a bool-shaped result both the guarded and
/// the unguarded implementation answer "not eligible" and the mutation ships green, which is the
/// concrete reason the type carries a reason at all. Do not weaken this to `matches!` or to
/// `is_ineligible()`: the whole value of the test is in the discrimination.
#[test]
fn a_repo_outside_the_allowlist_is_ineligible_for_that_reason() {
    let fixture = Fixture::with_remote("git@github.com:someone/not-allowlisted.git");

    assert_eq!(
        fixture.resolve(&config(&["other/repo"])),
        CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted)
    );
}

/// The allowlist is a list of `owner/repo` slugs a human typed, and GitHub treats those
/// case-insensitively, so a config entry that differs only in case is the same repository. The
/// assertion is the negative one on purpose: what matters is that the compare did NOT reject it,
/// and the reason it does reach is `NoCodexAuth`, the next guard along.
#[test]
fn the_allowlist_compare_is_case_insensitive() {
    let fixture = Fixture::new();

    assert_eq!(
        fixture.resolve(&config(&["Fixture-Owner/Fixture-Repo"])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
        "a slug differing only in case names the same repository, so the allowlist must admit it"
    );
}

/// An allowlisted repository still has to resolve an environment, and this fixture's codex home
/// holds no credential to go asking with. The reason names the credential rather than the
/// environment, because the two are genuinely different states and an operator who has to re-login
/// should not be told their repository is unconnected.
#[test]
fn an_allowlisted_repo_with_no_resolvable_environment_is_ineligible() {
    let fixture = Fixture::new();

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth)
    );
}

// ------------------------------------------------------------------ the git reads

/// `--dir` pointing at something that is not a repository at all. A cloud task is submitted against
/// a branch of a GitHub repository, so there is nothing to submit here, and this must read as its
/// own reason rather than as an unallowlisted repository.
#[test]
fn a_non_git_directory_is_ineligible() {
    let root = tempfile::tempdir().expect("tempdir");
    let plain = root.path().join("not a repository");
    fs::create_dir_all(&plain).expect("create the plain directory");
    fs::create_dir_all(root.path().join("codex-home")).expect("create the codex home");

    assert_eq!(
        eligibility_in(
            &plain,
            &config(&[OWNER_REPO]),
            &root.path().join("codex-home"),
            &root.path().join("state/cloud-environments.db"),
            environments().base_url(),
        ),
        CloudEligibility::Ineligible(CloudIneligible::NotAGitRepo)
    );
}

/// Both remote spellings name the same repository, and neither is more canonical than the other:
/// this box's own clones carry the SSH form and CI checkouts carry the HTTPS one, so a parser that
/// handled one would make eligibility depend on how the clone happened to be made.
///
/// The rejections are half the test. A GitLab remote and a bare path are not GitHub repositories,
/// and reading an owner and repo out of them would submit a task naming a repository that does not
/// exist on GitHub.
#[test]
fn both_remote_forms_resolve_to_the_same_owner_and_repo() {
    let expected = Some(("owner".to_string(), "repo".to_string()));
    for url in [
        "git@github.com:owner/repo.git",
        "https://github.com/owner/repo.git",
        "https://github.com/owner/repo",
        "ssh://git@github.com/owner/repo.git",
    ] {
        assert_eq!(parse_github_remote(url), expected, "{url}");
    }

    for url in ["https://gitlab.com/owner/repo.git", "/local/path.git"] {
        assert_eq!(
            parse_github_remote(url),
            None,
            "{url} is not a GitHub repository and must not be read as one"
        );
    }
}

/// `--dir` is routinely a subdirectory of the repository, because that is where the work is. Git
/// walks up to the root on its own for both reads, so this must resolve identically to the root,
/// and the assertion is against the root's own answer rather than against a literal so the case
/// cannot drift away from what it is comparing to.
#[test]
fn a_subdirectory_resolves_the_same_repo_as_its_root() {
    let fixture = Fixture::new();
    let nested = fixture.repo().join("nested/deeper");
    fs::create_dir_all(&nested).expect("create the nested directory");
    let config = config(&[OWNER_REPO]);

    let from_root = fixture.resolve(&config);
    let from_nested = eligibility_in(
        &nested,
        &config,
        &fixture.codex_home(),
        &fixture.cache_path(),
        environments().base_url(),
    );

    assert_eq!(from_nested, from_root);
    assert_eq!(
        from_nested,
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
        "both must have travelled past the git reads, or this compares two early exits"
    );
}

/// A detached HEAD has no branch name to submit against. `rev-parse --abbrev-ref HEAD` answers the
/// literal string `HEAD` there rather than failing, so an implementation that took the output at
/// face value would submit a cloud task against a branch called `HEAD`.
#[test]
fn a_detached_head_is_ineligible() {
    let fixture = Fixture::new();
    let sha = git_stdout(&fixture.repo(), &["rev-parse", "HEAD"]);
    git(&fixture.repo(), &["checkout", "--detach", &sha]);

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoBranch)
    );
}

// ------------------------------------------------------------------ the branch state

// The reason this whole section exists: `codex cloud exec --branch <branch>` runs the REMOTE ref,
// not the working tree the operator is looking at, and every one of the three cases below SUBMITS
// SUCCESSFULLY against an implementation that only resolves the branch name. There is no error to
// notice afterwards. The task simply runs different code, and the row it writes says it ran.
//
// Each case starts by resolving the untouched fixture and asserting it reaches `NoCodexAuth`, the
// guard past all of these. Without that control a case would pass just as well against a fixture
// that was already ineligible for some earlier reason, which is the failure mode that makes a
// negative assertion cheap and worthless.

/// Uncommitted edits to a tracked file exist nowhere but this box. A cloud task would run the
/// committed source, silently skipping exactly the work the task was almost certainly about.
#[test]
fn a_dirty_working_tree_is_ineligible() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
        "the untouched fixture must travel past the branch-state checks, or this proves nothing"
    );

    fs::write(fixture.repo().join("README"), "edited but not committed\n")
        .expect("modify a tracked file");

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::WorkingTreeDirty)
    );
}

/// An untracked file is NOT dirty, which is the boundary the dirty check is drawn at. A build
/// directory or a scratch file would otherwise make every repository on a working box permanently
/// ineligible, and an untracked file was never going to reach the remote under any workflow.
///
/// This is the case that fails if `--untracked-files=no` is ever dropped from the status read.
#[test]
fn an_untracked_file_does_not_make_the_tree_dirty() {
    let fixture = Fixture::new();
    fs::write(fixture.repo().join("scratch.txt"), "not tracked\n")
        .expect("write an untracked file");

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
        "an untracked file must not read as a dirty tree"
    );
}

/// A branch the remote has never seen has no ref for the cloud task to run at all. The operator's
/// work would not run in some altered form; it would not run.
#[test]
fn a_branch_with_no_remote_counterpart_is_ineligible() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
        "the untouched fixture must travel past the branch-state checks, or this proves nothing"
    );

    git(&fixture.repo(), &["update-ref", "-d", REMOTE_REF]);

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::BranchNotOnRemote)
    );
}

/// Commits that exist locally and not on the remote ref are the ordinary state of a branch somebody
/// is working on and has not pushed yet, and it is the case an operator is least likely to suspect:
/// the tree is clean, the branch exists on the remote, and the cloud task still runs source that is
/// missing the last however many commits.
///
/// The commit here also restores a clean tree, so what this case is left varying is only the
/// distance between HEAD and the remote ref.
#[test]
fn a_branch_ahead_of_its_remote_is_ineligible() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
        "the untouched fixture must travel past the branch-state checks, or this proves nothing"
    );

    fs::write(fixture.repo().join("later"), "committed but not pushed\n")
        .expect("write a file to commit");
    git(&fixture.repo(), &["add", "--all"]);
    git(&fixture.repo(), &["commit", "--message", "unpushed"]);

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::BranchAheadOfRemote)
    );
}

// ------------------------------------------------------------------ the environment cache

/// The cache is a resolution STEP that runs before the credential read, not a wrapper around the
/// HTTP call, and this is the test that pins the difference.
///
/// The fixture's codex home contains no `auth.json` at all. An implementation that opens
/// credentials before consulting the cache answers `Ineligible(NoCodexAuth)` here and fails. That
/// ordering is what lets the CLI dry-run test reach a cloud decision through pure filesystem state,
/// with no credential, no endpoint, and no test-only environment variable.
#[test]
fn a_cache_hit_resolves_without_reading_codex_auth() {
    let fixture = Fixture::new();
    fixture.seed_cache(OWNER_REPO, "env-seeded");
    assert!(
        !fixture.codex_home().join("auth.json").exists(),
        "the missing credential is the proof this test rests on"
    );

    let resolved = fixture.resolve(&config(&[OWNER_REPO]));

    let CloudEligibility::Eligible(target) = resolved else {
        panic!("a seeded cache entry must resolve without a credential, got {resolved:?}");
    };
    assert_eq!(target.environment_id(), "env-seeded");
    assert_eq!(
        target.branch(),
        BRANCH,
        "the branch comes from the repository, never from the cache"
    );
}

/// The cache can redirect an already-authorized repository; it can never authorize one. The
/// allowlist gate runs before the cache is consulted, so an entry for a repository the operator has
/// not allowlisted is never read and never acted on.
///
/// This is the assertion the trust-boundary argument rests on: whoever can write the cache file can
/// choose where an allowlisted repository's task runs, and cannot make cloud happen at all for a
/// repository the operator did not name.
#[test]
fn a_cache_hit_still_requires_the_allowlist() {
    let fixture = Fixture::new();
    fixture.seed_cache(OWNER_REPO, "env-seeded");

    assert_eq!(
        fixture.resolve(&config(&["other/repo"])),
        CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted)
    );
}

/// A cache the router cannot read is a MISS, never an error. The database is derived state the
/// router wrote for itself, so a corrupted one costs a round trip and nothing else; failing
/// resolution over it would let a damaged cache block routing entirely.
///
/// Four shapes, because they fail at four different layers: a file that is not a database at all, a
/// database with no such table, a row whose value is not text, and a row whose value is text this
/// crate will not hand onwards.
///
/// The third is not a hypothetical dressed up as a case. SQLite columns are dynamically typed, so
/// the `TEXT` on the schema line constrains nothing, and a read that unwrapped the conversion would
/// panic on it while handling the others perfectly. It is seeded as a BLOB specifically: TEXT
/// affinity silently converts an inserted integer to its decimal string, so an integer 7 comes back
/// out as a perfectly usable `"7"` and tests nothing. A blob is not converted.
///
/// The fourth is the one that matters most. The cache is the SECOND way an id reaches a URL path, an
/// argv slot, the decision log, and a raw terminal write, so validating only the endpoint's answer
/// would leave a row on disk able to supply exactly what the endpoint itself was refused.
#[test]
fn a_malformed_cache_reads_as_a_miss() {
    let not_a_database = Fixture::new();
    fs::create_dir_all(
        not_a_database
            .cache_path()
            .parent()
            .expect("the cache has a parent"),
    )
    .expect("create the cache directory");
    fs::write(not_a_database.cache_path(), "not a database at all")
        .expect("seed a file that is not a database");

    let no_such_table = Fixture::new();
    fs::create_dir_all(
        no_such_table
            .cache_path()
            .parent()
            .expect("the cache has a parent"),
    )
    .expect("create the cache directory");
    Connection::open(no_such_table.cache_path())
        .expect("open the cache database")
        .execute_batch("CREATE TABLE something_else (id INTEGER);")
        .expect("seed a database carrying no cache table");

    let wrong_value_type = Fixture::new();
    wrong_value_type
        .open_cache()
        .execute(
            "INSERT INTO cloud_environments (slug, environment_id) VALUES (?1, X'00ff10')",
            rusqlite::params![OWNER_REPO],
        )
        .expect("seed a row whose value is not text");

    let unusable_id = Fixture::new();
    unusable_id.seed_cache(OWNER_REPO, "../../etc/passwd\u{1b}[2J");

    for (label, fixture) in [
        ("a file that is not a database", &not_a_database),
        ("a database carrying no cache table", &no_such_table),
        ("a row whose value is not text", &wrong_value_type),
        ("a row whose id this crate refuses", &unusable_id),
    ] {
        assert_eq!(
            fixture.resolve(&config(&[OWNER_REPO])),
            CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth),
            "resolution must continue past a cache it cannot read: {label}"
        );
    }
}

/// A cold cache is the ordinary first-run state, so an absent file is a miss like any other and
/// resolution carries on to the credential read.
#[test]
fn an_absent_cache_reads_as_a_miss() {
    let fixture = Fixture::new();
    assert!(
        !fixture.cache_path().exists(),
        "the fixture must start with no cache file"
    );

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth)
    );
}

/// Only a successful resolution is written. A repository that is allowlisted but has no connected
/// environment yet must re-resolve on the next run, or connecting the environment upstream would
/// have no effect and the operator would be stuck with no way to clear it short of deleting a file
/// they do not know exists.
///
/// This fails against an implementation that memoizes failures, which is the obvious optimization
/// to reach for once the per-run cost of a negative result is noticed.
#[test]
fn a_negative_result_is_not_written_to_the_cache() {
    let fixture = Fixture::new();
    let cache = fixture.cache_path();
    fs::create_dir_all(cache.parent().expect("the cache has a parent"))
        .expect("a writable directory for the cache, so nothing else can explain an absent file");

    assert_eq!(
        fixture.resolve(&config(&[OWNER_REPO])),
        CloudEligibility::Ineligible(CloudIneligible::NoCodexAuth)
    );
    assert!(
        !cache.exists(),
        "a failed resolution was written to the cache, so connecting an environment upstream \
         would never take effect"
    );
}

// ------------------------------------------------------------------ the environments endpoint

/// The sentinel credential every endpoint case sends. Never a real token: the stub is on loopback
/// and the assertions are about what the request CARRIED, so a real value would buy nothing and
/// risk everything.
const SENTINEL_TOKEN: &str = "sk-sentinel-access-token";
const SENTINEL_ACCOUNT_ID: &str = "sentinel-account-id";

/// The id inside `CONNECTED_BODY`, named so a case can assert on it without restating the literal.
const CONNECTED_ID: &str = "69a1b2c3d4e5f60718293a4b5c6d7e8f";

/// What a repository nobody connected answers with. HTTP 200 and an empty array, measured on
/// 2026-08-03: it is not a 404, so the empty array is the only signal.
const UNCONNECTED_BODY: &str = "[]";

/// The one stub endpoint this binary runs, started exactly once at whichever case gets there first.
///
/// One shared stub rather than one per case, because the accept loop is a detached thread that never
/// stops: one per case would leave a listener and a thread behind for every test in the file, to
/// serve requests that are already distinguished by the request PATH, which is the thing production
/// varies anyway. The `OnceLock` is what makes it one; it is not a soundness device, and nothing
/// here writes process environment.
static ENVIRONMENTS: OnceLock<EnvironmentsStub> = OnceLock::new();

fn environments() -> &'static EnvironmentsStub {
    ENVIRONMENTS.get_or_init(|| EnvironmentsStub::routing(environments_body_for))
}

/// What the stub answers for a given request target.
///
/// Production spells the target `/{owner}/{repo}` out of the parsed remote, in the case the remote
/// stated it, so `Fixture-Owner/Connected-Repo` arrives capitalised. That is deliberate: the cache
/// key is lowercased and the request path is not, so registering a mixed-case path here is what
/// makes the lowercasing assertable rather than a coincidence of an all-lowercase fixture.
///
/// An unregistered path answers `[]` rather than failing, which is why every case asserts on the
/// number of requests it received: a path registered with a typo would otherwise read as a
/// repository nobody connected, and the case would pass for the wrong reason.
fn environments_body_for(target: &str) -> &'static str {
    match target {
        "/Fixture-Owner/Connected-Repo"
        | "/fixture-owner/unwritable-cache-repo"
        | "/fixture-owner/upsert-repo" => CONNECTED_BODY,
        _ => UNCONNECTED_BODY,
    }
}

/// Step 9, which nothing could reach before. A resolution that really went to the endpoint writes
/// the environment id under the LOWERCASED slug, and that lowercasing is the whole reason the cache
/// is useful: GitHub treats owner and repo names case-insensitively, so a key carrying whatever
/// case the remote happened to state would miss on the next run against a remote spelled
/// differently and re-resolve forever.
///
/// The fixture's remote is deliberately mixed case, so the request path and the cache key differ.
/// An implementation that keyed the cache on the slug as written fails here on the key, and one
/// that skipped the write entirely fails on the file not existing.
///
/// The request count is the non-vacuity control. Without it a case that never reached step 8 could
/// still satisfy the rest through some cached or short-circuited route.
#[test]
fn a_successful_resolution_writes_the_lowercased_slug_and_the_resolved_id_to_the_cache() {
    let stub = environments();
    let fixture = Fixture::with_remote("git@github.com:Fixture-Owner/Connected-Repo.git");
    fixture.write_codex_auth();
    assert!(
        !fixture.cache_path().exists(),
        "the cache must start cold, or a hit at step 6 would return before the write path"
    );

    let resolved = fixture.resolve(&config(&["Fixture-Owner/Connected-Repo"]));

    let CloudEligibility::Eligible(target) = resolved else {
        panic!("a connected repository must resolve through the endpoint, got {resolved:?}");
    };
    assert_eq!(
        target.environment_id(),
        CONNECTED_ID,
        "the id must be the one the endpoint answered with"
    );
    assert_eq!(
        target.branch(),
        BRANCH,
        "the branch comes from the repository, never from the response"
    );

    let requests = stub.requests_for("/Fixture-Owner/Connected-Repo");
    assert_eq!(
        requests.len(),
        1,
        "the resolution must have reached the endpoint, or it resolved some other way and this \
         case proves nothing about the write path: {requests:?}"
    );
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {SENTINEL_TOKEN}").as_str()),
        "the request must carry the credential read from auth.json: {:?}",
        requests[0]
    );

    assert!(
        fixture.cache_path().exists(),
        "a successful resolution must have written the cache"
    );
    assert_eq!(
        fixture.cache_rows(),
        BTreeMap::from([(
            "fixture-owner/connected-repo".to_string(),
            CONNECTED_ID.to_string()
        )]),
        "the cache must key the resolved id on the lowercased slug and hold nothing else"
    );

    // The cache is the router's own state and nobody else's business, and this is the path
    // production really creates the file on. SQLite would have created it at 0666 masked by the
    // umask, so a default 022 lands 0644 and fails here.
    let mode = fs::metadata(fixture.cache_path())
        .expect("cache metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the cache database must be owner-only");
}

/// A write for one repository leaves every other repository's entry alone, asserted through the real
/// resolution path rather than against the storage helper.
///
/// This is the case the storage change is for. The cache was a JSON map that every write read,
/// merged one key into, and wrote back whole, so two routers resolving different repositories at the
/// same time each merged into the map they had read and whichever finished second silently dropped
/// the other's entry. This box runs several agents concurrently by design, so that interleaving was
/// the expected condition rather than a corner case, and the atomic rename that made the FILE safe
/// did nothing about it.
///
/// The seeded row is what a concurrent router would have left behind. An implementation that wrote
/// the whole table, or that recreated the database, fails here on the missing row rather than on the
/// row it was asked to write.
#[test]
fn a_resolution_preserves_another_repositorys_cache_entry() {
    let stub = environments();
    let fixture = Fixture::with_remote("git@github.com:fixture-owner/upsert-repo.git");
    fixture.write_codex_auth();
    fixture.seed_cache("other-owner/other-repo", "other-environment");

    let resolved = fixture.resolve(&config(&["fixture-owner/upsert-repo"]));

    let requests = stub.requests_for("/fixture-owner/upsert-repo");
    assert_eq!(
        requests.len(),
        1,
        "the resolution must have reached the endpoint, or the write path was never attempted and \
         this case proves nothing about a write: {requests:?}"
    );
    let CloudEligibility::Eligible(target) = resolved else {
        panic!("a connected repository must resolve through the endpoint, got {resolved:?}");
    };
    assert_eq!(target.environment_id(), CONNECTED_ID);

    assert_eq!(
        fixture.cache_rows(),
        BTreeMap::from([
            (
                "other-owner/other-repo".to_string(),
                "other-environment".to_string()
            ),
            (
                "fixture-owner/upsert-repo".to_string(),
                CONNECTED_ID.to_string()
            ),
        ]),
        "a write for one repository must add its own row and evict nothing"
    );
}

/// A cache the router cannot write is a silent no-op, not a failure. The resolution genuinely
/// succeeded, so failing the whole route because a derived-state file could not be updated would be
/// the tail wagging the dog.
///
/// This is the case the version it replaced could not make. That one stopped at `NoCodexAuth` at
/// step 7, so the write at step 9 was never attempted and the 0500 directory was never written to:
/// it was `an_allowlisted_repo_with_no_resolvable_environment_is_ineligible` with a chmod, and
/// making `write_owner_only` panic on error would not have failed it. With the endpoint reachable
/// the resolution genuinely succeeds, so the write genuinely runs and genuinely fails, and the
/// assertion is now the strong one: `Eligible`, not merely "did not panic".
///
/// The unwritable path is NESTED inside the locked directory rather than directly in it, because
/// `write_owner_only` chmods an existing parent to 0700 and this process owns the locked directory,
/// so a cache file directly inside it would be written after all. A parent that cannot be CREATED
/// is the state a real box presents when the state directory's own parent is not writable.
#[test]
fn an_unwritable_cache_does_not_fail_resolution() {
    let stub = environments();
    let fixture = Fixture::with_remote("git@github.com:fixture-owner/unwritable-cache-repo.git");
    fixture.write_codex_auth();
    let locked = fixture.root.path().join("locked");
    fs::create_dir_all(&locked).expect("create the directory to lock");
    let mut permissions = fs::metadata(&locked).expect("metadata").permissions();
    permissions.set_mode(0o500);
    fs::set_permissions(&locked, permissions).expect("make the directory unwritable");

    // Root ignores the mode bits, so the premise this case rests on is established by exercising
    // it rather than assumed. The skip is printed and returns; it never passes silently.
    if fs::create_dir(locked.join("probe")).is_ok() {
        println!(
            "skipping an_unwritable_cache_does_not_fail_resolution: this process can write a 0500 \
             directory, so there is no unwritable cache path to test with"
        );
        return;
    }

    let resolved = eligibility_in(
        &fixture.repo(),
        &config(&["fixture-owner/unwritable-cache-repo"]),
        &fixture.codex_home(),
        &locked.join("state/cloud-environments.db"),
        stub.base_url(),
    );

    let requests = stub.requests_for("/fixture-owner/unwritable-cache-repo");
    assert_eq!(
        requests.len(),
        1,
        "the resolution must have reached the endpoint, or the write path was never attempted and \
         this case has regressed to asserting an early return: {requests:?}"
    );
    let CloudEligibility::Eligible(target) = resolved else {
        panic!(
            "a cache that cannot be written must not turn a successful resolution into an \
             ineligible one, got {resolved:?}"
        );
    };
    assert_eq!(target.environment_id(), CONNECTED_ID);
    assert!(
        !locked.join("state").exists(),
        "the write really must have failed, or this case proves nothing about a failed write"
    );
}

/// Negative results are never cached, asserted at the layer where the caching would happen rather
/// than one step short of it.
///
/// The sibling case `a_negative_result_is_not_written_to_the_cache` stops at `NoCodexAuth`, so it
/// pins the rule for a repository that never reached the endpoint at all. This one goes all the way
/// to the endpoint, gets the measured `[]` an unconnected repository answers with, and asserts the
/// same rule from the state the optimization would actually be reached from: it is the per-run cost
/// of an unconnected repository that tempts someone to memoize, and memoizing it would mean
/// connecting the environment upstream had no effect until the operator found and deleted a file
/// they do not know exists.
#[test]
fn an_unconnected_repository_reaches_the_endpoint_and_caches_nothing() {
    let stub = environments();
    let fixture = Fixture::with_remote("git@github.com:fixture-owner/unconnected-repo.git");
    fixture.write_codex_auth();
    let cache = fixture.cache_path();
    fs::create_dir_all(cache.parent().expect("the cache has a parent"))
        .expect("a writable directory for the cache, so nothing else can explain an absent file");

    let resolved = fixture.resolve(&config(&["fixture-owner/unconnected-repo"]));

    let requests = stub.requests_for("/fixture-owner/unconnected-repo");
    assert_eq!(
        requests.len(),
        1,
        "the resolution must have reached the endpoint, or this case is the credential-read one \
         again and says nothing about a negative RESULT: {requests:?}"
    );
    assert_eq!(
        requests[0].header("chatgpt-account-id"),
        Some(SENTINEL_ACCOUNT_ID),
        "the request must carry the account id read from auth.json: {:?}",
        requests[0]
    );
    assert_eq!(
        resolved,
        CloudEligibility::Ineligible(CloudIneligible::NoCloudEnvironment),
        "an empty array is the endpoint saying this repository is not connected"
    );
    assert!(
        !cache.exists(),
        "a negative result was written to the cache, so connecting an environment upstream would \
         never take effect"
    );
}

/// A connected repository's response body, transcribed from the live endpoint on 2026-08-03 with
/// every value scrubbed and the structure left intact.
///
/// The measurement: `GET .../wham/environments/by-repo/github/{owner}/{repo}` answered HTTP 200 with
/// a TOP-LEVEL ARRAY of length 1 for a connected repository, and HTTP 200 with exactly `[]` for one
/// that was not connected. An unconnected repository is not a 404. The id is a 32 character
/// lowercase hex string with no `env-` prefix and no prefix of any kind.
///
/// It is kept as a multi-key object rather than a one-key stub on purpose. A fixture of
/// `[{"id":"..."}]` would pass against a parser that reads the sole key of the sole element, or one
/// that folds the array away, and would go on passing while the real 28-key payload returned None.
const CONNECTED_BODY: &str = r#"[{"id":"69a1b2c3d4e5f60718293a4b5c6d7e8f","label":"example","repos":["github-1105248136"],"created_at":1773140014.26244,"is_pinned":false}]"#;

/// The connected shape, which is the only shape that yields an id.
///
/// This is the case that was wrong: the parser previously read `/id`, `/environment/id`, and
/// `/environments/0/id`, all three of which miss a top-level array, so every real response resolved
/// as `NoCloudEnvironment` and cloud never activated. Nothing in the suite noticed, because every
/// other case here reaches `Eligible` through a seeded cache.
#[test]
fn the_connected_shape_yields_the_first_elements_id() {
    assert_eq!(
        parse_environment_id(CONNECTED_BODY).as_deref(),
        Some("69a1b2c3d4e5f60718293a4b5c6d7e8f")
    );
}

/// An unconnected repository answers `[]` with HTTP 200, so the empty array is the ONLY signal that
/// there is no environment. Reading it as anything but None would hand a cloud dispatch an id the
/// endpoint never issued.
#[test]
fn the_unconnected_shape_yields_no_id() {
    assert_eq!(parse_environment_id("[]"), None);
}

/// Every body that is not the connected shape degrades to None, which `resolve_environment` turns
/// into `Ineligible(NoCloudEnvironment)` and a local route.
///
/// The cases are separate layers of one read, and a parser that unwrapped at any of them would panic
/// rather than degrade: not JSON at all, JSON that is not an array, an element that is not an object,
/// an element with no `id`, an `id` of the wrong type, and an `id` that is present but empty. The
/// empty string matters most, because it is the one that would otherwise pass a None check and reach
/// the server as a blank environment id.
#[test]
fn every_other_shape_degrades_to_none() {
    for body in [
        "not json at all",
        r#"{"id":"69a1b2c3d4e5f60718293a4b5c6d7e8f"}"#,
        r#"{"environments":[{"id":"69a1b2c3d4e5f60718293a4b5c6d7e8f"}]}"#,
        "[7]",
        r#"[{"label":"example","repos":["github-1105248136"]}]"#,
        r#"[{"id":7}]"#,
        r#"[{"id":null}]"#,
        r#"[{"id":""}]"#,
    ] {
        assert_eq!(
            parse_environment_id(body),
            None,
            "a body that carries no usable environment id must degrade to None: {body}"
        );
    }
}
