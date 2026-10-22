use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::formats::TextDataCache;
use crate::index::writer::IndexWriterWrapper;
use crate::testutil::unique_temp_dir;

use super::*;

/// Watch-dir contents shared by most tests.
const DEFAULT_FILES: [(&str, &str); 3] = [
    ("hello.rs", "fn hello() { println!(\"Hello\"); }"),
    ("world.rs", "fn world() {}"),
    ("README.md", "# Project README"),
];

fn make_config(watch_dir: &Path, index_dir: &Path) -> Arc<Config> {
    Arc::new(Config {
        directory: watch_dir.to_path_buf(),
        index_path: index_dir.to_path_buf(),
        port: 8080,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,
        allowed_extensions: vec![],
    })
}

async fn make_writer(config: Arc<Config>) -> IndexWriterWrapper {
    IndexWriterWrapper::new(
        &config.index_path,
        config.clone(),
        crate::formats::new_text_data_cache(),
    )
    .await
    .unwrap()
}

/// A committed index plus everything [`search`] needs, so each test describes
/// only params and expectations.
struct Fixture {
    /// Held only to keep the index open for as long as the reader is used.
    _writer: IndexWriterWrapper,
    watch_dir: PathBuf,
    reader: tantivy::IndexReader,
    /// Search-time cache, deliberately not the writer's (which indexing warms):
    /// a file changed after indexing must be re-read from disk.
    cache: TextDataCache,
}

impl Fixture {
    /// Fresh watch dir and index containing `files` (`(name, content)`), committed.
    async fn with_files(files: &[(&str, &str)]) -> Self {
        let watch_dir = unique_temp_dir("search_test_watch");
        let config = make_config(&watch_dir, &unique_temp_dir("search_test_index"));
        Self::with_config_and_files(config, files).await
    }

    /// Like [`Fixture::with_files`] with a custom config (e.g. an explicit
    /// `allowed_extensions` list for formats the indexer does not recognize).
    async fn with_config_and_files(config: Arc<Config>, files: &[(&str, &str)]) -> Self {
        let writer = make_writer(config.clone()).await;
        for (name, content) in files {
            let path = config.directory.join(name);
            std::fs::write(&path, content).unwrap();
            writer.add_file(path).await.unwrap();
        }
        writer.commit().await;
        Self::from_writer(config.directory.clone(), writer)
    }

    /// Wrap an already committed writer, for tests that need custom indexing
    /// (batched commits, files mutated after the commit).
    fn from_writer(watch_dir: PathBuf, writer: IndexWriterWrapper) -> Self {
        let reader = writer.index().reader().unwrap();
        Self {
            _writer: writer,
            watch_dir,
            reader,
            cache: crate::formats::new_text_data_cache(),
        }
    }

    /// [`search`] with plain-text snippets and no size limit.
    async fn search(&self, params: SearchParams) -> SearchResponse {
        self.try_search(params, false, u64::MAX).await.unwrap()
    }

    /// [`search`] with the snippet-format and size-limit knobs, unwrapped only
    /// by the caller, so error-case tests can assert on the `Result`.
    async fn try_search(
        &self,
        params: SearchParams,
        snippets_as_html: bool,
        max_bytes: u64,
    ) -> Result<SearchResponse, SearchError> {
        search(
            &self.reader,
            params,
            &self.watch_dir,
            &self.cache,
            snippets_as_html,
            max_bytes,
        )
        .await
    }
}

/// `SearchParams` matching `q` only.
fn q_params(q: &str) -> SearchParams {
    SearchParams {
        q: q.to_string(),
        ..Default::default()
    }
}

/// The result whose path contains `part` (panics if none matches).
fn result_item<'a>(response: &'a SearchResponse, part: &str) -> &'a SearchResultItem {
    response
        .results
        .iter()
        .find(|r| r.path.contains(part))
        .unwrap_or_else(|| panic!("no result with path containing {part}"))
}

#[tokio::test]
async fn test_search_by_content() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx.search(q_params("Hello")).await;

    assert!(result.total >= 1);
    assert!(result.results.iter().any(|r| r.path.contains("hello.rs")));
    assert!(!result.results[0].snippet.is_empty());
}

#[tokio::test]
async fn test_snippet_html_highlighting() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    // Plain snippets must not contain any markup.
    let result = fx.search(q_params("Hello")).await;
    let item = result_item(&result, "hello.rs");
    assert!(!item.snippet.is_empty());
    assert!(!item.snippet.contains('<'));

    // HTML snippets wrap matches in <mark> and escape raw HTML.
    let result = fx
        .try_search(q_params("Hello"), true, u64::MAX)
        .await
        .unwrap();
    let item = result_item(&result, "hello.rs");
    assert!(item.snippet.contains("<mark>"));
    assert!(item.snippet.contains("</mark>"));
    assert!(!item.snippet.contains("\"Hello\"")); // quotes escaped as &quot;
}

#[tokio::test]
async fn test_snippet_empty_when_content_unmatched() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    // "md" matches only the README.md filename, not its content.
    let result = fx.search(q_params("md")).await;

    assert!(result_item(&result, "README.md").snippet.is_empty());
}

#[tokio::test]
async fn test_result_order_preserved_with_concurrent_snippets() {
    // Committing in batches forces multiple segments, so this also covers
    // string fast-field sorting across segments. Snippets are generated
    // with bounded concurrency (`StreamExt::buffered`); results must still
    // come back in collector order for both sort directions.
    let watch_dir = unique_temp_dir("search_test_watch");
    let config = make_config(&watch_dir, &unique_temp_dir("search_test_index"));

    let writer = make_writer(config).await;
    let mut names = Vec::new();
    for i in 0..12 {
        let name = format!("file_{i:02}.txt");
        let path = watch_dir.join(&name);
        std::fs::write(&path, format!("zebra content number {i}")).unwrap();
        writer.add_file(path).await.unwrap();
        names.push(name);
        // Commit in batches of 4 so the index ends up with multiple segments.
        if (i + 1) % 4 == 0 {
            writer.commit().await;
        }
    }

    let fx = Fixture::from_writer(watch_dir, writer);

    for (order, reversed) in [(SortOrder::Asc, false), (SortOrder::Desc, true)] {
        let mut expected = names.clone();
        if reversed {
            expected.reverse();
        }

        let result = fx
            .search(SearchParams {
                q: "zebra".to_string(),
                sort_by: SortBy::Path,
                sort_order: order,
                ..Default::default()
            })
            .await;

        assert_eq!(result.total, 12);
        let actual: Vec<String> = result.results.iter().map(|r| r.filename.clone()).collect();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn test_snippet_empty_when_file_grows_past_limit() {
    // A file can grow past the size limit after indexing. The search must
    // still return the result, with an empty snippet.
    let watch_dir = unique_temp_dir("search_test_watch");
    let config = make_config(&watch_dir, &unique_temp_dir("search_test_index"));

    std::fs::write(watch_dir.join("grow.txt"), "needle text").unwrap();
    let writer = make_writer(config).await;
    writer.add_file(watch_dir.join("grow.txt")).await.unwrap();
    writer.commit().await;

    // Grow the file past the search-time limit (max_bytes = 4).
    std::fs::write(watch_dir.join("grow.txt"), "needle text grew a lot").unwrap();

    let fx = Fixture::from_writer(watch_dir, writer);

    let result = fx.try_search(q_params("needle"), false, 4).await.unwrap();

    assert!(result_item(&result, "grow.txt").snippet.is_empty());
}

#[tokio::test]
async fn test_search_by_filename() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx.search(q_params("world")).await;

    assert!(result.total >= 1);
    assert!(result.results.iter().any(|r| r.path.contains("world.rs")));
}

#[tokio::test]
async fn test_search_with_extension_filter() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx
        .search(SearchParams {
            ext: Some("rs".to_string()),
            ..Default::default()
        })
        .await;

    assert!(result.total >= 2);
    for r in &result.results {
        assert_eq!(r.extension, "rs");
    }
}

#[tokio::test]
async fn test_search_extension_case_insensitive() {
    // PNG/JPG are not recognized formats: an empty allow-list would skip
    // them, so index them via an explicit extension list.
    let watch_dir = unique_temp_dir("search_test_watch");
    let mut config = (*make_config(&watch_dir, &unique_temp_dir("search_test_index"))).clone();
    config.allowed_extensions = vec!["png".to_string(), "jpg".to_string(), "rs".to_string()];

    // Indexed filenames use mixed-case extensions; the stored extension is
    // normalized to lowercase.
    let fx = Fixture::with_config_and_files(
        Arc::new(config),
        &[
            ("photo.PNG", "image data"),
            ("snapshot.Jpg", "jpeg data"),
            ("script.rs", "fn main() {}"),
        ],
    )
    .await;

    // A lowercase query must match an uppercase extension and vice versa.
    for query_ext in ["png", "JPG"] {
        let result = fx
            .search(SearchParams {
                ext: Some(query_ext.to_string()),
                ..Default::default()
            })
            .await;
        assert_eq!(result.total, 1, "ext filter: {query_ext}");
        assert_eq!(
            result.results[0].extension,
            query_ext.to_lowercase(),
            "ext filter: {query_ext}"
        );
    }
}

#[tokio::test]
async fn test_search_with_path_filter() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx
        .search(SearchParams {
            path: Some("README".to_string()),
            ..Default::default()
        })
        .await;

    assert!(result.total >= 1);
    assert!(result.results.iter().any(|r| r.path.contains("README")));
}

#[tokio::test]
async fn test_empty_query_returns_all() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx
        .search(SearchParams {
            max_results: 10,
            ..Default::default()
        })
        .await;

    assert_eq!(result.total, 3);
}

#[tokio::test]
async fn test_search_content_false() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    // Search only in path/filename fields — "README" is both in filename and content,
    // so it should still be found. But a word only in content won't be.
    let result = fx
        .search(SearchParams {
            q: "README".to_string(),
            search_content: false,
            ..Default::default()
        })
        .await;

    // Should find README.md by filename
    assert!(result.total >= 1);
}

#[tokio::test]
async fn test_search_max_results() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx
        .search(SearchParams {
            max_results: 2,
            ..Default::default()
        })
        .await;

    assert!(result.results.len() <= 2);
    assert_eq!(result.total, 3);
}

#[tokio::test]
async fn test_search_no_results() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx.search(q_params("nonexistent_zxywvu")).await;

    assert_eq!(result.total, 0);
}

#[test]
fn test_search_params_default() {
    let params = SearchParams::default();
    assert!(params.q.is_empty());
    assert!(params.search_content);
    assert!(params.ext.is_none());
    assert_eq!(params.max_results, 50);
}

#[tokio::test]
async fn test_search_result_modified_is_populated() {
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    let result = fx
        .search(SearchParams {
            max_results: 10,
            ..Default::default()
        })
        .await;

    assert_eq!(result.total, 3);
    for r in &result.results {
        assert!(!r.modified.is_empty(), "modified empty for {}", r.path);
        let dt: chrono::DateTime<chrono::Utc> = r.modified.parse().unwrap();
        let diff = chrono::Utc::now()
            .signed_duration_since(dt)
            .num_seconds()
            .abs();
        assert!(diff < 300, "modified should be recent, got {}", r.modified);
    }
}

#[tokio::test]
async fn test_search_query_with_special_chars_falls_back_to_term_query() {
    // Queries containing backticks or other special chars should not cause HTTP 400.
    // Instead, the parser falls back to a term-level boolean query.
    let fx = Fixture::with_files(&DEFAULT_FILES).await;

    for q in ["`std::vector`", "hello [unmatched"] {
        let result = fx.try_search(q_params(q), false, u64::MAX).await;
        assert!(
            result.is_ok(),
            "query {q:?}: expected success, got error: {:?}",
            result.err()
        );
    }
}
