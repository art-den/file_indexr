use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use file_indexr::config::Config;
use file_indexr::index::IndexCoordinator;
use file_indexr::watch::FileChange;
use file_indexr::watch::spawn_watcher;

use tokio::sync::mpsc;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("integ_test")
}

fn make_config(watch_dir: &Path, index_dir: &Path) -> Config {
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
// IndexCoordinator tests
// ============================================================================

#[tokio::test]
async fn test_coordinator_creation() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let coordinator = IndexCoordinator::new(config, file_indexr::formats::new_text_data_cache())
        .await
        .unwrap();
    assert!(
        coordinator
            .writer()
            .index()
            .schema()
            .get_field("path")
            .is_ok()
    );
}

#[tokio::test]
async fn test_startup_scan() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    // Create test files
    std::fs::write(watch_dir.join("a.txt"), "content a").unwrap();
    std::fs::write(watch_dir.join("b.rs"), "fn main() {}").unwrap();

    let coordinator = IndexCoordinator::new(config, file_indexr::formats::new_text_data_cache())
        .await
        .unwrap();
    let count = coordinator.startup_scan().await.unwrap();
    assert_eq!(count, 2);

    // Verify documents are in index
    let reader = coordinator.writer().index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn test_startup_scan_empty() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let coordinator = IndexCoordinator::new(config, file_indexr::formats::new_text_data_cache())
        .await
        .unwrap();
    let count = coordinator.startup_scan().await.unwrap();
    assert_eq!(count, 0);
}

// ============================================================================
// Shutdown sequence tests
// ============================================================================

/// Regression: `start_watching` must return promptly after a shutdown signal.
/// Before the fix it deadlocked: the drain loop waited for the channel sender
/// to be dropped, but the sender's exit depended on the watcher being dropped,
/// which was held by `start_watching` itself.
#[tokio::test]
async fn test_start_watching_returns_after_shutdown_signal() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let coordinator = IndexCoordinator::new(config, file_indexr::formats::new_text_data_cache())
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let start = tokio::spawn(coordinator.start_watching(shutdown_rx));

    // Let the watcher come up before requesting shutdown.
    tokio::time::sleep(Duration::from_millis(300)).await;
    shutdown_tx.send(true).unwrap();

    tokio::time::timeout(Duration::from_secs(10), start)
        .await
        .expect("start_watching did not return after shutdown signal — deadlock regression")
        .expect("start_watching task panicked")
        .expect("start_watching returned Err");
}

/// Dropping the `RecommendedWatcher` must close the change channel: the
/// watcher thread exits, the bridge closes, and the debouncer task drops its
/// sender after the final flush.
#[tokio::test]
async fn test_dropping_watcher_closes_change_channel() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let (watcher, _handle) = spawn_watcher(&config, tx).await.unwrap();
    drop(watcher);

    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        while rx.recv().await.is_some() {}
    })
    .await;
    assert!(
        drained.is_ok(),
        "change channel did not close after the watcher was dropped"
    );
}

/// A change made just before shutdown must survive: the debouncer's final
/// flush delivers it and the final commit persists it to the index.
#[tokio::test]
async fn test_change_before_shutdown_is_committed() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    let coordinator = IndexCoordinator::new(config, file_indexr::formats::new_text_data_cache())
        .await
        .unwrap();
    // `start_watching` consumes the coordinator; keep a handle to the writer
    // for post-shutdown verification.
    let writer = coordinator.writer_arc();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let start = tokio::spawn(coordinator.start_watching(shutdown_rx));

    // Let the watcher come up, then create a file. Wait past the debouncer's
    // delay so the event is emitted and buffered in the writer (the batch
    // timer has not committed it yet), then shut down.
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(watch_dir.join("late_change.txt"), "late content").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), start)
        .await
        .expect("start_watching did not return after shutdown signal")
        .expect("start_watching task panicked")
        .expect("start_watching returned Err");

    // The late change must be visible in the committed index.
    let reader = writer.index().reader().unwrap();
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
        )
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "change made before shutdown must be flushed and committed"
    );
}

#[tokio::test]
async fn test_startup_scan_indexes_new_folder_after_restart() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    // Simulate first run: create initial file and scan
    std::fs::write(watch_dir.join("existing.txt"), "hello world").unwrap();

    {
        let coordinator =
            IndexCoordinator::new(config.clone(), file_indexr::formats::new_text_data_cache())
                .await
                .unwrap();
        let count = coordinator.startup_scan().await.unwrap();
        assert_eq!(count, 1);
    }

    // Wait for filesystem timestamps to separate from checkpoint time
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Simulate app restart: new folder appears while app was offline
    let new_folder = watch_dir.join("new_docs");
    std::fs::create_dir_all(&new_folder).unwrap();
    std::fs::write(new_folder.join("report.md"), "quarterly report").unwrap();
    std::fs::write(new_folder.join("data.txt"), "some data here").unwrap();

    // Second startup — new folder must be indexed
    {
        let coordinator =
            IndexCoordinator::new(config.clone(), file_indexr::formats::new_text_data_cache())
                .await
                .unwrap();
        let count = coordinator.startup_scan().await.unwrap();
        assert_eq!(count, 2, "Expected 2 new files from new_docs folder");

        // Verify total documents in index: 1 existing + 2 new = 3
        let reader = coordinator.writer().index().reader().unwrap();
        let searcher = reader.searcher();
        let results: Vec<_> = searcher
            .search(
                &tantivy::query::AllQuery {},
                &tantivy::collector::TopDocs::with_limit(10).order_by_score(),
            )
            .unwrap();
        assert_eq!(results.len(), 3, "Expected 3 total documents in index");
    }
}
