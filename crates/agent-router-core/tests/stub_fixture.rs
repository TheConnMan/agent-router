//! The fixture helper's own regression test.
//!
//! libtest runs every test as a thread of one process, so a `Command::spawn` anywhere in the suite
//! forks a child that inherits a copy of the whole descriptor table. A child forked while
//! `fs::write`'s descriptor is still open holds a writable descriptor on the stub's inode until it
//! reaches its own `execve`, and Linux refuses to exec a file any process holds open for writing:
//! `ETXTBSY`, errno 26. `execvp` does not fall through to the next PATH entry on `ETXTBSY`, it
//! returns the error, so the fixture hands back a path that is not yet executable.
//!
//! This test recreates that exact shape: workers write stubs through the shared helper and exec
//! them, while noise threads supply the concurrent forks. The assertion is on the observed exec
//! outcome only, never on the helper's names or on the generated script's text, so renaming
//! `write_stub` or the probe sentinel leaves it passing.
#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

/// `ETXTBSY`. Checked as a raw errno rather than an `ErrorKind`, which does not name it.
const TEXT_FILE_BUSY: i32 = 26;

/// Sizing. A standalone harness reproducing the unfixed idiom measured 335 `ETXTBSY` in 3200
/// execs, about 10 percent per exec. Four workers times 100 iterations is 400 execs, so the
/// chance of a clean run against the unfixed helper is 0.9^400, roughly 1e-18. Even if this box
/// reproduces at a tenth of the measured rate, 0.99^400 is about 0.018, still a fail. Four hundred
/// stub writes and execs spread over four threads costs a couple of seconds, which is the budget.
const WORKER_THREADS: usize = 4;
const ITERATIONS_PER_WORKER: usize = 100;
const NOISE_THREADS: usize = 4;

/// A trivial child whose only job is to fork. It is the fork, not the child, that inherits the
/// descriptor and blocks the exec.
const NOISE_BINARY: &str = "/bin/true";

/// Assert an executed stub exited 0, with a message naming the observed status.
fn assert_stub_exited_0(output: &Output) {
    assert!(
        output.status.success(),
        "the stub ran but did not exit 0: {:?}",
        output.status
    );
}

#[test]
fn a_written_stub_is_executable_by_the_time_the_fixture_returns_it() {
    let root_dir = tempfile::tempdir().expect("create the test root");
    let root: PathBuf = root_dir.path().to_path_buf();

    let etxtbsy = Arc::new(AtomicUsize::new(0));
    let stop_noise = Arc::new(AtomicBool::new(false));

    let noise: Vec<_> = (0..NOISE_THREADS)
        .map(|_| {
            let stop_noise = Arc::clone(&stop_noise);
            thread::spawn(move || {
                while !stop_noise.load(Ordering::Relaxed) {
                    let mut child = Command::new(NOISE_BINARY)
                        .spawn()
                        .expect("spawn a forking noise child");
                    child.wait().expect("reap a noise child");
                }
            })
        })
        .collect();

    let workers: Vec<_> = (0..WORKER_THREADS)
        .map(|worker| {
            let root: PathBuf = root.clone();
            let etxtbsy = Arc::clone(&etxtbsy);
            thread::spawn(move || {
                for iteration in 0..ITERATIONS_PER_WORKER {
                    let directory = root.join(format!("worker-{worker}-iteration-{iteration}"));
                    fs::create_dir_all(&directory).expect("create a stub directory");
                    let stub = directory.join("stub");
                    common::write_stub(&stub, "exit 0\n");
                    match Command::new(&stub).output() {
                        Ok(output) => assert_stub_exited_0(&output),
                        Err(error) if error.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                            etxtbsy.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => panic!(
                            "the stub at {} failed to exec for a reason other than ETXTBSY: {error}",
                            stub.display()
                        ),
                    }
                }
            })
        })
        .collect();

    let outcomes: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
    stop_noise.store(true, Ordering::Relaxed);
    for thread in noise {
        let _ = thread.join();
    }

    for outcome in outcomes {
        outcome.expect("a stub worker thread panicked");
    }

    let observed = etxtbsy.load(Ordering::Relaxed);
    let attempts = WORKER_THREADS * ITERATIONS_PER_WORKER;
    assert_eq!(
        observed, 0,
        "{observed} of {attempts} freshly written stubs could not be executed: a concurrently \
         forked child inherited the fixture's still-open write descriptor on the stub's inode and \
         Linux refused to exec a file held open for writing (ETXTBSY). The fixture returned a path \
         that was not yet executable."
    );
}

/// The probe execs the stub, so the guard is what keeps that exec out of the body. Several stubs
/// append their argv to a log on every invocation and other tests assert that log stayed empty, so
/// a probe reaching a body would invert those assertions in a different crate.
///
/// The body here creates a marker file, which stands in for those log appends. The assertion is on
/// the marker, never on the sentinel's spelling, so renaming `PROBE_ARG` leaves it passing; delete
/// the guard from the assembled script and it fails.
#[test]
fn the_fixture_probe_runs_none_of_the_stub_body() {
    let root_dir = tempfile::tempdir().expect("create the test root");
    let root = root_dir.path();
    let stub = root.join("stub");
    let marker = root.join("marker");

    common::write_stub(&stub, &format!("touch '{}'\n", marker.display()));

    let probed = marker.exists();

    // Run the stub for real, so a body that could never have created the marker (a quoting
    // mistake, an unwritable path) cannot pass this test vacuously.
    let output = Command::new(&stub).output().expect("exec the stub");
    assert_stub_exited_0(&output);
    let ran = marker.exists();

    assert!(
        !probed,
        "the fixture probe executed the stub body: {} existed as soon as write_stub returned, so \
         a stub that logs its argv would have logged the probe and inverted the tests that read \
         that log",
        marker.display()
    );
    assert!(
        ran,
        "the stub body never created {} even when invoked for real, so the absence of that marker \
         after write_stub proves nothing about the probe",
        marker.display()
    );
}
