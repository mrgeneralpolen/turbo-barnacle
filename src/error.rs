use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("config: {0}")]
    Config(String),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("vk: {0}")]
    Vk(String),
    #[error("telegram: {0}")]
    Telegram(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("date: {0}")]
    Date(String),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn config(msg: impl Into<String>) -> Self {
        AppError::Config(msg.into())
    }
    pub fn vk(msg: impl Into<String>) -> Self {
        AppError::Vk(msg.into())
    }
    pub fn parse(msg: impl Into<String>) -> Self {
        AppError::Parse(msg.into())
    }
    pub fn other(msg: impl Into<String>) -> Self {
        AppError::Other(msg.into())
    }
}
