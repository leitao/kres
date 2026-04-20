//! MCP stdio JSON-RPC client.
//!
//! Invariants owed (see ../../bugs.md):
//! - M9: server stderr is persisted to `<log_dir>/mcp-<server>.log`,
//!   never dropped silently.
//! - M10: every request has a bounded timeout; a wedged server
//!   surfaces as `McpError::Timeout`, not an infinite block.
//!
//! MCP protocol reference: JSON-RPC 2.0 over stdin/stdout. Line-
//! delimited objects; `id` correlates request to response. Notifications
//! have no `id`.

pub mod client;
pub mod config;
pub mod error;
pub mod message;
pub mod transport;

pub use client::McpClient;
pub use config::{ServerConfig, ServerRegistry};
pub use error::McpError;
pub use message::{Request, Response, ResponseResult};
