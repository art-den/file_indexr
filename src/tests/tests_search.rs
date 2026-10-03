use std::sync::Arc;

use crate::config::Config;
use crate::index::writer::IndexWriterWrapper;

use super::*;

fn make_temp_dir() -> std::path::PathBuf {
    crate::testutil::unique_temp_dir("search_test")
}

fn make_config(watch_dir: &std::path::Path, index_dir: std::path::PathBuf) -> Arc<Config> {
    Arc::new(Config {
        directory: watch_dir.to_path_buf(),
        index_path: index_dir,
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

async fn setup_test_index() -> (IndexWriterWrapper, std::path::PathBuf, tantivy::IndexReader) {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(&watch_dir, index_dir);

    std::fs::write(
        watch_dir.join("hello.rs"),
        "fn hello() { println!(\"Hello\"); }",
    )
    .unwrap();
    std::fs::write(watch_dir.join("world.rs"), "fn world() {}").unwrap();
    std::fs::write(watch_dir.join("README.md"), "# Project README").unwrap();

    let writer = make_writer(config).await;
    writer.add_file(watch_dir.join("hello.rs")).await.unwrap();
    writer.add_file(watch_dir.join("world.rs")).await.unwrap();
    writer.add_file(watch_dir.join("README.md")).await.unwrap();
    writer.commit().await;

    let reader = writer.index().reader().unwrap();
    (writer, watch_dir, reader)
}

#[tokio::test]
async fn test_search_by_content() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            q: "Hello".to_string(),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    assert!(result.total >= 1);
    assert!(result.results.iter().any(|r| r.path.contains("hello.rs")));
    assert!(!result.results[0].snippet.is_empty());
}

#[tokio::test]
async fn test_snippet_html_highlighting() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();
    let make_params = |q: &str| SearchParams {
        q: q.to_string(),
        ..Default::default()
    };

    // Plain snippets must not contain any markup.
    let result = search(
        &reader,
        make_params("Hello"),
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();
    let item = result
        .results
        .iter()
        .find(|r| r.path.contains("hello.rs"))
        .unwrap();
    assert!(!item.snippet.is_empty());
    assert!(!item.snippet.contains('<'));

    // HTML snippets wrap matches in <mark> and escape raw HTML.
    let result = search(
        &reader,
        make_params("Hello"),
        &watch_dir,
        &cache,
        true,
        u64::MAX,
    )
    .await
    .unwrap();
    let item = result
        .results
        .iter()
        .find(|r| r.path.contains("hello.rs"))
        .unwrap();
    assert!(item.snippet.contains("<mark>"));
    assert!(item.snippet.contains("</mark>"));
    assert!(!item.snippet.contains("\"Hello\"")); // quotes escaped as &quot;
}

#[tokio::test]
async fn test_snippet_empty_when_content_unmatched() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    // "md" matches only the README.md filename, not its content.
    let result = search(
        &reader,
        SearchParams {
            q: "md".to_string(),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    let item = result
        .results
        .iter()
        .find(|r| r.path.contains("README.md"))
        .unwrap();

    assert!(item.snippet.is_empty());
}

#[tokio::test]
async fn test_result_order_preserved_with_concurrent_snippets() {
    // Committing in batches forces multiple segments, so this also covers
    // string fast-field sorting across segments. Snippets are generated
    // with bounded concurrency (`StreamExt::buffered`); results must still
    // come back in collector order for both sort directions.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(&watch_dir, index_dir);

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

    let reader = writer.index().reader().unwrap();
    let cache = crate::formats::new_text_data_cache();

    for (order, reversed) in [(SortOrder::Asc, false), (SortOrder::Desc, true)] {
        let mut expected = names.clone();
        if reversed {
            expected.reverse();
        }

        let result = search(
            &reader,
            SearchParams {
                q: "zebra".to_string(),
                sort_by: SortBy::Path,
                sort_order: order,
                ..Default::default()
            },
            &watch_dir,
            &cache,
            false,
            u64::MAX,
        )
        .await
        .unwrap();

        assert_eq!(result.total, 12);
        let actual: Vec<String> = result.results.iter().map(|r| r.filename.clone()).collect();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn test_snippet_empty_when_file_grows_past_limit() {
    // A file can grow past the size limit after indexing. The search must
    // still return the result, with an empty snippet.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(&watch_dir, index_dir);

    std::fs::write(watch_dir.join("grow.txt"), "needle text").unwrap();
    let writer = make_writer(config).await;
    writer.add_file(watch_dir.join("grow.txt")).await.unwrap();
    writer.commit().await;

    // Grow the file past the search-time limit (max_bytes = 4).
    std::fs::write(watch_dir.join("grow.txt"), "needle text grew a lot").unwrap();

    let reader = writer.index().reader().unwrap();
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            q: "needle".to_string(),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        4,
    )
    .await
    .unwrap();

    let item = result
        .results
        .iter()
        .find(|r| r.path.contains("grow.txt"))
        .unwrap();
    assert!(item.snippet.is_empty());
}

#[tokio::test]
async fn test_search_by_filename() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            q: "world".to_string(),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    assert!(result.total >= 1);
    assert!(result.results.iter().any(|r| r.path.contains("world.rs")));
}

#[tokio::test]
async fn test_search_with_extension_filter() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            ext: Some("rs".to_string()),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    assert!(result.total >= 2);
    for r in &result.results {
        assert_eq!(r.extension, "rs");
    }
}

#[tokio::test]
async fn test_search_extension_case_insensitive() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    // PNG/JPG are not recognized formats: an empty allow-list would skip
    // them, so index them via an explicit extension list.
    let mut config = (*make_config(&watch_dir, index_dir)).clone();
    config.allowed_extensions = vec!["png".to_string(), "jpg".to_string(), "rs".to_string()];
    let config = Arc::new(config);

    // Create files with mixed-case extensions
    std::fs::write(watch_dir.join("photo.PNG"), "image data").unwrap();
    std::fs::write(watch_dir.join("snapshot.Jpg"), "jpeg data").unwrap();
    std::fs::write(watch_dir.join("script.rs"), "fn main() {}").unwrap();

    let writer = make_writer(config).await;
    writer.add_file(watch_dir.join("photo.PNG")).await.unwrap();
    writer
        .add_file(watch_dir.join("snapshot.Jpg"))
        .await
        .unwrap();
    writer.add_file(watch_dir.join("script.rs")).await.unwrap();
    writer.commit().await;
    let reader = writer.index().reader().unwrap();

    // Query with lowercase should match files regardless of original case
    let result = search(
        &reader,
        SearchParams {
            ext: Some("png".to_string()),
            ..Default::default()
        },
        &watch_dir,
        writer.text_data_cache(),
        false,
        u64::MAX,
    )
    .await
    .unwrap();
    assert_eq!(result.total, 1);
    assert_eq!(result.results[0].extension, "png");

    // Query with uppercase should also work
    let result = search(
        &reader,
        SearchParams {
            ext: Some("JPG".to_string()),
            ..Default::default()
        },
        &watch_dir,
        writer.text_data_cache(),
        false,
        u64::MAX,
    )
    .await
    .unwrap();
    assert_eq!(result.total, 1);
    assert_eq!(result.results[0].extension, "jpg");
}

#[tokio::test]
async fn test_search_with_path_filter() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            path: Some("README".to_string()),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    assert!(result.total >= 1);
    assert!(result.results.iter().any(|r| r.path.contains("README")));
}

#[tokio::test]
async fn test_empty_query_returns_all() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            max_results: 10,
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    assert_eq!(result.total, 3);
}

#[tokio::test]
async fn test_search_content_false() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    // Search only in path/filename fields — "README" is both in filename and content,
    // so it should still be found. But a word only in content won't be.
    let result = search(
        &reader,
        SearchParams {
            q: "README".to_string(),
            search_content: false,
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    // Should find README.md by filename
    assert!(result.total >= 1);
}

#[tokio::test]
async fn test_search_max_results() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            max_results: 2,
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

    assert!(result.results.len() <= 2);
    assert_eq!(result.total, 3);
}

#[tokio::test]
async fn test_search_no_results() {
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            q: "nonexistent_zxywvu".to_string(),
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

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
    let (_writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let result = search(
        &reader,
        SearchParams {
            max_results: 10,
            ..Default::default()
        },
        &watch_dir,
        &cache,
        false,
        u64::MAX,
    )
    .await
    .unwrap();

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
    let (writer, watch_dir, reader) = setup_test_index().await;
    let cache = crate::formats::new_text_data_cache();

    let params = SearchParams {
        q: "`std::vector`".to_string(),
        ..SearchParams::default()
    };
    let result = search(&reader, params, &watch_dir, &cache, false, u64::MAX).await;
    assert!(
        result.is_ok(),
        "expected success, got error: {:?}",
        result.err()
    );

    // Also test other tricky characters
    let params = SearchParams {
        q: "hello [unmatched".to_string(),
        ..SearchParams::default()
    };
    let result = search(&reader, params, &watch_dir, &cache, false, u64::MAX).await;
    assert!(
        result.is_ok(),
        "expected success, got error: {:?}",
        result.err()
    );

    drop(writer);
}
