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
    /// was. Distinct from `Io` on purpose: `Io`'s `Display` is `No such file or directory (os
    /// error 2)`, which is the useless string the lost production rows recorded. This variant's
    /// message names the binary, the override that pins it, and where the resolver looked.
    #[error("launch failed: {0}")]
    Launch(String),
}

pub type Result<T> = std::result::Result<T, Error>;
