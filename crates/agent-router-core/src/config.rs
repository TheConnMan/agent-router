//! `~/.config/agent-router/config.toml`: the routing ceilings and the connector inventory the
//! classifier scores gate 5 against. Written with defaults on first run.

use crate::error::Result;
use crate::runtime::home_dir;
use std::path::{Path, PathBuf};

/// Weekly percent at which a provider counts as exhausted.
const DEFAULT_HARD_CEILING_PCT: f64 = 97.0;
/// Weekly-headroom gap (in points) that flips a borderline classification.
const DEFAULT_HEADROOM_FLIP_GAP: f64 = 25.0;
/// Ceiling on the classifier call.
const DEFAULT_CLASSIFIER_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultProvider {
    Codex,
    Claude,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Policy {
    pub default_provider: DefaultProvider,
    pub weekly_routing: bool,
    pub usage_failover_changes_model: bool,
    pub usage_failover_changes_effort: bool,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            default_provider: DefaultProvider::Codex,
            weekly_routing: true,
            usage_failover_changes_model: false,
            usage_failover_changes_effort: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ParityConfig {
    pub roots: Vec<PathBuf>,
    pub exceptions: Vec<ParityException>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ParityExceptionDocument")]
pub struct ParityException {
    pub path: PathBuf,
    pub reason: String,
    pub server: Option<String>,
    pub kind: Option<ParityKind>,
}

#[derive(serde::Deserialize)]
struct ParityExceptionDocument {
    path: PathBuf,
    reason: String,
    server: Option<String>,
    kind: Option<ParityKind>,
}

impl TryFrom<ParityExceptionDocument> for ParityException {
    type Error = String;

    fn try_from(document: ParityExceptionDocument) -> std::result::Result<Self, Self::Error> {
        if document.path.as_os_str().is_empty() {
            return Err("parity exception path must not be empty".to_string());
        }
        if document.reason.trim().is_empty() {
            return Err("parity exception reason must not be blank".to_string());
        }
        Ok(ParityException {
            path: document.path,
            reason: document.reason,
            server: document.server,
            kind: document.kind,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParityKind {
    MissingInCodex,
    MissingInClaude,
    CommandDiffers,
    ArgsDiffer,
    EnvKeysDiffer,
    StandaloneClaudeMd,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Weekly percent used at or above which a provider is treated as exhausted.
    pub hard_ceiling_pct: f64,
    /// How many points of weekly-headroom advantage flip a borderline verdict.
    pub headroom_flip_gap: f64,
    /// How long the classifier call may take before it counts as failed.
    pub classifier_timeout_secs: u64,
    /// What Codex can actually reach on this box. Human-maintained: gate 5 of the rubric
    /// ("Codex has every required connector") is scored against exactly this list, so anything
    /// absent here is what forces a task to Claude.
    pub connectors: Vec<String>,
    pub policy: Policy,
    pub parity: ParityConfig,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            hard_ceiling_pct: DEFAULT_HARD_CEILING_PCT,
            headroom_flip_gap: DEFAULT_HEADROOM_FLIP_GAP,
            classifier_timeout_secs: DEFAULT_CLASSIFIER_TIMEOUT_SECS,
            connectors: vec![
                "local shell".to_string(),
                "git".to_string(),
                "gh (github)".to_string(),
                "airtable".to_string(),
            ],
            policy: Policy::default(),
            parity: ParityConfig::default(),
        }
    }
}

impl Config {
    /// IMPURE: the config at the default path, created with defaults when absent.
    pub fn load() -> Result<Config> {
        Config::load_from(&default_config_path())
    }

    /// IMPURE: the config at `path`, created with defaults when absent. A file that exists but
    /// does not parse is an Err: silently substituting defaults would route jobs against
    /// ceilings and a connector list the operator never wrote.
    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.write_to(path)?;
                Ok(config)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| crate::error::Error::Command(format!("serializing config: {e}")))?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

pub fn default_config_path() -> PathBuf {
    home_dir().join(".config/agent-router/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_created_with_defaults_and_reads_back_identically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/config.toml");
        let created = Config::load_from(&path).expect("creates defaults");
        assert_eq!(created, Config::default());
        assert!(path.exists(), "the default config must be written to disk");
        assert_eq!(Config::load_from(&path).expect("re-reads"), created);
    }

    #[test]
    fn a_partial_config_keeps_defaults_for_the_keys_it_omits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "hard_ceiling_pct = 90.0\n").expect("write");
        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.hard_ceiling_pct, 90.0);
        assert_eq!(
            config.headroom_flip_gap,
            Config::default().headroom_flip_gap
        );
        assert_eq!(config.connectors, Config::default().connectors);
    }

    #[test]
    fn a_malformed_config_is_an_error_rather_than_a_silent_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "hard_ceiling_pct = \"ninety\"\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }
}
