use crate::index::*;
use crate::index::checkpoint::{CHECKPOINT_FILENAME, load_checkpoint};
use crate::search::{SearchParams, search};

fn make_temp_dir(tag: &str) -> std::path::PathBuf {
    crate::testutil::unique_temp_dir(&format!("rescan_test_{tag}"))
}

fn make_config(watch_dir: &std::path::Path, index_dir: &std::path::Path) -> Arc<Config> {
    Arc::new(Config {
        directory: watch_dir.to_path_buf(),
        index_path: index_dir.to_path_buf(),
        port: 0,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 1,
        batch_size: 500,
        batch_timeout_ms: 1000,
        allowed_extensions: vec![],
    })
}

async fn found(
    reader: &tantivy::IndexReader,
    watch_dir: &std::path::Path,
    q: &str,
    cache: &crate::formats::TextDataCache,
) -> usize {
    search(
        reader,
        SearchParams {
            q: q.to_string(),
            ..Default::default()
        },
        watch_dir,
        cache,
        false,
    )
    .await
    .unwrap()
    .total
}

/// Simulate events lost to an inotify queue overflow: change files on
/// disk without any watcher, then verify `rescan_directory` recovers
/// them (modified content, new files, deletions) and is idempotent.
#[tokio::test]
async fn test_rescan_directory_recovers_lost_changes() {
    let watch_dir = make_temp_dir("watch");
    let index_dir = make_temp_dir("index");
    let config = make_config(&watch_dir, &index_dir);

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        crate::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    // Baseline index
    std::fs::write(watch_dir.join("a.txt"), "alpha original").unwrap();
    std::fs::write(watch_dir.join("b.txt"), "beta content").unwrap();
    writer.add_file(watch_dir.join("a.txt")).await.unwrap();
    writer.add_file(watch_dir.join("b.txt")).await.unwrap();
    writer.commit().await;

    // "Lost" changes: modify a, create c, delete b
    std::fs::write(watch_dir.join("a.txt"), "alpha modified").unwrap();
    std::fs::write(watch_dir.join("c.txt"), "gamma content").unwrap();
    std::fs::remove_file(watch_dir.join("b.txt")).unwrap();

    rescan_directory(&writer, &config).await.unwrap();

    let reader = writer.index().reader().unwrap();
    let cache = writer.text_data_cache();
    assert_eq!(
        found(&reader, &watch_dir, "alpha modified", cache).await,
        1,
        "modified content recovered"
    );
    assert_eq!(
        found(&reader, &watch_dir, "gamma", cache).await,
        1,
        "new file indexed"
    );
    assert_eq!(
        found(&reader, &watch_dir, "beta", cache).await,
        0,
        "deleted file removed from index"
    );

    // Second rescan with no further changes: still consistent
    rescan_directory(&writer, &config).await.unwrap();
    assert_eq!(found(&reader, &watch_dir, "gamma", cache).await, 1);
    assert_eq!(found(&reader, &watch_dir, "beta", cache).await, 0);
}

/// A corrupted checkpoint file must not block the startup scan: the
/// scan falls back to a full reindex, and the damaged file is
/// atomically overwritten once the scan and commit succeed.
#[tokio::test]
async fn test_startup_scan_self_heals_corrupted_checkpoint() {
    let watch_dir = make_temp_dir("watch");
    let index_dir = make_temp_dir("index");
    let config = make_config(&watch_dir, &index_dir);

    std::fs::write(watch_dir.join("a.txt"), "alpha content").unwrap();

    // Corrupt the checkpoint before the startup scan
    std::fs::write(index_dir.join(CHECKPOINT_FILENAME), "not valid json {{{").unwrap();

    let writer = IndexWriterWrapper::new(
        &index_dir,
        config.clone(),
        crate::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    let coordinator = IndexCoordinator::with_writer(Arc::new(writer), config.clone());
    coordinator.startup_scan().await.unwrap();

    // The damaged file is replaced with a valid, completed checkpoint
    let cp = load_checkpoint(&index_dir)
        .await
        .unwrap()
        .expect("checkpoint rewritten after successful scan");
    assert_eq!(cp.version, 1);
    assert!(cp.completed_at.is_some());

    // The full reindex ran: the file is searchable
    let reader = coordinator.writer().index().reader().unwrap();
    let cache = coordinator.writer().text_data_cache();
    assert_eq!(
        found(&reader, &watch_dir, "alpha", cache).await,
        1,
        "file reindexed despite corrupted checkpoint"
    );
}
