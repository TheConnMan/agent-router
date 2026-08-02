//! Shared fixture helper for writing an executable shell stub.
//!
//! Every stub-writing fixture in this workspace used to inline the same three steps: `fs::write`
//! the script, `set_mode(0o700)`, then exec it. This module is the single place that shape lives,
//! so the fixture's postcondition can be established once rather than nine times.
#![cfg(unix)]
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long the probe keeps retrying before it gives up and fails the fixture.
///
/// Deliberately generous. See the probe loop for why a tight deadline is the wrong shape here.
const PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// Pause between probe attempts, so a retry yields the CPU to the child that is holding the
/// descriptor instead of spinning against it.
const PROBE_BACKOFF: Duration = Duration::from_millis(2);

/// `ETXTBSY`, the only errno the probe retries on. `ErrorKind` has no name for it.
const TEXT_FILE_BUSY: i32 = 26;

/// The one argument a stub answers by exiting 0 before any side effect in its body.
///
/// The token has to sit outside the whole space of first arguments a stub may be asked to branch
/// on, not merely outside the stubs that exist today. Enumerated from the source rather than from
/// memory:
///
/// * `grep -rn '\$1' crates/*/tests/*.rs` finds two branches, both `[ "$1" = "agents" ]`
///   (`run_json_tests.rs:79`, `provider_contract_tests.rs:547`).
/// * The only shell `case` in the suite is `stats_cli.rs:131-136`, over the *last* argument:
///   `*'CLASSIFIER FAILS'*`, `*'SCORED LOW'*`, `*'SCORED ULTRA'*`, and a `*)` catch-all that
///   matches anything at all, including this sentinel. The guard runs before the `case`, which is
///   what keeps the catch-all out of reach.
/// * The first arguments production actually emits: `-p` and `agents` and `--bg` for claude
///   (`src/classify.rs:136`, `src/dispatch/claude.rs:72,187`), `exec` and `app-server` and
///   `features` for codex (`src/classify.rs:186`, `src/dispatch/codex.rs:312`,
///   `src/classify.rs:663`), `run` and `serve` for opencode (`src/dispatch/opencode.rs:321,750`),
///   plus arbitrary free-text prompts.
///
/// A `--`-prefixed token naming this fixture layer is outside all of it and no production call
/// site can emit it.
pub const PROBE_ARG: &str = "--agent-router-fixture-probe";

/// Write `body` as an executable `/bin/sh` stub at `path`.
///
/// The interpreter line and the probe guard are assembled here rather than by the caller, so the
/// guard structurally precedes every side effect in `body`. Several stubs append their argv to a
/// log on every invocation, and a test that asserts a binary was never invoked reads that log, so
/// a probe reaching it would invert the assertion.
pub fn write_stub(path: &Path, body: &str) {
    assert!(
        !body.starts_with("#!"),
        "write_stub supplies the interpreter line itself, so the body must not carry one; \
         a second shebang would run as a comment and hide the mistake: {}",
        path.display()
    );
    let script = format!("#!/bin/sh\nif [ \"$1\" = \"{PROBE_ARG}\" ]; then exit 0; fi\n{body}");
    fs::write(path, script).expect("write the stub");
    let mut permissions = fs::metadata(path).expect("stub metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("make the stub executable");

    // Establish the fixture's postcondition by exercising it: exec the stub once, and do not
    // return until that exec succeeds.
    //
    // This loop is not a retry of the behaviour under test, and it is not a convenience that
    // waits for a flake to pass. It is a bounded proof, and the bound is a property of the
    // system rather than a hope. `fs::write` above closed its own descriptor before returning,
    // and that instant closes the set of descriptors that can ever hold this inode open for
    // writing: only a child forked before it could have inherited a copy, no child forked after
    // it can see one, and no further write to this path overlaps the probe. Rust opens files
    // `O_CLOEXEC`, so every such child drops its copy at its own `execve`, which it reaches on
    // its own without any help from us. The population of blockers is therefore finite and
    // strictly shrinking, so a successful exec is not merely a good sample: it proves no writable
    // descriptor remains on the inode and that none can appear afterwards. That is exactly the
    // guarantee the caller needs, so the loop terminates as soon as it holds.
    //
    // The closed set bound also assumes every child forked in this process reaches `execve` or
    // `_exit` promptly. Rust's `Command` guarantees it and there is no raw `libc::fork` anywhere
    // in the tree, so it holds today; a fixture using raw fork would break the bound, and
    // `PROBE_DEADLINE` is the backstop for that case.
    //
    // The proof rests on one precondition the caller owns: no write to this path may be
    // concurrent with an exec of it, and every write goes through `write_stub`. `fs::write`
    // truncates in place and keeps the inode, so a write racing an exec would open a fresh
    // writable descriptor on an inode already probed clean and reopen the window this probe
    // closed. Per test temp directories are what make the first clause true; a test that
    // deliberately rewrites the same stub between runs satisfies both, because each rewrite
    // carries its own probe and none of them overlaps an exec.
    //
    // The deadline is generous on purpose. This probe spawns, so it forks, so it is itself a
    // participant in the race it resolves: our fork can inherit some other fixture's in flight
    // write descriptor and block that fixture's probe, exactly as its forks block ours. A tight
    // deadline would turn that mutual interference into a spurious failure under load, while a
    // generous one costs nothing on the overwhelmingly common path where the first exec
    // succeeds. Hitting the deadline is a real failure and is reported as one; the fixture never
    // hands back a stub it could not prove executable.
    let started = Instant::now();
    loop {
        // `status` waits for the child. A bare `spawn` would leave one zombie per stub write and
        // the suite writes hundreds into a single libtest process.
        match Command::new(path).arg(PROBE_ARG).status() {
            Ok(status) => {
                // The probe answers through the guard, whose only path is `exit 0`. Any other
                // status means the guard did not match and the body ran instead, which is the
                // side effect leak the guard exists to prevent.
                assert!(
                    status.success(),
                    "the probe guard did not answer at {}: the stub ran its body instead, \
                     status {status:?}",
                    path.display()
                );
                return;
            }
            Err(error) if error.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                assert!(
                    started.elapsed() < PROBE_DEADLINE,
                    "the stub at {} was still held open for writing after {:?}, so it could not \
                     be proven executable: {error}",
                    path.display(),
                    PROBE_DEADLINE
                );
                std::thread::sleep(PROBE_BACKOFF);
            }
            Err(error) => panic!(
                "the stub at {} failed to exec for a reason other than ETXTBSY: {error}",
                path.display()
            ),
        }
    }
}
