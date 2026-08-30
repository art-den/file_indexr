use std::path::PathBuf;
use std::sync::Arc;

use file_indexr::config::Config;
use file_indexr::index::writer::IndexWriterWrapper;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("integ_test")
}

fn make_config(watch_dir: &std::path::Path, index_dir: &std::path::Path) -> Config {
    Config {
        directory: watch_dir.to_path_buf(),
        index_path: index_dir.to_path_buf(),
        port: 8080,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,

        allowed_extensions: vec![],
    }
}

// ============================================================================
// IndexWriterWrapper tests
// ============================================================================

#[tokio::test]
async fn test_writer_create_new_index() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    assert!(writer.index().schema().get_field("path").is_ok());
}

#[tokio::test]
async fn test_writer_add_and_commit() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let test_file = watch_dir.join("hello.txt");
    std::fs::write(&test_file, "Hello, world!").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    writer.add_file(test_file).await.unwrap();
    writer.commit().await;

    // Verify by searching
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_writer_delete_document() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let test_file = watch_dir.join("to_delete.txt");
    std::fs::write(&test_file, "delete me").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    writer.add_file(test_file.clone()).await.unwrap();
    writer.commit().await;

    // Verify document exists
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 1);

    // Delete and commit
    writer.delete_file(test_file).await.unwrap();
    writer.commit().await;

    // Reopen reader to see committed changes
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_writer_reopen_existing_index() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let test_file = watch_dir.join("persist.txt");
    std::fs::write(&test_file, "persistent data").unwrap();

    // First writer — add and commit
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap();
        writer.add_file(test_file).await.unwrap();
        writer.commit().await;
    }

    // Second writer — reopen existing index
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap();

        let reader = writer.index().reader().unwrap();
        let searcher = reader.searcher();
        let results: Vec<_> = searcher
            .search(
                &tantivy::query::AllQuery {},
                &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
            )
            .unwrap();
        assert_eq!(results.len(), 1);
    }
}

#[tokio::test]
async fn test_writer_buffer_auto_commit_on_threshold() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(Config {
        directory: watch_dir.clone(),
        index_path: index_dir.clone(),
        port: 8080,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 2, // Auto-commit after 2 changes
        batch_timeout_ms: 1000,

        allowed_extensions: vec![],
    });

    for i in 0..2 {
        let test_file = watch_dir.join(format!("file_{}.txt", i));
        std::fs::write(&test_file, format!("content {}", i)).unwrap();
    }

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    for i in 0..2 {
        let test_file = watch_dir.join(format!("file_{}.txt", i));
        writer.add_file(test_file).await.unwrap();
    }

    // After 2 adds (== batch_size), buffer should be empty (auto-flushed)
    assert_eq!(writer.buffer_len().await, 0);
}

#[tokio::test]
async fn test_writer_multiple_adds_and_single_commit() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    for i in 0..3 {
        let test_file = watch_dir.join(format!("file_{}.txt", i));
        std::fs::write(&test_file, format!("content {}", i)).unwrap();
    }

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    for i in 0..3 {
        let test_file = watch_dir.join(format!("file_{}.txt", i));
        writer.add_file(test_file).await.unwrap();
    }

    // 3 adds + 3 deletes (dedup), not committed yet
    assert_eq!(writer.buffer_len().await, 6);

    writer.commit().await;
    assert_eq!(writer.buffer_len().await, 0);

    // Verify all documents are in index (3 unique files)
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 3);
}

#[tokio::test]
async fn test_writer_add_file_deduplicates_on_reindex() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let test_file = watch_dir.join("hello.txt");
    std::fs::write(&test_file, "Hello, world!").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    // Add the same file twice — should not create duplicates
    writer.add_file(test_file.clone()).await.unwrap();
    writer.commit().await;
    writer.add_file(test_file).await.unwrap();
    writer.commit().await;

    // Should still have exactly 1 document
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_writer_delete_deleted_file() {
    // Regression test: deleting a file that no longer exists on disk.
    // The writer must reconstruct the canonical relative path by
    // canonicalizing the parent and reattaching the suffix.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let test_file = watch_dir.join("gone.txt");
    std::fs::write(&test_file, "will be deleted").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    writer.add_file(test_file.clone()).await.unwrap();
    writer.commit().await;

    // Verify document exists
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 1);

    // Remove file from disk
    std::fs::remove_file(&test_file).unwrap();

    // Delete via writer — should still work despite file not existing
    writer.delete_file(test_file).await.unwrap();
    writer.commit().await;

    // Verify document is gone
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_writer_delete_deleted_dir() {
    // Regression test: deleting a directory that no longer exists on disk.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let sub_dir = watch_dir.join("subdir");
    std::fs::create_dir_all(&sub_dir).unwrap();
    std::fs::write(sub_dir.join("a.txt"), "content a").unwrap();
    std::fs::write(sub_dir.join("b.txt"), "content b").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    writer.add_file(sub_dir.join("a.txt")).await.unwrap();
    writer.add_file(sub_dir.join("b.txt")).await.unwrap();
    writer.commit().await;

    // Verify 2 documents exist
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 2);

    // Remove dir from disk
    std::fs::remove_dir_all(&sub_dir).unwrap();

    // Delete dir via writer — should still work
    writer.delete_dir(sub_dir).await.unwrap();
    writer.commit().await;

    // Verify all docs are gone
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_writer_html_file_strips_tags_on_index() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let html_file = watch_dir.join("page.html");
    std::fs::write(
        &html_file,
        r#"<html><body>
<h1>Welcome to FileIndexr</h1>
<p>This is a <strong>bold</strong> statement about <em>Rust</em>.</p>
<div class="footer">Copyright 2024</div>
</body></html>"#,
    )
    .unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    writer.add_file(html_file).await.unwrap();
    writer.commit().await;

    let index = writer.index();
    let schema = index.schema();
    let content_field = schema.get_field("content").unwrap();
    let reader = index.reader().unwrap();

    // Helper: search by term and return doc count
    fn doc_count_for_term(
        reader: &tantivy::IndexReader,
        field: tantivy::schema::Field,
        text: &str,
    ) -> usize {
        let s = reader.searcher();
        let term = tantivy::Term::from_field_text(field, text);
        let tquery =
            tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let results: Vec<_> = s
            .search(
                &tquery,
                &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
            )
            .unwrap();
        results.len()
    }

    // Text content should be searchable (lowercase, because default tokenizer lowercases)
    assert_eq!(doc_count_for_term(&reader, content_field, "welcome"), 1);
    assert_eq!(doc_count_for_term(&reader, content_field, "fileindexr"), 1);
    assert_eq!(doc_count_for_term(&reader, content_field, "bold"), 1);
    assert_eq!(doc_count_for_term(&reader, content_field, "rust"), 1);
    assert_eq!(doc_count_for_term(&reader, content_field, "copyright"), 1);

    // Tag names should NOT appear as indexed tokens
    for tag_name in ["html", "body", "strong", "em", "h1", "div", "p"] {
        assert_eq!(
            doc_count_for_term(&reader, content_field, tag_name),
            0,
            "HTML tag '{tag_name}' should not be indexed as a token"
        );
    }
}

#[tokio::test]
async fn test_writer_reindex_reads_fresh_content_not_stale_cache() {
    // Regression test: a file that is "warm" in the text data cache must be
    // re-indexed with fresh disk content, not the stale cached copy.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let test_file = watch_dir.join("stale_check.txt");
    std::fs::write(&test_file, "original alpha content").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    writer.add_file(test_file.clone()).await.unwrap();
    writer.commit().await;

    // Simulate process_change's full cache clear, then a web/MCP reader
    // re-warming the cache with the current content.
    let cache = writer.text_data_cache();
    cache.invalidate_all();
    let warmed = file_indexr::formats::load_file_text_data(cache, &test_file)
        .await
        .unwrap();
    assert_eq!(warmed.text.as_str(), "original alpha content");

    // Modify the file on disk; the cache entry is now stale.
    std::fs::write(&test_file, "updated beta content").unwrap();

    // Re-index the file the same way process_change does.
    writer.add_file(test_file).await.unwrap();
    writer.commit().await;

    // The index must hold the new content, not the stale cached copy.
    let index = writer.index();
    let content_field = index.schema().get_field("content").unwrap();
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let doc_count = |term: &str| -> usize {
        let t = tantivy::Term::from_field_text(content_field, term);
        let query = tantivy::query::TermQuery::new(t, tantivy::schema::IndexRecordOption::Basic);
        searcher
            .search(
                &query,
                &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
            )
            .unwrap()
            .len()
    };
    assert_eq!(doc_count("beta"), 1);
    assert_eq!(doc_count("alpha"), 0);
}
