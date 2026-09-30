//! FileIndexr — local search engine for files with full-text search, web UI, and MCP integration.

use std::sync::Arc;

use tantivy::IndexReader;

use crate::config::Config;
use crate::index::writer::IndexWriterWrapper;

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub reader: Arc<IndexReader>,
    pub config: Arc<Config>,
    pub writer: Arc<IndexWriterWrapper>,
    /// LRU cache of normalized file text, shared by indexing, search and MCP tools.
    pub text_data_cache: formats::TextDataCache,
}

pub mod api;
pub mod change;
pub mod config;
pub mod formats;
pub mod index;
pub mod mcp;
pub mod schema;
pub mod search;
#[doc(hidden)]
pub mod testutil;
pub mod utils;
pub mod watch;
pub mod web;
