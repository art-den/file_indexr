use serde_json::Value;

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
