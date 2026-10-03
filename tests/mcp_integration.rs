use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::http::StatusCode;
use base64::Engine;
use file_indexr::config::Config;
use file_indexr::index::writer::IndexWriterWrapper;
use file_indexr::mcp::jsonrpc::{INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR};
use file_indexr::{AppState, api::create_router, mcp};
use serde_json::json;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("mcp_test")
}

/// A minimal valid 1x1 PNG, base64-encoded.
const TEST_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// Build a minimal EPUB whose document references `images/pic.png` relative
/// to its own location; the actual archive entry is `OEBPS/Text/images/pic.png`
/// (so document-relative requests need suffix resolution).
fn build_epub_bytes(png: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let container = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
    <item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="Text/images/pic.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    let xhtml = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
<h1>Chapter</h1>
<p>See <img src="images/pic.png" alt="pic"/> in this chapter.</p>
</body></html>"#;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, content) in [
        ("META-INF/container.xml", container.as_bytes()),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/Text/ch1.xhtml", xhtml.as_bytes()),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(content).unwrap();
    }
    writer
        .start_file("OEBPS/Text/images/pic.png", options)
        .unwrap();
    writer.write_all(png).unwrap();
    writer.finish().unwrap().into_inner()
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

/// Assert that `item` is an MCP image content of `mime_type` whose base64
/// payload decodes to exactly the `expected` bytes.
fn assert_image_item(item: &serde_json::Value, mime_type: &str, expected: &[u8]) {
    assert_eq!(item["type"], "image");
    assert_eq!(item["mimeType"], mime_type);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(item["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, expected);
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

    // 1x1 PNG for image tool testing
    let png = base64::engine::general_purpose::STANDARD
        .decode(TEST_PNG_B64)
        .unwrap();
    std::fs::write(watch_dir.join("pixel.png"), &png).unwrap();
    // EPUB containing that image for virtual-path testing
    std::fs::write(watch_dir.join("book.epub"), build_epub_bytes(&png)).unwrap();

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
    // An empty (but present) query is a tool execution failure, not a
    // protocol error: isError result, no JSON-RPC error.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);

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
    assert_eq!(resp["result"]["isError"], true);

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("empty"));
}

// ============================================================================
// POST /mcp — tools/call docs_get
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
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
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
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
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
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
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
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
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
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outside")
    );
}

// ============================================================================
// POST /mcp — unknown tool
// ============================================================================

#[tokio::test]
async fn test_mcp_tools_call_docs_get_directory() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    std::fs::create_dir(watch_dir.join("subdir")).unwrap();
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "dg_dir",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "subdir"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not a regular file")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_empty_path() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "dg_empty",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": ""
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("file not found")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_image() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "pixel.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    // Round-trip: the decoded bytes must match the file on disk.
    assert_image_item(
        &resp["result"]["content"][0],
        "image/png",
        &std::fs::read(watch_dir.join("pixel.png")).unwrap(),
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_image_too_large() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    // Exceed the configured max_file_size_mb (2 → 2_000_000 bytes).
    let big = vec![0u8; state.config.max_file_size_bytes() as usize + 1];
    std::fs::write(watch_dir.join("big.png"), &big).unwrap();
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "img_large",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "big.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("too large")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_image_at_size_limit() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    // Exactly at the limit — must succeed (the limit is inclusive).
    let exact = vec![0u8; state.config.max_file_size_bytes() as usize];
    std::fs::write(watch_dir.join("exact.png"), &exact).unwrap();
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "img_exact",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "exact.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    assert_image_item(&resp["result"]["content"][0], "image/png", &exact);
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_image_doc_relative() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    let router = create_router(state);

    // The document writes `images/pic.png`; the archive entry is
    // `OEBPS/Text/images/pic.png` — resolved via unique suffix match.
    let body = json!({
        "jsonrpc": "2.0",
        "id": 14,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "book.epub/images/pic.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());

    // The decoded bytes must match the PNG stored inside the archive.
    assert_image_item(
        &resp["result"]["content"][0],
        "image/png",
        &std::fs::read(watch_dir.join("pixel.png")).unwrap(),
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_image_literal() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    let router = create_router(state);

    // The literal archive entry path also works.
    let body = json!({
        "jsonrpc": "2.0",
        "id": 15,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "book.epub/OEBPS/Text/images/pic.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    assert_image_item(
        &resp["result"]["content"][0],
        "image/png",
        &std::fs::read(watch_dir.join("pixel.png")).unwrap(),
    );
}

/// Full agent cycle: read the book text, extract the image link from the
/// converted Markdown, and request the image by exactly that path.
#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_image_end_to_end() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    let router = create_router(state.clone());

    // 1. The agent reads the book.
    let body = json!({
        "jsonrpc": "2.0",
        "id": 16,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": { "path": "book.epub" }
        }
    });
    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("Chapter"), "unexpected book text: {text}");

    // 2. The agent extracts the image link from the converted Markdown.
    let img = text.find("![").expect("converted text has no image link");
    let rest = &text[img + 2..];
    let src = rest
        .find("](")
        .map_or("", |i| &rest[i + 2..])
        .split(')')
        .next()
        .unwrap_or("");
    assert!(!src.is_empty(), "no image link src in: {text}");

    // 3. The agent requests the image by that link, relative to the EPUB.
    let router = create_router(state);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 17,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": { "path": format!("book.epub/{src}") }
        }
    });
    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp["error"].is_null(),
        "unexpected error: {}",
        resp["error"]
    );

    // The served bytes must be the PNG stored inside the archive.
    assert_image_item(
        &resp["result"]["content"][0],
        "image/png",
        &std::fs::read(watch_dir.join("pixel.png")).unwrap(),
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_image_not_found() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "epub_nf",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "book.epub/missing.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not found in EPUB archive")
    );
}

/// Create an EPUB with an image, index it, then fetch the image via docs_get.
#[tokio::test]
async fn test_mcp_tools_call_docs_get_indexed_epub_image() {
    let state = make_test_state().await;

    // Index the book (make_test_state writes it to disk but does not index it;
    // add_file returns Ok even when skipping, so verify the index below).
    state
        .writer
        .add_file(state.config.directory.join("book.epub"))
        .await
        .unwrap();
    state.writer.commit().await;

    let res = file_indexr::search::search(
        &state.reader,
        file_indexr::search::SearchParams {
            q: "chapter".to_string(),
            ..Default::default()
        },
        &state.config.directory,
        &state.text_data_cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();
    assert!(
        res.results.iter().any(|r| r.path == "book.epub"),
        "book.epub was not indexed; got: {:?}",
        res.results
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>()
    );

    // The image must be fetchable through the virtual path after indexing.
    let router = create_router(state);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 18,
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "book.epub/images/pic.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    assert_image_item(
        &resp["result"]["content"][0],
        "image/png",
        &base64::engine::general_purpose::STANDARD
            .decode(TEST_PNG_B64)
            .unwrap(),
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_virtual_rejects_non_image() {
    let state = make_test_state().await;
    let router = create_router(state);

    // The extension check is the only guard against extracting arbitrary
    // archive entries via the virtual path — pin it.
    let body = json!({
        "jsonrpc": "2.0",
        "id": "epub_nonimage",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "book.epub/OEBPS/Text/ch1.xhtml"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unsupported format: xhtml")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_dir_falls_through_to_disk() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();

    // A directory named `x.epub` must NOT trigger the virtual branch:
    // the on-disk file inside it is served through the normal flow.
    std::fs::create_dir(watch_dir.join("fake.epub")).unwrap();
    std::fs::write(
        watch_dir.join("fake.epub/pic.png"),
        base64::engine::general_purpose::STANDARD
            .decode(TEST_PNG_B64)
            .unwrap(),
    )
    .unwrap();
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "epub_dir",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "fake.epub/pic.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["error"].is_null());
    assert_image_item(
        &resp["result"]["content"][0],
        "image/png",
        &std::fs::read(watch_dir.join("fake.epub/pic.png")).unwrap(),
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_epub_missing_archive() {
    let state = make_test_state().await;
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "epub_noarchive",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "nothere.epub/x.png"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("file not found")
    );
}

#[tokio::test]
async fn test_mcp_tools_call_docs_get_unsupported_format() {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    std::fs::write(watch_dir.join("data.bin"), b"binary").unwrap();
    let router = create_router(state);

    let body = json!({
        "jsonrpc": "2.0",
        "id": "bin_format",
        "method": "tools/call",
        "params": {
            "name": "docs_get",
            "arguments": {
                "path": "data.bin"
            }
        }
    });

    let (status, resp) = call_post(router, "/mcp", body.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    // Per the MCP spec, tool execution errors are results with
    // isError: true, not JSON-RPC protocol errors.
    assert!(resp["error"].is_null());
    assert_eq!(resp["result"]["isError"], true);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unsupported format")
    );
}

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
