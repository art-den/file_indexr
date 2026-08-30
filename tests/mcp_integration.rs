use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::http::StatusCode;
use file_indexr::config::Config;
use file_indexr::index::writer::IndexWriterWrapper;
use file_indexr::mcp::jsonrpc::{INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR};
use file_indexr::{AppState, api::create_router, mcp};
use serde_json::json;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("mcp_test")
}

/// Helper to call the router with a GET request.
async fn call_get(router: axum::Router, uri: &str) -> StatusCode {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let mut router = router;
    let response = tower::Service::<Request<Body>>::call(&mut router, request)
        .await
        .unwrap();
    response.status()
}

/// Helper to call the router with a POST request and JSON body.
async fn call_post(
    router: axum::Router,
    uri: &str,
    body: String,
) -> (StatusCode, serde_json::Value) {
    let req_body = Body::from(body.into_bytes());
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(req_body)
        .unwrap();
    let mut router = router;
    let response = tower::Service::<Request<Body>>::call(&mut router, request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(json!({"parse_fail": true}));
    (status, json)
}

/// Create test state with files for MCP testing.
async fn make_test_state() -> AppState {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();

    // Create files for searching
    std::fs::write(
        watch_dir.join("hello.txt"),
        "Hello, world!\nWelcome to FileIndexr.",
    )
    .unwrap();
    std::fs::write(
        watch_dir.join("test.rs"),
        "fn main() {\n    println!(\"Hello Rust\");\n}",
    )
    .unwrap();
    std::fs::write(
        watch_dir.join("README.md"),
        "# FileIndexr\n\nA local search engine.\n\n## Features\n\n- Full-text search\n- REST API\n",
    )
    .unwrap();

    // Large file (>8000 bytes) for auto-truncation testing
    let large_content: String = (1..=200)
        .map(|i| format!("Line {}: This is some content for line number {}.", i, i))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(watch_dir.join("large.txt"), &large_content).unwrap();

    let config = Arc::new(Config {
        directory: watch_dir,
        index_path: index_dir.clone(),
        port: 8080,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,

        allowed_extensions: vec![],
    });

    let text_data_cache = file_indexr::formats::new_text_data_cache();
    let writer = IndexWriterWrapper::new(&index_dir, config.clone(), text_data_cache.clone())
        .await
        .unwrap();
    writer
        .add_file(config.directory.join("hello.txt"))
        .await
        .unwrap();
    writer
        .add_file(config.directory.join("test.rs"))
        .await
        .unwrap();
    writer
        .add_file(config.directory.join("README.md"))
        .await
        .unwrap();
    writer.commit().await;

    let index = writer.index();
    let reader = index.reader().unwrap();

    AppState {
        reader: Arc::new(reader),
        config,
        writer: Arc::new(writer),
        text_data_cache,
    }
}

// ============================================================================
// GET /mcp — SSE endpoint
// ============================================================================

#[tokio::test]
async fn test_mcp_get_returns_ok() {
    let state = make_test_state().await;
    let router = create_router(state);

    let status = call_get(router, "/mcp").await;
    assert_eq!(status, StatusCode::OK);
}

// ============================================================================
// POST /mcp — server/discover (modern protocol)
// ============================================================================

#[tokio::test]
async fn test_mcp_server_discover() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {}
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["protocolVersion"], mcp::MCP_VERSION_MODERN);
    assert_eq!(resp["result"]["serverInfo"]["name"], "file_indexr");
    let versions = resp["result"]["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!(mcp::MCP_VERSION_MODERN)));
    assert!(versions.contains(&json!(mcp::MCP_VERSION_LEGACY)));
}

// ============================================================================
// POST /mcp — initialize (legacy protocol)
// ============================================================================

#[tokio::test]
async fn test_mcp_initialize_legacy() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {}
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["protocolVersion"], mcp::MCP_VERSION_LEGACY);
    assert_eq!(resp["result"]["serverInfo"]["name"], "file_indexr");
}

// ============================================================================
// POST /mcp — tools/list
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_list() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/list",
        "params": {}
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 3);
    assert!(resp["error"].is_null());

    let tools = &resp["result"]["tools"];
    assert!(tools.is_array());
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"docs_search"));
    assert!(names.contains(&"docs_headings"));
    assert!(names.contains(&"docs_get"));
}

// ============================================================================
// POST /mcp — tools/call docs_search
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_call_docs_search() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "docs_search",
            "arguments": {
                "query": "Hello"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], 4);
    assert!(resp["error"].is_null());

    let content = &resp["result"]["content"][0];
    assert_eq!(content["type"], "text");
    let text = content["text"].as_str().unwrap();
    assert!(text.contains("result"));
    assert!(text.contains("Hello"));
}

#[tokio::test]
async fn test_mcp_tools_call_docs_search_with_max_results() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "docs_search",
            "arguments": {
                "query": "Hello",
                "max_results": 1
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let content = &resp["result"]["content"][0];
    let text = content["text"].as_str().unwrap();
    // Count actual result entries by the "(score:" marker emitted per result line —
    // the total in the header may be higher than the number of entries returned
    let result_count = text.matches("(score:").count();
    assert_eq!(
        result_count, 1,
        "Expected exactly 1 result entry, got {}: {}",
        result_count, text
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_search_empty_query() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "docs_search",
            "arguments": {
                "query": ""
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("empty"));
}

#[tokio::test]
async fn test_mcp_tools_call_docs_search_missing_query() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "docs_search",
            "arguments": {}
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Missing required param → JSON-RPC error INVALID_PARAMS (-32602)
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], INVALID_PARAMS);
}

#[tokio::test]
async fn test_mcp_tools_call_docs_search_whitespace_query() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "ws_query",
        "method": "tools/call",
        "params": {
            "name": "docs_search",
            "arguments": {
                "query": "   "
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("empty"));
}

// ============================================================================
// POST /mcp — tools/call docs_headings
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_call_docs_headings() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "docs_headings",
            "arguments": {
                "path": "README.md"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("FileIndexr"));
    assert!(text.contains("Features"));
    assert!(text.contains("lines"));
}

#[tokio::test]
async fn test_mcp_tools_call_docs_headings_file_not_found() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "docs_headings",
            "arguments": {
                "path": "nonexistent.txt"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_headings_path_traversal() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "ht_traversal",
        "method": "tools/call",
        "params": {
            "name": "docs_headings",
            "arguments": {
                "path": "../Cargo.toml"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_headings_absolute_path() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "ht_absolute",
        "method": "tools/call",
        "params": {
            "name": "docs_headings",
            "arguments": {
                "path": "/etc/passwd"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside")
    );
}

// ============================================================================
// POST /mcp — tools/call docs_get
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_call_docs_get() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "hello.txt"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("hello.txt"));
    assert!(text.contains("Hello"));
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_with_lines() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "hello.txt",
                "start_line": 1,
                "end_line": 1
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Hello"));
    // Should NOT contain the second line
    assert!(!text.contains("Welcome"));
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_beyond_end() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "hello.txt",
                "start_line": 999
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    // Should return empty content (beyond file length)
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("hello.txt"));
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_auto_truncation() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "dg_truncate",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "large.txt"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("FILE TRUNCATED"));
    assert!(text.contains("Line 1"));
    assert!(!text.contains("Line 101")); // truncated
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_path_traversal() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "dg_traversal",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "../Cargo.toml"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_absolute_path() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "dg_absolute",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "/etc/passwd"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside")
    );
}

// ============================================================================
// POST /mcp — unknown tool
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_call_unknown_tool() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "tools/call",
        "params": {
            "name": "nonexistent_tool",
            "arguments": {}
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Unknown tool returns JSON-RPC error METHOD_NOT_FOUND (-32601)
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown tool")
    );
}

// ============================================================================
// POST /mcp — tools/call missing 'name' parameter
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_call_missing_name() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 14,
        "method": "tools/call",
        "params": {
            "arguments": { "query": "hello" }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], INVALID_PARAMS);
}

// ============================================================================
// POST /mcp — unknown method
// ============================================================================

#[tokio::test]
async fn test_mcp_unknown_method() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 15,
        "method": "unknown/method",
        "params": {}
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
    );
}

// ============================================================================
// POST /mcp — invalid JSON
// ============================================================================

#[tokio::test]
async fn test_mcp_invalid_json() {
    let state = make_test_state().await;
    let router = create_router(state);

    let (status, resp) = call_post(router, "/mcp", "not valid json at all".to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], PARSE_ERROR);
    // JSON-RPC 2.0: undetermined id must be serialized as "id": null.
    assert!(resp.get("id").is_some());
    assert!(resp["id"].is_null());
}

#[tokio::test]
async fn test_mcp_empty_body() {
    let state = make_test_state().await;
    let router = create_router(state);

    let (status, resp) = call_post(router, "/mcp", String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], PARSE_ERROR);
    // JSON-RPC 2.0: undetermined id must be serialized as "id": null.
    assert!(resp.get("id").is_some());
    assert!(resp["id"].is_null());
}

// ============================================================================
// POST /mcp — notification (no id, no response expected)
// ============================================================================

#[tokio::test]
async fn test_mcp_notification_returns_accepted() {
    let state = make_test_state().await;
    let router = create_router(state);

    let req_body = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });

    let body_bytes = Body::from(req_body.to_string().into_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Content-Type", "application/json")
        .body(body_bytes)
        .unwrap();
    let mut router = router;
    let response = tower::Service::<Request<Body>>::call(&mut router, request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

// ============================================================================
// POST /mcp — response message
// ============================================================================

#[tokio::test]
async fn test_mcp_response_message_returns_accepted() {
    let state = make_test_state().await;
    let router = create_router(state);

    let req_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {}
    });

    let body_bytes = Body::from(req_body.to_string().into_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Content-Type", "application/json")
        .body(body_bytes)
        .unwrap();
    let mut router = router;
    let response = tower::Service::<Request<Body>>::call(&mut router, request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

// ============================================================================
// POST /mcp — JSON-RPC id preservation
// ============================================================================

#[tokio::test]
async fn test_mcp_preserves_string_id() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "abc-123",
        "method": "tools/list",
        "params": {}
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "abc-123");
}

#[tokio::test]
async fn test_mcp_null_id_omitted_in_response() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": null,
        "method": "server/discover",
        "params": {}
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // serde deserializes null as None → field is omitted by skip_serializing_if
    assert!(resp.get("id").is_none());
}
