use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp server `{server}` failed to start: {source}")]
    Spawn {
        server: String,
        #[source]
        source: std::io::Error,
    },

    #[error("mcp server `{server}` returned rpc error {code}: {message}")]
    Rpc {
        server: String,
        code: i64,
        message: String,
    },

    #[error("mcp server `{server}` stdin closed")]
    StdinClosed { server: String },

    #[error("mcp server `{server}` stdout closed before response for id={id}")]
    StdoutClosed { server: String, id: i64 },

    #[error("mcp server `{server}` response timed out after {millis}ms (request id={id})")]
    Timeout {
        server: String,
        id: i64,
        millis: u64,
    },

    #[error("mcp server `{server}` produced malformed JSON: {source}")]
    Json {
        server: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("mcp server `{server}` not registered")]
    UnknownServer { server: String },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
