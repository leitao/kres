use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("agent response contains no parseable JSON")]
    NoJson,

    #[error("{0}")]
    Other(String),
}
