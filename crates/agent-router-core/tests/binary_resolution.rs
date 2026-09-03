//! The provider-CLI resolver: override, then `$PATH`, then a small user-local fallback list.
//!
//! Every case here drives the explicit `Environment` seam and mutates no process state. Mutating
//! the process environment is forbidden here and must stay forbidden: the std setter is `unsafe`
//! in Rust 2024 and is process-global, and libtest runs this whole suite as threads of one
//! process, so a test that changed `PATH` or `HOME` would corrupt every test running beside it.
//! No user-namespace isolation primitive appears either: this box permits unprivileged user
//! namespaces and GitHub runners do not, so a jail-based PATH strip would pass here and fail in CI
//! naming a pid file rather than the namespace. A stripped `PATH` is a value, not a sandbox.
//!
//! No case here can reach a host directory, and that is structural rather than careful. The system
//! half of the fallback list rides on the `Environment` instead of on a constant the search
//! consulted unconditionally, so an environment built by `Environment::new` searches only `PATH`
//! and its own `HOME` — both of which are temp dirs here. The one claim that needs the production
//! list, that it is exactly `/usr/local/bin`, is asserted as an equality against
//! `Environment::from_process` rather than by letting a test walk the real directory.

#![cfg(unix)]

use agent_router_core::binary::{
    CLAUDE_BIN_ENV, CODEX_BIN_ENV, Environment, GROK_BIN_ENV, launch_error, resolve, resolve_named,
    search_path,
};
use agent_router_core::{Error, Provider};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

mod common;

/// The review-specific override `adversarial_review.rs` already owns. Not re-exported from
/// `binary.rs`, so the precedence test names it literally, which is also what pins the string.
const CODEX_REVIEW_BIN_ENV: &str = "AGENT_ROUTER_CODEX_REVIEW_BIN";

/// One `Environment` built entirely from data. `path` is joined the way the platform joins it, so
/// the resolver's own `split_paths` is exercised rather than a hand-rolled separator.
fn env_with(path: &[&Path], home: Option<&Path>, overrides: &[(&str, &str)]) -> Environment {
    let joined = if path.is_empty() {
        None
    } else {
        Some(std::env::join_paths(path).expect("join the fixture PATH"))
    };
    let overrides: BTreeMap<String, OsString> = overrides
        .iter()
        .map(|(name, value)| ((*name).to_string(), OsString::from(*value)))
        .collect();
    Environment::new(joined, home.map(PathBuf::from), overrides)
}

/// An environment that resolves nothing at all: no `PATH`, no `HOME`, no override, and no system
/// fallback directory.
///
/// The shared, drift-proof fixture lives in `tests/common`; see its doc comment for why the empty
/// system fallback list is load-bearing. This file wants the `root: None` variant specifically,
/// because several cases here test the HOME-absent / PATH-absent paths, not merely "resolves
/// nothing".
fn stripped() -> Environment {
    common::stripped_environment(None)
}

/// A temp dir plus an executable stub inside it, through the shared fixture rather than a
/// hand-rolled `fs::write` + `set_mode`: `write_stub` proves the stub executable before returning.
fn stub_in(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create the stub directory");
    let path = dir.join(name);
    common::write_stub(&path, "exit 0\n");
    path
}

fn launch_message(error: &Error) -> String {
    match error {
        Error::Launch(message) => message.clone(),
        other => panic!("expected Error::Launch, got {other:?}"),
    }
}

// ------------------------------------------------------------------ the override, first in order

/// Plan test #1, first case. An operator who pinned `AGENT_ROUTER_CODEX_BIN` to a path gets that
/// exact binary. If this breaks, the documented escape hatch for every install layout the fallback
/// list does not cover stops working, and the resolver silently runs a different codex.
#[test]
fn a_path_shaped_override_is_used_when_path_does_not_contain_the_binary() {
    let root = tempfile::tempdir().expect("tempdir");
    let elsewhere = root.path().join("elsewhere");
    let stub = stub_in(&elsewhere, "codex");
    let empty = root.path().join("empty-path-dir");
    std::fs::create_dir_all(&empty).expect("create the empty PATH directory");

    let environment = env_with(&[&empty], None, &[(CODEX_BIN_ENV, &stub.to_string_lossy())]);

    let resolved = resolve(Provider::Codex, &environment).expect("the override resolves");
    assert_eq!(resolved, stub);
}

/// Plan test #1, second case, and edge case E2. An override with no path separator is a NAME, so
/// it is searched exactly like a bare binary name — `AGENT_ROUTER_CODEX_BIN=codex-next` must find
/// `~/.local/bin/codex-next`. If this breaks, the override becomes usable only as an absolute
/// path and the documented bare-name form silently fails.
#[test]
fn a_bare_name_override_is_searched_rather_than_used_verbatim() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let stub = stub_in(&home.join(".local/bin"), "codex-next");
    let empty = root.path().join("empty-path-dir");
    std::fs::create_dir_all(&empty).expect("create the empty PATH directory");

    let environment = env_with(&[&empty], Some(&home), &[(CODEX_BIN_ENV, "codex-next")]);

    let resolved =
        resolve(Provider::Codex, &environment).expect("a bare-name override is searched");
    assert_eq!(resolved, stub);
    assert!(
        resolved.is_absolute(),
        "a searched override still resolves to an absolute path: {}",
        resolved.display()
    );
}

/// Plan test #6, and edge case E1. An override naming a path that does not exist, or one that
/// exists and is not executable, must FAIL rather than fall through to `PATH`.
///
/// An operator who pinned a binary has stated an intention. Silently routing around a typo runs
/// the job on a different binary than the one that was named, which is a worse defect than the
/// ENOENT this ticket fixes, and it is invisible in the log.
#[test]
fn an_override_naming_a_missing_or_unexecutable_file_fails_rather_than_falling_through_to_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let on_path = root.path().join("bin");
    let shadowed = stub_in(&on_path, "codex");

    // A path that does not exist at all.
    let missing = root.path().join("opt/typo/codex");
    let environment = env_with(
        &[&on_path],
        None,
        &[(CODEX_BIN_ENV, &missing.to_string_lossy())],
    );
    let error = resolve(Provider::Codex, &environment).expect_err("a typo'd override must fail");
    let message = launch_message(&error);
    assert!(
        message.contains(&missing.to_string_lossy().to_string()),
        "the failure must name the path the operator pinned: {message}"
    );
    assert!(
        !message.contains(&shadowed.to_string_lossy().to_string()),
        "a pinned override must not fall through to the PATH hit: {message}"
    );

    // A path that exists and is a regular file, but carries no execute bit.
    let unexecutable = root.path().join("not-executable");
    std::fs::write(&unexecutable, "#!/bin/sh\nexit 0\n").expect("write the non-executable file");
    let environment = env_with(
        &[&on_path],
        None,
        &[(CODEX_BIN_ENV, &unexecutable.to_string_lossy())],
    );
    let error = resolve(Provider::Codex, &environment)
        .expect_err("a non-executable override must fail rather than fall through");
    assert!(
        launch_message(&error).contains(&unexecutable.to_string_lossy().to_string()),
        "the failure must name the file the operator pinned"
    );
}

/// Plan test #6, positive half. An override that contains a path separator is used verbatim: it is
/// never re-searched, and it never picks up the fallback directories.
#[test]
fn an_override_containing_a_path_separator_is_used_verbatim() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let fallback = stub_in(&home.join(".local/bin"), "grok");
    let pinned = stub_in(&root.path().join("opt"), "grok");

    let environment = env_with(
        &[],
        Some(&home),
        &[(GROK_BIN_ENV, &pinned.to_string_lossy())],
    );

    let resolved = resolve(Provider::Grok, &environment).expect("the pinned path resolves");
    assert_eq!(
        resolved, pinned,
        "the override outranks the fallback directory, which also holds a grok"
    );
    assert_ne!(resolved, fallback);
}

/// Plan test #7. `adversarial_review` keeps its own `AGENT_ROUTER_CODEX_REVIEW_BIN`, and it must
/// outrank the generic per-provider override: an operator who pins a separate reviewer binary is
/// making a narrower statement than the one that pins the dispatch binary.
#[test]
fn an_explicit_review_override_outranks_the_generic_provider_override() {
    let root = tempfile::tempdir().expect("tempdir");
    let reviewer = stub_in(&root.path().join("review"), "codex");
    let workhorse = stub_in(&root.path().join("dispatch"), "codex");

    let environment = env_with(
        &[],
        None,
        &[
            (CODEX_REVIEW_BIN_ENV, &reviewer.to_string_lossy()),
            (CODEX_BIN_ENV, &workhorse.to_string_lossy()),
        ],
    );

    let resolved = resolve_named(
        "codex",
        &[CODEX_REVIEW_BIN_ENV, CODEX_BIN_ENV],
        &environment,
    )
    .expect("the review override resolves");
    assert_eq!(resolved, reviewer);

    // With only the generic override set, the review path still resolves — the ordered list is a
    // precedence, not a requirement that the first entry be present.
    let environment = env_with(&[], None, &[(CODEX_BIN_ENV, &workhorse.to_string_lossy())]);
    let resolved = resolve_named(
        "codex",
        &[CODEX_REVIEW_BIN_ENV, CODEX_BIN_ENV],
        &environment,
    )
    .expect("the generic override still resolves");
    assert_eq!(resolved, workhorse);
}

// ------------------------------------------------------------------ the fallback: the actual fix

/// Plan test #2. THIS IS THE TEST THAT ENCODES THE PRODUCTION FIX. Under systemd and cron the
/// inherited `PATH` is `/usr/bin:/bin`, `$HOME/.local/bin` is absent, and every provider CLI on
/// this box is a user-scope install living exactly there. 13 decision rows in three days recorded
/// `error: No such file or directory (os error 2)` for that reason and lost the work behind them.
///
/// If this test is deleted the ticket is undone.
#[test]
fn a_binary_in_the_home_local_bin_fallback_is_found_off_a_stripped_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let stub = stub_in(&home.join(".local/bin"), "claude");
    let service_manager_path = root.path().join("usr-bin");
    std::fs::create_dir_all(&service_manager_path).expect("create the service-manager PATH dir");

    let environment = env_with(&[&service_manager_path], Some(&home), &[]);

    let resolved =
        resolve(Provider::Claude, &environment).expect("the user-local install must be found");
    assert_eq!(resolved, stub);
    assert!(resolved.is_absolute());
}

/// Plan test #5, and the D1 guard. `doctor`'s checks are NAMED `claude_on_path` and its message
/// reads `no executable claude on PATH`. If `search_path` ever grew the fallback directories,
/// doctor would report a binary found only in `$HOME/.local/bin` as being "on PATH", which is a
/// false statement in the exact configuration this ticket exists to handle.
///
/// One search, two honestly-named entry points. This is what fails if someone "simplifies" them
/// into one.
#[test]
fn search_path_does_not_consult_the_fallback_directories() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    stub_in(&home.join(".local/bin"), "claude");
    let empty = root.path().join("empty-path-dir");
    std::fs::create_dir_all(&empty).expect("create the empty PATH directory");

    let environment = env_with(&[&empty], Some(&home), &[]);

    assert_eq!(
        search_path("claude", &environment),
        None,
        "search_path answers about PATH alone, so doctor's `on PATH` message stays true"
    );
    assert!(
        resolve(Provider::Claude, &environment).is_ok(),
        "resolve consults the fallback, which is the whole difference between the two entry points"
    );
}

/// Plan test #9, and edge case E5. The fallback list is exactly `$HOME/.local/bin` and
/// `/usr/local/bin`, and deliberately NOT `/usr/bin` or `/bin`.
///
/// `PATH` is the operator's allow-list; every directory the resolver adds beyond it is a directory
/// the unit file chose to omit. `/usr/bin` and `/bin` are already on the incident PATH, so they
/// contribute nothing to the failure being fixed and firing only on a deliberately emptied `PATH`
/// would override an explicit isolation intent. A later "helpful" widening of the list must be a
/// red test rather than a silent policy change.
#[test]
fn the_fallback_list_is_exactly_the_two_user_local_directories() {
    // The system half, pinned by equality against the one constructor that carries it. This is
    // what proves moving the list off a constant and onto the `Environment` did not change what a
    // real dispatch searches — and it pins the literal harder than the substring check it
    // replaces, since an added third directory now fails here.
    assert_eq!(
        Environment::from_process().system_fallbacks(),
        [PathBuf::from("/usr/local/bin")],
        "production searches exactly one system fallback directory, and it is /usr/local/bin"
    );

    // `sh` is guaranteed at `/bin/sh` on every target this crate builds for, and `/usr/bin` is on
    // essentially every box. With PATH and HOME both absent, resolution must still fail: the FHS
    // directories are not searched, not even when a system fallback directory IS configured.
    let root = tempfile::tempdir().expect("tempdir");
    let system = root.path().join("system-fallback");
    std::fs::create_dir_all(&system).expect("create the system fallback directory");
    let environment = stripped().with_system_fallbacks([&system]);

    let error =
        resolve_named("sh", &[], &environment).expect_err("/bin and /usr/bin must not be searched");
    let message = launch_message(&error);
    assert!(
        !message.contains("/usr/bin") && !message.split(", ").any(|entry| entry.trim() == "/bin"),
        "the searched list must not claim /usr/bin or /bin: {message}"
    );
    assert!(
        message.contains(&system.to_string_lossy().to_string()),
        "the configured system fallback directory is searched and named: {message}"
    );
}

/// The structural half of the safety property, and the guard the P1 review finding asked for.
///
/// The search used to consult `/usr/local/bin` unconditionally, so a fixture claiming "no provider
/// resolves" would, on a box with a provider installed there, resolve the REAL CLI — and in the
/// dispatch fixtures, spawn it. The positive half is asserted first so the negative cannot pass
/// vacuously: the very same stub, in the very same directory, resolves when the environment opts
/// in and is unreachable when it does not.
#[test]
fn a_constructed_environment_cannot_reach_a_directory_that_would_be_a_system_fallback() {
    let root = tempfile::tempdir().expect("tempdir");
    let would_be_system = root.path().join("system-fallback");
    let stub = stub_in(&would_be_system, "claude");

    let opted_in = stripped().with_system_fallbacks([&would_be_system]);
    assert_eq!(
        resolve(Provider::Claude, &opted_in).expect("an opted-in system fallback is searched"),
        stub,
        "the stub is genuinely reachable, so the negative below is about the opt-in and nothing else"
    );

    let error = resolve(Provider::Claude, &stripped())
        .expect_err("a constructed environment must not reach a system fallback directory");
    let message = launch_message(&error);
    assert!(
        !message.contains(&would_be_system.to_string_lossy().to_string()),
        "an un-opted-in directory is neither searched nor named: {message}"
    );
    assert!(
        !message.contains("/usr/local/bin"),
        "no host directory is consulted by an environment built from data: {message}"
    );
}

/// Edge case E6. `HOME` is genuinely unset under some systemd units, and the resolver must not
/// panic there. It must also not NAME `$HOME/.local/bin` in the searched list, because a message
/// listing a directory that was not actually searched is a diagnostic lie.
#[test]
fn an_absent_home_omits_the_user_local_directory_from_the_search_and_from_the_message() {
    // A temp stand-in for `/usr/local/bin` rather than the directory itself: the claim under test
    // is that the HOME-derived entry drops out while the system half survives, and that claim does
    // not need a host directory. That production's system half IS `/usr/local/bin` is pinned by
    // equality in `the_fallback_list_is_exactly_the_two_user_local_directories`.
    let root = tempfile::tempdir().expect("tempdir");
    let system = root.path().join("system-fallback");
    std::fs::create_dir_all(&system).expect("create the system fallback directory");
    let environment = stripped().with_system_fallbacks([&system]);

    let error = resolve(Provider::Codex, &environment).expect_err("nothing resolves");
    let message = launch_message(&error);
    assert!(
        !message.contains(".local/bin"),
        "with no HOME there is no user-local directory to have searched: {message}"
    );
    assert!(
        message.contains(&system.to_string_lossy().to_string()),
        "the fallback directory that does not depend on HOME is still searched: {message}"
    );
}

// ------------------------------------------------------------------ the failure message

/// Plan test #3, and the direct encoding of the reported defect.
///
/// The failure must name three things: the binary, the env var that would pin it, and every
/// directory that was actually searched. And it must NOT read `No such file or directory (os error
/// 2)` — that string is what 13 lost production rows recorded, and a refactor that reintroduced a
/// passthrough io error would still "fail", just uselessly.
#[test]
fn resolution_failure_names_the_binary_the_env_var_and_every_searched_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".local/bin")).expect("create an empty user-local bin");
    let first = root.path().join("path-one");
    let second = root.path().join("path-two");
    std::fs::create_dir_all(&first).expect("create the first PATH directory");
    std::fs::create_dir_all(&second).expect("create the second PATH directory");

    let system = root.path().join("system-fallback");
    std::fs::create_dir_all(&system).expect("create the system fallback directory");

    let environment =
        env_with(&[&first, &second], Some(&home), &[]).with_system_fallbacks([&system]);
    let error = resolve(Provider::Codex, &environment).expect_err("nothing resolves");
    assert!(
        matches!(error, Error::Launch(_)),
        "a missing provider CLI is a Launch failure, not an Io or Command one: {error:?}"
    );
    let message = launch_message(&error);

    assert!(message.contains("codex"), "names the binary: {message}");
    assert!(
        message.contains(CODEX_BIN_ENV),
        "names the override that would fix it: {message}"
    );
    for directory in [
        first.to_string_lossy().to_string(),
        second.to_string_lossy().to_string(),
        home.join(".local/bin").to_string_lossy().to_string(),
        system.to_string_lossy().to_string(),
    ] {
        assert!(
            message.contains(&directory),
            "names every directory searched, missing {directory}: {message}"
        );
    }

    // The mutation-resistant half. Scoped to this one error, per edge case E13: a blanket check
    // over process output would trip on an unrelated io error.
    assert!(
        !message.contains("os error 2"),
        "the resolver's own failure must never render as the production string: {message}"
    );
    assert!(
        !message.contains("No such file or directory"),
        "the resolver's own failure must never render as the production string: {message}"
    );
}

/// Plan test #4. A relative result would spawn correctly from the resolving process and fail from
/// a child with a different cwd — `run_from_home` changes cwd for every classifier call and
/// `ensure_server` sets `current_dir(&durable_cwd)`, so absoluteness is a real invariant.
///
/// All three resolution sources are checked, because each one builds its path differently.
#[test]
fn every_resolution_source_returns_an_absolute_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let on_path_dir = root.path().join("bin");

    let from_fallback = stub_in(&home.join(".local/bin"), "grok");
    let from_path = stub_in(&on_path_dir, "grok");
    let from_override = stub_in(&root.path().join("opt"), "grok");
    let override_text = from_override.to_string_lossy().into_owned();

    for (label, environment, expected) in [
        (
            "the fallback directory",
            env_with(&[], Some(&home), &[]),
            from_fallback,
        ),
        (
            "a PATH entry",
            env_with(&[&on_path_dir], Some(&home), &[]),
            from_path,
        ),
        (
            "a path-shaped override",
            env_with(
                &[&on_path_dir],
                Some(&home),
                &[(GROK_BIN_ENV, override_text.as_str())],
            ),
            from_override.clone(),
        ),
    ] {
        let resolved = resolve(Provider::Grok, &environment)
            .unwrap_or_else(|error| panic!("{label} must resolve: {error}"));
        assert!(
            resolved.is_absolute(),
            "{label} must return an absolute path, got {}",
            resolved.display()
        );
        assert_eq!(resolved, expected, "{label} resolved to the wrong stub");
    }
}

// ------------------------------------------------------------------ the io-laundry backstop

/// Plan test #8, and the guard for decision D3(c). This mapper is the backstop for every spawn
/// site the enumeration missed: `runtime.rs`'s `spawn_detached`, `list_agents`, `run_review`, and
/// the Grok lifecycle wrap all convert `std::io::Error` directly, and `Error::Io`'s `Display` IS
/// `No such file or directory (os error 2)`.
///
/// It also closes the TOCTOU window: a binary that resolved and then vanished, or lost its exec
/// bit, before the exec still reports a named launch failure rather than the production string.
#[test]
fn a_post_resolve_enoent_is_still_a_launch_failure() {
    let binary = Path::new("/home/operator/.local/bin/codex");

    let mapped = launch_error(
        binary,
        CODEX_BIN_ENV,
        std::io::Error::from(std::io::ErrorKind::NotFound),
    );
    assert!(
        matches!(mapped, Error::Launch(_)),
        "a NotFound at the exec is the same event as a NotFound at the resolve: {mapped:?}"
    );
    let message = launch_message(&mapped);
    assert!(message.contains("codex"), "names the binary: {message}");
    assert!(
        message.contains(CODEX_BIN_ENV),
        "names the override that would fix it: {message}"
    );
    assert!(
        !message.contains("os error 2"),
        "the mapped failure must not carry the production string: {message}"
    );
}

/// The P2 review finding, and the reason the mode check is only a heuristic. `is_executable` tests
/// `mode & 0o111 != 0`, which says the file carries SOME execute bit — not that this process holds
/// it. A root-owned `0100` binary earlier on `PATH` passes that check, gets selected, and then
/// denies the exec. The spawn is the authority, so its `PermissionDenied` must arrive as a named
/// launch failure rather than as a bare `Error::Io` rendering `Permission denied (os error 13)`
/// with no path and no fix in it.
///
/// The old guard mapped this only when `!is_executable(binary)`, so precisely the case that can
/// actually happen — the file IS mode-executable — fell through unmapped.
#[test]
fn a_permission_denied_on_the_resolved_binary_is_a_launch_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let binary = stub_in(&root.path().join("bin"), "claude");
    assert!(
        std::fs::metadata(&binary).expect("stat the stub").is_file(),
        "the fixture must be the mode-executable case, which is the one the old guard missed"
    );

    let mapped = launch_error(
        &binary,
        CLAUDE_BIN_ENV,
        std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    );
    assert!(
        matches!(mapped, Error::Launch(_)),
        "a denied exec on the resolved binary is a launch failure: {mapped:?}"
    );
    let message = launch_message(&mapped);
    assert!(
        message.contains(&binary.to_string_lossy().to_string()),
        "names the binary that could not be executed: {message}"
    );
    assert!(
        message.contains(CLAUDE_BIN_ENV),
        "names the override that would fix it: {message}"
    );
    assert!(
        message.contains("found but could not be executed"),
        "says the binary was found, which is what separates this from the missing case: {message}"
    );
}

/// The P1 review finding. The mode check says the file carries an execute bit; it says nothing
/// about the file being a runnable image. A CLI built for another architecture, or a script whose
/// shebang is missing or names an absent interpreter, passes `is_executable`, gets resolved, and
/// then fails the exec `ENOEXEC` — which Rust leaves in the unstable `Uncategorized` kind, so the
/// wildcard used to call it transient and hand back `Error::Io`.
///
/// It is permanent: no retry makes a wrong-architecture image run. Classifying it as transient
/// means the persisted row omits `classifier_unlaunchable` and routing stays free to select a
/// provider whose CLI cannot execute at all — the silent misrouting this module exists to remove.
///
/// The error is constructed from the raw code so the assertion holds on every unix target rather
/// than only where a bad image can be produced; the case below drives a real one.
#[test]
fn an_exec_format_failure_on_the_resolved_binary_is_a_launch_failure() {
    const ENOEXEC: i32 = 8;

    let binary = Path::new("/home/operator/.local/bin/codex");
    let mapped = launch_error(
        binary,
        CODEX_BIN_ENV,
        std::io::Error::from_raw_os_error(ENOEXEC),
    );

    assert!(
        matches!(mapped, Error::Launch(_)),
        "a binary that is not a runnable image can never be retried into working: {mapped:?}"
    );
    let message = launch_message(&mapped);
    assert!(
        message.contains("codex"),
        "names the binary that could not be executed: {message}"
    );
    assert!(
        message.contains(CODEX_BIN_ENV),
        "names the override that would pin a working one: {message}"
    );
    assert!(
        message.contains("found but is not executable as a program"),
        "says the binary was found, which is what separates this from the missing case: {message}"
    );
    assert!(
        !message.contains("os error 8"),
        "the mapped failure must not carry the bare io string: {message}"
    );
}

/// The same claim driven end to end, because the constructed case above can only prove the mapper
/// agrees with a number this test would have to state twice. Here the kernel supplies the errno: a
/// mode-executable file whose contents are not a valid image and carry no shebang, spawned through
/// the same `Command` every provider spawn uses.
///
/// This is the fixture shape `resolve` would actually hand a spawn — `is_executable` accepts it,
/// which is precisely why the mapper has to be the one to reject it.
#[test]
fn a_real_bad_image_spawn_maps_to_a_launch_failure() {
    let root = tempfile::tempdir().expect("tempdir");
    let binary = root.path().join("codex");
    // ELF magic followed by nothing the loader can use, so the kernel rejects the image rather
    // than falling back to a shell the way it would for a shebang-less text file.
    std::fs::write(&binary, b"\x7fELF\x02\x01\x01\x00not a real image").expect("write bad image");
    let mut permissions = std::fs::metadata(&binary)
        .expect("stat the bad image")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    std::fs::set_permissions(&binary, permissions).expect("make the bad image executable");

    let error = std::process::Command::new(&binary)
        .spawn()
        .expect_err("a file that is not a runnable image cannot be exec'd");
    assert_eq!(
        error.raw_os_error(),
        Some(8),
        "the fixture must reproduce ENOEXEC, or it proves nothing: {error:?}"
    );

    let mapped = launch_error(&binary, CODEX_BIN_ENV, error);
    assert!(
        matches!(mapped, Error::Launch(_)),
        "a real bad-image spawn is a launch failure: {mapped:?}"
    );
    assert!(
        launch_message(&mapped).contains(&binary.to_string_lossy().to_string()),
        "names the binary that could not be executed"
    );
}

/// The other half of plan test #8, and the bound on the `ENOEXEC` case above. The mapper must NOT
/// swallow io errors that are not about the binary being missing or unrunnable: a full disk, a
/// broken pipe, a descriptor limit, a fork that lost to memory pressure is a different event and
/// must keep its own kind, or every unrelated spawn fault starts reading as a missing CLI — and,
/// worse, excludes a healthy provider from automatic routing.
///
/// `EMFILE` is here as a raw-errno case on purpose: the mapper now inspects `raw_os_error()`, so a
/// transient failure that carries an errno is exactly what a too-wide match would capture.
#[test]
fn an_io_error_that_is_not_a_missing_binary_passes_through_unchanged() {
    const EMFILE: i32 = 24;

    let transient = [
        std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        std::io::Error::from(std::io::ErrorKind::AlreadyExists),
        std::io::Error::from(std::io::ErrorKind::InvalidInput),
        std::io::Error::from(std::io::ErrorKind::WouldBlock),
        std::io::Error::from(std::io::ErrorKind::Interrupted),
        std::io::Error::from(std::io::ErrorKind::OutOfMemory),
        std::io::Error::from_raw_os_error(EMFILE),
    ];

    for error in transient {
        let described = format!("{error:?}");
        let mapped = launch_error(
            Path::new("/home/operator/.local/bin/claude"),
            CLAUDE_BIN_ENV,
            error,
        );
        assert!(
            matches!(mapped, Error::Io(_)),
            "{described} is not a launch failure and must stay Error::Io: {mapped:?}"
        );
    }
}
