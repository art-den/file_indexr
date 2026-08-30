use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_request() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let msg = parse_message(body.as_bytes()).unwrap();
        match msg {
            Message::Request(req) => {
                assert_eq!(req.method, "tools/list");
                assert!(req.id.is_some());
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_notification() {
        let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        let msg = parse_message(body.as_bytes()).unwrap();
        assert!(matches!(msg, Message::Notification(_)));
    }

    #[test]
    fn test_parse_request_with_null_id() {
        // "id": null is still a Request (the key is present), not a Notification.
        let body =
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized","params":{}}"#;
        let msg = parse_message(body.as_bytes()).unwrap();
        match msg {
            Message::Request(req) => {
                assert_eq!(req.method, "notifications/initialized");
                assert!(req.id.is_none());
            }
            _ => panic!("Expected Request with null id"),
        }
    }

    #[test]
    fn test_parse_response_message() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let msg = parse_message(body.as_bytes()).unwrap();
        assert!(matches!(msg, Message::Response));
    }

    #[test]
    fn test_parse_invalid_json() {
        let body = b"not json at all";
        let result = parse_message(body);
        assert!(result.is_err());
    }

    #[test]
    fn test_response_ok() {
        let resp = Response::ok(Some(Value::from(1)), Value::Array(vec![]));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert!(resp.id.is_some());
    }

    #[test]
    fn test_response_err() {
        let err = Error::new(METHOD_NOT_FOUND, "Unknown method");
        let resp = Response::err(Some(Value::from(2)), err);
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
    }

    #[test]
    fn test_response_err_undetermined_has_null_id() {
        // JSON-RPC 2.0: an error response with an undetermined id must serialize "id": null,
        // not omit the field.
        let err = Error::new(PARSE_ERROR, "Invalid JSON");
        let resp = Response::err_undetermined(err);
        let parsed: Value = serde_json::from_slice(&resp.to_bytes()).unwrap();
        assert!(parsed.get("id").is_some(), "id member must be present");
        assert!(parsed["id"].is_null());
    }

    #[test]
    fn test_response_to_bytes() {
        let resp = Response::ok(None, Value::Bool(true));
        let bytes = resp.to_bytes();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["result"], true);
    }
}
