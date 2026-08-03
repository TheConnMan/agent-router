//! Whether a repository may run its tasks as Codex cloud tasks, resolved before the decision
//! engine runs and handed to it as a value.
//!
//! Resolution is impure by nature: it shells out to git, reads the codex credential file, and calls
//! an HTTP endpoint. Doing it here rather than inside `decide` is what keeps the engine pure and
//! replayable, so `pace_backtest` can re-decide a recorded row at the instant it was decided
//! without any of that happening again.
//!
//! **Every failure degrades to `Ineligible(<reason>)`.** Nothing here returns an `Err` and nothing
//! panics. A repository whose environment cannot be resolved routes local, which is the whole point:
//! a credential problem, a network problem, or a corrupted cache must never be the thing that blocks
//! a local dispatch.
//!
//! **The reason is not decoration.** A bool would answer "not eligible" identically for a repository
//! the operator never allowlisted and one whose Codex login expired, so deleting the allowlist guard
//! would leave every test over it green. The reason is what the rationale prints, and it is what
//! makes the allowlist guard's removal visible as a changed answer rather than the same one.
//!
//! **Credentials are locals, never values.** The access token and account id are read inside
//! `resolve_environment`, used once as header values, and dropped there. They are not fields on
//! `CloudTarget`, not in the cache, not in a `Debug` rendering, and not in any error: `ureq` errors
//! are discarded with `.ok()?` rather than formatted, and `CloudIneligible::NoCodexAuth` carries no
//! payload precisely so nothing read from `auth.json` can ride out on it. What IS recorded is the
//! environment id, which is a workspace identifier rather than a credential and is the one value
//! that makes a cloud decision auditable afterwards.
//!
//! **The cache is a resolution STEP, not a wrapper around the HTTP call.** It is read after the
//! allowlist guard and before the credential read, so the steady state for an allowlisted repository
//! is zero HTTP calls and zero credential reads per run. That ordering is load bearing in both
//! directions: a cache entry can redirect an already-authorized repository, and it can never
//! authorize one, because step 4 runs before step 6 on every single run.
//!
//! **The cache is a SQLite table, `cloud_environments` in its own database beside `router.db`.** One
//! row per repository, keyed on the lowercased slug, written with an upsert. It was a JSON map read,
//! merged, and renamed into place, which is the read-modify-write shape the repository's storage rule
//! names outright: two routers resolving different repositories at once each merged into the map they
//! had read and the second rename dropped the first one's entry. The rename made the FILE atomic and
//! left the MAP racy, which is the failure that looks fixed. An upsert is a single statement against a
//! single row, so a concurrent writer touching a different slug cannot evict it.
//!
//! **The cache is a database of its own and not a table inside `router.db`.** Sharing the file would
//! be the tidier layout and it is the wrong one here, because it would fuse two failure domains that
//! this module deliberately keeps apart. A cache write can happen on a run where `DecisionLog::open_at`
//! never ran, so this module has to be able to create and initialize the storage itself; sharing the
//! file would mean two modules each able to create `router.db`, each with its own schema fragment,
//! its own migration, and its own idea of who established the permissions. And the failure directions
//! are opposite: a damaged cache must degrade to a MISS and cost one HTTP call, while a damaged
//! decision log is a real error the operator is told about. One file cannot have both contracts, and
//! the one that would win is the log's, which would let a corrupt cache block routing.
//!
//! **The branch NAME is not what runs.** `codex cloud exec --branch <branch>` runs the REMOTE ref of
//! that branch, not the working tree the operator is looking at, so a branch that is dirty, ahead of
//! its remote, or absent from the remote entirely submits a task against different source than the
//! operator believes they submitted. Nothing downstream reports it: the submit succeeds and the task
//! runs the wrong code. Step 5 therefore resolves the branch NAME and then proves the remote would
//! run the same source, and each of the three ways it can fail is its own reason, because "route
//! local" without saying which one leaves the operator with no way to fix it. Every failure routes
//! local, which is the safe direction in a way the reverse is not: running locally against the
//! operator's actual working tree is always correct, and running remotely against different source
//! never is.
//!
//! What this does NOT catch:
//!
//! - The branch-state checks read LOCAL git state only, and deliberately make no network call: this
//!   is the hot path of every auto-routed run and a fetch would be both slow and a side effect on
//!   the operator's repository. So `refs/remotes/origin/<branch>` is what the last fetch left
//!   behind, not what the remote holds now. A remote branch that moved, was deleted, or was
//!   force-pushed since then reads as matching, and a stale remote-tracking ref reads as matching a
//!   remote that no longer looks like that. What is proven is that the operator's local view of the
//!   remote agrees with their working tree, which is the thing they can actually act on.
//! - A branch BEHIND its remote is eligible. The remote tip then contains every local commit plus
//!   somebody else's, so the operator's work is present rather than absent, which is a different and
//!   much weaker surprise than the three that are rejected.
//! - Untracked files do not count as dirty (`--untracked-files=no`). A build directory, a scratch
//!   file, or an ignored artifact would otherwise make every repository on this box permanently
//!   ineligible, and an untracked file was never going to reach the remote under any workflow.
//! - Submodules, the index's staged-but-unmodified state, and anything `git status --porcelain`
//!   summarises rather than lists are only as accurate as that summary.
//! - The environments endpoint is undocumented and can change shape without notice. Nothing here
//!   would report that it had: a shape change reads as `NoCloudEnvironment` and every cloud job
//!   silently routes local, which looks exactly like a repository nobody connected.
//! - A cache entry is never expired and never revalidated. There is no TTL. An environment deleted
//!   or re-created upstream leaves a stale id here and this module keeps handing it out; the
//!   correction signal is the `codex cloud exec` failure that id produces, which lands in that row's
//!   `outcome` column as `error: ...` and is visible in `agent-router log`.
//! - Negative results are never cached, deliberately. An allowlisted repository with no connected
//!   environment therefore pays one `auth.json` read and one HTTP call on every run. That is the
//!   correct trade: memoizing the failure would make connecting the environment upstream have no
//!   effect until the operator found and deleted a file they do not know exists.
//! - A repository can be connected to an environment that is itself misconfigured. Resolution proves
//!   the connection exists, not that it works.
//! - 401 and 404 are not distinguished, so an expired Codex login is indistinguishable from a
//!   repository that was never connected. Both read `NoCloudEnvironment`. Reporting a credential
//!   problem is `doctor`'s surface, and making it an error here would let an expired token block
//!   local dispatch.
//! - The remote is looked up by the literal name `origin`, never by the branch's upstream. A
//!   repository whose GitHub remote is named something else reads as ineligible, because guessing
//!   across several remotes is how the wrong repository gets submitted to the cloud.
//! - The environments base URL is overridable through `$AGENT_ROUTER_ENVIRONMENTS_URL`, and that is
//!   a redirection primitive rather than an oversight. Anyone who controls this process's
//!   environment can point the request carrying the Codex access token at a host of their choosing.
//!   It is accepted rather than defended: the same attacker already controls `PATH`, and this module
//!   executes `git` and (through `dispatch`) `codex` off it, so the override grants no capability
//!   they did not already hold. It exists for the caller that cannot pass an argument: the CLI's
//!   end-to-end test spawns the router as a child process and redirects it with `Command::env`,
//!   which is the only way to reach steps 7 through 9 through the real binary offline. An in-process
//!   caller passes the base URL to `eligibility_in` instead and never touches the variable. An
//!   attacker who holds the environment but somehow not `PATH` is not modelled here, and this
//!   override would be the way in.

use crate::config::Config;
use crate::runtime::{default_codex_home, home_dir};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The environments endpoint, minus the `owner/repo` it is asked about.
const ENVIRONMENTS_BY_REPO: &str =
    "https://chatgpt.com/backend-api/wham/environments/by-repo/github";

/// Ceiling on an environment id, and the character class one may be spelled with. See
/// `acceptable_environment_id` for why both are wider than the measured format.
const ENVIRONMENT_ID_MAX_LEN: usize = 128;

/// Ceiling on the environments call, matching the usage readers' own HTTP timeout. A resolution
/// that hangs would delay a dispatch that is going to happen anyway, just locally.
const ENVIRONMENTS_HTTP_TIMEOUT: Duration = Duration::from_secs(6);

/// Ceiling on each of the git reads below, for the reasons `git_output` records.
const GIT_TIMEOUT: Duration = Duration::from_secs(6);

/// The cache table. One row per repository, the slug as the primary key so the upsert has a
/// conflict target and two routers resolving different repositories touch different rows.
const CACHE_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS cloud_environments (
    slug           TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL
);
";

/// How long a cache connection waits on another writer before giving up. The same 500ms
/// `DecisionLog::open_at` uses, and the wait is bounded for the same reason the git reads are: a
/// lock held by a concurrent router must delay a routing decision, never block one, and the worst
/// honest answer here is a miss costing one HTTP call.
const CACHE_BUSY_TIMEOUT: Duration = Duration::from_millis(500);

/// The remote spellings that name a GitHub repository. All three are canonical: this box's own
/// clones carry the scp-like SSH form, CI checkouts carry the HTTPS one, and `ssh://` is that same
/// SSH access spelled as a URL, so honouring one would make eligibility depend on how the clone
/// happened to be made.
const GITHUB_REMOTE_PREFIXES: [&str; 3] = [
    "git@github.com:",
    "https://github.com/",
    "ssh://git@github.com/",
];

/// A repository that resolved to a Codex cloud environment, and the branch its task would run
/// against.
///
/// Holds the environment id and the branch and nothing else. No credential is a field here, which
/// is what stops one riding out through `Decision`'s `Serialize` into `--json`, the printed line,
/// and the decision log.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CloudTarget {
    environment_id: String,
    branch: String,
}

impl CloudTarget {
    /// The one intended constructor, hidden from the rendered API surface rather than made
    /// crate-private: `cloud_target_routing.rs` and `cloud_decision_log.rs` are integration tests
    /// and therefore separate crates, so a `pub(crate)` constructor could not compile them.
    ///
    /// This is a discoverability measure and NOT an enforcement one, and nothing here should be read
    /// as claiming otherwise. `Decision` derives `Deserialize`, so serde can build a `CloudTarget`
    /// out of arbitrary JSON without passing through this function at all. What actually keeps a
    /// claude or opencode job from being DISPATCHED against one is elsewhere: `decide` has exactly
    /// one arm that produces the cloud variant and its pattern names `Provider::Codex` literally,
    /// and `dispatch` matches the execution target first with a cloud arm that never reads
    /// `decision.provider`.
    #[doc(hidden)]
    pub fn resolved(environment_id: String, branch: String) -> CloudTarget {
        CloudTarget {
            environment_id,
            branch,
        }
    }

    pub fn environment_id(&self) -> &str {
        &self.environment_id
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }
}

/// Why a repository may not run its tasks in the cloud.
///
/// Carries no payload on any variant. `NoCodexAuth` in particular is deliberately empty: the natural
/// message to attach would be the parse error, and a serde parse error can quote the surrounding
/// bytes of the file it failed on, which here is `auth.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudIneligible {
    /// No `cloud_repos` entry names this repository, or the allowlist is empty entirely.
    NotAllowlisted,
    /// The directory is not inside a git repository, so there is no branch to submit.
    NotAGitRepo,
    /// No `origin` remote, or one that does not name a GitHub repository.
    NoGitHubRemote,
    /// A detached HEAD: no branch name to submit a cloud task against.
    NoBranch,
    /// The branch has no `refs/remotes/origin/<branch>`, so as far as this clone knows the remote
    /// has never seen it. `codex cloud exec --branch` runs the remote ref, so submitting would ask
    /// the server for a branch that is not there, and the operator's work would not run at all.
    BranchNotOnRemote,
    /// Tracked files are modified, staged, or deleted in the working tree. Those changes exist
    /// nowhere but this box, so the cloud task would run the committed source and silently skip
    /// exactly the edits the task was almost certainly about.
    WorkingTreeDirty,
    /// The branch carries commits that `refs/remotes/origin/<branch>` does not, so the remote ref
    /// the cloud task would run is behind the operator's own work. This is the unpushed-commit case
    /// and the diverged case both: what makes it ineligible is that HEAD holds something the remote
    /// does not, whatever else the remote may also hold.
    BranchAheadOfRemote,
    /// `auth.json` is absent, unparseable, or missing the fields the endpoint needs. The three are
    /// not distinguished, because distinguishing them tempts a message that quotes the file.
    NoCodexAuth,
    /// The endpoint reported no environment for this repository. Also what an expired login, a
    /// timeout, and a changed response shape all read as.
    NoCloudEnvironment,
}

/// Whether this repository's tasks may run in the cloud, with the reason when they may not.
#[derive(Debug, Clone, PartialEq)]
pub enum CloudEligibility {
    Eligible(CloudTarget),
    Ineligible(CloudIneligible),
}

/// IMPURE: eligibility for `dir`, resolving the codex home from `$CODEX_HOME` (or `$HOME/.codex`),
/// the cache from the default state path, and the environments base URL from
/// `$AGENT_ROUTER_ENVIRONMENTS_URL` (or the real endpoint).
///
/// All three environment reads happen HERE and nowhere below, which is what makes `eligibility_in`
/// injectable for every external input it has rather than for two of the three.
pub fn eligibility(dir: &Path, config: &Config) -> CloudEligibility {
    eligibility_in(
        dir,
        config,
        &default_codex_home(),
        &default_cache_path(),
        &environments_base_url(),
    )
}

/// IMPURE: eligibility for `dir`, against an explicit codex home, cache path, and environments base
/// URL.
///
/// The pair mirrors `codex_headroom` / `codex_headroom_in` and `Config::load` / `load_from`: the
/// environment is read in exactly one place, so a caller that needs to point the resolution at a
/// different tree does it with arguments rather than by mutating process environment that every
/// other thread shares. The base URL is a parameter for exactly that reason and not for symmetry:
/// with it read inside, the one test that reaches steps 7 through 9 had to write process environment
/// from a binary whose cases run in parallel threads, which is unsound against any concurrent
/// `getenv` and is why Rust 2024 made `set_var` unsafe. There is one entry point per resolution
/// strategy and no defaulted argument between them, so a caller cannot half-inject.
///
/// The nine steps run in this order, and the order is the design rather than an implementation
/// detail:
///
/// 1. An empty allowlist short-circuits before any subprocess or socket, so an operator who has not
///    asked for cloud pays nothing for the feature and cloud is never inferred from a repository
///    merely happening to be connected upstream.
/// 2. `rev-parse --show-toplevel`, which answers whether this is a repository at all.
/// 3. `remote get-url origin`, parsed for an owner and repo.
/// 4. The allowlist membership guard. Ahead of the cache and ahead of the credential read.
/// 5. `rev-parse --abbrev-ref HEAD`, rejecting a detached HEAD, and then the branch-state proof that
///    the remote ref would run the source the operator is looking at. It sits here, BEFORE the cache
///    read, and that is load bearing: a cache hit returns at step 6, so a branch-state check placed
///    after it would be skipped on exactly the fast path every steady-state cloud run takes, which
///    is every run this check exists for.
/// 6. The cache read. A hit returns here, having opened no credential and made no request.
/// 7. The credential read, on a genuine miss only.
/// 8. The environments call.
/// 9. The cache write, on success only.
pub fn eligibility_in(
    dir: &Path,
    config: &Config,
    codex_home: &Path,
    cache_path: &Path,
    environments_base_url: &str,
) -> CloudEligibility {
    // 1. Nothing configured: no git call, no file read, no request.
    if config.cloud_repos.is_empty() {
        return CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted);
    }

    // 2. Both git reads below walk up to the repository root on their own, so a `--dir` pointing at
    //    a subdirectory resolves identically to one pointing at the root.
    if git_stdout(dir, &["rev-parse", "--show-toplevel"]).is_none() {
        return CloudEligibility::Ineligible(CloudIneligible::NotAGitRepo);
    }

    // 3. A repository with no `origin`, or one that is not on GitHub, has nothing to submit.
    let Some(remote) = git_stdout(dir, &["remote", "get-url", "origin"]) else {
        return CloudEligibility::Ineligible(CloudIneligible::NoGitHubRemote);
    };
    let Some((owner, repo)) = parse_github_remote(&remote) else {
        return CloudEligibility::Ineligible(CloudIneligible::NoGitHubRemote);
    };

    // 4. The allowlist. This runs before the cache is consulted, which is what makes the cache a
    //    redirect for an already-authorized repository rather than an authorization of its own.
    //    GitHub treats owner and repo names case-insensitively, so a config entry differing only in
    //    case names the same repository.
    let slug = format!("{owner}/{repo}");
    if !config
        .cloud_repos
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&slug))
    {
        return CloudEligibility::Ineligible(CloudIneligible::NotAllowlisted);
    }

    // 5. `--abbrev-ref HEAD` answers the literal string `HEAD` on a detached head rather than
    //    failing, so taking the output at face value would submit a task against a branch called
    //    `HEAD`.
    let Some(branch) = git_stdout(dir, &["rev-parse", "--abbrev-ref", "HEAD"]) else {
        return CloudEligibility::Ineligible(CloudIneligible::NoBranch);
    };
    if branch == "HEAD" {
        return CloudEligibility::Ineligible(CloudIneligible::NoBranch);
    }
    if let Err(reason) = remote_would_run_the_local_source(dir, &branch) {
        return CloudEligibility::Ineligible(reason);
    }

    // 6. The cache read, keyed on the lowercased slug. The branch always comes from the repository
    //    and never from the cache: only the environment id is cached.
    let key = slug.to_ascii_lowercase();
    if let Some(environment_id) = cached_environment(cache_path, &key) {
        return CloudEligibility::Eligible(CloudTarget::resolved(environment_id, branch));
    }

    // 7 and 8, both inside one function, so the credentials never exist as locals out here.
    let environment_id = match resolve_environment(codex_home, environments_base_url, &owner, &repo)
    {
        Ok(environment_id) => environment_id,
        Err(reason) => return CloudEligibility::Ineligible(reason),
    };

    // 9. Only a successful resolution is written, and an unwritable cache is a silent no-op: the
    //    resolution genuinely succeeded, so failing the route over a derived-state file would be the
    //    tail wagging the dog.
    write_cache_entry(cache_path, &key, &environment_id);
    CloudEligibility::Eligible(CloudTarget::resolved(environment_id, branch))
}

/// PURE: the owner and repo a GitHub remote URL names, or None when it names something else.
///
/// The rejections matter as much as the matches. Reading an owner and repo out of a GitLab URL or a
/// bare path would submit a cloud task naming a GitHub repository that does not exist.
pub fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let path = GITHUB_REMOTE_PREFIXES
        .iter()
        .find_map(|prefix| url.trim().strip_prefix(prefix))?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// PURE: the environment id out of an environments response, or None when the body carries none.
///
/// The shape is OBSERVED, not guessed. Measured against the live endpoint on 2026-08-03:
///
/// - A connected repository answers HTTP 200 with a top-level JSON ARRAY of environment objects. The
///   measured array held one element of 28 keys, of which `id` is the one read here. The id is a
///   32 character lowercase hex string carrying no `env-` prefix or any other prefix.
/// - A repository that is NOT connected answers HTTP 200 with exactly `[]`. It is not a 404, so an
///   unconnected repository is distinguished by an empty array and by nothing else.
///
/// So the id is the `id` string of the FIRST element of a top-level array, and there is exactly one
/// read: no pointer chain, no fallback shape. A second candidate pointer here would be a shape this
/// code does not know the endpoint to use, and the next reader could not tell which one was real.
///
/// What this does NOT catch:
///
/// - The endpoint remains undocumented. This records what it did on one day against two
///   repositories, not a contract anyone owes us, and it can change shape without notice. A shape
///   change reads as None, which the caller turns into `NoCloudEnvironment`, which reads identically
///   to a repository nobody connected. The measurement date above is what dates that risk.
/// - An array with more than one element is not distinguished from one with a single element; the
///   first is taken. The measured connected repository had exactly one, so which element a
///   multi-environment repository ought to run against is unmeasured and unhandled.
/// - The id's exact 32-hex form is NOT asserted, only the bound and charset in
///   `acceptable_environment_id`. An upstream that starts issuing longer or mixed-case ids keeps
///   working; one that starts issuing ids carrying a character outside that class reads as
///   `NoCloudEnvironment` and looks like a repository nobody connected.
pub fn parse_environment_id(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = value.as_array()?.first()?.get("id")?.as_str()?;
    acceptable_environment_id(id).then(|| id.to_string())
}

/// PURE: whether an identifier read off a third party is one this crate will hand onwards.
///
/// One function for both of the ids this crate accepts from outside itself, the cloud environment id
/// here and the cloud task id in `dispatch/codex_cloud.rs`, because they are the same rule guarding
/// the same sinks. Two copies of four conditions would stay in step; two copies of the reasoning
/// below would not, and this rule is one somebody widens later against a newly observed sample. A
/// widening has to happen in one place, or the sink nobody remembered stays narrow while the
/// rationale next to it claims otherwise.
///
/// The bound is the caller's, because it is a per-caller fact about a different upstream format,
/// even though both callers pass 128 today.
///
/// Both measured forms (live surfaces, 2026-08-03) are narrower than this: a 32 character lowercase
/// hex environment id, and a `task_e_` plus 32 hex task id. What is accepted is deliberately WIDER,
/// because both upstreams are undocumented and pinning the measured format would turn a legitimate
/// format change into a silent feature outage: an id that fails here reads as no id at all, which is
/// indistinguishable from a repository nobody connected. A rule that breaks the feature quietly when
/// the format widens is worse than no rule.
///
/// So this is a bound and a character class rather than a format. Alphanumerics plus `-`, `_`, and
/// `.` admit every plausible identifier spelling (the measured hex, a dashed uuid, a prefixed slug)
/// while excluding what the four sinks downstream cannot take safely:
///
/// - control characters, `\x1b` above all, which reaches a raw terminal write through the rationale
///   `main.rs` prints and can repaint the `execution_target=` line an operator is reading;
/// - whitespace and quotes, which reach the decision log and `--json`;
/// - path separators and shell metacharacters, which reach a URL path and a cache file;
/// - a LEADING `-`, which sits in an argv slot as the value of `--env` or as the id a later
///   `codex cloud status` is handed, where a CLI can read it as a flag. Nothing rejects it
///   downstream that this repository controls.
///
/// Widening the charset is a one-way door, so widen it once against an observed sample recorded in a
/// comment rather than removing the check.
pub(crate) fn is_safe_identifier(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// PURE: whether an environment id is one this module will hand onwards. The shared rule at
/// `is_safe_identifier`, bounded by this module's own ceiling.
///
/// A rejection is `None`, which the caller turns into `Ineligible(NoCloudEnvironment)`: the answer
/// this module already gives for a response shape it does not recognize.
fn acceptable_environment_id(id: &str) -> bool {
    is_safe_identifier(id, ENVIRONMENT_ID_MAX_LEN)
}

/// IMPURE: `~/.local/state/agent-router/cloud-environments.db`, beside `router.db`.
///
/// The router's own derived state, so it lives with the router's state and not in the codex home,
/// which belongs to a different tool. Beside `router.db` rather than inside it, for the reason the
/// module doc gives.
///
/// Crate-private: `eligibility` is its only caller, and every out-of-crate caller reaches the cache
/// by handing `eligibility_in` a path of its own rather than by asking for the default one.
pub(crate) fn default_cache_path() -> PathBuf {
    home_dir().join(".local/state/agent-router/cloud-environments.db")
}

/// `$AGENT_ROUTER_ENVIRONMENTS_URL` if set (the test hook), else the real endpoint.
///
/// The same shape `usage::codex_sessions_dir` gives the sessions directory: the environment is read
/// in exactly one place and the constant is what an unset variable resolves to, so there is no
/// second spelling of the endpoint anywhere in the tree. That one place is `eligibility`, alongside
/// its two other resolutions, which is what keeps the read out of the resolution path itself. The
/// security tradeoff this opens is recorded in the module doc rather than here, because it is a
/// property of the module and not of this function.
fn environments_base_url() -> String {
    match std::env::var("AGENT_ROUTER_ENVIRONMENTS_URL") {
        Ok(url) => url,
        Err(_) => ENVIRONMENTS_BY_REPO.to_string(),
    }
}

/// IMPURE: the environment id the endpoint reports for `owner/repo`, or the reason it could not be
/// read.
///
/// The credentials live and die inside this function. They are read into two locals, passed once as
/// header values, and dropped when it returns; neither reaches a field, a log line, an error, or the
/// value this returns. The endpoint's own errors are dropped with `.ok()?` rather than formatted,
/// because a response body can echo the request that produced it.
///
/// `base` arrives as an argument rather than being read here, so the one function that holds a
/// credential is also the one function with no ambient input deciding where it gets sent.
fn resolve_environment(
    codex_home: &Path,
    base: &str,
    owner: &str,
    repo: &str,
) -> std::result::Result<String, CloudIneligible> {
    let auth = std::fs::read_to_string(codex_home.join("auth.json"))
        .map_err(|_| CloudIneligible::NoCodexAuth)?;
    let value: serde_json::Value =
        serde_json::from_str(&auth).map_err(|_| CloudIneligible::NoCodexAuth)?;
    let (Some(token), Some(account_id)) = (
        value
            .pointer("/tokens/access_token")
            .and_then(|t| t.as_str()),
        value.pointer("/tokens/account_id").and_then(|a| a.as_str()),
    ) else {
        return Err(CloudIneligible::NoCodexAuth);
    };

    let body = ureq::get(format!("{base}/{owner}/{repo}").as_str())
        .config()
        .timeout_global(Some(ENVIRONMENTS_HTTP_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("ChatGPT-Account-Id", account_id)
        .call()
        .ok()
        .and_then(|mut response| response.body_mut().read_to_string().ok())
        .ok_or(CloudIneligible::NoCloudEnvironment)?;

    parse_environment_id(&body).ok_or(CloudIneligible::NoCloudEnvironment)
}

/// IMPURE: whether `refs/remotes/origin/<branch>` would run the source the operator is looking at,
/// or the reason it would not.
///
/// `codex cloud exec --branch <branch>` runs the REMOTE ref. The name resolved at step 5 says
/// nothing about that ref, so without this the three ordinary states of a working branch (edits not
/// yet committed, commits not yet pushed, a branch never pushed at all) each submit a task against
/// source the operator is not looking at, and every one of them SUCCEEDS. There is no error to
/// notice and no row to read afterwards that says the wrong code ran.
///
/// All three reads are local and no fetch happens, which is a deliberate limit rather than an
/// oversight: this runs on the hot path of every auto-routed dispatch, and a fetch would be both a
/// network round trip on that path and a side effect on the operator's repository. What is proven is
/// therefore that the operator's OWN view of the remote agrees with their working tree. The module
/// doc records what that leaves uncovered.
///
/// The order is cheapest-first and each check needs the previous one's answer: the ahead comparison
/// has nothing to compare against until the remote ref is known to exist.
///
/// Every read that fails for any other reason is treated as the ineligible answer for the step it
/// was on, never as a pass. A git that cannot tell us whether the tree is clean has not told us it
/// is clean, and the safe direction is local: local runs the operator's actual working tree and is
/// correct whatever the answer would have been.
fn remote_would_run_the_local_source(
    dir: &Path,
    branch: &str,
) -> std::result::Result<(), CloudIneligible> {
    // The FULLY QUALIFIED ref, not the `origin/<branch>` shorthand. The shorthand resolves through
    // git's ref search order, so a local branch or a tag literally named `origin/<branch>` would
    // satisfy it while no remote-tracking ref existed at all.
    let remote_ref = format!("refs/remotes/origin/{branch}");
    if git_stdout(dir, &["rev-parse", "--verify", "--quiet", &remote_ref]).is_none() {
        return Err(CloudIneligible::BranchNotOnRemote);
    }

    // `--untracked-files=no` for the reason the module doc records. Empty stdout is the clean
    // answer, which is why this goes through `git_output` rather than `git_stdout`: the latter
    // folds empty output into None, and a check that read "clean" as a failure would reject every
    // repository that is in fact fine.
    let Some(status) = git_output(dir, &["status", "--porcelain", "--untracked-files=no"]) else {
        return Err(CloudIneligible::WorkingTreeDirty);
    };
    if !status.is_empty() {
        return Err(CloudIneligible::WorkingTreeDirty);
    }

    // Counting `<remote>..HEAD` asks only whether HEAD holds commits the remote ref does not, so a
    // branch that is merely BEHIND counts zero and stays eligible, while a diverged branch counts
    // its own side and does not.
    let ahead = git_output(
        dir,
        &["rev-list", "--count", &format!("{remote_ref}..HEAD")],
    );
    if ahead.as_deref() != Some("0") {
        return Err(CloudIneligible::BranchAheadOfRemote);
    }

    Ok(())
}

/// IMPURE: `git -C <dir> <args>` stdout, trimmed, where empty output is a successful empty answer.
/// None on a failed spawn, a timeout, a non-zero exit, or output that is not UTF-8.
///
/// The pair with `git_stdout` below exists because the two callers want opposite things from empty
/// output: a branch name that came back empty is a failure, and a `status --porcelain` that came
/// back empty is the whole point. Folding them together is what would make a clean tree read as a
/// broken git call.
fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args).stdin(Stdio::null());
    let stdout = crate::dispatch::codex::run_reporting_failure(command, GIT_TIMEOUT).ok()?;
    Some(stdout.trim().to_string())
}

/// IMPURE: `git -C <dir> <args>` stdout, trimmed. None on a failed spawn, a timeout, a non-zero
/// exit, output that is not UTF-8, or empty output.
///
/// stdin is closed so a git that decides to prompt (a credential helper, a pager) fails immediately
/// rather than hanging a routing decision, and stderr is captured rather than inherited so a
/// resolution that fails does not print git's complaint over the router's own output.
///
/// The timeout is what closes stdin does not cover. A wedged git is not a hypothetical here: an
/// index lock left by a killed process, a hook, a `core.fsmonitor` that stopped answering, or a
/// repository on a stalled network filesystem all hang without ever reading stdin, and this runs on
/// the hot path of every auto-routed `agent-router run`. Unbounded, one of those turns a resolution
/// whose worst honest answer is "route local" into a router that never returns. Six seconds is
/// generous for three local reads of the object store and the config, which answer in single-digit
/// milliseconds on a warm repository, and is the same order as this module's HTTP ceiling.
///
/// A timeout is None like every other failure, never an `Err`, so the module's degradation rule
/// holds: the caller turns it into the `Ineligible` reason for the step it was on and the job
/// dispatches locally. `run_reporting_failure` kills the child on expiry, so a hung git is reaped
/// rather than left behind, and its error string is discarded here rather than formatted, for the
/// same reason git's stderr is not inherited.
///
/// What this does NOT catch: `run_reporting_failure` caps captured stdout at 64 KiB
/// (`dispatch::codex::read_limited`). Past that cap the child wedges writing to a full pipe instead
/// of exiting, so a call that would have produced more output does not return the long output the
/// way a bare `Command::output()` would; it times out instead and reads as this step's failure.
///
/// Four of the five calls made through this pair (`rev-parse --show-toplevel`, `remote get-url
/// origin`, `rev-parse --abbrev-ref HEAD`, `rev-list --count`) cannot plausibly emit 64 KiB. The
/// fifth, `status --porcelain`, genuinely can: a tree with a few hundred modified paths reaches it.
/// That call degrades in the safe direction by construction, because a timeout is None and the
/// caller reads None as `WorkingTreeDirty`, which is the answer a tree that large deserves anyway.
/// A sixth git call added here later is not exempt from the bound, and one whose failure would need
/// to read as ELIGIBLE does not belong on this path at all.
fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    git_output(dir, args).filter(|text| !text.is_empty())
}

/// IMPURE: the cached environment id for `key`, or None.
///
/// Absent, unopenable, not a database at all, missing the table, holding no row for this key, and
/// holding a row whose value is not usable are all one answer: a MISS. The database is state the
/// router wrote for itself, so a damaged one costs a round trip and nothing else, and failing
/// resolution over it would let a corrupted cache block routing entirely.
///
/// READ_ONLY is not a hardening flourish, it is what keeps the read from having a side effect.
/// `Connection::open` CREATES an empty database when the path is absent, so a plain open on a cold
/// cache would leave a file behind on every miss, and "a negative result is never cached" would
/// become "a negative result leaves an empty database that a later reader has to distinguish from a
/// real one". Read-only turns an absent path into the error this function already treats as a miss.
///
/// An entry failing `acceptable_environment_id` is a miss for the same reason and by the same rule
/// as an unrecognized response body. The cache is the second way an id reaches the four sinks, so
/// validating only the endpoint would leave a row on disk able to supply what the endpoint itself
/// could not. SQLite columns are dynamically typed, so `TEXT` on the schema line constrains
/// nothing: a row holding an integer is a real state this has to answer for, and it answers by
/// failing the `String` conversion into the same miss.
fn cached_environment(cache_path: &Path, key: &str) -> Option<String> {
    let conn = Connection::open_with_flags(
        cache_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(CACHE_BUSY_TIMEOUT).ok()?;
    let id: String = conn
        .query_row(
            "SELECT environment_id FROM cloud_environments WHERE slug = ?1",
            [key],
            |row| row.get(0),
        )
        .ok()?;
    acceptable_environment_id(&id).then_some(id)
}

/// IMPURE: record `key -> environment_id`, preserving every other repository's entry.
///
/// An upsert against one row, rather than the read-modify-write over a whole map this replaced. The
/// distinction is the entire point of the storage change: this box runs several routers at once by
/// design, and two of them resolving different repositories concurrently each merged into the map
/// they had read, so whichever wrote second dropped the other's entry. The write was atomic and the
/// UPDATE was not, which is the shape that looks fixed. One statement against one primary key
/// cannot evict a row it does not name.
///
/// `DO UPDATE` rather than `DO NOTHING`, because an environment that was re-created upstream must
/// be able to replace the id already on file once a resolution goes out and comes back with a new
/// one.
///
/// Every failure is a no-op. Nothing here can turn a successful resolution into an unsuccessful one.
fn write_cache_entry(cache_path: &Path, key: &str, environment_id: &str) {
    if create_owner_only(cache_path).is_err() {
        return;
    }
    let _ = upsert_cache_entry(cache_path, key, environment_id);
}

/// IMPURE: the upsert itself, split out so the caller has one place to drop the error rather than
/// five, and so the statement is readable without the error handling wrapped around every line.
fn upsert_cache_entry(
    cache_path: &Path,
    key: &str,
    environment_id: &str,
) -> rusqlite::Result<usize> {
    let conn = Connection::open(cache_path)?;
    conn.busy_timeout(CACHE_BUSY_TIMEOUT)?;
    conn.execute_batch(CACHE_SCHEMA)?;
    conn.execute(
        "INSERT INTO cloud_environments (slug, environment_id) VALUES (?1, ?2) \
         ON CONFLICT(slug) DO UPDATE SET environment_id = excluded.environment_id",
        rusqlite::params![key, environment_id],
    )
}

/// IMPURE: make `path` exist as an owner-only regular file inside an owner-only leaf directory,
/// ready for SQLite to open.
///
/// This module establishes its own permissions rather than reusing the log's, and does not depend on
/// `DecisionLog::open_at` having created the state directory first. A cache write can happen on a
/// path where the log was never opened, and depending on another module's side effect for a
/// permission is wrong however convenient the sharing would be.
///
/// The file is created HERE rather than left to SQLite, and that is the reason this function exists
/// at all rather than being a `create_dir_all`. SQLite creates a database file at 0666 masked by the
/// umask, so on the default 022 it lands 0644 and is world readable for the whole window between its
/// creation and any chmod that follows. Creating it first with `create_new(...).mode(0o600)` closes
/// that window: SQLite is content to initialize an existing zero-length file, and it copies the
/// database file's own mode onto the rollback journal it writes beside it, so the journal inherits
/// 0600 rather than needing its own handling.
///
/// Three properties, each of which the obvious spelling of this function does not have:
///
/// 1. The mode 0700 is applied to the LEAF directory only (`path`'s immediate parent, e.g.
///    `~/.local/state/agent-router`), never to an intermediate this module does not own. Everything
///    above the leaf (`~/.local`, `~/.local/state`) is created first, by a plain `create_dir_all`
///    that leaves each one at the process umask, exactly as `log.rs`'s `restrict_to_owner` narrows
///    only its own leaf and never touches a parent. Restricting an intermediate would be a side
///    effect on a shared XDG directory this module does not own, imposed on every other tool that
///    also creates entries under it.
/// 2. The leaf's own mode is set AT CREATION by `DirBuilderExt::mode` rather than chmodded on the
///    next syscall. A `create_dir_all` on the leaf followed by `set_permissions` would leave a
///    window, however brief, in which the leaf exists at the umask default and any local user can
///    create entries inside it. The `set_permissions` call that follows is not that window's fix; it
///    narrows a leaf that ALREADY existed at a wider mode (created by an earlier run, or by
///    something else entirely), which is the one remaining reason for the call.
/// 3. A `path` that is a SYMLINK is refused rather than written through. `create_new` is
///    `O_CREAT|O_EXCL`, which does not follow one, but it reports the same `AlreadyExists` a real
///    file does, and the `Connection::open` that follows WOULD follow it and lay a database over
///    whatever it points at. The explicit `symlink_metadata` check is what the exclusive create
///    alone does not give once the write is no longer a rename over the path.
///
/// The steady state is `AlreadyExists` on both the directory and the file, since this cache is
/// written to repeatedly, and both fall through to a chmod that narrows what is already there.
#[cfg(unix)]
fn create_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    if let Some(leaf) = path.parent() {
        // Everything above the leaf is a shared directory this module does not own (`~/.local`,
        // `~/.local/state`), so it is created at the umask default rather than 0700.
        if let Some(above_leaf) = leaf.parent() {
            std::fs::create_dir_all(above_leaf)?;
        }
        // The leaf itself is created at 0700 directly, non-recursively, so there is no window at a
        // wider mode.
        match std::fs::DirBuilder::new().mode(0o700).create(leaf) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        std::fs::set_permissions(leaf, std::fs::Permissions::from_mode(0o700))?;
    }

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the cloud environment cache path is a symlink",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn create_owner_only(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body shape the endpoint really answers with, carrying whatever id the case is about. Built
    /// through `serde_json` rather than by string concatenation so a case whose id contains a quote
    /// or an escape is testing the validation rather than testing whether the fixture is valid JSON.
    fn body_with_id(id: &str) -> String {
        serde_json::json!([{ "id": id }]).to_string()
    }

    /// The measured live format must survive, or the validation has broken the feature it guards
    /// rather than the attack it guards against. This is the case that fails if the character class
    /// is ever tightened past what the endpoint actually issues.
    #[test]
    fn the_measured_thirty_two_character_hex_id_is_accepted() {
        let measured = "0f8c1a2b3d4e5f60718293a4b5c6d7e8";
        assert_eq!(measured.len(), 32);
        assert_eq!(
            parse_environment_id(&body_with_id(measured)).as_deref(),
            Some(measured)
        );
    }

    /// An ESC reaching the rationale reaches a raw terminal write, where it can repaint the
    /// `execution_target=` line an operator is reading to decide whether the dispatch went where they
    /// expected.
    #[test]
    fn an_id_carrying_an_ansi_escape_is_rejected() {
        assert_eq!(parse_environment_id(&body_with_id("abc\u{1b}[2Jdef")), None);
        assert_eq!(parse_environment_id(&body_with_id("\u{1b}")), None);
    }

    /// A leading hyphen sits in an argv slot as the value of `--env`. Today a clap parser downstream
    /// rejects it, but that safety lives in a third party's parser configuration and nothing here
    /// pins it.
    #[test]
    fn an_id_beginning_with_a_hyphen_is_rejected() {
        assert_eq!(parse_environment_id(&body_with_id("-whatever")), None);
        // A hyphen anywhere else is ordinary: a dashed uuid must still resolve.
        assert_eq!(
            parse_environment_id(&body_with_id("a-b-c")).as_deref(),
            Some("a-b-c")
        );
    }

    /// The bound is what stops an unbounded response field becoming an unbounded log column, an
    /// unbounded cache entry, and an unbounded argv value.
    #[test]
    fn an_over_long_id_is_rejected() {
        let at_bound = "a".repeat(ENVIRONMENT_ID_MAX_LEN);
        assert_eq!(
            parse_environment_id(&body_with_id(&at_bound)).as_deref(),
            Some(at_bound.as_str()),
            "the bound itself must be accepted, or the rule is off by one"
        );
        let over_bound = "a".repeat(ENVIRONMENT_ID_MAX_LEN + 1);
        assert_eq!(parse_environment_id(&body_with_id(&over_bound)), None);
    }

    /// An empty id is a well-formed response naming no environment. Passing it on would put an empty
    /// string in `--env` and print a rationale that reads as though an environment was found.
    #[test]
    fn an_empty_id_is_rejected() {
        assert_eq!(parse_environment_id(&body_with_id("")), None);
    }

    /// A separator reaches a URL path and a cache file; a quote reaches the decision log and
    /// `--json`. Neither is spelling any identifier this endpoint has ever issued.
    #[test]
    fn an_id_carrying_a_path_separator_or_a_quote_is_rejected() {
        for hostile in [
            "../../etc/passwd",
            "a/b",
            "a\\b",
            "a\"b",
            "a'b",
            "a b",
            "a;rm -rf /",
            "a$(id)",
        ] {
            assert_eq!(
                parse_environment_id(&body_with_id(hostile)),
                None,
                "an id spelled {hostile:?} must not reach an argv slot, a cache file, or a terminal"
            );
        }
    }

    /// The mode 0700 belongs to the leaf directory this module owns, never to an intermediate
    /// created along the way to it, and the database file itself is 0600 BEFORE SQLite ever opens
    /// it. Not root-dependent: it reads permission bits that any user can set and inspect, and not
    /// flaky under a normal umask, because neither 0700 nor 0600 has group or other bits for a
    /// typical umask (022, 027, ...) to have left in place by coincidence.
    ///
    /// The file assertion is what pins the reason this function creates the file at all. Left to
    /// SQLite, the database is created at 0666 masked by the umask, so a default 022 lands it 0644
    /// and the mode assertion below fails at 0o644.
    #[cfg(unix)]
    #[test]
    fn create_owner_only_restricts_the_leaf_and_the_file_but_not_a_fresh_intermediate() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("tempdir");
        // Two levels that do not exist yet, standing in for `~/.local` and `~/.local/state`: shared
        // directories this module does not own and must not narrow.
        let intermediate = root.path().join("shared_a").join("shared_b");
        let leaf = intermediate.join("agent-router");
        let cache_path = leaf.join("cloud-environments.db");

        create_owner_only(&cache_path).expect("create the cache file");

        let leaf_mode = std::fs::metadata(&leaf)
            .expect("leaf metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            leaf_mode, 0o700,
            "the leaf directory must be exactly owner-only"
        );

        let file_mode = std::fs::metadata(&cache_path)
            .expect("cache file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "the database file must be owner-only before SQLite is ever handed it"
        );

        let intermediate_mode = std::fs::metadata(&intermediate)
            .expect("intermediate metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_ne!(
            intermediate_mode, 0o700,
            "a freshly created intermediate must be left at the umask default, not narrowed to 0700"
        );
    }

    /// A cache file left at a wider mode by an earlier build, or by anything else, is narrowed
    /// rather than accepted. `create_new` reports `AlreadyExists` on it and returns no handle to set
    /// a mode through, so without the chmod on that branch the 0600 rule would hold only for a file
    /// this build happened to create.
    #[cfg(unix)]
    #[test]
    fn create_owner_only_narrows_a_cache_file_that_already_exists_at_a_wider_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("tempdir");
        let cache_path = root.path().join("state/cloud-environments.db");
        std::fs::create_dir_all(cache_path.parent().expect("parent"))
            .expect("create the directory");
        std::fs::write(&cache_path, b"").expect("create the pre-existing file");
        std::fs::set_permissions(&cache_path, std::fs::Permissions::from_mode(0o644))
            .expect("widen the pre-existing file");

        create_owner_only(&cache_path).expect("narrow the existing cache file");

        let mode = std::fs::metadata(&cache_path)
            .expect("cache file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "an existing wider cache file must be narrowed");
    }

    /// A symlink at the cache path is refused rather than written through. `create_new` does not
    /// follow one, but it answers `AlreadyExists` exactly as a real file does, and the
    /// `Connection::open` that follows would follow the link and lay a database over its target.
    #[cfg(unix)]
    #[test]
    fn create_owner_only_refuses_a_symlink_at_the_cache_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("someone-elses-file");
        std::fs::write(&target, b"do not overwrite me").expect("write the link target");
        let cache_path = root.path().join("state/cloud-environments.db");
        std::fs::create_dir_all(cache_path.parent().expect("parent"))
            .expect("create the directory");
        std::os::unix::fs::symlink(&target, &cache_path).expect("plant the symlink");

        assert!(
            create_owner_only(&cache_path).is_err(),
            "a symlink at the cache path must be refused, not followed"
        );
        assert_eq!(
            std::fs::read(&target).expect("read the link target"),
            b"do not overwrite me",
            "the link target must be untouched"
        );
    }

    /// The upsert preserves a row it does not name. This is the whole reason the cache is a table
    /// rather than a JSON map: the map version read every entry, merged one, and wrote them all
    /// back, so a concurrent writer that had read the same map dropped this row on its own write.
    /// A statement naming one primary key cannot.
    #[test]
    fn the_upsert_preserves_an_unrelated_entry_and_replaces_its_own() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache_path = root.path().join("state/cloud-environments.db");

        write_cache_entry(&cache_path, "other-owner/other-repo", "other-environment");
        write_cache_entry(&cache_path, "owner/repo", "first-environment");
        write_cache_entry(&cache_path, "owner/repo", "second-environment");

        assert_eq!(
            cached_environment(&cache_path, "other-owner/other-repo").as_deref(),
            Some("other-environment"),
            "a write for one repository must not evict another repository's row"
        );
        assert_eq!(
            cached_environment(&cache_path, "owner/repo").as_deref(),
            Some("second-environment"),
            "a second write for the same repository must replace the id rather than be ignored"
        );
    }

    /// A read must not bring the database into existence. `Connection::open` creates an empty
    /// database for an absent path, so a plain open here would leave a file behind on every cold
    /// miss and make "a negative result is never cached" a statement about rows rather than about
    /// what the operator finds on disk.
    #[test]
    fn a_read_of_an_absent_cache_creates_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache_path = root.path().join("state/cloud-environments.db");

        assert_eq!(cached_environment(&cache_path, "owner/repo"), None);
        assert!(
            !cache_path.exists(),
            "reading a cold cache must not create the database"
        );
    }
}
