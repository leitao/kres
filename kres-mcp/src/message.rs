//! JSON-RPC 2.0 request/response envelopes for MCP.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Request<'a> {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<&'a Value>,
}

impl<'a> Request<'a> {
    pub fn new(id: i64, method: &'a str, params: Option<&'a Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// JSON-RPC notification: no `id` field, no response expected.
/// Used for `notifications/initialized` in the MCP handshake.
#[derive(Debug, Clone, Serialize)]
pub struct Notification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<&'a Value>,
}

impl<'a> Notification<'a> {
    pub fn new(method: &'a str, params: Option<&'a Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>, // i64 typically, but may be null on notification
    #[serde(flatten)]
    pub result: ResponseResult,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResponseResult {
    Ok { result: Value },
    Err { error: JsonRpcError },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let p = json!({"name": "x"});
        let r = Request::new(3, "tools/call", Some(&p));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":3"));
        assert!(s.contains("\"method\":\"tools/call\""));
        assert!(s.contains("\"params\":{\"name\":\"x\"}"));
    }

    #[test]
    fn request_without_params_omits_field() {
        let r = Request::new(1, "ping", None);
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("params"));
    }

    #[test]
    fn response_parses_ok() {
        let raw = r#"{"jsonrpc":"2.0","id":3,"result":{"hello":"world"}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        match r.result {
            ResponseResult::Ok { result } => {
                assert_eq!(result.get("hello").unwrap(), "world");
            }
            _ => panic!("expected ok"),
        }
    }

    #[test]
    fn response_parses_err() {
        let raw = r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"no method"}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        match r.result {
            ResponseResult::Err { error } => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "no method");
                assert!(error.data.is_none());
            }
            _ => panic!("expected err"),
        }
    }

    #[test]
    fn response_err_with_data_preserved() {
        let raw =
            r#"{"jsonrpc":"2.0","id":4,"error":{"code":-1,"message":"x","data":{"why":"bad"}}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        match r.result {
            ResponseResult::Err { error } => {
                assert!(error.data.is_some());
            }
            _ => panic!("expected err"),
        }
    }
}
