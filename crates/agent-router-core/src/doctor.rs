//! Preflight checks over the environment the router routes from: the provider binaries, the
//! credentials the usage readers authenticate with, the provenance of the usage numbers
//! themselves, the config file, and the decision log.
//!
//! One severity rule decides every check. `Fail` means the router would keep running on inputs it
//! cannot trust, or could not run at all. `Warn` means a degraded path that fails loudly at the
//! moment it is used, so nothing silently routes on a wrong number because of it.

use crate::binary::{self, Environment};
use crate::config::{Config, default_config_path};
use crate::error::Error;
use crate::log::DecisionLog;
use crate::provider::Provider;
use crate::runtime::home_dir;
use crate::usage::UsageSnapshot;
use agent_viewer_core::GrokLifecycle;
use std::path::PathBuf;

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
/// The usage checks read one snapshot, so doctor asks each provider once rather than once per
/// check, and the lines it prints cannot disagree about the same read.
pub fn run() -> Report {
    let environment = Environment::from_process();
    let (usage, grok_source) = UsageSnapshot::read_with_grok_source();
    let claude_installed = on_path("claude", &environment).is_some();
    let codex_installed = on_path("codex", &environment).is_some();
    let grok_installed = on_path("grok", &environment).is_some();
    let mut checks = vec![
        required_binary_in(&environment, "claude_on_path", Provider::Claude),
        claude_credentials(),
        usage_source("claude_usage", usage.claude.stale, claude_installed),
        optional_binary_in(&environment, "codex_on_path", Provider::Codex),
        codex_app_server(),
        usage_source("codex_rate_limits", usage.codex.stale, codex_installed),
        grok_usage_source(grok_source, usage.grok, grok_installed),
    ];
    checks.extend(grok_checks(&environment));
    checks.extend([config_parses(), log_writable()]);
    Report { checks }
}

/// Observe both Grok lifecycle prerequisites without starting a leader or creating configuration.
/// Grok is explicit only, so an unavailable path warns rather than failing the whole router.
fn grok_checks(environment: &Environment) -> [Check; 2] {
    // Resolve before constructing the lifecycle, so this observes the same binary a dispatch would
    // run rather than whatever `execvp` finds on doctor's own PATH. An unresolvable grok reuses the
    // diagnostics-unavailable Warn shape below: Grok is explicit-only, so an unavailable path warns
    // rather than failing the whole router, and `Error::Launch` already names AGENT_ROUTER_GROK_BIN.
    let binary = match binary::resolve(Provider::Grok, environment) {
        Ok(binary) => binary,
        Err(error) => return grok_unavailable(&one_line(&error)),
    };
    let lifecycle = GrokLifecycle::new(binary, grok_home());
    match lifecycle.diagnostics() {
        Ok(diagnostics) => {
            let binary = if diagnostics.binary_available {
                pass(
                    "grok_binary",
                    format!("grok is available through {}", diagnostics.binary.display()),
                )
            } else {
                warn(
                    "grok_binary",
                    format!(
                        "no executable {} is available, so explicit Grok dispatch will error",
                        diagnostics.binary.display()
                    ),
                )
            };
            let leader = if diagnostics.registered {
                pass(
                    "grok_leader_registration",
                    format!(
                        "an authoritative leader is registered among {} candidate(s)",
                        diagnostics.leader_count
                    ),
                )
            } else {
                warn(
                    "grok_leader_registration",
                    format!(
                        "no authoritative leader is registered among {} candidate(s), and doctor does not start one",
                        diagnostics.leader_count
                    ),
                )
            };
            [binary, leader]
        }
        Err(error) => grok_unavailable(&one_line(&error)),
    }
}

/// The two Warn lines a Grok path that could not even be observed produces. Shared so an
/// unresolvable binary and unavailable diagnostics report identically: in both cases doctor knows
/// only that the path is unusable and why, and neither is a reason to fail the whole preflight.
fn grok_unavailable(detail: &str) -> [Check; 2] {
    [
        warn(
            "grok_binary",
            format!("Grok lifecycle diagnostics were unavailable: {detail}"),
        ),
        warn(
            "grok_leader_registration",
            format!(
                "Grok lifecycle diagnostics were unavailable and doctor did not start a leader: {detail}"
            ),
        ),
    ]
}

fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".grok"))
}

/// A binary the router cannot work without. The classifier runs on every auto route, and a
/// classifier that cannot start falls back to the configured default provider without saying so.
///
/// The check reports the PATH *fact* and takes its *consequence* from the resolver. Fail is
/// doctor's process exit code, so failing on a binary reachable through `$HOME/.local/bin` would
/// exit 1 on precisely the machines the resolver teaches the router to handle — a preflight raising
/// a false alarm on the fixed configuration is a worse defect than the one that was fixed.
pub fn required_binary_in(
    environment: &Environment,
    name: &'static str,
    provider: Provider,
) -> Check {
    binary_check(environment, name, provider, Health::Fail)
}

/// A binary whose absence degrades one dispatch path rather than the router. Every dispatch to
/// that provider then errors at the moment it is attempted, which is loud enough to be a warning.
pub fn optional_binary_in(
    environment: &Environment,
    name: &'static str,
    provider: Provider,
) -> Check {
    binary_check(environment, name, provider, Health::Warn)
}

/// The two questions behind every `*_on_path` check, asked in order.
///
/// `search_path` answers whether it is on PATH, which is what the check is named after and what its
/// message asserts. `resolve` answers whether a dispatch will actually find it, which is what
/// decides the severity. Conflating them either makes the check name lie (if the PATH answer
/// widened) or makes the consequence lie (if the severity ignored the fallback).
fn binary_check(
    environment: &Environment,
    name: &'static str,
    provider: Provider,
    absent: Health,
) -> Check {
    let program = provider.name();
    if let Some(path) = on_path(program, environment) {
        return pass(name, format!("{program} at {}", path.display()));
    }
    match binary::resolve(provider, environment) {
        Ok(resolved) => warn(
            name,
            format!(
                "no executable {program} on PATH, but dispatch will find it at {}; pin it with {}",
                resolved.display(),
                binary::override_env(provider)
            ),
        ),
        Err(_) => {
            let detail = match absent {
                Health::Fail => format!("no executable {program} on PATH"),
                _ => format!("no executable {program} on PATH, so any dispatch to it will error"),
            };
            Check {
                name,
                health: absent,
                detail,
            }
        }
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
///
/// It is only a failure when the provider is installed. With no binary on PATH there is nothing to
/// sign in to and nothing that read could have said, and the binary check one line earlier already
/// reports its absence as a warning, so failing here would make a box that does not use the
/// provider exit nonzero forever over a provider it never routes to.
fn usage_source(name: &'static str, stale: bool, installed: bool) -> Check {
    if stale {
        let detail = "fail-open, so the provider reads as completely unused whatever it has spent"
            .to_string();
        if installed {
            fail(name, detail)
        } else {
            warn(
                name,
                format!("{detail}, and no binary on PATH to read a real number from"),
            )
        }
    } else {
        pass(
            name,
            "live, read from the provider's own source".to_string(),
        )
    }
}

/// Grok capacity has four useful sources rather than the live/fail-open distinction the other
/// providers use. A missing source fails closed for routing, so doctor must surface it as a
/// failure when Grok is installed instead of reporting a misleading exhausted percentage.
fn grok_usage_source(
    source: crate::usage::GrokUsageSource,
    headroom: crate::usage::Headroom,
    installed: bool,
) -> Check {
    let name = "grok_usage";
    let detail = match source {
        crate::usage::GrokUsageSource::Live => "live, read from Grok billing",
        crate::usage::GrokUsageSource::Cache => "cache, read from Grok billing cache",
        crate::usage::GrokUsageSource::Log => "log, read from Grok CLI billing log",
        crate::usage::GrokUsageSource::None => "none, no billing data available",
    };
    if headroom.weekly_known() {
        return pass(name, detail.to_string());
    }

    let detail = if source == crate::usage::GrokUsageSource::None {
        detail.to_string()
    } else {
        format!("{detail}, but no usable weekly capacity was present")
    };
    if installed {
        fail(name, detail)
    } else {
        warn(
            name,
            format!("{detail}, and no binary on PATH to read a real number from"),
        )
    }
}

/// Every Codex dispatch goes through the app-server daemon, so this reports whether one answers.
/// It only observes: dispatch starts a daemon when none is running, and a diagnostic command must
/// not create the state it is diagnosing, so this asks and reports the answer. A daemon that does
/// not answer errors at dispatch time, which is why an absent one is a warning.
fn codex_app_server() -> Check {
    let name = "codex_app_server";
    match crate::dispatch::codex::probe_daemon() {
        Some(_) => pass(name, "the app-server daemon answers".to_string()),
        None => warn(
            name,
            "no app-server daemon answers, and doctor does not start one; the next Codex \
             dispatch will start it"
                .to_string(),
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
        Err(error) => match write_probe_health(&error) {
            Health::Warn => warn(
                name,
                format!(
                    "{} is held by another writer, so the probe timed out rather than proving \
                     anything: {}",
                    path.display(),
                    one_line(&error)
                ),
            ),
            _ => fail(
                name,
                format!(
                    "{} opens but cannot be written: {}",
                    path.display(),
                    one_line(&error)
                ),
            ),
        },
    }
}

/// PURE: how a failed write probe is reported.
///
/// A busy database is contention, not unwritability. Several agents write this log at once, the
/// probe takes the same RESERVED lock a dispatch takes, and the connection's busy timeout is short
/// enough that a concurrent writer holding the lock past it returns `SQLITE_BUSY`. The next
/// dispatch takes that lock on its own and nothing has been lost, so the severity rule's Fail (the
/// router could not run at all) does not apply. Every other error is a genuine readonly or
/// permission fault, which it does apply to.
pub fn write_probe_health(error: &Error) -> Health {
    match error {
        Error::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DatabaseBusy =>
        {
            Health::Warn
        }
        _ => Health::Fail,
    }
}

/// IMPURE: the first executable named `binary` on `$PATH`. The walk now lives in `binary.rs`, but
/// it is still hand-rolled rather than shelled out to `which`, which would make doctor depend on a
/// binary it does not check for.
///
/// This stays PATH-only. Doctor's checks are *named* `*_on_path` and its messages say `on PATH`, so
/// answering the wider resolver question here would report a binary found only in
/// `$HOME/.local/bin` as being on PATH. The fallback changes the *severity*, in `binary_check`, not
/// the fact.
fn on_path(binary: &str, environment: &Environment) -> Option<PathBuf> {
    binary::search_path(binary, environment)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{GrokUsageSource, Headroom};

    #[test]
    fn grok_usage_check_preserves_live_cache_log_and_none_provenance() {
        let known_capacity = Headroom {
            weekly_capacity_known: true,
            ..Headroom::closed()
        };
        for (source, expected) in [
            (GrokUsageSource::Live, "live"),
            (GrokUsageSource::Cache, "cache"),
            (GrokUsageSource::Log, "log"),
        ] {
            let check = grok_usage_source(source, known_capacity, true);
            assert_eq!(check.name, "grok_usage");
            assert_eq!(check.health, Health::Pass, "{expected} is usable capacity");
            assert!(
                check.detail.contains(expected),
                "the check must say {expected}, got: {}",
                check.detail
            );
        }

        let installed_none = grok_usage_source(GrokUsageSource::None, Headroom::closed(), true);
        assert_eq!(installed_none.name, "grok_usage");
        assert_eq!(installed_none.health, Health::Fail);
        assert!(installed_none.detail.contains("none"));

        let absent_none = grok_usage_source(GrokUsageSource::None, Headroom::closed(), false);
        assert_eq!(absent_none.name, "grok_usage");
        assert_eq!(absent_none.health, Health::Warn);
        assert!(absent_none.detail.contains("none"));
    }

    #[test]
    fn grok_log_without_weekly_capacity_is_unhealthy_but_keeps_its_provenance() {
        for (installed, expected_health) in [(true, Health::Fail), (false, Health::Warn)] {
            let check = grok_usage_source(GrokUsageSource::Log, Headroom::closed(), installed);

            assert_eq!(check.name, "grok_usage");
            assert_eq!(check.health, expected_health);
            assert!(
                check.detail.contains("log"),
                "the check must retain its provenance, got: {}",
                check.detail
            );
        }
    }
}
