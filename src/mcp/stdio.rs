//! MCP (Model Context Protocol) server over STDIO transport.
//!
//! Reads JSON-RPC messages from stdin, writes responses to stdout.
//! Designed for integration with MCP clients (Claude Desktop, CLI tools, etc.).

use std::io::{self, BufRead, Write};

use tracing::{error, info};

use super::McpEngine;
use crate::AppState;

/// Run the MCP STDIO event loop.
///
/// Reads JSON-RPC requests from stdin, dispatches them through [`McpEngine`],
/// and writes JSON-RPC responses to stdout (one JSON per line).
pub async fn run_stdio_loop(state: AppState) -> anyhow::Result<()> {
    // Stdin/Stdout locks are not Send, so the blocking I/O loop must run
    // on a background thread via spawn_blocking.
    tokio::task::spawn_blocking(move || stdio_loop_inner(state)).await?
}

/// Inner synchronous loop that runs on a background thread but has access
/// to the current Tokio runtime for dispatching async handlers.
fn stdio_loop_inner(state: AppState) -> anyhow::Result<()> {
    let mut reader = io::stdin().lock();
    let mut stdout_lock = io::stdout().lock();
    let rt = tokio::runtime::Handle::current();

    info!("MCP STDIO mode: reading from stdin");

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            // stdin closed — the normal way out of the loop.
            Ok(0) => {
                info!("EOF on stdin — shutting down STDIO loop");
                break;
            }
            // Malformed client wrote non-UTF-8 bytes. The line's bytes are
            // fully consumed at this point, so skipping it cannot loop or
            // drop valid input.
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                error!(error = %e, "Invalid UTF-8 on stdin — skipping line");
                continue;
            }
            // Persistent read errors (EIO etc.) — retrying would hot-loop,
            // so surface the failure (non-zero exit).
            Err(e) => {
                error!(error = %e, "Read error on stdin — shutting down STDIO loop");
                return Err(e.into());
            }
            Ok(_) => {}
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse, dispatch, and build the response using the current runtime
        // handle (we're on a blocking thread).
        let resp = rt.block_on(McpEngine::handle_message(line.as_bytes(), "stdio", &state));

        // Write response to stdout (None = notification / client response, no reply).
        if let Some(resp) = resp {
            let mut resp_bytes = resp.to_bytes();
            resp_bytes.push(b'\n');
            match stdout_lock
                .write_all(&resp_bytes)
                .and_then(|_| stdout_lock.flush())
            {
                Ok(()) => {}
                // Client closed the pipe — the normal way a STDIO session ends.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    info!("Client disconnected (stdout closed) — shutting down STDIO loop");
                    break;
                }
                // Unexpected write failure — surface it as a real error.
                Err(e) => {
                    error!(error = %e, "Write error on stdout — shutting down STDIO loop");
                    return Err(e.into());
                }
            }
        }
    }

    Ok(())
}
