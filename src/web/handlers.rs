//! Web UI HTTP handlers: search, file content, stats.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::AppState;
use crate::config::{Config, PathValidateResult};
use crate::formats::{FileFormat, FileTextData, TextDataCache, load_file_text_data};
use crate::search::{self, SearchParams, SortBy, SortOrder};

// ============================================================================
// Shared helpers
// ============================================================================

/// Build a JSON error response `{"error": message}` with the given status.
pub(crate) fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    (status, Json(json!({"error": message}))).into_response()
}

/// Handler error carrying a status code and message.
/// Rendered as a JSON `{"error": message}` response.
/// Kept small so it does not trigger `clippy::result_large_err`.
pub(crate) struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

async fn load_validated_file(
    config: &Config,
    path: &str,
    cache: &TextDataCache,
) -> Result<FileTextData, HttpError> {
    let canonical_path = match config.validate_path(path).await {
        PathValidateResult::Valid(p) => p,
        PathValidateResult::OutsideDirectory => {
            return Err(HttpError::new(
                StatusCode::FORBIDDEN,
                "Path outside watched directory",
            ));
        }
        PathValidateResult::NotFound => {
            return Err(HttpError::new(StatusCode::NOT_FOUND, "File not found"));
        }
    };

    load_file_text_data(cache, &canonical_path)
        .await
        .map_err(|err| {
            warn!("Failed to load file {:?}: {err}", canonical_path);
            HttpError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load file: {err}"),
            )
        })
}

// ============================================================================
// Query parameter structs
// ============================================================================

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    q: Option<String>,
    ext: Option<String>,
    path: Option<String>,
    search_content: Option<bool>,
    max_results: Option<usize>,
    sort_by: Option<String>,
    sort_order: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    path: String,
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /search - full-text search against the index.
pub(crate) async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Result<impl IntoResponse, HttpError> {
    let SearchQuery {
        q,
        ext,
        path,
        search_content,
        max_results,
        sort_by,
        sort_order,
    } = params;

    // `search::search` clamps the limit to [1, MAX_RESULTS_LIMIT] itself.
    let max_results = max_results.unwrap_or(search::DEFAULT_MAX_RESULTS);

    let sort_by = match sort_by.as_deref() {
        Some("modified") => SortBy::Modified,
        Some("size") => SortBy::Size,
        Some("path") => SortBy::Path,
        _ => SortBy::Score,
    };

    let sort_order = match sort_order.as_deref() {
        Some("asc") => SortOrder::Asc,
        _ => SortOrder::Desc,
    };

    // The web UI renders snippets as HTML with <mark> highlighting.
    search::search(
        &state.reader,
        SearchParams {
            q: q.unwrap_or_default(),
            search_content: search_content.unwrap_or(true),
            ext,
            path,
            max_results,
            sort_by,
            sort_order,
        },
        &state.config.directory,
        &state.text_data_cache,
        true,
    )
    .await
    .map_err(|err| {
        warn!("Search error: {err}");
        HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    })
    .map(Json)
}

/// GET /file - serve normalized file content via `formats` module.
/// HTML/PDF/EPUB are converted to Markdown; unsupported extensions return an error.
pub(crate) async fn file_handler(
    State(state): State<AppState>,
    Query(params): Query<FileQuery>,
) -> Result<impl IntoResponse, HttpError> {
    let data = load_validated_file(&state.config, &params.path, &state.text_data_cache).await?;

    let content_type = content_type_for_format(data.format);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        String::clone(&data.text),
    ))
}

/// GET /structure - return document structure (headings) for a file.
pub(crate) async fn structure_handler(
    State(state): State<AppState>,
    Query(params): Query<FileQuery>,
) -> Result<impl IntoResponse, HttpError> {
    let data = load_validated_file(&state.config, &params.path, &state.text_data_cache).await?;
    Ok((StatusCode::OK, Json(data.structure())))
}

/// GET /stats - return index statistics.
pub(crate) async fn stats_handler(State(state): State<AppState>) -> Response {
    // Blocking reload is offloaded to a dedicated thread pool so the
    // tokio worker is not starved when the index has many segments.
    let reader_clone = state.reader.clone();
    match tokio::task::spawn_blocking(move || reader_clone.reload()).await {
        Ok(Err(e)) => warn!("Failed to reload reader for stats: {e}"),
        Err(e) => warn!("Reload task for stats failed: {e}"),
        Ok(Ok(())) => {}
    }
    let total = state.reader.searcher().num_docs();
    (StatusCode::OK, Json(json!({"total_files_indexed": total}))).into_response()
}

/// Content-Type for the normalized body based on the actual file format.
/// The body is always text: HTML/PDF/EPUB are converted to Markdown, so
/// never serve it as `text/html`/`application/pdf`.
fn content_type_for_format(format: FileFormat) -> &'static str {
    match format {
        FileFormat::Html
        | FileFormat::Pdf
        | FileFormat::Epub
        | FileFormat::Rst
        | FileFormat::Markdown => "text/markdown; charset=utf-8",
        FileFormat::Text | FileFormat::Python | FileFormat::Rust => "text/plain; charset=utf-8",
    }
}
