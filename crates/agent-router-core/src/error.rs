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
}

pub type Result<T> = std::result::Result<T, Error>;
