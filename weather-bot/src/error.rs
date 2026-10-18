use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("execution error: {0}")]
    Execution(String),
}

pub type Result<T> = std::result::Result<T, BotError>;

impl BotError {
    /// Process exit code. Configuration problems exit with 2.
    pub fn exit_code(&self) -> i32 {
        match self {
            BotError::Config(_) => 2,
            _ => 1,
        }
    }
}

impl From<rusqlite::Error> for BotError {
    fn from(e: rusqlite::Error) -> Self {
        BotError::Storage(e.to_string())
    }
}
impl From<csv::Error> for BotError {
    fn from(e: csv::Error) -> Self {
        BotError::Storage(e.to_string())
    }
}
impl From<std::io::Error> for BotError {
    fn from(e: std::io::Error) -> Self {
        BotError::Storage(e.to_string())
    }
}
impl From<serde_json::Error> for BotError {
    fn from(e: serde_json::Error) -> Self {
        BotError::Parse(e.to_string())
    }
}
impl From<reqwest::Error> for BotError {
    fn from(e: reqwest::Error) -> Self {
        BotError::Network(e.to_string())
    }
}
