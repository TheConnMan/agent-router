//! `~/.config/agent-router/config.toml`: the routing ceilings and the connector inventory the
//! classifier scores gate 5 against. Written with defaults on first run.

use crate::classify::Complexity;
use crate::error::Result;
use crate::runtime::home_dir;
use std::path::{Path, PathBuf};

/// Weekly percent at which a provider counts as exhausted.
const DEFAULT_HARD_CEILING_PCT: f64 = 97.0;
/// Weekly-headroom gap (in points) that flips a borderline classification.
const DEFAULT_HEADROOM_FLIP_GAP: f64 = 25.0;
/// Claude five hour percent used at or above which a task is paced away from Claude.
const DEFAULT_CLAUDE_FIVE_HOUR_PACING_PCT: f64 = 90.0;
/// Ceiling on the classifier call. Headroom over the measured worst case rather than a target:
/// the fast path is 3.4-7.0s, so this only has to be generous enough that a slow tail falls back
/// far less often than it did at 30s, where 6 of 18 measured calls lost the deadline.
const DEFAULT_CLASSIFIER_TIMEOUT_SECS: u64 = 60;
/// The classifier timeout this tool used to generate into new config files.
const PRE_MIGRATION_CLASSIFIER_TIMEOUT_SECS: u64 = 30;

/// The migration level a config file written by this build carries. Raise this, and add a step to
/// `migrate`, whenever a stale generated value has to be corrected in place.
const CURRENT_CONFIG_VERSION: u32 = 1;

/// The level a file that predates versioning reads as. This is deliberately NOT
/// `CURRENT_CONFIG_VERSION`: an absent key has to be distinguishable from a stamped one, or every
/// file would look already-migrated and no migration could ever run.
fn pre_versioning() -> u32 {
    0
}

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
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            default_provider: DefaultProvider::Codex,
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
            engine: ClassifierEngine::Claude,
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
/// There is no effort table on purpose: the model is the toggle, and the reasoning effort is left
/// to the backend to resolve. What each backend resolves it to differs, and on codex it is not the
/// model's default. See `Decision::effort` and docs/configuration.md.
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
    /// Which in-place migrations have already been applied to this file. Absent means the file
    /// predates versioning and every migration is still owed. Stamped on write, so an operator who
    /// restores a migrated value keeps it: a migration runs once per file, never on every load.
    #[serde(default = "pre_versioning")]
    pub config_version: u32,
    /// Weekly percent used at or above which a provider is treated as exhausted.
    pub hard_ceiling_pct: f64,
    /// How many points of weekly-headroom advantage flip a borderline verdict.
    pub headroom_flip_gap: f64,
    /// Claude five hour percent used at or above which a task is paced away from Claude, provided
    /// Codex has weekly room. Codex's own five hour number never influences routing. Declared here
    /// rather than below `policy`, because a scalar after a table typed field makes
    /// `toml::to_string_pretty` fail when the default file is written on first run.
    pub claude_five_hour_pacing_pct: f64,
    /// How long the classifier call may take before it counts as failed.
    pub classifier_timeout_secs: u64,
    /// What Codex can actually reach on this box. Human-maintained: gate 5 of the rubric
    /// ("Codex has every required connector") is scored against exactly this list, so anything
    /// absent here is what forces a task to Claude.
    pub connectors: Vec<String>,
    pub policy: Policy,
    /// Which engine and model score a task.
    pub classifier: Classifier,
    /// Which model each provider runs per task complexity.
    pub models: Models,
    pub parity: ParityConfig,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            config_version: CURRENT_CONFIG_VERSION,
            hard_ceiling_pct: DEFAULT_HARD_CEILING_PCT,
            headroom_flip_gap: DEFAULT_HEADROOM_FLIP_GAP,
            claude_five_hour_pacing_pct: DEFAULT_CLAUDE_FIVE_HOUR_PACING_PCT,
            classifier_timeout_secs: DEFAULT_CLASSIFIER_TIMEOUT_SECS,
            connectors: vec![
                "local shell".to_string(),
                "git".to_string(),
                "gh (github)".to_string(),
                "airtable".to_string(),
            ],
            policy: Policy::default(),
            classifier: Classifier::default(),
            models: Models::default(),
            parity: ParityConfig::default(),
        }
    }
}

impl Config {
    /// IMPURE: the config at the default path, created with defaults when absent.
    pub fn load() -> Result<Config> {
        Config::load_from(&default_config_path())
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

    /// PURE-ish: bring a file written by an older build up to `CURRENT_CONFIG_VERSION`, returning
    /// whether it now needs rewriting. A step may only touch a value still equal to the default
    /// this tool used to generate, which is what keeps this compatible with the file's stated
    /// contract that the operator's config is authoritative.
    ///
    /// KNOWN LIMIT, on an unstamped file only: "still equal to the old generated default" cannot
    /// distinguish a value the operator deliberately typed from the one the tool wrote, so a v1
    /// migration will move a hand-pinned 30. That is accepted rather than solved, because there is
    /// no evidence in the file to solve it with: the cost is one value, once, in the direction of
    /// more headroom, and it is documented in `docs/configuration.md`.
    ///
    /// After the stamp the ambiguity is gone, and that is why steps are keyed on the version and
    /// not on the value. Migrating on the value alone would re-apply on every load, so an operator
    /// who set the old default back deliberately would have it overwritten every time, forever.
    fn migrate(&mut self) -> bool {
        if self.config_version >= CURRENT_CONFIG_VERSION {
            return false;
        }
        // v1: the classifier timeout was generated as 30s, which the measured call time then
        // outgrew (see `classify.rs`). A file still carrying the old generated value gets the new
        // one; anything else is a choice and is left alone.
        if self.config_version < 1
            && self.classifier_timeout_secs == PRE_MIGRATION_CLASSIFIER_TIMEOUT_SECS
        {
            self.classifier_timeout_secs = DEFAULT_CLASSIFIER_TIMEOUT_SECS;
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

    /// The whole point of the migration: a file this tool generated before the timeout changed
    /// still says 30, and changing only the default constant would never reach it.
    #[test]
    fn a_pre_versioning_file_carrying_the_old_generated_timeout_is_migrated_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "classifier_timeout_secs = 30\n").expect("write");

        let config = Config::load_from(&path).expect("loads");
        assert_eq!(
            config.classifier_timeout_secs,
            DEFAULT_CLASSIFIER_TIMEOUT_SECS
        );
        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);

        // The new value must be on disk, not merely in memory: a file that still reads 30 while
        // the router behaves as 60 is a config that lies about what is running.
        let text = std::fs::read_to_string(&path).expect("re-read");
        assert!(text.contains("classifier_timeout_secs = 60"), "{text}");
        assert!(text.contains("config_version = 1"), "{text}");
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
        assert_eq!(
            config.headroom_flip_gap,
            Config::default().headroom_flip_gap
        );
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
        assert_eq!(defaults.classifier.engine, ClassifierEngine::Claude);
        assert_eq!(defaults.classifier.claude_model, "haiku");
        assert_eq!(defaults.classifier.codex_model, "gpt-5.6-luna");
        assert_eq!(defaults.classifier.model(), "haiku");

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
        assert_eq!(claude_only.classifier.engine, ClassifierEngine::Claude);
        assert_eq!(claude_only.classifier.model(), "sonnet");
    }

    /// An engine name that is not a supported CLI is an error, not a silent fall back to claude:
    /// a typo must be visible rather than quietly routing every scoring call at the wrong budget.
    #[test]
    fn an_unknown_classifier_engine_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[classifier]\nengine = \"opencode\"\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn a_malformed_config_is_an_error_rather_than_a_silent_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "hard_ceiling_pct = \"ninety\"\n").expect("write");
        assert!(Config::load_from(&path).is_err());
    }
}
