use futures::StreamExt;
use serde::Serialize;
use tantivy::Order;
use tantivy::collector::{Count, TopDocs};
use tantivy::schema::Value;
use tantivy::snippet::SnippetGenerator;
use tantivy::{DocAddress, Score};
use thiserror::Error;
use tracing::debug;

use crate::schema::field;

#[cfg(test)]
#[path = "tests/tests_search.rs"]
mod tests;

/// Errors produced by the search subsystem.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Internal error — index corruption, I/O failure, panicked worker, etc.
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Default max results for REST API searches.
pub const DEFAULT_MAX_RESULTS: usize = 50;

/// Default max results for MCP searches.
pub const MCP_DEFAULT_MAX_RESULTS: usize = 10;

/// Hard upper bound on search results.
pub const MAX_RESULTS_LIMIT: usize = 100;

/// Tags wrapping matched terms in HTML snippets (the web UI styles `mark` via CSS).
const SNIPPET_MARK: (&str, &str) = ("<mark>", "</mark>");

/// Max number of snippets generated in parallel (via `StreamExt::buffered`).
/// Each item's PDF/HTML/EPUB conversion runs on the blocking pool (plain file
/// reads are async I/O), so this only bounds the number of overlapping
/// conversions; result order is preserved.
const SNIPPET_CONCURRENCY: usize = 8;

/// Search parameters from the HTTP API.
#[derive(Debug, Clone)]
pub struct SearchParams {
    pub q: String,
    pub search_content: bool,
    pub ext: Option<String>,
    pub path: Option<String>,
    pub max_results: usize,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
}

#[derive(Debug, Clone, Copy)]
pub enum SortBy {
    Score,
    Modified,
    Size,
    Path,
}

#[derive(Debug, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Desc,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            q: String::new(),
            search_content: true,
            ext: None,
            path: None,
            max_results: DEFAULT_MAX_RESULTS,
            sort_by: SortBy::Score,
            sort_order: SortOrder::Desc,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct SearchResultItem {
    pub path: String,
    pub filename: String,
    pub dir: String,
    pub size: u64,
    pub modified: String,
    pub extension: String,
    pub has_content: bool,
    pub score: f32,
    /// Text snippet of the matched fragment.
    /// When the search was run with `snippets_as_html`, matched terms are wrapped
    /// in <mark>...</mark> and the content is HTML-escaped (safe for innerHTML);
    /// otherwise this is plain text.
    pub snippet: String,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub total: usize,
    /// Search duration in milliseconds.
    pub search_time_ms: f64,
    pub limit_applied: usize,
    pub results: Vec<SearchResultItem>,
}

/// Execute a search against the index.
///
/// When `snippets_as_html` is set, result snippets are HTML with matched terms
/// wrapped in `<mark>...</mark>` (content HTML-escaped); otherwise plain text.
///
/// `max_bytes` bounds snippet generation: a file can grow beyond the indexing
/// limit after it was indexed, so oversized files are skipped rather than read
/// into memory (they keep their index entry, just without a snippet).
pub async fn search(
    reader: &tantivy::IndexReader,
    params: SearchParams,
    watched_dir: &std::path::Path,
    text_data_cache: &crate::formats::TextDataCache,
    snippets_as_html: bool,
    max_bytes: u64,
) -> Result<SearchResponse, SearchError> {
    let start = std::time::Instant::now();

    // Blocking reload is offloaded to a dedicated thread pool so the
    // tokio worker is not starved when the index has many sections.
    let reader_clone = reader.clone();
    tokio::task::spawn_blocking(move || reader_clone.reload())
        .await
        .map_err(|e| SearchError::Internal(format!("reload task failed: {}", e)))?
        .map_err(|e| SearchError::Internal(format!("reload failed: {}", e)))?;

    let searcher = reader.searcher();

    let limit = params.max_results.clamp(1, MAX_RESULTS_LIMIT);

    // The whole synchronous search phase (query building, hit count, top docs,
    // stored-field reads) is CPU-heavy and runs on the blocking pool so the
    // tokio workers stay responsive for the HTTP server, watcher, and MCP.
    let phase = tokio::task::spawn_blocking(move || run_search_phase(&searcher, &params, limit))
        .await
        .map_err(|e| SearchError::Internal(format!("search task failed: {e}")))?;
    let (total_hits, raw_items, snippet_gen) = phase?;

    // Generate snippets with bounded concurrency: each item's PDF/HTML/EPUB
    // conversion runs on the blocking pool, so overlapping the items turns a
    // sum of latencies into ~the slowest one. `buffered` (unlike
    // `buffer_unordered`) preserves the input order of the results.
    let snippet_gen = snippet_gen.as_ref();
    let items: Vec<SearchResultItem> = futures::stream::iter(raw_items)
        .map(|raw| async move {
            let snippet = if raw.has_content {
                generate_snippet_from_file(
                    snippet_gen,
                    watched_dir,
                    &raw.path,
                    text_data_cache,
                    snippets_as_html,
                    max_bytes,
                )
                .await
                .unwrap_or_default()
            } else {
                String::new()
            };
            SearchResultItem {
                path: raw.path,
                filename: raw.filename,
                dir: raw.dir,
                size: raw.size,
                modified: raw.modified,
                extension: raw.extension,
                has_content: raw.has_content,
                score: raw.score,
                snippet,
            }
        })
        .buffered(SNIPPET_CONCURRENCY)
        .collect()
        .await;

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    Ok(SearchResponse {
        total: total_hits,
        search_time_ms: elapsed_ms,
        limit_applied: limit,
        results: items,
    })
}

/// Fallback query builder: tokenizes `text` and creates a `BooleanQuery`
/// where each term is a `Should` clause across the given fields.
fn build_term_query(
    text: &str,
    fields: &[tantivy::schema::Field],
) -> Box<dyn tantivy::query::Query> {
    let lower = text.to_lowercase();
    let mut clauses: Vec<(tantivy::query::Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
    for token in lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
    {
        for &field in fields {
            let term = tantivy::Term::from_field_text(field, token);
            clauses.push((
                tantivy::query::Occur::Should,
                Box::new(tantivy::query::TermQuery::new(
                    term,
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ));
        }
    }

    if clauses.is_empty() {
        Box::new(tantivy::query::EmptyQuery)
    } else {
        Box::new(tantivy::query::BooleanQuery::new(clauses))
    }
}

/// Parse a query, falling back to term-level matching on syntax errors.
fn parse_query_lenient(
    parser: &tantivy::query::QueryParser,
    query: &str,
    fallback_fields: &[tantivy::schema::Field],
) -> Box<dyn tantivy::query::Query> {
    parser.parse_query(query).unwrap_or_else(|_| {
        debug!(query = %query, "Query parse failed, using term-level fallback");
        build_term_query(query, fallback_fields)
    })
}

/// Wrap `query` and `filter` in a `BooleanQuery` requiring both to match.
fn must(
    query: Box<dyn tantivy::query::Query>,
    filter: Box<dyn tantivy::query::Query>,
) -> Box<dyn tantivy::query::Query> {
    Box::new(tantivy::query::BooleanQuery::new(vec![
        (tantivy::query::Occur::Must, query),
        (tantivy::query::Occur::Must, filter),
    ]))
}

/// Stored-field data extracted for a single search hit, before snippet generation.
struct RawSearchResult {
    path: String,
    filename: String,
    dir: String,
    size: u64,
    modified: String,
    extension: String,
    has_content: bool,
    score: Score,
}

/// Synchronous phase of a search: build the query, count hits, collect the
/// top docs, and extract stored fields. Runs on the blocking pool (see `search`).
fn run_search_phase(
    searcher: &tantivy::Searcher,
    params: &SearchParams,
    limit: usize,
) -> Result<(usize, Vec<RawSearchResult>, Option<SnippetGenerator>), SearchError> {
    let schema = searcher.index().schema();

    let path_field = schema
        .get_field(field::PATH)
        .map_err(|e| SearchError::Internal(e.to_string()))?;
    let filename_field = schema
        .get_field(field::FILENAME)
        .map_err(|e| SearchError::Internal(e.to_string()))?;
    let content_field = schema.get_field(field::CONTENT).ok();

    // Build query using Tantivy's query parser
    let search_fields = [path_field, filename_field];

    let query_parser =
        tantivy::query::QueryParser::for_index(searcher.index(), search_fields.to_vec());

    // Build base query
    let is_empty_query = params.q.is_empty();
    let mut query: Box<dyn tantivy::query::Query> = if is_empty_query {
        Box::new(tantivy::query::AllQuery)
    } else {
        let text_query = parse_query_lenient(&query_parser, &params.q, &search_fields);

        // If content search is enabled, combine text + content queries
        match (params.search_content, content_field) {
            (true, Some(cf)) => {
                let content_parser =
                    tantivy::query::QueryParser::for_index(searcher.index(), vec![cf]);
                let content_query = parse_query_lenient(&content_parser, &params.q, &[cf]);
                Box::new(tantivy::query::BooleanQuery::new(vec![
                    (tantivy::query::Occur::Should, text_query),
                    (tantivy::query::Occur::Should, content_query),
                ]))
            }
            _ => text_query,
        }
    };

    // Apply extension filter (always)
    if let Some(ext) = &params.ext {
        let extension_field = schema
            .get_field(field::EXTENSION)
            .map_err(|e| SearchError::Internal(e.to_string()))?;
        let term = tantivy::Term::from_field_text(extension_field, &ext.to_lowercase());
        let ext_query = Box::new(tantivy::query::TermQuery::new(
            term,
            tantivy::schema::IndexRecordOption::Basic,
        ));
        query = must(query, ext_query);
    }

    // Apply path filter (search in tokenized path field)
    if let Some(path_filter) = &params.path {
        let sub_path_parser =
            tantivy::query::QueryParser::for_index(searcher.index(), vec![path_field]);
        let path_query = parse_query_lenient(&sub_path_parser, path_filter, &[path_field]);
        query = must(query, path_query);
    }

    // Execute the search in a single pass: `Count` totals the hits while the
    // top-docs collector (chosen by `sort_by`) keeps the top `limit` hits.
    let order = match params.sort_order {
        SortOrder::Asc => Order::Asc,
        SortOrder::Desc => Order::Desc,
    };

    let (total_hits, doc_addresses, scores): (usize, Vec<DocAddress>, Vec<Score>) = match params
        .sort_by
    {
        SortBy::Score => split_top_docs(
            searcher,
            &*query,
            (Count, TopDocs::with_limit(limit).order_by_score()),
        )?,
        SortBy::Modified => {
            let (total_hits, doc_addresses, _) = split_top_docs(
                searcher,
                &*query,
                (
                    Count,
                    TopDocs::with_limit(limit)
                        .order_by_fast_field::<tantivy::DateTime>(field::MODIFIED, order),
                ),
            )?;
            (total_hits, doc_addresses, Vec::new())
        }
        SortBy::Size => {
            let (total_hits, doc_addresses, _) = split_top_docs(
                searcher,
                &*query,
                (
                    Count,
                    TopDocs::with_limit(limit).order_by_u64_field(field::SIZE, order),
                ),
            )?;
            (total_hits, doc_addresses, Vec::new())
        }
        SortBy::Path => {
            let (total_hits, doc_addresses, _) = split_top_docs(
                searcher,
                &*query,
                (
                    Count,
                    TopDocs::with_limit(limit).order_by_string_fast_field(field::PATH_EXACT, order),
                ),
            )?;
            (total_hits, doc_addresses, Vec::new())
        }
    };

    // Build snippet generator only when there's an actual query
    let snippet_gen = if !is_empty_query {
        content_field.and_then(|cf| SnippetGenerator::create(searcher, &*query, cf).ok())
    } else {
        None
    };

    let raw_items = extract_raw_items(searcher, &doc_addresses, &scores, schema);

    Ok((total_hits, raw_items, snippet_gen))
}

/// Runs the search with the given collector and splits the collected hits
/// into doc addresses and per-hit sort keys (BM25 scores for score-sorting).
fn split_top_docs<K>(
    searcher: &tantivy::Searcher,
    query: &dyn tantivy::query::Query,
    collector: impl tantivy::collector::Collector<Fruit = (usize, Vec<(K, DocAddress)>)>,
) -> Result<(usize, Vec<DocAddress>, Vec<K>), SearchError> {
    let (total_hits, results) = searcher
        .search(query, &collector)
        .map_err(|e| SearchError::Internal(e.to_string()))?;
    let (keys, doc_addresses) = results.into_iter().unzip();
    Ok((total_hits, doc_addresses, keys))
}

/// Extract stored fields for the given doc addresses (runs on the blocking
/// pool as part of `run_search_phase`).
fn extract_raw_items(
    searcher: &tantivy::Searcher,
    doc_addresses: &[DocAddress],
    scores: &[Score],
    schema: tantivy::schema::Schema,
) -> Vec<RawSearchResult> {
    let path_field = schema.get_field(field::PATH).ok();
    let filename_field = schema.get_field(field::FILENAME).ok();
    let dir_field = schema.get_field(field::DIR).ok();
    let size_field = schema.get_field(field::SIZE).ok();
    let extension_field = schema.get_field(field::EXTENSION).ok();
    let has_content_field = schema.get_field(field::HAS_CONTENT).ok();

    let mut items = Vec::with_capacity(doc_addresses.len());
    for (idx, doc_address) in doc_addresses.iter().enumerate() {
        let score = scores.get(idx).copied().unwrap_or_default();
        let doc = searcher
            .doc::<tantivy::TantivyDocument>(*doc_address)
            .unwrap_or_default();

        let get_str = |f: Option<tantivy::schema::Field>| {
            f.and_then(|f| doc.get_first(f))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };

        let path = get_str(path_field).to_string();
        let filename = get_str(filename_field).to_string();
        let dir = get_str(dir_field).to_string();

        let size = size_field
            .and_then(|f| doc.get_first(f))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let modified = read_modified(searcher, doc_address);

        let extension = get_str(extension_field).to_string();

        let has_content = has_content_field
            .and_then(|f| doc.get_first(f))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        items.push(RawSearchResult {
            path,
            filename,
            dir,
            size,
            modified,
            extension,
            has_content,
            score,
        });
    }
    items
}

/// Formats the doc's `modified` fast field as RFC 3339. The field is fast
/// (not stored), so it is read from the segment's fast-field column rather
/// than the stored document.
fn read_modified(searcher: &tantivy::Searcher, doc_address: &DocAddress) -> String {
    searcher
        .segment_reader(doc_address.segment_ord)
        .fast_fields()
        .date(field::MODIFIED)
        .ok()
        .and_then(|col| col.first(doc_address.doc_id))
        .map(|dt| {
            let ts = dt.into_timestamp_secs();
            chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
                .unwrap_or_default()
                .to_rfc3339()
        })
        .unwrap_or_default()
}

/// Read file from disk and generate a snippet from its normalized content.
///
/// When `snippets_as_html` is set, the result is HTML with matched terms wrapped
/// in `<mark>...</mark>` (content HTML-escaped by tantivy); otherwise a plain-text
/// fragment. Returns `None` when no snippet generator is available or the file
/// content cannot be loaded.
async fn generate_snippet_from_file(
    snippet_gen: Option<&SnippetGenerator>,
    watched_dir: &std::path::Path,
    rel_path: &str,
    text_data_cache: &crate::formats::TextDataCache,
    snippets_as_html: bool,
    max_bytes: u64,
) -> Option<String> {
    let snippet_gen = snippet_gen?;

    // Load and normalize content via formats module. One unreadable file must
    // not fail the search; TooLarge is routine (files can grow past the limit
    // after indexing), so debug level.
    let full_path = watched_dir.join(rel_path);
    let format =
        match crate::formats::load_file_text_data(text_data_cache, &full_path, max_bytes).await {
            Ok(data) => data,
            Err(e) => {
                tracing::debug!(path = %full_path.display(), error = %e, "Snippet skipped");
                return None;
            }
        };

    let mut snippet = snippet_gen.snippet(&format.text);
    if snippets_as_html {
        snippet.set_snippet_prefix_postfix(SNIPPET_MARK.0, SNIPPET_MARK.1);
        Some(snippet.to_html())
    } else {
        Some(snippet.fragment().to_string())
    }
}
