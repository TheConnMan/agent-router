//! `~/.config/agent-router/config.toml`: the routing ceilings and the connector inventory the
//! classifier scores gate 5 against. Written with defaults on first run.

use crate::classify::Complexity;
use crate::error::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Weekly percent at which a provider counts as exhausted: within 2 points of the weekly limit is
/// close enough that the provider is no longer a routing destination.
const DEFAULT_HARD_CEILING_PCT: f64 = 98.0;
/// Ceiling on the classifier call. Headroom over the fast-path worst case rather than a target;
/// 30s lost the slow tail. See docs/decisions/0001-classifier-hermeticity.md.
const DEFAULT_CLASSIFIER_TIMEOUT_SECS: u64 = 60;

/// The migration level a config file written by this build carries. A file stamped below this is
/// rewritten once so dead keys drop off disk; serde already ignores them on parse.
const CURRENT_CONFIG_VERSION: u32 = 5;

/// The level a file that predates versioning reads as. This is deliberately NOT
/// `CURRENT_CONFIG_VERSION`: an absent key has to be distinguishable from a stamped one, or every
/// file would look already-migrated and no migration could ever run.
fn pre_versioning() -> u32 {
    0
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Policy {
    pub weekly_routing: bool,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            weekly_routing: true,
        }
    }
}

/// Which CLI runs the classifier call. Scoring is one small strict-JSON answer, so either engine
/// can do it; the choice is about which weekly budget the per-task call is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierEngine {
    Claude,
    Codex,
}

impl ClassifierEngine {
    pub const fn name(self) -> &'static str {
        match self {
            ClassifierEngine::Claude => "claude",
            ClassifierEngine::Codex => "codex",
        }
    }
}

/// The classifier call: which engine scores a task, and the model each engine scores it with.
/// Both models are kept regardless of the engine in force, so flipping `engine` is a one-word
/// edit rather than a re-pick of the model.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Classifier {
    pub engine: ClassifierEngine,
    /// The model used when `engine = "claude"`. Wants the cheapest model that holds the rubric.
    pub claude_model: String,
    /// The model used when `engine = "codex"`. Same intent, one tier down the codex catalogue.
    pub codex_model: String,
}

impl Default for Classifier {
    fn default() -> Classifier {
        Classifier {
            engine: ClassifierEngine::Codex,
            claude_model: "haiku".to_string(),
            codex_model: "gpt-5.6-luna".to_string(),
        }
    }
}

impl Classifier {
    /// PURE: the model the configured engine scores with.
    pub fn model(&self) -> &str {
        match self.engine {
            ClassifierEngine::Claude => &self.claude_model,
            ClassifierEngine::Codex => &self.codex_model,
        }
    }
}

/// The per-provider model tiers, one model per task complexity. Each table is optional in the
/// file and each key within it is optional, so an omitted section is exactly the defaults below.
/// There is no effort table. Effort uses a fixed complexity mapping in the decision engine.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Models {
    pub codex: CodexModels,
    pub claude: ClaudeModels,
}

/// Ultra and high share sol, because sol is the top of the codex catalogue on this box.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CodexModels {
    pub low: String,
    pub medium: String,
    pub high: String,
    pub ultra: String,
}

impl Default for CodexModels {
    fn default() -> CodexModels {
        CodexModels {
            low: "gpt-5.6-luna".to_string(),
            medium: "gpt-5.6-terra".to_string(),
            high: "gpt-5.6-sol".to_string(),
            ultra: "gpt-5.6-sol".to_string(),
        }
    }
}

impl CodexModels {
    pub fn pick(&self, complexity: Complexity) -> &str {
        match complexity {
            Complexity::Low => &self.low,
            Complexity::Medium => &self.medium,
            Complexity::High => &self.high,
            Complexity::Ultra => &self.ultra,
        }
    }
}

/// Ultra is the only tier that reaches fable, which is why the classifier rubric keeps ultra rare.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ClaudeModels {
    pub low: String,
    pub medium: String,
    pub high: String,
    pub ultra: String,
}

impl Default for ClaudeModels {
    fn default() -> ClaudeModels {
        ClaudeModels {
            low: "sonnet".to_string(),
            medium: "opus[1m]".to_string(),
            high: "opus[1m]".to_string(),
            ultra: "fable".to_string(),
        }
    }
}

impl ClaudeModels {
    pub fn pick(&self, complexity: Complexity) -> &str {
        match complexity {
            Complexity::Low => &self.low,
            Complexity::Medium => &self.medium,
            Complexity::High => &self.high,
            Complexity::Ultra => &self.ultra,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ParityConfig {
    pub roots: Vec<PathBuf>,
    pub exceptions: Vec<ParityException>,
}

/// Capacity preferences for adversarial reviews. These adjust selection only after a candidate
/// has passed the independent, fresh-capacity, and raw-usage eligibility gates.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AdversarialReviewConfig {
    /// Reserve Claude capacity for work that needs its stronger sealed review environment. A
    /// positive value makes Claude win only when its raw weekly use is this many points lower than
    /// another eligible reviewer.
    pub claude_usage_reserve_pct: f64,
}

impl Default for AdversarialReviewConfig {
    fn default() -> AdversarialReviewConfig {
        AdversarialReviewConfig {
            claude_usage_reserve_pct: 25.0,
        }
    }
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
    TransportDiffers,
    EndpointDiffers,
    CommandDiffers,
    ArgsDiffer,
    EnvKeysDiffer,
    StandaloneClaudeMd,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which in-place migrations have already been applied to this file. Absent means the file
    /// predates versioning and every migration is still owed. Stamped on write, so an operator who
    /// restores a migrated value keeps it: a migration runs once per file, never on every load.
    #[serde(default = "pre_versioning")]
    pub config_version: u32,
    /// Weekly percent used at or above which a provider is treated as exhausted.
    ///
    /// The default of 98 keeps a 2 point reserve: a provider within 2 points of its weekly limit
    /// is no longer a routing destination. The reserve is what the last points are for, since the
    /// router is not the only thing spending them. Interactive sessions, the classifier's own
    /// per-task call, and a `--provider` dispatch all draw on the same weekly window without
    /// consulting this ceiling, so a router that spends down to the limit leaves nothing for the
    /// work a person is doing by hand.
    pub hard_ceiling_pct: f64,
    /// How long the classifier call may take before it counts as failed.
    ///
    /// Declared here rather than below `policy`, because a scalar after a table typed field makes
    /// `toml::to_string_pretty` fail when the default file is written on first run.
    pub classifier_timeout_secs: u64,
    /// What the local shell can actually reach on this box. Human-maintained: gate 5 of the
    /// rubric is scored against exactly this list. An absent capability blocks dispatch unless a
    /// provider-specific capability is established elsewhere; it must never be assumed for Claude.
    pub connectors: Vec<String>,
    /// Capabilities established for one provider rather than assumed for every provider. The
    /// Codex entry is augmented at load time from its local MCP inventory; operator entries are
    /// useful for providers whose inventory cannot be inspected locally.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_capabilities: BTreeMap<String, Vec<String>>,
    pub policy: Policy,
    /// Which engine and model score a task.
    pub classifier: Classifier,
    /// Which model each provider runs per task complexity.
    pub models: Models,
    pub adversarial_review: AdversarialReviewConfig,
    pub parity: ParityConfig,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            config_version: CURRENT_CONFIG_VERSION,
            hard_ceiling_pct: DEFAULT_HARD_CEILING_PCT,
            classifier_timeout_secs: DEFAULT_CLASSIFIER_TIMEOUT_SECS,
            // Local shell is one capability, not a duplicated inventory of every executable,
            // file, or authenticated endpoint that the shell can reach.
            connectors: vec!["local shell".to_string()],
            provider_capabilities: BTreeMap::new(),
            policy: Policy::default(),
            classifier: Classifier::default(),
            models: Models::default(),
            adversarial_review: AdversarialReviewConfig::default(),
            parity: ParityConfig::default(),
        }
    }
}

impl Config {
    /// IMPURE: the config at the default path under `home`, created with defaults when absent.
    pub fn load_in(home: &Path) -> Result<Config> {
        let mut config = Config::load_from(&default_config_path(home))?;
        config.register_discovered_provider_capabilities(home);
        Ok(config)
    }

    /// Return providers whose declared inventory names the capability described by classifier
    /// rationale. Discovery is deliberately additive and only trusts parsed local config names;
    /// an unreadable or malformed Codex config establishes nothing.
    pub fn capability_providers(&self, rationale: &str) -> Vec<crate::provider::Provider> {
        let rationale = rationale.to_ascii_lowercase();
        self.provider_capabilities
            .iter()
            .filter_map(|(provider, capabilities)| {
                let supported = capabilities.iter().any(|capability| {
                    let capability = capability.trim().to_ascii_lowercase();
                    !capability.is_empty() && rationale.contains(&capability)
                });
                supported
                    .then_some(match provider.as_str() {
                        "codex" => Some(crate::provider::Provider::Codex),
                        "claude" => Some(crate::provider::Provider::Claude),
                        "grok" => Some(crate::provider::Provider::Grok),
                        _ => None,
                    })
                    .flatten()
            })
            .collect()
    }

    fn register_discovered_provider_capabilities(&mut self, home: &Path) {
        if let Ok(text) = std::fs::read_to_string(home.join(".codex/config.toml"))
            && let Ok(document) = toml::from_str::<toml::Value>(&text)
        {
            self.register_capabilities("codex", codex_capabilities(&document));
        }
        // Account connectors are not project MCP servers. Their absence is unknown, never a
        // negative capability assertion; a recorded connector is positive availability evidence.
        if let Ok(text) = std::fs::read_to_string(home.join(".claude.json"))
            && let Ok(document) = serde_json::from_str::<serde_json::Value>(&text)
        {
            self.register_capabilities("claude", claude_capabilities(&document));
        }
    }

    fn register_capabilities<I>(&mut self, provider: &str, discovered: I)
    where
        I: IntoIterator<Item = String>,
    {
        let capabilities = self
            .provider_capabilities
            .entry(provider.to_string())
            .or_default();
        for name in discovered {
            if !capabilities
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&name))
            {
                capabilities.push(name);
            }
        }
    }

    /// IMPURE: the config at `path`, created with defaults when absent, and migrated in place when
    /// it predates the current version. A file that exists but does not parse is an Err: silently
    /// substituting defaults would route jobs against ceilings and a connector list the operator
    /// never wrote.
    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut config: Config = toml::from_str(&text)?;
                if config.migrate() {
                    config.write_to(path)?;
                }
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.write_to(path)?;
                Ok(config)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// PURE-ish: stamp a file written by an older build to `CURRENT_CONFIG_VERSION`, returning
    /// whether it now needs rewriting. The rewrite is what drops dead keys from disk: serde
    /// already ignores unknown keys on parse, so a v4 file still carrying `default_provider` or
    /// `projection_overdraw_pct` keeps routing the same either way, and the one rewrite is what
    /// stops the file from naming keys the router no longer has.
    ///
    /// Operator-chosen values are left alone. The v1–v4 steps that used to correct generated
    /// defaults this tool itself wrote are gone: those corrections already ran on every stamped
    /// file, and collapsing them is the point of this cut.
    fn migrate(&mut self) -> bool {
        if self.config_version >= CURRENT_CONFIG_VERSION {
            return false;
        }
        self.config_version = CURRENT_CONFIG_VERSION;
        true
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

/// Safe, name-only discovery. Both enabled plugins and MCP servers establish Codex capabilities;
/// their configuration bodies can contain secrets and are deliberately never retained.
fn codex_capabilities(document: &toml::Value) -> Vec<String> {
    let mut capabilities = Vec::new();
    for key in ["mcp_servers", "plugins"] {
        if let Some(table) = document.get(key).and_then(toml::Value::as_table) {
            for (name, value) in table {
                if key != "plugins"
                    || value.get("enabled").and_then(toml::Value::as_bool) != Some(false)
                {
                    capabilities.push(name.split('@').next().unwrap_or(name).to_string());
                }
            }
        }
    }
    capabilities
}

/// Claude records account connectors separately from its MCP configuration. This extracts only a
/// display-name suffix (for example `Granola`), never connector metadata or credential values.
fn claude_capabilities(document: &serde_json::Value) -> Vec<String> {
    document
        .get("claudeAiMcpEverConnected")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter_map(|name| name.strip_prefix("claude.ai "))
        .map(str::to_string)
        .collect()
}

pub fn default_config_path(home: &Path) -> PathBuf {
    home.join(".config/agent-router/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_capabilities_are_provider_scoped_and_name_only() {
        let codex: toml::Value = toml::from_str(
            "[plugins.\"granola@openai-curated\"]\nenabled = true\n\n[mcp_servers.airtable]\ncommand = 'runner'\n",
        )
        .expect("parse Codex config");
        assert_eq!(codex_capabilities(&codex), vec!["airtable", "granola"]);

        let claude = serde_json::json!({
            "claudeAiMcpEverConnected": ["claude.ai Granola", "claude.ai Notion"]
        });
        assert_eq!(claude_capabilities(&claude), vec!["Granola", "Notion"]);
    }

    #[test]
    fn a_missing_config_is_created_with_defaults_and_reads_back_identically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/config.toml");
        let created = Config::load_from(&path).expect("creates defaults");
        assert_eq!(created, Config::default());
        assert!(path.exists(), "the default config must be written to disk");
        assert_eq!(Config::load_from(&path).expect("re-reads"), created);
    }

    /// A file stamped below 5 is rewritten once. Operator-chosen values stay put: the collapsed
    /// migration no longer corrects generated defaults this tool itself wrote weeks earlier.
    #[test]
    fn a_pre_versioning_file_is_rewritten_and_stamped_without_correcting_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "classifier_timeout_secs = 30\n").expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.classifier_timeout_secs, 30);
        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);

        let text = std::fs::read_to_string(&path).expect("re-read");
        assert!(text.contains("classifier_timeout_secs = 30"), "{text}");
        assert!(text.contains("config_version = 5"), "{text}");
    }

    /// A v4 file carrying the three dead keys parses, is rewritten without them, and is stamped 5.
    #[test]
    fn a_v4_file_carrying_dead_keys_is_rewritten_without_them_and_stamped_five() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "config_version = 4\n\
             projection_overdraw_pct = 100.0\n\
             claude_five_hour_pacing_pct = 90.0\n\
             \n\
             [policy]\n\
             default_provider = \"codex\"\n\
             weekly_routing = true\n",
        )
        .expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
        assert!(config.policy.weekly_routing);

        let text = std::fs::read_to_string(&path).expect("re-read");
        assert!(text.contains("config_version = 5"), "{text}");
        assert!(
            !text.contains("default_provider"),
            "dead policy key must leave the file: {text}"
        );
        assert!(
            !text.contains("projection_overdraw_pct"),
            "dead key must leave the file: {text}"
        );
        assert!(
            !text.contains("claude_five_hour_pacing_pct"),
            "dead key must leave the file: {text}"
        );
        assert!(text.contains("weekly_routing = true"), "{text}");
    }

    /// A ceiling the operator chose is theirs. 90 is not the value this tool generated, so the v3
    /// step must leave it alone even while stamping the same file to the current version.
    #[test]
    fn migration_stamps_the_version_without_touching_a_deliberate_ceiling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "config_version = 2\nhard_ceiling_pct = 90.0\n").expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.hard_ceiling_pct, 90.0);
        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
    }

    /// The stamp is what makes the correction a one-time event. An operator who deliberately puts
    /// 97 back on an already-migrated file keeps it, rather than having it rewritten on every load.
    #[test]
    fn a_migrated_file_never_has_its_ceiling_corrected_a_second_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "config_version = 2\nhard_ceiling_pct = 97.0\n").expect("write");
        Config::load_from(&path).expect("first load migrates");

        let migrated = std::fs::read_to_string(&path).expect("read");
        std::fs::write(
            &path,
            migrated.replace("hard_ceiling_pct = 98.0", "hard_ceiling_pct = 97.0"),
        )
        .expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(
            config.hard_ceiling_pct, 97.0,
            "a stamped file must keep the operator's value"
        );
    }

    /// A value the operator chose is not the tool's to correct, even while the same file is being
    /// migrated to the current version.
    #[test]
    fn migration_stamps_the_version_without_touching_a_deliberate_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "classifier_timeout_secs = 45\n").expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.classifier_timeout_secs, 45);
        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
    }

    /// The regression the version stamp exists to prevent. Without it the migration keys off the
    /// value alone, so an operator who deliberately restores 30 has it overwritten on every single
    /// load and can never make the choice stick.
    #[test]
    fn a_migrated_file_never_has_its_timeout_corrected_a_second_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "classifier_timeout_secs = 30\n").expect("write");
        Config::load_from(&path).expect("first load migrates");

        // The operator now deliberately puts the old deadline back, on an already-stamped file.
        let migrated = std::fs::read_to_string(&path).expect("read");
        std::fs::write(
            &path,
            migrated.replace(
                "classifier_timeout_secs = 60",
                "classifier_timeout_secs = 30",
            ),
        )
        .expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(
            config.classifier_timeout_secs, 30,
            "a stamped file must keep the operator's value"
        );
    }

    /// A file this build writes is already current, so it must not be rewritten on the next load.
    #[test]
    fn a_freshly_created_config_is_stamped_current_and_is_not_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let created = Config::load_from(&path).expect("creates");
        assert_eq!(created.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(created.hard_ceiling_pct, 98.0);

        let mut again = created.clone();
        assert!(!again.migrate(), "a current file has nothing to migrate");
        assert_eq!(again, created);
    }

    #[test]
    fn a_partial_config_keeps_defaults_for_the_keys_it_omits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "hard_ceiling_pct = 90.0\n").expect("write");
        let config = Config::load_from(&path).expect("loads");
        assert_eq!(config.hard_ceiling_pct, 90.0);
        assert_eq!(config.connectors, Config::default().connectors);
    }

    /// The tiers are the routing policy an operator tunes, so an absent file section is the
    /// documented default and a partial one overrides only the key it names.
    #[test]
    fn model_tiers_default_when_absent_and_override_one_key_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        let defaults = Config::default();
        assert_eq!(defaults.models.codex.pick(Complexity::Low), "gpt-5.6-luna");
        assert_eq!(
            defaults.models.codex.pick(Complexity::Medium),
            "gpt-5.6-terra"
        );
        assert_eq!(defaults.models.codex.pick(Complexity::High), "gpt-5.6-sol");
        assert_eq!(defaults.models.codex.pick(Complexity::Ultra), "gpt-5.6-sol");
        assert_eq!(defaults.models.claude.pick(Complexity::Low), "sonnet");
        assert_eq!(defaults.models.claude.pick(Complexity::Medium), "opus[1m]");
        assert_eq!(defaults.models.claude.pick(Complexity::High), "opus[1m]");
        assert_eq!(defaults.models.claude.pick(Complexity::Ultra), "fable");

        // No models section at all.
        std::fs::write(&path, "hard_ceiling_pct = 90.0\n").expect("write");
        let absent = Config::load_from(&path).expect("loads");
        assert_eq!(absent.models, defaults.models);

        std::fs::write(
            &path,
            "[models.codex]\nlow = \"gpt-5.6-tiny\"\n\n[models.claude]\nultra = \"opus[1m]\"\n",
        )
        .expect("write");
        let partial = Config::load_from(&path).expect("loads");
        assert_eq!(partial.models.codex.low, "gpt-5.6-tiny");
        assert_eq!(partial.models.codex.medium, "gpt-5.6-terra");
        assert_eq!(partial.models.codex.high, "gpt-5.6-sol");
        assert_eq!(partial.models.codex.ultra, "gpt-5.6-sol");
        assert_eq!(partial.models.claude.ultra, "opus[1m]");
        assert_eq!(partial.models.claude.low, "sonnet");
        assert_eq!(partial.models.claude.high, "opus[1m]");
    }

    /// The classifier engine is the setting that decides which weekly budget the per-task scoring
    /// call is drawn from, so an absent section must be the documented default and each key must
    /// override on its own.
    #[test]
    fn the_classifier_engine_and_models_default_when_absent_and_override_one_key_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        let defaults = Config::default();
        assert_eq!(defaults.classifier.engine, ClassifierEngine::Codex);
        assert_eq!(defaults.classifier.claude_model, "haiku");
        assert_eq!(defaults.classifier.codex_model, "gpt-5.6-luna");
        assert_eq!(defaults.classifier.model(), "gpt-5.6-luna");
        assert_eq!(defaults.connectors, vec!["local shell"]);
        assert!(defaults.provider_capabilities.is_empty());

        std::fs::write(&path, "hard_ceiling_pct = 90.0\n").expect("write");
        let absent = Config::load_from(&path).expect("loads");
        assert_eq!(absent.classifier, defaults.classifier);

        // Flipping the engine alone is the whole switch: the codex model was already configured,
        // so no second edit is needed to make the change take effect.
        std::fs::write(&path, "[classifier]\nengine = \"codex\"\n").expect("write");
        let flipped = Config::load_from(&path).expect("loads");
        assert_eq!(flipped.classifier.engine, ClassifierEngine::Codex);
        assert_eq!(flipped.classifier.model(), "gpt-5.6-luna");
        assert_eq!(flipped.classifier.claude_model, "haiku");

        std::fs::write(
            &path,
            "[classifier]\nengine = \"codex\"\ncodex_model = \"gpt-5.6-terra\"\n",
        )
        .expect("write");
        let retuned = Config::load_from(&path).expect("loads");
        assert_eq!(retuned.classifier.model(), "gpt-5.6-terra");

        std::fs::write(&path, "[classifier]\nclaude_model = \"sonnet\"\n").expect("write");
        let claude_only = Config::load_from(&path).expect("loads");
        assert_eq!(claude_only.classifier.engine, ClassifierEngine::Codex);
        assert_eq!(claude_only.classifier.model(), "gpt-5.6-luna");
        assert_eq!(claude_only.classifier.claude_model, "sonnet");
    }

    /// An engine name that is not a supported CLI is an error, not a silent fall back to claude:
    /// a typo must be visible rather than quietly routing every scoring call at the wrong budget.
    #[test]
    fn an_unknown_classifier_engine_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[classifier]\nengine = \"not-a-real-engine\"\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }

    fn load_config(text: &str, path: &Path) -> Result<Config> {
        std::fs::write(path, text).expect("write config fixture");
        Config::load_from(path)
    }

    /// A parity exception silences a real difference, so an incomplete one is a difference nobody
    /// is looking at. Each way of writing one badly must be rejected rather than half-applied.
    #[test]
    fn incomplete_or_invalid_parity_exceptions_are_rejected() {
        let fixtures = [
            ("missing path", "[[parity.exceptions]]\nreason = \"why\"\n"),
            (
                "empty path",
                "[[parity.exceptions]]\npath = \"\"\nreason = \"why\"\n",
            ),
            (
                "missing reason",
                "[[parity.exceptions]]\npath = \"project\"\n",
            ),
            (
                "blank reason",
                "[[parity.exceptions]]\npath = \"project\"\nreason = \"   \"\n",
            ),
            (
                "unknown kind",
                "[[parity.exceptions]]\npath = \"project\"\nreason = \"why\"\nkind = \"other\"\n",
            ),
        ];
        for (label, text) in fixtures {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("config.toml");
            assert!(
                load_config(text, &path).is_err(),
                "{label} must be rejected"
            );
        }
    }

    #[test]
    fn adversarial_review_claude_reserve_defaults_and_is_operator_configurable() {
        assert_eq!(
            Config::default()
                .adversarial_review
                .claude_usage_reserve_pct,
            25.0
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let config = load_config(
            "[adversarial_review]\nclaude_usage_reserve_pct = 12.5\n",
            &path,
        )
        .expect("load the configured reserve");

        assert_eq!(config.adversarial_review.claude_usage_reserve_pct, 12.5);
    }

    #[test]
    fn a_malformed_config_is_an_error_rather_than_a_silent_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "hard_ceiling_pct = \"ninety\"\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }
}
