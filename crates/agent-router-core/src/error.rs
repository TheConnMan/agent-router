#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Command(String),
    /// A provider CLI could not be turned into a runnable path, or could not be exec'd once it
    /// was. Distinct from `Io` on purpose: this variant names the binary, the override that pins
    /// it, and where the resolver looked. See
    /// docs/decisions/0005-launch-error-and-binary-resolver.md.
    #[error("launch failed: {0}")]
    Launch(String),
}

pub type Result<T> = std::result::Result<T, Error>;
