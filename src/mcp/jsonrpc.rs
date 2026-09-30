use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
#[path = "tests/tests_jsonrpc.rs"]
mod tests;

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// Per-request metadata (MCP modern protocol).
    #[serde(rename = "_meta", default)]
    pub meta: Option<Value>,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize)]
pub struct Error {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    /// An error with no `data` payload.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Parsed incoming message.
#[derive(Debug)]
pub enum Message {
    Request(Request),
    /// A request without the `"id"` key — no response is sent.
    /// The inner `Request` always has `id == None`.
    Notification(Request),
    /// A response to a server-sent request — no reply is sent.
    Response,
}

/// Parse a JSON string into an MCP message.
pub fn parse_message(body: &[u8]) -> Result<Message, Error> {
    let value: Value = serde_json::from_slice(body).map_err(|e| parse_error("Invalid JSON", e))?;
    let obj = value.as_object().ok_or_else(invalid_request)?;

    // Requests and notifications have a "method" field.
    if obj.contains_key("method") {
        // Presence of the "id" key (even null) distinguishes a Request from a Notification.
        let is_request = obj.contains_key("id");
        let req: Request =
            serde_json::from_value(value).map_err(|e| parse_error("Invalid request", e))?;
        return if is_request {
            Ok(Message::Request(req))
        } else {
            Ok(Message::Notification(req))
        };
    }
    if obj.contains_key("result") || obj.contains_key("error") {
        return Ok(Message::Response);
    }
    Err(invalid_request())
}

/// Build a `PARSE_ERROR` for a serde failure, prefixed with context.
fn parse_error(prefix: &str, e: serde_json::Error) -> Error {
    Error::new(PARSE_ERROR, format!("{prefix}: {e}"))
}

/// `INVALID_REQUEST` for well-formed JSON that is not a JSON-RPC message.
fn invalid_request() -> Error {
    Error::new(INVALID_REQUEST, "Not a valid JSON-RPC message")
}

impl Response {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn err(id: Option<Value>, error: Error) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(error),
            id,
        }
    }

    /// Error response with an undetermined id.
    /// JSON-RPC 2.0 requires the `id` member to be present (as `null`) when it cannot be determined.
    pub fn err_undetermined(error: Error) -> Self {
        Self::err(Some(Value::Null), error)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Response fields are all infallible to serialize")
    }
}

// JSON-RPC 2.0 standard error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
