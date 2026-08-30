//! Integration tests for the axum router: web UI index page and 404 fallback.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use file_indexr::AppState;
use file_indexr::api::create_router;
use file_indexr::config::Config;
use file_indexr::index::writer::IndexWriterWrapper;

/// Make a unique temp dir for this test.
fn make_temp_dir() -> std::path::PathBuf {
    file_indexr::testutil::unique_temp_dir("api_test")
}

/// Call the router with a GET request, returning status, headers and body bytes.
async fn call_get(router: axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let mut router = router;
    let response = tower::Service::<Request<Body>>::call(&mut router, request)
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

/// Create minimal test state with an empty index.
async fn make_test_state() -> AppState {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();

    let config = Arc::new(Config {
        directory: watch_dir,
        index_path: index_dir,
        port: 8080,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,
        allowed_extensions: vec![],
    });

    let text_data_cache = file_indexr::formats::new_text_data_cache();
    let writer =
        IndexWriterWrapper::new(&config.index_path, config.clone(), text_data_cache.clone())
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

/// Create test state and index the given `(relative_path, content)` files.
async fn make_state_with_files(files: &[(&str, &str)]) -> (AppState, std::path::PathBuf) {
    let state = make_test_state().await;
    let watch_dir = state.config.directory.clone();
    for (rel_path, content) in files {
        tokio::fs::write(watch_dir.join(rel_path), content)
            .await
            .unwrap();
        state
            .writer
            .add_file(watch_dir.join(rel_path))
            .await
            .unwrap();
    }
    state.writer.commit().await;
    (state, watch_dir)
}

/// Search `/search` through the router and return the parsed response.
async fn call_search(state: &AppState, uri: &str) -> serde_json::Value {
    let router = create_router(state.clone());
    let (status, _headers, body) = call_get(router, uri).await;
    assert_eq!(status, StatusCode::OK, "GET {uri} should succeed");
    serde_json::from_slice(&body).unwrap()
}

/// Extract the `size` field of each search result, in result order.
fn result_sizes(response: &serde_json::Value) -> Vec<u64> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["size"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn test_index_serves_web_ui() {
    let state = make_test_state().await;
    let router = create_router(state);

    let (status, headers, body) = call_get(router, "/").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    let html = String::from_utf8(body).unwrap();
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "body should be the embedded web UI"
    );
}

#[tokio::test]
async fn test_unknown_endpoint_returns_404_json() {
    let state = make_test_state().await;
    let router = create_router(state);

    let (status, headers, body) = call_get(router, "/does/not/exist").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let error = value["error"].as_str().unwrap();
    assert!(error.contains("Unknown endpoint: /does/not/exist"));
    assert!(error.contains("Available endpoints"));
}

#[tokio::test]
async fn test_file_endpoint_rejects_path_outside_directory() {
    let state = make_test_state().await;
    let router = create_router(state);

    let (status, _headers, body) = call_get(router, "/file?path=../outside.txt").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"], "Path outside watched directory");
}

#[tokio::test]
async fn test_file_endpoint_missing_file_returns_404() {
    let state = make_test_state().await;
    let router = create_router(state);

    let (status, _headers, body) = call_get(router, "/file?path=nonexistent.txt").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"], "File not found");
}

#[tokio::test]
async fn test_search_sort_parameter_mapping() {
    // Distinct sizes make the sort order observable in the response.
    let big_content = "alpha\n".repeat(50);
    let (state, _watch_dir) =
        make_state_with_files(&[("small.md", "alpha\n"), ("big.md", big_content.as_str())]).await;

    let ascending = call_search(&state, "/search?q=alpha&sort_by=size&sort_order=asc").await;
    assert_eq!(ascending["total"], 2);
    let sizes = result_sizes(&ascending);
    assert!(sizes[0] < sizes[1]);

    let descending = call_search(&state, "/search?q=alpha&sort_by=size&sort_order=desc").await;
    assert_eq!(descending["total"], 2);
    let sizes = result_sizes(&descending);
    assert!(sizes[0] > sizes[1]);

    // Unknown sort values fall back to the defaults instead of erroring.
    let bogus = call_search(&state, "/search?q=alpha&sort_by=bogus&sort_order=bogus").await;
    assert_eq!(bogus["total"], 2);

    let default = call_search(&state, "/search?q=alpha").await;
    assert_eq!(default["total"], 2);
}
