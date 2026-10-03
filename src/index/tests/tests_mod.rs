use crate::search::{SearchParams, search};

use super::*;

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
        u64::MAX,
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
