use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agent_router_parity_cli_{}_{}_{}",
            std::process::id(),
            serial,
            label
        ));
        fs::create_dir_all(&path).expect("create isolated test directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn router_command(home: &Path, current_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-router"));
    command
        .current_dir(current_dir)
        .env("HOME", home)
        .env("GROK_HOME", home.join("grok-home"))
        .env("GROK_USAGE_CACHE", home.join("grok-usage-cache.json"))
        .env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

fn diagnostic(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit status\n{}",
        diagnostic(output)
    );
}

#[test]
fn invalid_run_provider_preserves_the_existing_exit_one_contract() {
    let tree = TempTree::new("invalid_run");
    let home = tree.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let output = router_command(&home, tree.path())
        .args([
            "run",
            "do not dispatch",
            "--provider",
            "not-a-provider",
            "--dry-run",
        ])
        .output()
        .expect("run agent-router");

    assert_exit(&output, 1);
    assert!(
        diagnostic(&output).contains("unknown provider"),
        "{}",
        diagnostic(&output)
    );
}

#[test]
fn parity_is_an_unknown_subcommand() {
    let tree = TempTree::new("unknown_parity");
    let home = tree.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let output = router_command(&home, tree.path())
        .arg("parity")
        .output()
        .expect("run agent-router parity");

    assert_exit(&output, 2);
    let error = diagnostic(&output);
    assert!(
        error.contains("unrecognized subcommand") && error.contains("parity"),
        "expected clap unknown-subcommand error for parity\n{error}"
    );
}
