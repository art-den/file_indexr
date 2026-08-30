/// Embedded HTML page for the web UI.
const INDEX_HTML: &str = include_str!("../web/ui.html");

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, Uri};
use axum::middleware::{Next, from_fn};
use axum::response::{Html, Response};
use axum::routing::get;

use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::AppState;
use crate::mcp::{mcp_get_handler, mcp_post_handler};
use crate::web::handlers::{
    error_response, file_handler, search_handler, stats_handler, structure_handler,
};

/// Build the axum router with all routes.
pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/", get(Html(INDEX_HTML)))
        .route("/search", get(search_handler))
        .route("/file", get(file_handler))
        .route("/structure", get(structure_handler))
        .route("/stats", get(stats_handler))
        .route("/mcp", get(mcp_get_handler).post(mcp_post_handler))
        .fallback(not_found_handler)
        .with_state(state)
        .layer(cors)
        .layer(from_fn(log_request))
}

/// Fallback handler for unknown routes.
async fn not_found_handler(uri: Uri) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        format!(
            r#"Unknown endpoint: {}

Available endpoints:
  GET /         — Web UI
  GET /search   — Full-text search
  GET /file     — File content
  GET /structure — Document structure
  GET /stats    — Index statistics
  GET/POST /mcp  — MCP server
"#,
            uri.path()
        ),
    )
}

// ============================================================================
// Middleware: log incoming requests
// ============================================================================

async fn log_request(request: Request<Body>, next: Next) -> Response {
    let method = request.method().as_str();
    let uri = request.uri();
    // Skip logging for CORS preflight and MCP SSE polling.
    let skip_log = method == "OPTIONS" || (method == "GET" && uri.path() == "/mcp");
    if !skip_log {
        // Uri's Display renders origin-form targets as `path?query`.
        info!("{method} {uri}");
    }
    next.run(request).await
}
