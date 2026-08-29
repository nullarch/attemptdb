//! JSON-RPC 2.0 envelopes and the MCP result shapes, built by hand with
//! `serde_json` (no SDK). Only what the stdio transport needs.

use serde_json::{Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// MCP: `resources/read` for a URI the server does not serve.
pub const RESOURCE_NOT_FOUND: i64 = -32002;

/// A JSON-RPC error to be sent back for a request id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }
}

/// `{"jsonrpc":"2.0","id":…,"result":…}`.
pub fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `{"jsonrpc":"2.0","id":…,"error":{code,message[,data]}}`.
pub fn error(id: Value, err: &RpcError) -> Value {
    let mut e = json!({ "code": err.code, "message": err.message });
    if let Some(d) = &err.data {
        e["data"] = d.clone();
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": e })
}

/// One `{"type":"text","text":…}` content block.
pub fn text_block(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

/// A JSON document as a text content block (pretty-printed so an LLM can
/// read it; MIME hint kept in an annotation-free way: it is just text).
pub fn json_block(value: &Value) -> Value {
    text_block(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
}

/// A successful tool result.
pub fn tool_ok(blocks: Vec<Value>) -> Value {
    json!({ "content": blocks })
}

/// A failed tool call (`isError: true`), rendered as text for the caller.
pub fn tool_error(message: impl Into<String>) -> Value {
    json!({ "content": [text_block(message)], "isError": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes() {
        let r = response(json!(1), json!({"ok": true}));
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["ok"], true);
        let e = error(Value::Null, &RpcError::method_not_found("x"));
        assert_eq!(e["error"]["code"], METHOD_NOT_FOUND);
        assert!(e["error"].get("data").is_none());
        let t = tool_error("boom");
        assert_eq!(t["isError"], true);
        assert_eq!(t["content"][0]["text"], "boom");
    }
}
