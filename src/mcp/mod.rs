//! MCP (Model Context Protocol) server — shared logic + HTTP transport.
//!
//! Handles JSON-RPC messages for both:
//! - **Streamable HTTP** (`POST /mcp`, `GET /mcp` SSE stub)
//! - **STDIO** (`src/mcp/stdio.rs`)

pub mod jsonrpc;
pub mod stdio;
pub mod tools;

use std::borrow::Cow;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use itertools::Itertools;
use serde_json::json;
use tracing::warn;

use self::jsonrpc::{
    Error as JsonRpcError, INVALID_PARAMS, METHOD_NOT_FOUND, Message, Response as JsonRpcResponse,
    parse_message,
};
use self::tools::Tools;
use crate::AppState;
use crate::utils::truncate_bytes;

/// Protocol versions supported by the MCP server.
pub const MCP_VERSION_LEGACY: &str = "2025-11-25";
pub const MCP_VERSION_MODERN: &str = "2026-07-28";

/// Maximum byte length of a string argument kept in MCP request logs.
const MCP_LOG_VALUE_MAX_BYTES: usize = 128;

// ============================================================================
// Reusable engine — works independently of transport layer
// ============================================================================

/// Shared MCP handler logic, usable by both HTTP and STDIO transports.
pub struct McpEngine;

impl McpEngine {
    /// Parse an incoming message, dispatch it, and build the JSON-RPC response.
    ///
    /// Returns `None` for notifications and client responses (no reply is sent).
    pub async fn handle_message(
        body: &[u8],
        transport: &str,
        state: &AppState,
    ) -> Option<JsonRpcResponse> {
        let message = match parse_message(body) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e.message, "Invalid JSON-RPC message");
                return Some(JsonRpcResponse::err_undetermined(e));
            }
        };

        match message {
            Message::Request(req) => {
                log_mcp_request(&req.method, &req.params, transport);
                let result = Self::resolve_method(&req.method, &req.params, state).await;
                Some(match result {
                    Ok(value) => JsonRpcResponse::ok(req.id, value),
                    Err(e) => JsonRpcResponse::err(req.id, e),
                })
            }
            Message::Notification(_) | Message::Response => None,
        }
    }

    /// Resolve an MCP method and return the result or error.
    pub async fn resolve_method(
        method: &str,
        params: &serde_json::Value,
        state: &AppState,
    ) -> Result<serde_json::Value, JsonRpcError> {
        match method {
            // Modern protocol
            "server/discover" => Ok(Self::handle_server_discover()),
            // Legacy protocol (MCP_VERSION_LEGACY and earlier)
            "initialize" => Ok(Self::handle_initialize()),
            // Common methods
            "tools/list" => Ok(Self::handle_tools_list()),
            "tools/call" => Self::handle_tools_call(params, state).await,
            _ => {
                warn!(method = %method, "Unknown MCP method");
                Err(JsonRpcError::new(
                    METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                ))
            }
        }
    }

    fn handle_initialize() -> serde_json::Value {
        json!({
            "protocolVersion": MCP_VERSION_LEGACY,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": Self::server_info()
        })
    }

    fn handle_server_discover() -> serde_json::Value {
        json!({
            "protocolVersion": MCP_VERSION_MODERN,
            "serverCapabilities": {
                "tools": {}
            },
            "serverInfo": Self::server_info(),
            "supportedVersions": [MCP_VERSION_MODERN, MCP_VERSION_LEGACY]
        })
    }

    /// Common `serverInfo` object shared by `initialize` and `server/discover`.
    fn server_info() -> serde_json::Value {
        json!({
            "name": "file_indexr",
            "version": env!("CARGO_PKG_VERSION")
        })
    }

    fn handle_tools_list() -> serde_json::Value {
        json!({
            "tools": Tools::list()
        })
    }

    async fn handle_tools_call(
        params: &serde_json::Value,
        state: &AppState,
    ) -> Result<serde_json::Value, JsonRpcError> {
        let tool_name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, "Missing parameter 'name'"))?;

        // `arguments` is optional; `Null` behaves like an empty object in the tools.
        Tools::call(
            tool_name,
            params.get("arguments").unwrap_or(&serde_json::Value::Null),
            state,
        )
        .await
    }
}

// ============================================================================
// HTTP transport — these are exported for use in api/mod.rs
// ============================================================================

/// Handle POST /mcp — JSON-RPC request from client.
pub async fn mcp_post_handler(State(state): State<AppState>, body: Bytes) -> Response {
    match McpEngine::handle_message(&body, "http", &state).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// Handle GET /mcp — open an SSE stream for server-to-client communication.
pub async fn mcp_get_handler() -> Response {
    Sse::new(futures::stream::empty::<Result<Event, anyhow::Error>>()).into_response()
}

/// Log an incoming MCP request with transport tag.
fn log_mcp_request(method: &str, params: &serde_json::Value, transport: &str) {
    if method == "tools/call" {
        let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let args_str = params
            .get("arguments")
            .and_then(|a| a.as_object())
            .map(|args| {
                let parts = args
                    .iter()
                    .filter_map(|(k, v)| compact_value(v).map(|val| format!("{k}={val}")))
                    .join(", ");
                format!(" ({parts})")
            })
            .unwrap_or_default();
        tracing::info!(method = %method, ?tool_name, %args_str, "MCP request [{transport}]");
    } else {
        tracing::info!(method = %method, "MCP request [{transport}]");
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn compact_value(v: &serde_json::Value) -> Option<Cow<'_, str>> {
    match v {
        serde_json::Value::String(s) => {
            if s.len() <= MCP_LOG_VALUE_MAX_BYTES {
                Some(Cow::Borrowed(s))
            } else {
                Some(Cow::Owned(format!(
                    "{}…",
                    truncate_bytes(s.as_bytes(), MCP_LOG_VALUE_MAX_BYTES)
                )))
            }
        }
        serde_json::Value::Number(n) => Some(Cow::Owned(n.to_string())),
        serde_json::Value::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
        _ => None,
    }
}
