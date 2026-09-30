use std::{path::{Path, PathBuf}, sync::{Arc, atomic::Ordering}};

use super::*;

fn make_test_temp_dir(kind: &str) -> PathBuf {
    crate::testutil::unique_temp_dir(&format!("unit_test_{kind}"))
}

fn make_test_config(watch_dir: &Path, index_dir: &Path) -> Config {
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

/// Create three files in `watch_dir` and buffer them for indexing
/// (each adds a Delete+Add pair, so 6 buffered items total).
async fn buffer_three_files(writer: &IndexWriterWrapper, watch_dir: &Path) {
    for name in ["a.txt", "b.txt", "c.txt"] {
        let path = watch_dir.join(name);
        std::fs::write(&path, "content").unwrap();
        writer.add_file(path).await.unwrap();
    }
}

#[test]
fn test_chrono_to_tantivy_conversion() {
    let chrono_ts: i64 = 1_700_000_000;
    let chrono_dt = chrono::DateTime::from_timestamp(chrono_ts, 0).unwrap();
    let tantivy_dt = chrono_to_tantivy(chrono_dt);
    let ts = tantivy_dt.into_timestamp_secs();
    assert_eq!(ts, chrono_ts);

    // Nanosecond round-trip must be exact (the scanner relies on equality).
    let chrono_ns_dt = chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap();
    let tantivy_ns_dt = chrono_to_tantivy(chrono_ns_dt);
    assert_eq!(
        tantivy_ns_dt.into_timestamp_nanos(),
        1_700_000_000_123_456_789
    );
}

#[test]
fn test_term_for_path() {
    let schema = schema::build_schema();
    let _term = DocumentModel::term_for_path(&schema, "src/main.rs").unwrap();
}

#[test]
fn test_requeue_failed_increments_try_count() {
    let items = vec![ChangeItem {
        data: Change::Delete("a.txt".into()),
        try_count: 0,
    }];
    let requeued = requeue_failed(items);
    assert_eq!(requeued.len(), 1);
    assert_eq!(requeued[0].try_count, 1);
}

#[test]
fn test_requeue_failed_drops_at_max_retry_count() {
    let items = vec![
        ChangeItem {
            data: Change::Delete("dropped.txt".into()),
            try_count: MAX_RETRY_COUNT - 1,
        },
        ChangeItem {
            data: Change::Delete("kept.txt".into()),
            try_count: 0,
        },
    ];
    let requeued = requeue_failed(items);
    assert_eq!(requeued.len(), 1);
    assert_eq!(requeued[0].try_count, 1);
    match &requeued[0].data {
        Change::Delete(path) => assert_eq!(path, "kept.txt"),
        _ => panic!("expected Delete change"),
    }
}

#[tokio::test]
async fn test_commit_task_panic_requeues_batch() {
    let watch_dir = make_test_temp_dir("watch");
    let index_dir = make_test_temp_dir("index");
    let config = Arc::new(make_test_config(&watch_dir, &index_dir));

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        crate::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    buffer_three_files(&writer, &watch_dir).await;
    assert_eq!(writer.buffer_len().await, 6);

    // Inject a panic into the blocking task: the batch must survive the
    // JoinError and come back to the buffer, not be lost.
    writer.panic_on_commit.store(true, Ordering::SeqCst);
    assert!(!writer.commit().await);
    assert_eq!(writer.buffer_len().await, 6);

    // The follow-up clean commit applies the requeued batch.
    assert!(writer.commit().await);
    assert_eq!(writer.buffer_len().await, 0);
    // A fresh reader reflects the latest committed state (commit awaited above).
    let reader = writer.index().reader().unwrap();
    assert_eq!(reader.searcher().num_docs(), 3);
}

#[tokio::test]
async fn test_commit_task_panic_drops_batch_after_max_retries() {
    let watch_dir = make_test_temp_dir("watch");
    let index_dir = make_test_temp_dir("index");
    let config = Arc::new(make_test_config(&watch_dir, &index_dir));

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        crate::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    buffer_three_files(&writer, &watch_dir).await;

    // Two failed commits: try_count goes 0 -> 1 -> 2 == MAX_RETRY_COUNT,
    // so the batch is dropped per the retry contract.
    writer.panic_on_commit.store(true, Ordering::SeqCst);
    assert!(!writer.commit().await);
    assert_eq!(writer.buffer_len().await, 6);

    writer.panic_on_commit.store(true, Ordering::SeqCst);
    assert!(!writer.commit().await);
    assert_eq!(writer.buffer_len().await, 0);
}
