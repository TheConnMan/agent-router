//! Shared fixtures: an executable shell stub, a throwaway git repository, and a stand-in for the
//! environments endpoint.
//!
//! Every stub-writing fixture in this workspace used to inline the same three steps: `fs::write`
//! the script, `set_mode(0o700)`, then exec it. This module is the single place that shape lives,
//! so the fixture's postcondition can be established once rather than nine times.
//!
//! The endpoint stub and the `git` runner arrived the same way, as near-verbatim copies in
//! `cloud_eligibility.rs` (core) and `cloud_target_cli.rs` (cli), which are separate crates and
//! reach this file by `#[path]` include. They live here for the same reason: an HTTP head parser and
//! a fixture repository builder are not what either of those files is about, and two copies of a
//! parser drift into two different ideas of what the endpoint said.
#![cfg(unix)]
#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
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

/// Run `git` in `repo` and fail the fixture with git's own stderr if it did not succeed.
///
/// Identity and signing are passed inline on every invocation, so the fixture works on a box with no
/// git identity configured and a globally configured signing key cannot block a commit. `git` is not
/// mocked anywhere it is used: it is the real dependency of the things under test, and a mocked one
/// would test the fixture's idea of a repository rather than a repository.
pub fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Ceiling on reading one request head, so a client that opens a socket and says nothing cannot
/// wedge the accept loop and therefore cannot hang the test binary.
const STUB_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on one request head, so a client that never sends the blank line cannot grow it
/// unboundedly either.
const STUB_MAX_HEAD: usize = 16 * 1024;

/// One request the stub received, reduced to the two things worth asserting on: where it was aimed
/// and what it carried.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl RecordedRequest {
    /// The request line's target and every header, names lowercased because HTTP header names are
    /// case-insensitive and which case `ureq` sends in is not this suite's business.
    fn parse(head: &str) -> Option<RecordedRequest> {
        let mut lines = head.split("\r\n");
        let target = lines.next()?.split_whitespace().nth(1)?.to_string();
        let headers = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        Some(RecordedRequest { target, headers })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A stand-in for the environments endpoint, bound on a loopback port.
///
/// This exists so the credential path can be executed offline. Without it the only route to
/// `Eligible` is a seeded cache, which returns before the credential is ever opened, and the
/// credential-confinement property is then asserted by a test that never runs the code it is about.
///
/// The accept loop runs on a detached thread and never stops. Nothing joins it: the libtest process
/// exits when its tests finish and takes the thread with it, and a stop signal would buy only a
/// tidier shutdown of a socket the kernel closes anyway. What the loop does guarantee is that no
/// single connection can wedge it, hence the read timeout and the head ceiling.
pub struct EnvironmentsStub {
    /// What a case hands `eligibility_in`, or a child process, as its base URL. No trailing slash:
    /// production joins `{base}/{owner}/{repo}`, so one here would send a doubled separator and the
    /// target assertions would be testing the fixture.
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl EnvironmentsStub {
    /// One canned body for every request target, for a case that resolves exactly one repository.
    pub fn serving(body: &'static str) -> EnvironmentsStub {
        EnvironmentsStub::routing(move |_| body)
    }

    /// A body chosen by request PATH, for one stub shared by several cases.
    ///
    /// Routing on the path is what makes sharing sound: it is the thing production varies anyway, so
    /// a shared stub distinguishes cases the same way the real endpoint does rather than by holding
    /// per-case state. A case that answers `[]` because its path was registered with a typo would
    /// read as a repository nobody connected, which is why every case asserts its own request count.
    pub fn routing(body_for: impl Fn(&str) -> &'static str + Send + 'static) -> EnvironmentsStub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub endpoint");
        let port = listener.local_addr().expect("stub endpoint address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(STUB_READ_TIMEOUT));
                let Some(head) = read_request_head(&mut stream) else {
                    continue;
                };
                let Some(request) = RecordedRequest::parse(&head) else {
                    continue;
                };
                let body = body_for(&request.target);
                // Recorded BEFORE the response is written, which is what makes reading the log
                // race-free: a caller that has received its response has necessarily already had
                // its request recorded, so a case reading the log after the run returns never
                // observes a half-handled request.
                recorded.lock().expect("stub request log").push(request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        EnvironmentsStub {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Every request this stub received, for a stub only one case is using.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("stub request log").clone()
    }

    /// The requests aimed at one target, which is how a case reads its own traffic out of a stub
    /// shared with every other case in the binary.
    pub fn requests_for(&self, target: &str) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .expect("stub request log")
            .iter()
            .filter(|request| request.target == target)
            .cloned()
            .collect()
    }
}

/// Everything up to and including the blank line that ends an HTTP request head. None on a closed
/// connection, a timeout, a head past the ceiling, or bytes that are not UTF-8.
///
/// A GET carries no body, so the head is the whole request and there is nothing further to drain.
fn read_request_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= STUB_MAX_HEAD {
            return None;
        }
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => head.push(byte[0]),
        }
    }
    String::from_utf8(head).ok()
}
