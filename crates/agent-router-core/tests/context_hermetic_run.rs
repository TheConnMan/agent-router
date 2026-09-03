//! `run()` must honour a constructed Context and ignore the process HOME/PATH.
//!
//! The Context is built from a temp home and stub `AGENT_ROUTER_CLAUDE_BIN` before the process
//! environment is poisoned. If `run` still read process state, the stub would not resolve and the
//! decision log would not land under the temp home.

#![cfg(unix)]

mod common;

use agent_router_core::binary::{CLAUDE_BIN_ENV, Environment};
use agent_router_core::config::Config;
use agent_router_core::run::{Request, run};
use agent_router_core::{Context, Provider};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENVIRONMENT: Mutex<()> = Mutex::new(());

struct EnvGuard {
    home: Option<OsString>,
    path: Option<OsString>,
}

impl EnvGuard {
    fn poison() -> Self {
        let home = std::env::var_os("HOME");
        let path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("HOME", "/no-such-home-agent-router-context-test");
            std::env::set_var("PATH", "/no-such-path-agent-router-context-test");
        }
        Self { home, path }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.path.take() {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

fn fake_claude(root: &Path) -> PathBuf {
    let binary = root.join("claude");
    let listing = json!([
        {
            "id": "hermetic",
            "cwd": root.join("work"),
            "name": "Hermetic Context Run",
            "startedAt": i64::MAX,
            "kind": "background",
            "state": "working"
        }
    ]);
    let listing = serde_json::to_string(&listing).expect("listing json");
    let quoted = format!("'{}'", listing.replace('\'', "'\"'\"'"));
    common::write_stub(
        &binary,
        &format!(
            "if [ \"$1\" = \"agents\" ]; then\n  printf '%s\\n' {quoted}\n  exit 0\nfi\nexit 0\n"
        ),
    );
    binary
}

fn wait_for_db(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if path.is_file() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the decision log was not written at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn run_uses_a_constructed_context_and_ignores_poisoned_process_home_and_path() {
    let _lock = ENVIRONMENT.lock().expect("environment lock");
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    let work = root.path().join("work");
    fs::create_dir_all(&home).expect("create HOME");
    fs::create_dir_all(&work).expect("create workdir");
    let binary = fake_claude(root.path());
    let environment = Environment::new(
        None,
        Some(home.clone()),
        BTreeMap::from([(CLAUDE_BIN_ENV.to_string(), OsString::from(&binary))]),
    );
    let ctx = Context::new(environment, home.clone(), Config::default())
        .with_claude_usage_cache(root.path().join("claude-usage.json"))
        .with_grok_usage_cache(root.path().join("grok-usage.json"))
        .with_codex_sessions_dir(root.path().join("codex-sessions"));
    let db_path = ctx.db_path();
    let request = Request {
        task: "exercise the hermetic context run",
        dir: &work,
        provider: Some(Provider::Claude),
        model: Some("opus[1m]".to_string()),
        effort: Some("high".to_string()),
        name: Some("Hermetic Context Run".to_string()),
        dry_run: false,
        mcp_configs: &[],
        strict_mcp_config: false,
    };

    let _poison = EnvGuard::poison();
    let outcome = run(&request, &ctx).expect("run against a constructed Context");

    assert_eq!(
        outcome
            .dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.job_id.as_deref()),
        Some("hermetic"),
        "dispatch must have used the stub binary from Context, not process PATH"
    );
    wait_for_db(&db_path);
    assert!(
        db_path.starts_with(&home),
        "the decision log must be under the Context home {}, not the poisoned HOME: {}",
        home.display(),
        db_path.display()
    );
    assert!(
        !std::path::Path::new("/no-such-home-agent-router-context-test").exists(),
        "poisoned HOME must not have been created as a real directory"
    );
}
