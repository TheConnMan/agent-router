//! Process-facing inputs, constructed once and passed down.
//!
//! Impure seams used to re-read `Environment::from_process`, `home_dir`, `now_epoch`, and
//! cache-path environment variables at every call. This type is that read, done once.

use crate::binary::Environment;
use crate::config::Config;
use crate::error::Result;
use crate::runtime;
use crate::usage::{
    CLAUDE_USAGE_CACHE_ENV, GROK_USAGE_CACHE_ENV, claude_usage_cache_from, grok_usage_cache_from,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The environment, home, config, clock, and resolved cache paths one router invocation uses.
#[derive(Debug, Clone)]
pub struct Context {
    pub environment: Environment,
    pub home: PathBuf,
    pub config: Config,
    pub now_epoch: fn() -> i64,
    pub claude_usage_cache: PathBuf,
    pub grok_usage_cache: PathBuf,
    pub codex_sessions_dir: PathBuf,
    grok_home: PathBuf,
}

impl Context {
    /// IMPURE: the real process environment, home, cache paths, and clock.
    ///
    /// The only production constructor, and the only production caller of
    /// [`Environment::from_process`] and [`runtime::home_dir`]. Does not load or write
    /// `config.toml`: [`Config::load_from`] creates a default file when absent, and doctor’s
    /// `config_parses` check treats absence as a pass. Commands that today load config call
    /// [`Self::load_config`] afterwards.
    pub fn from_process() -> Self {
        let home = runtime::home_dir();
        let environment = Environment::from_process();
        let grok_home = resolve_grok_home(&home, std::env::var_os("GROK_HOME"));
        Self {
            environment,
            claude_usage_cache: claude_usage_cache_from(
                std::env::var_os(CLAUDE_USAGE_CACHE_ENV).as_deref(),
            ),
            grok_usage_cache: grok_usage_cache_from(
                std::env::var_os(GROK_USAGE_CACHE_ENV).as_deref(),
            ),
            codex_sessions_dir: codex_sessions_dir_from(&home),
            home,
            config: Config::default(),
            now_epoch: runtime::now_epoch,
            grok_home,
        }
    }

    /// PURE: a context built entirely from data. Cache paths start at the shared defaults;
    /// sessions and grok home are derived from `home`. Tests that need overrides use the
    /// `with_*` builders.
    pub fn new(environment: Environment, home: PathBuf, config: Config) -> Self {
        let grok_home = home.join(".grok");
        let codex_sessions_dir = home.join(".codex").join("sessions");
        Self {
            environment,
            claude_usage_cache: claude_usage_cache_from(None),
            grok_usage_cache: grok_usage_cache_from(None),
            codex_sessions_dir,
            grok_home,
            home,
            config,
            now_epoch: runtime::now_epoch,
        }
    }

    #[must_use]
    pub fn with_clock(mut self, now_epoch: fn() -> i64) -> Self {
        self.now_epoch = now_epoch;
        self
    }

    #[must_use]
    pub fn with_claude_usage_cache(mut self, path: PathBuf) -> Self {
        self.claude_usage_cache = path;
        self
    }

    #[must_use]
    pub fn with_grok_usage_cache(mut self, path: PathBuf) -> Self {
        self.grok_usage_cache = path;
        self
    }

    #[must_use]
    pub fn with_codex_sessions_dir(mut self, path: PathBuf) -> Self {
        self.codex_sessions_dir = path;
        self
    }

    #[must_use]
    pub fn with_grok_home(mut self, path: PathBuf) -> Self {
        self.grok_home = path;
        self
    }

    /// IMPURE: load (and possibly create) config from this context’s home.
    pub fn load_config(&mut self) -> Result<()> {
        self.config = Config::load_in(&self.home)?;
        Ok(())
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join(".config/agent-router/config.toml")
    }

    pub fn db_path(&self) -> PathBuf {
        self.home.join(".local/state/agent-router/router.db")
    }

    pub fn claude_projects(&self) -> PathBuf {
        self.home.join(".claude/projects")
    }

    pub fn claude_credentials(&self) -> PathBuf {
        self.home.join(".claude/.credentials.json")
    }

    /// The Grok state directory: `$GROK_HOME` when set, otherwise `{home}/.grok`.
    pub fn grok_home(&self) -> &Path {
        &self.grok_home
    }
}

fn resolve_grok_home(home: &Path, grok_home: Option<OsString>) -> PathBuf {
    grok_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".grok"))
}

/// `$CODEX_SESSIONS_DIR`, else `{CODEX_HOME}/sessions`, else `{home}/.codex/sessions`.
///
/// `CODEX_HOME` is the sessions parent, not `{CODEX_HOME}/.codex`. Empty values are kept, matching
/// the previous `var_os` + `PathBuf::from` behaviour.
fn codex_sessions_dir_from(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("CODEX_SESSIONS_DIR") {
        return PathBuf::from(dir);
    }
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    codex_home.join("sessions")
}
