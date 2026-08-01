//! Preflight checks over the environment the router routes from: the provider binaries, the
//! credentials the usage readers authenticate with, the provenance of the usage numbers
//! themselves, the config file, and the decision log.
//!
//! One severity rule decides every check. `Fail` means the router would keep running on inputs it
//! cannot trust, or could not run at all. `Warn` means a degraded path that fails loudly at the
//! moment it is used, so nothing silently routes on a wrong number because of it.

use crate::config::{Config, default_config_path};
use crate::log::DecisionLog;
use crate::runtime::home_dir;
use crate::usage::UsageSnapshot;
use std::path::{Path, PathBuf};

/// How one check landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Pass,
    Warn,
    Fail,
}

/// One check and what it found. `detail` is a single line, so a report is one line per check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub health: Health,
    pub detail: String,
}

/// Every check, in the order they were run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// PURE: whether any check failed, which is the whole of the command's exit code contract.
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|check| check.health == Health::Fail)
    }
}

/// IMPURE: run every preflight check.
///
/// Both usage checks read one snapshot, so doctor asks each provider once rather than once per
/// check, and the two lines it prints cannot disagree about the same read.
pub fn run() -> Report {
    let usage = UsageSnapshot::read();
    Report {
        checks: vec![
            required_binary("claude_on_path", "claude"),
            claude_credentials(),
            usage_source("claude_usage", usage.claude.stale),
            optional_binary("codex_on_path", "codex"),
            codex_app_server(),
            usage_source("codex_rate_limits", usage.codex.stale),
            optional_binary("opencode_on_path", "opencode"),
            config_parses(),
            log_writable(),
        ],
    }
}

/// A binary the router cannot work without. The classifier runs on every auto route, and a
/// classifier that cannot start falls back to the configured default provider without saying so.
fn required_binary(name: &'static str, binary: &str) -> Check {
    match on_path(binary) {
        Some(path) => pass(name, format!("{binary} at {}", path.display())),
        None => fail(name, format!("no executable {binary} on PATH")),
    }
}

/// A binary whose absence degrades one dispatch path rather than the router. Every dispatch to
/// that provider then errors at the moment it is attempted, which is loud enough to be a warning.
fn optional_binary(name: &'static str, binary: &str) -> Check {
    match on_path(binary) {
        Some(path) => pass(name, format!("{binary} at {}", path.display())),
        None => warn(
            name,
            format!("no executable {binary} on PATH, so any dispatch to it will error"),
        ),
    }
}

/// The token the Claude usage reader authenticates with. Without it the reader has nothing to
/// call the usage endpoint with, so it fails open and Claude reads as completely unused.
fn claude_credentials() -> Check {
    let path = home_dir().join(".claude/.credentials.json");
    let name = "claude_credentials";
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return fail(
                name,
                format!("{} is unreadable: {}", path.display(), one_line(&error)),
            );
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return fail(
                name,
                format!("{} does not parse: {}", path.display(), one_line(&error)),
            );
        }
    };
    match value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|token| token.as_str())
    {
        Some(token) if !token.is_empty() => {
            pass(name, format!("{} carries an OAuth token", path.display()))
        }
        _ => fail(
            name,
            format!("{} has no /claudeAiOauth/accessToken", path.display()),
        ),
    }
}

/// The live versus fail open distinction, said out loud.
///
/// A fail open read reports the same two zeroes as a provider that has genuinely consumed nothing,
/// and those zeroes win every headroom tiebreak, so the router keeps routing on a number nobody
/// can trust. That is the failure this check exists to name.
fn usage_source(name: &'static str, stale: bool) -> Check {
    if stale {
        fail(
            name,
            "fail-open, so the provider reads as completely unused whatever it has spent"
                .to_string(),
        )
    } else {
        pass(
            name,
            "live, read from the provider's own source".to_string(),
        )
    }
}

/// Every Codex dispatch goes through the app-server daemon, so this takes the same path a dispatch
/// takes, minus the thread it would start. A daemon that does not answer errors at dispatch time.
fn codex_app_server() -> Check {
    let name = "codex_app_server";
    match crate::dispatch::codex::app_server_status() {
        Ok(()) => pass(name, "the app-server daemon answers".to_string()),
        Err(error) => warn(
            name,
            format!(
                "the app-server daemon does not answer: {}",
                one_line(&error)
            ),
        ),
    }
}

/// Read and parse the file directly rather than through `Config::load()`, which writes a default
/// file when one is absent: a diagnostic command must not create the state it is diagnosing. An
/// absent file is a pass, because the router runs on the same defaults `load()` would have written.
fn config_parses() -> Check {
    let name = "config_parses";
    let path = default_config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return pass(name, format!("absent, defaults apply ({})", path.display()));
        }
        Err(error) => {
            return fail(
                name,
                format!("{} is unreadable: {}", path.display(), one_line(&error)),
            );
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(_) => pass(name, format!("{} parses", path.display())),
        Err(error) => fail(
            name,
            format!("{} does not parse: {}", path.display(), one_line(&error)),
        ),
    }
}

/// Opening the log is not evidence it can be written, so this probes an actual write. A log that
/// cannot take a row loses the record of every decision made while it stays that way.
fn log_writable() -> Check {
    let name = "log_writable";
    let path = crate::log::default_db_path();
    let log = match DecisionLog::open() {
        Ok(log) => log,
        Err(error) => {
            return fail(
                name,
                format!("{} cannot be opened: {}", path.display(), one_line(&error)),
            );
        }
    };
    match log.probe_writable() {
        Ok(()) => pass(name, format!("{} takes a write", path.display())),
        Err(error) => fail(
            name,
            format!(
                "{} opens but cannot be written: {}",
                path.display(),
                one_line(&error)
            ),
        ),
    }
}

/// IMPURE: the first executable named `binary` on `$PATH`. Walked here rather than shelled out to
/// `which`, which would make doctor depend on a binary it does not check for.
fn on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(binary))
        .find(|candidate| is_executable(candidate))
}

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

/// PURE: an error rendered onto one line, because a report is one line per check and a TOML parse
/// error spans several.
fn one_line(error: &impl std::fmt::Display) -> String {
    error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn pass(name: &'static str, detail: String) -> Check {
    Check {
        name,
        health: Health::Pass,
        detail,
    }
}

fn warn(name: &'static str, detail: String) -> Check {
    Check {
        name,
        health: Health::Warn,
        detail,
    }
}

fn fail(name: &'static str, detail: String) -> Check {
    Check {
        name,
        health: Health::Fail,
        detail,
    }
}
