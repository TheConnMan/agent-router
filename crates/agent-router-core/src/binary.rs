//! The one place a provider CLI turns into a path on disk.
//!
//! Every provider spawn used to name its binary as a bare string and hand it to `Command::new`,
//! which delegates to `execvp` and therefore to whatever `$PATH` the calling process inherited.
//! Under systemd and cron that `PATH` is `/usr/bin:/bin`, `$HOME/.local/bin` — where every
//! provider CLI on these boxes actually installs — is absent, and the spawn died `ENOENT` before a
//! job id or a session existed. The decision log recorded `No such file or directory (os error 2)`
//! and the queued work behind it was lost silently.
//!
//! Two entry points over one search, deliberately:
//!
//! - [`search_path`] answers about `$PATH` alone. `doctor`'s checks are *named* `claude_on_path`
//!   and its messages say `on PATH`, so it must keep asking the narrow question or start lying.
//! - [`resolve`] / [`resolve_named`] answer the question a spawn actually has: the per-provider
//!   env override first, then `$PATH`, then a two-entry user-local fallback list. The system half
//!   of that list rides on the [`Environment`] rather than on a constant, so an environment built
//!   from data has no path to a host directory at all.
//!
//! [`launch_error`] is the backstop for the gap the search cannot close: a binary that resolved
//! and then vanished, or lost its exec bit, before the exec. `Error::Io`'s `Display` *is* the
//! production string, so every provider spawn maps its io error through here instead.

use crate::error::{Error, Result};
use crate::provider::Provider;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Per-provider overrides. These are the contract: the fallback directory list is a convenience
/// for the one install shape that caused the incident, and every install layout it does not cover
/// is covered by pinning the binary here. That is why every failure message names one.
pub const CLAUDE_BIN_ENV: &str = "AGENT_ROUTER_CLAUDE_BIN";
pub const CODEX_BIN_ENV: &str = "AGENT_ROUTER_CODEX_BIN";
pub const GROK_BIN_ENV: &str = "AGENT_ROUTER_GROK_BIN";
pub const OPENCODE_BIN_ENV: &str = "AGENT_ROUTER_OPENCODE_BIN";

/// The prefix and suffix that mark an environment variable as a binary override, including the
/// review-specific ones `adversarial_review` owns. `Environment::from_process` captures exactly
/// this set, so a caller of [`resolve_named`] can name an override this module does not declare.
const OVERRIDE_PREFIX: &str = "AGENT_ROUTER_";
const OVERRIDE_SUFFIX: &str = "_BIN";

/// The system half of the fallback list. On systemd's built-in default `PATH` this is already
/// present; on cron's `/usr/bin:/bin` it is not, which is the case it earns its place for.
///
/// It is not consulted unconditionally. [`Environment::from_process`] puts it into the environment
/// as data, so it is searched exactly when the environment under test is the real one — see
/// [`Environment::system_fallbacks`] for why that distinction is load-bearing.
const SYSTEM_FALLBACK_DIR: &str = "/usr/local/bin";

/// The user half of the fallback list, relative to `$HOME`. This single directory is what closes
/// the reported defect: it is the directory a login shell adds and a service manager does not.
const USER_FALLBACK_SUFFIX: &str = ".local/bin";

/// PURE: the override variable that pins `provider`'s binary.
pub const fn override_env(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => CLAUDE_BIN_ENV,
        Provider::Codex => CODEX_BIN_ENV,
        Provider::Grok => GROK_BIN_ENV,
        Provider::Opencode => OPENCODE_BIN_ENV,
    }
}

/// The environment resolution reads, as data rather than as process state.
///
/// `std::env::set_var` is `unsafe` in Rust 2024 and is process-global, and libtest runs a suite as
/// threads of one process, so a test that mutated `PATH` or `HOME` would corrupt every test
/// running beside it. The environment is therefore a parameter everywhere, with
/// [`Environment::from_process`] as the single production constructor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    path: Option<OsString>,
    home: Option<PathBuf>,
    overrides: BTreeMap<String, OsString>,
    system_fallbacks: Vec<PathBuf>,
}

impl Environment {
    /// PURE: an environment built entirely from data, and therefore one that searches **nothing**
    /// it was not handed.
    ///
    /// The system fallback list starts empty here on purpose. It used to be a constant the search
    /// consulted unconditionally, which meant a test claiming to strip the environment still
    /// reached `/usr/local/bin` — and a dispatch fixture asserting "no provider resolves" would,
    /// on a box with a provider installed there, resolve the real CLI and start a real billable
    /// job. A constructed environment cannot reach a host directory now; only
    /// [`Environment::from_process`] carries the real list, and a test that wants the system half
    /// asks for it explicitly with [`Environment::with_system_fallbacks`].
    pub fn new(
        path: Option<OsString>,
        home: Option<PathBuf>,
        overrides: BTreeMap<String, OsString>,
    ) -> Self {
        Environment {
            path,
            home,
            overrides,
            system_fallbacks: Vec::new(),
        }
    }

    /// PURE: the same environment with an explicit system fallback list.
    ///
    /// The user half of the fallback list is NOT set here: it stays derived from this
    /// environment's own `home`, so a test that exercises the `$HOME/.local/bin` fallback still
    /// exercises the real derivation rather than a directory handed to it.
    #[must_use]
    pub fn with_system_fallbacks<I, P>(mut self, directories: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.system_fallbacks = directories.into_iter().map(Into::into).collect();
        self
    }

    /// IMPURE: the real process environment. The only place `std::env` is read for resolution, and
    /// the only constructor that carries the production system fallback list.
    pub fn from_process() -> Self {
        let overrides = std::env::vars_os()
            .filter_map(|(name, value)| {
                let name = name.into_string().ok()?;
                (name.starts_with(OVERRIDE_PREFIX) && name.ends_with(OVERRIDE_SUFFIX))
                    .then_some((name, value))
            })
            .collect();
        Environment {
            path: std::env::var_os("PATH"),
            home: nonempty("HOME").map(PathBuf::from),
            overrides,
            system_fallbacks: vec![PathBuf::from(SYSTEM_FALLBACK_DIR)],
        }
    }

    /// PURE: the system half of this environment's fallback list, in order.
    ///
    /// Public because it is the only way to state, as an assertion, that production still searches
    /// exactly `/usr/local/bin` — the equality that proves moving the list out of a constant did
    /// not change what a real dispatch resolves.
    pub fn system_fallbacks(&self) -> &[PathBuf] {
        &self.system_fallbacks
    }

    /// PURE: the `$PATH` value, if one is set.
    fn path(&self) -> Option<&OsStr> {
        self.path.as_deref()
    }

    /// PURE: the `$HOME` value, if one is set. Whether it is a directory is a separate question,
    /// asked at the point of use.
    fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// PURE: one override value, absent when unset or empty. An empty override is treated as
    /// unset, matching how a shell's `FOO=` reads to every other consumer.
    fn override_value(&self, name: &str) -> Option<&OsStr> {
        self.overrides
            .get(name)
            .map(OsString::as_os_str)
            .filter(|value| !value.is_empty())
    }
}

/// IMPURE: the first executable named `binary` on `$PATH`, and nothing else.
///
/// Walked here rather than shelled out to `which`, which would make doctor depend on a binary it
/// does not itself check for. The walk moved out of `doctor.rs` when the resolver was introduced;
/// the reasoning moved with it and still holds.
///
/// This deliberately does **not** consult the fallback directories. `doctor`'s checks are named
/// `*_on_path` and say `on PATH`, so widening this would make it report a binary found only in
/// `$HOME/.local/bin` as being on `PATH` — a false statement in the exact configuration this
/// module exists to handle. [`resolve`] is the entry point that answers the wider question.
pub fn search_path(binary: &str, environment: &Environment) -> Option<PathBuf> {
    path_directories(environment)
        .into_iter()
        .map(|directory| directory.join(binary))
        .find(|candidate| is_executable(candidate))
        .map(absolutize)
}

/// IMPURE: where `provider`'s CLI actually lives — override, then `$PATH`, then the fallback list.
///
/// Returns an absolute path, because a relative one would spawn correctly from the resolving
/// process and fail from a child with a different cwd, and both the classifier and the opencode
/// server change directory before spawning.
pub fn resolve(provider: Provider, environment: &Environment) -> Result<PathBuf> {
    resolve_named(provider.name(), &[override_env(provider)], environment)
}

/// IMPURE: the general form. `override_envs` is an ordered precedence list, so the review lane can
/// put `AGENT_ROUTER_CODEX_REVIEW_BIN` ahead of `AGENT_ROUTER_CODEX_BIN` — pinning a separate
/// reviewer binary is a narrower statement than pinning the dispatch binary, and must outrank it.
///
/// An override that names a path that does not exist, or one that is not executable, **fails**. It
/// never falls through to `$PATH`: an operator who pinned a binary has stated an intention, and
/// silently routing around a typo runs the job on a binary nobody named, which is a worse defect —
/// and an invisible one — than the `ENOENT` this module fixes.
pub fn resolve_named(
    binary: &str,
    override_envs: &[&str],
    environment: &Environment,
) -> Result<PathBuf> {
    for name in override_envs {
        let Some(value) = environment.override_value(name) else {
            continue;
        };
        return if has_separator(value) {
            let pinned = Path::new(value);
            if is_executable(pinned) {
                Ok(absolutize(pinned.to_path_buf()))
            } else {
                Err(Error::Launch(format!(
                    "{name} pins the {binary} executable to {}, which is not an executable file",
                    pinned.display()
                )))
            }
        } else {
            // A separator-free override is a NAME, not a path, so it is searched exactly like a
            // bare binary name — `AGENT_ROUTER_CODEX_BIN=codex-next` finds `~/.local/bin/codex-next`.
            let pinned = value.to_string_lossy().into_owned();
            search(&pinned, environment).ok_or_else(|| not_found(&pinned, Some(name), environment))
        };
    }

    search(binary, environment)
        .ok_or_else(|| not_found(binary, override_envs.first().copied(), environment))
}

/// The exec-format failure, as a raw OS code rather than an [`std::io::ErrorKind`].
///
/// A resolved file that carries an execute bit but is not a runnable image — a binary built for
/// another architecture, or a script with no valid shebang — fails the exec with `ENOEXEC` on Unix
/// and `ERROR_BAD_EXE_FORMAT` on Windows. Rust maps neither to a named `ErrorKind`: both land in
/// `io::ErrorKind::Uncategorized`, which is **unstable** and cannot be matched on stable, so the
/// only stable way to ask this question is `raw_os_error()`. Hence the bare number: it is `errno`,
/// not a magic constant, and taking `libc` for it would mean a dependency this module deliberately
/// does without (see `doctor.rs` on hand-rolling rather than shelling out).
#[cfg(unix)]
const EXEC_FORMAT_ERROR: i32 = 8;
#[cfg(not(unix))]
const EXEC_FORMAT_ERROR: i32 = 193;

/// PURE: whether this spawn failure means the resolved file is not a runnable image.
fn is_exec_format_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(EXEC_FORMAT_ERROR)
}

/// PURE: an `io::Error` from a provider spawn, classified.
///
/// This is the backstop for every spawn site, including any the enumeration missed. `Error::Io`'s
/// `Display` is literally `No such file or directory (os error 2)` — the string the lost
/// production rows recorded — so a `NotFound` reaching the log unmapped recreates the defect after
/// a correct resolution. It also closes the TOCTOU window between resolving and exec'ing.
///
/// Everything that is not the binary being missing or unusable passes through unchanged: a broken
/// pipe or a full disk is a different event, and flattening it into a launch failure would make
/// every unrelated spawn fault read as a missing CLI.
pub fn launch_error(binary: &Path, override_env: &str, error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::Launch(format!(
            "could not launch {}: it is missing or not executable; set {override_env} to pin a \
             working one",
            binary.display()
        )),
        // `is_executable`'s mode check (`mode & 0o111 != 0`) proves the file carries SOME execute
        // bit, not that THIS process may execute it: a root-owned `0100` binary earlier on PATH
        // passes the check, gets selected, and then denies the exec. The spawn is the real
        // authority, so its `PermissionDenied` on the resolved binary is a launch failure here
        // rather than a bare `Error::Io`. Deciding it any other way needs `faccessat`, and this
        // module hand-rolls its checks rather than take that dependency.
        std::io::ErrorKind::PermissionDenied => Error::Launch(format!(
            "could not launch {}: it was found but could not be executed; set {override_env} to \
             pin a working one",
            binary.display()
        )),
        // `is_executable` proves the file carries an execute bit, never that it is a runnable
        // image: a binary built for another architecture, or a script whose shebang is missing or
        // unresolvable, passes the mode check, gets selected, and then fails the exec `ENOEXEC`.
        // That is permanent — no retry makes a wrong-architecture image run — so it belongs on the
        // launch side of the split, where it sets `Classification::unlaunchable` and takes the
        // provider out of automatic routing instead of leaving routing free to pick a CLI that
        // cannot execute. Matched by `errno` because the kind is `Uncategorized`; see
        // [`EXEC_FORMAT_ERROR`].
        _ if is_exec_format_error(&error) => Error::Launch(format!(
            "could not launch {}: it was found but is not executable as a program; set \
             {override_env} to pin a working one",
            binary.display()
        )),
        _ => Error::Io(error),
    }
}

/// IMPURE: override-free resolution — `$PATH`, then the fallback directories.
fn search(binary: &str, environment: &Environment) -> Option<PathBuf> {
    search_path(binary, environment).or_else(|| {
        fallback_directories(environment)
            .into_iter()
            .map(|directory| directory.join(binary))
            .find(|candidate| is_executable(candidate))
            .map(absolutize)
    })
}

/// PURE: the failure, naming the binary, the override that would fix it, and every directory that
/// was **actually** searched. A message listing a directory the resolver skipped is a diagnostic
/// lie, which is why the searched list is built from the same two helpers the search itself walks.
fn not_found(binary: &str, override_env: Option<&str>, environment: &Environment) -> Error {
    let searched = path_directories(environment)
        .into_iter()
        .chain(fallback_directories(environment))
        .map(|directory| directory.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let fix = match override_env {
        Some(name) => format!("set {name}, or add it to PATH"),
        None => "add it to PATH".to_string(),
    };
    Error::Launch(format!(
        "could not find the {binary} executable: {fix} (searched: {searched})"
    ))
}

/// PURE: the `$PATH` entries, in order. An unset `PATH` searches nothing rather than defaulting to
/// anything: `PATH` is the operator's allow-list.
fn path_directories(environment: &Environment) -> Vec<PathBuf> {
    match environment.path() {
        Some(path) => std::env::split_paths(path).collect(),
        None => Vec::new(),
    }
}

/// PURE: the fallback list, in order. In production it is exactly two entries.
///
/// `$HOME/.local/bin` is derived from the environment's own `HOME`, because that derivation IS the
/// fix: it is where every provider CLI here installs and the directory a service manager's `PATH`
/// omits. The system half comes from the environment as data — `/usr/local/bin` under
/// [`Environment::from_process`], and nothing at all under [`Environment::new`], so no constructed
/// environment can reach a real install.
///
/// `/usr/bin` and `/bin` are deliberately absent. They are already on the incident `PATH`, so they
/// contribute nothing to the failure being fixed, and the only situation they would ever fire in
/// is a deliberately emptied `PATH` — an operator isolating a process, whose intent the resolver
/// has no business overriding. The list is not complete against every install layout and is not
/// trying to be; the per-provider override is the complete answer.
///
/// `HOME` is genuinely unset under some systemd units, and it can name something that is not a
/// directory. Either way the user-local entry is dropped from the search *and* from the message.
fn fallback_directories(environment: &Environment) -> Vec<PathBuf> {
    let mut directories = Vec::with_capacity(1 + environment.system_fallbacks().len());
    if let Some(home) = environment.home()
        && home.is_dir()
    {
        directories.push(home.join(USER_FALLBACK_SUFFIX));
    }
    directories.extend(environment.system_fallbacks().iter().cloned());
    directories
}

/// PURE: whether an override value is a path rather than a name.
fn has_separator(value: &OsStr) -> bool {
    Path::new(value).components().count() > 1
}

/// IMPURE only when the input is relative: a resolved binary must be absolute, or it spawns
/// correctly from here and fails from a child with a different cwd.
fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

/// PURE-ish: a heuristic, and deliberately so. The mode test says the file carries some execute
/// bit, never that this process holds it; [`launch_error`] owns the authoritative answer, because
/// only the spawn has one.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// IMPURE: one environment variable, absent when unset or empty.
fn nonempty(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}
