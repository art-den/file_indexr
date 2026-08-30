use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use file_indexr::config::Config;
use file_indexr::index::scanner::has_hidden_component;
use file_indexr::index::writer::IndexWriterWrapper;
use file_indexr::watch::FileChange;
use file_indexr::watch::spawn_watcher;

use tokio::sync::mpsc;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("integ_test")
}

fn make_config(watch_dir: PathBuf, index_dir: PathBuf) -> Config {
    Config {
        directory: watch_dir,
        index_path: index_dir,
        port: 0,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,

        allowed_extensions: vec![],
    }
}

/// Create config with specific allowed extensions.
fn make_config_with_extensions(watch_dir: PathBuf, index_dir: PathBuf, allowed: &[&str]) -> Config {
    Config {
        directory: watch_dir,
        index_path: index_dir,
        port: 0,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,

        allowed_extensions: allowed.iter().map(|s| s.to_string()).collect(),
    }
}

/// Apply a FileChange to the writer the way the coordinator's event loop
/// does. Rescan (inotify queue overflow) is a coordinator-level concern and
/// is not exercised by these tests.
async fn apply_change(writer: &IndexWriterWrapper, change: FileChange) {
    match change {
        FileChange::Modified(path) => {
            let _ = writer.add_file(path).await;
        }
        FileChange::Deleted(path) => {
            let _ = writer.delete_file(path).await;
        }
        FileChange::DeletedDir(path) => {
            let _ = writer.delete_dir(path).await;
        }
        FileChange::Rescan => {}
    }
}

/// Collect FileChange events from the watcher channel using idle-based timeout.
///
/// Waits up to `overall_timeout` for the first event (needed for debounce).
/// After receiving at least one event, returns as soon as no new events
/// arrive within `idle_timeout`. This avoids waiting for the full overall
/// timeout after the last event has been received.
async fn collect_changes(
    rx: &mut mpsc::Receiver<FileChange>,
    idle_timeout: Duration,
    overall_timeout: Duration,
) -> Vec<FileChange> {
    let mut changes = Vec::new();
    let start = std::time::Instant::now();

    loop {
        let elapsed = start.elapsed();
        if elapsed >= overall_timeout {
            break;
        }

        let remaining = overall_timeout - elapsed;
        // Use idle timeout only after receiving at least one event;
        // before that, wait the full remaining time (debounce may not have fired yet).
        let recv_timeout = if changes.is_empty() {
            remaining
        } else if idle_timeout < remaining {
            idle_timeout
        } else {
            remaining
        };

        match tokio::time::timeout(recv_timeout, rx.recv()).await {
            Ok(Some(change)) => changes.push(change),
            Ok(None) => break, // channel closed
            Err(_) => break,   // timeout (idle or overall)
        }
    }
    changes
}

// ============================================================================
// Tests: File creation — channel-based verification
// ============================================================================

// ============================================================================
// Tests: File creation — channel-based verification
// ============================================================================

#[tokio::test]
async fn test_watcher_file_created_in_root() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    // Initial commit so the index is ready
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    // Allow watcher to initialize
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a file
    let file_path = watch_dir.join("hello.txt");
    std::fs::write(&file_path, "Hello, world!")?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;
    eprintln!("DEBUG: received changes: {:?}", changes);
    let has_modified = changes
        .iter()
        .any(|c| matches!(c, FileChange::Modified(p) if p == &file_path));
    assert!(
        has_modified,
        "Expected Modified({:?}) but got {:?}",
        file_path, changes
    );

    Ok(())
}

#[tokio::test]
async fn test_watcher_file_created_in_nested_dir() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    // Longer init wait to ensure recursive watching is set up
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create nested directory and file
    let nested_dir = watch_dir.join("sub").join("deep");
    std::fs::create_dir_all(&nested_dir)?;
    // Small delay between directory creation and file creation
    tokio::time::sleep(Duration::from_millis(200)).await;
    let file_path = nested_dir.join("nested.txt");
    std::fs::write(&file_path, "nested content")?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(4)).await;
    let has_modified = changes
        .iter()
        .any(|c| matches!(c, FileChange::Modified(p) if p == &file_path));
    assert!(
        has_modified,
        "Expected Modified({:?}) but got {:?}",
        file_path, changes
    );

    Ok(())
}

// ============================================================================
// Tests: File modification — channel-based verification
// ============================================================================

#[tokio::test]
async fn test_watcher_file_modified_in_root() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create initial file and drain its events
    let file_path = watch_dir.join("modify_me.txt");
    std::fs::write(&file_path, "original")?;
    // Drain creation events
    let _ = collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;

    // Modify the file
    std::fs::write(&file_path, "modified content")?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;
    let has_modified = changes
        .iter()
        .any(|c| matches!(c, FileChange::Modified(p) if p == &file_path));
    assert!(
        has_modified,
        "Expected Modified({:?}) but got {:?}",
        file_path, changes
    );

    Ok(())
}

#[tokio::test]
async fn test_watcher_file_modified_in_nested_dir() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create nested file and drain creation events
    let nested_dir = watch_dir.join("sub");
    std::fs::create_dir_all(&nested_dir)?;
    let file_path = nested_dir.join("nested_modify.txt");
    std::fs::write(&file_path, "original")?;
    // Drain creation events
    let _ = collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;

    // Modify the nested file
    std::fs::write(&file_path, "modified nested content")?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;
    let has_modified = changes
        .iter()
        .any(|c| matches!(c, FileChange::Modified(p) if p == &file_path));
    assert!(
        has_modified,
        "Expected Modified({:?}) but got {:?}",
        file_path, changes
    );

    Ok(())
}

// ============================================================================
// Tests: File deletion — channel-based verification (immediate)
// ============================================================================

#[tokio::test]
async fn test_watcher_file_deleted_in_root() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a file first and drain creation events
    let file_path = watch_dir.join("to_delete.txt");
    std::fs::write(&file_path, "delete me")?;
    // Drain creation events
    let _ = collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;

    // Delete the file — deletion should be immediate (no debounce)
    std::fs::remove_file(&file_path)?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(300), Duration::from_secs(2)).await;
    let has_deleted = changes
        .iter()
        .any(|c| matches!(c, FileChange::Deleted(p) if p == &file_path));
    assert!(
        has_deleted,
        "Expected Deleted({:?}) but got {:?}",
        file_path, changes
    );

    Ok(())
}

#[tokio::test]
async fn test_watcher_file_deleted_in_nested_dir() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a nested file and drain creation events
    let nested_dir = watch_dir.join("sub");
    std::fs::create_dir_all(&nested_dir)?;
    let file_path = nested_dir.join("to_delete_nested.txt");
    std::fs::write(&file_path, "delete me nested")?;
    // Drain creation events
    let _ = collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;

    // Delete the nested file — immediate
    std::fs::remove_file(&file_path)?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(300), Duration::from_secs(2)).await;
    let has_deleted = changes
        .iter()
        .any(|c| matches!(c, FileChange::Deleted(p) if p == &file_path));
    assert!(
        has_deleted,
        "Expected Deleted({:?}) but got {:?}",
        file_path, changes
    );

    Ok(())
}

// ============================================================================
// Tests: Index directory exclusion
// ============================================================================

#[tokio::test]
async fn test_index_dir_events_are_excluded() -> Result<()> {
    // When index_path is inside the watched directory, its events should be ignored.
    // The index dir is intentionally non-hidden: a hidden name (e.g. `.file_indexr`)
    // would be excluded by the hidden-component rule anyway, masking the
    // index-prefix rule under test.
    let watch_dir = make_temp_dir();
    let index_dir = watch_dir.join("file_indexr_data");
    // Anti-masking guard: a hidden index dir name would be excluded by the
    // hidden-component rule, masking the index-prefix rule under test.
    assert!(!has_hidden_component(
        &index_dir.join("fake_data.tmp"),
        &watch_dir
    ));
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a file INSIDE the index directory — should be ignored
    std::fs::write(index_dir.join("fake_data.tmp"), "should be ignored")?;

    // Wait — no events for this path should arrive
    let changes =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(2)).await;
    let has_index_event = changes.iter().any(|c| match c {
        FileChange::Modified(p) | FileChange::Deleted(p) | FileChange::DeletedDir(p) => {
            p.starts_with(&index_dir)
        }
        FileChange::Rescan => false,
    });
    assert!(
        !has_index_event,
        "Events from the index directory should be excluded, but got: {:?}",
        changes
    );

    // Verify that a file OUTSIDE the index dir IS tracked
    std::fs::write(watch_dir.join("real_file.txt"), "this should be tracked")?;

    let changes =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;
    let real_path = watch_dir.join("real_file.txt");
    let has_real_event = changes
        .iter()
        .any(|c| matches!(c, FileChange::Modified(p) if p == &real_path));
    assert!(
        has_real_event,
        "Events for files outside the index directory should still be tracked, but got: {:?}",
        changes
    );

    Ok(())
}

// ============================================================================
// Tests: Full index lifecycle (spawn processor, verify via search)
// These are heavier — they verify the end-to-end flow.
// ============================================================================

/// Helper to count docs in an index — uses a separate Index open (no writer lock)
async fn doc_count(index_dir: &PathBuf, _config: &Config) -> Result<u64> {
    let directory = tantivy::directory::MmapDirectory::open(index_dir)?;
    let index = tantivy::Index::open(directory)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let results: Vec<_> = searcher
        .search(
            &tantivy::query::AllQuery {},
            &tantivy::collector::TopDocs::with_limit(10_000).order_by_score(),
        )
        .unwrap();
    Ok(results.len() as u64)
}

#[tokio::test]
async fn test_full_lifecycle_create_modify_delete_root() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    // Initial empty index
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    // Start watcher + processor
    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 1: Create a file
    std::fs::write(watch_dir.join("test.txt"), "hello world")?;
    // Wait for debounce (1s) + commit (0.5s) + margin
    tokio::time::sleep(Duration::from_secs(3)).await;

    {
        let count = doc_count(&index_dir, &config).await?;
        assert_eq!(count, 1, "Expected 1 doc after creation");
    }

    // Step 2: Modify the file
    std::fs::write(watch_dir.join("test.txt"), "modified content")?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    {
        let count = doc_count(&index_dir, &config).await?;
        assert_eq!(
            count, 1,
            "Expected 1 doc after modification (re-indexed, not duplicated)"
        );
    }

    // Step 3: Delete the file
    std::fs::remove_file(watch_dir.join("test.txt"))?;
    // Deletion is immediate + commit
    tokio::time::sleep(Duration::from_secs(2)).await;

    {
        let count = doc_count(&index_dir, &config).await?;
        assert_eq!(count, 0, "Expected 0 docs after deletion");
    }

    processor.abort();
    Ok(())
}

#[tokio::test]
async fn test_full_lifecycle_nested_directory() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    // Initial empty index
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    // Start watcher + processor
    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 1: Create a file in nested directory
    let sub = watch_dir.join("subdir");
    std::fs::create_dir_all(&sub)?;
    // Small delay to ensure recursive watching catches the new dir
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(sub.join("nested.txt"), "nested content")?;
    // Wait: debounce (1s) + commit timer (~0.5s) + margin
    tokio::time::sleep(Duration::from_secs(4)).await;

    {
        let count = doc_count(&index_dir, &config).await?;
        assert_eq!(count, 1, "Expected 1 doc after nested creation");
    }

    // Step 2: Delete the nested file
    std::fs::remove_file(sub.join("nested.txt"))?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    {
        let count = doc_count(&index_dir, &config).await?;
        assert_eq!(count, 0, "Expected 0 docs after nested deletion");
    }

    processor.abort();
    Ok(())
}

// ============================================================================
// Tests: Allowed extensions filtering during file watching
// ============================================================================

/// Helper to check if a document with exact path exists in the index
async fn has_doc_with_path(index_dir: &PathBuf, rel_path: &str) -> Result<bool> {
    let directory = tantivy::directory::MmapDirectory::open(index_dir)?;
    let index = tantivy::Index::open(directory)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();

    // Use path_exact (STRING field) for exact matching
    let schema = index.schema();
    let path_field = schema.get_field("path_exact").unwrap();
    let term = tantivy::Term::from_field_text(path_field, rel_path);

    let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
    let results: Vec<_> = searcher
        .search(
            &query,
            &tantivy::collector::TopDocs::with_limit(1).order_by_score(),
        )
        .unwrap();
    Ok(!results.is_empty())
}

/// Test that only files with allowed extensions are indexed when watcher detects them.
/// When allowed_extensions=["txt", "md"], a .log file should NOT be indexed.
#[tokio::test]
async fn test_watcher_skips_disallowed_extension_in_root() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    // Only .txt and .md are allowed
    let config = Arc::new(make_config_with_extensions(
        watch_dir.clone(),
        index_dir.clone(),
        &["txt", "md"],
    ));

    // Initial empty index
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create a disallowed .log file — should NOT be indexed
    std::fs::write(watch_dir.join("disallowed.log"), "log content")?;
    // Create an allowed .txt file — SHOULD be indexed
    std::fs::write(watch_dir.join("allowed.txt"), "text content")?;

    // Wait for debounce + commit
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Verify only the allowed file is in the index
    let total = doc_count(&index_dir, &config).await?;
    assert_eq!(total, 1, "Expected 1 doc (.txt), not .log");

    // Verify it's specifically the .txt file that's indexed
    let has_txt = has_doc_with_path(&index_dir, "allowed.txt").await?;
    assert!(has_txt, "Expected 'allowed.txt' to be in index");

    let has_log = has_doc_with_path(&index_dir, "disallowed.log").await?;
    assert!(!has_log, "Expected 'disallowed.log' to NOT be in index");

    processor.abort();
    Ok(())
}

/// Test allowed extensions with files in nested directories.
#[tokio::test]
async fn test_watcher_skips_disallowed_extension_in_nested_dir() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config_with_extensions(
        watch_dir.clone(),
        index_dir.clone(),
        &["rs", "toml"], // Only Rust files
    ));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create mixed extensions in a nested directory
    let sub = watch_dir.join("src");
    std::fs::create_dir_all(&sub)?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    std::fs::write(sub.join("lib.rs"), "fn main() {}")?;
    std::fs::write(sub.join("notes.md"), "# Notes")?; // not allowed
    std::fs::write(sub.join("data.json"), r#"{"key":"val"}"#)?; // not allowed

    tokio::time::sleep(Duration::from_secs(4)).await;

    let total = doc_count(&index_dir, &config).await?;
    assert_eq!(total, 1, "Expected only .rs file to be indexed");

    let has_rs = has_doc_with_path(&index_dir, "src/lib.rs").await?;
    assert!(has_rs, "Expected 'src/lib.rs' to be in index");

    processor.abort();
    Ok(())
}

/// Test that changing allowed extensions doesn't break the watcher.
/// Files with newly-allowed extensions should be indexed when created.
#[tokio::test]
async fn test_watcher_allowed_extension_filter_applies_on_watch() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    // Only .txt files allowed
    let config = Arc::new(make_config_with_extensions(
        watch_dir.clone(),
        index_dir.clone(),
        &["txt"],
    ));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create multiple files with different extensions
    std::fs::write(watch_dir.join("a.txt"), "allowed")?;
    std::fs::write(watch_dir.join("b.json"), r#"{}"#)?;
    std::fs::write(watch_dir.join("c.py"), "print('hi')")?;
    std::fs::write(watch_dir.join("d.txt"), "also allowed")?;

    tokio::time::sleep(Duration::from_secs(4)).await;

    // Only .txt files should be indexed (2 files)
    let total = doc_count(&index_dir, &config).await?;
    assert_eq!(total, 2, "Expected exactly 2 .txt files indexed");

    assert!(
        has_doc_with_path(&index_dir, "a.txt").await?,
        "a.txt missing"
    );
    assert!(
        has_doc_with_path(&index_dir, "d.txt").await?,
        "d.txt missing"
    );
    assert!(
        !has_doc_with_path(&index_dir, "b.json").await?,
        "b.json should not be indexed"
    );
    assert!(
        !has_doc_with_path(&index_dir, "c.py").await?,
        "c.py should not be indexed"
    );

    processor.abort();
    Ok(())
}

// ============================================================================
// Tests: File rename handling — verifies old path is removed from index
// ============================================================================

/// Channel-based test: renaming a file emits Deleted(old) + Modified(new).
///
/// Deleted(old) is emitted immediately (bypasses debounce).
/// Modified(new) is debounced — arrives ~1s later.
/// We collect in two phases to handle both.
#[tokio::test]
async fn test_watcher_rename_emits_delete_and_modified() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    // Allow watcher to initialize
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create initial file and drain its events
    let old_path = watch_dir.join("old_name.txt");
    std::fs::write(&old_path, "initial content")?;
    let _ = collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(3)).await;

    // Rename the file
    let new_path = watch_dir.join("new_name.txt");
    std::fs::rename(&old_path, &new_path)?;

    // Phase 1: collect immediate events (Deleted bypasses debounce)
    let immediate =
        collect_changes(&mut rx, Duration::from_millis(500), Duration::from_secs(1)).await;
    eprintln!("DEBUG rename immediate: {:?}", immediate);

    // Phase 2: wait for debounced Modified(new) — debounce is ~1s
    tokio::time::sleep(Duration::from_secs(2)).await;
    let debounced =
        collect_changes(&mut rx, Duration::from_millis(200), Duration::from_secs(2)).await;
    eprintln!("DEBUG rename debounced: {:?}", debounced);

    let all_changes: Vec<_> = immediate.into_iter().chain(debounced).collect();

    // Must have Deleted(old_path)
    let has_deleted = all_changes
        .iter()
        .any(|c| matches!(c, FileChange::Deleted(p) if p == &old_path));
    assert!(
        has_deleted,
        "Expected Deleted({:?}) but got {:?}. Without this, the old index entry becomes a ghost.",
        old_path, all_changes
    );

    // Must have Modified(new_path)
    let has_modified = all_changes
        .iter()
        .any(|c| matches!(c, FileChange::Modified(p) if p == &new_path));
    assert!(
        has_modified,
        "Expected Modified({:?}) but got {:?}",
        new_path, all_changes
    );

    Ok(())
}

/// Full lifecycle test: after renaming a file, the index should contain only
/// the new path — not the old one. This catches the bug where RenameMode::From
/// was silently ignored, leaving a ghost record for the old path in the index.
#[tokio::test]
async fn test_full_lifecycle_rename_removes_old_path() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    // Initial empty index
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    // Start watcher + processor
    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 1: Create a file and verify it's in the index
    let old_path = watch_dir.join("old.txt");
    std::fs::write(&old_path, "hello world")?;
    // Wait for debounce (1s) + commit (0.5s) + margin
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        has_doc_with_path(&index_dir, "old.txt").await?,
        "old.txt should be indexed after creation"
    );
    let count = doc_count(&index_dir, &config).await?;
    assert_eq!(count, 1, "Expected 1 doc after creation");

    // Step 2: Rename the file
    let new_path = watch_dir.join("renamed.txt");
    std::fs::rename(&old_path, &new_path)?;
    // Deleted(old) is immediate, Modified(new) goes through debounce + commit
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The critical assertions:
    // 1. Old path must NOT be in the index anymore
    assert!(
        !has_doc_with_path(&index_dir, "old.txt").await?,
        "old.txt should NOT be in the index after rename — this was the bug"
    );

    // 2. New path MUST be in the index
    assert!(
        has_doc_with_path(&index_dir, "renamed.txt").await?,
        "renamed.txt should be in the index after rename"
    );

    // 3. Total doc count should still be 1 (not 2)
    let final_count = doc_count(&index_dir, &config).await?;
    assert_eq!(
        final_count, 1,
        "Expected 1 doc after rename (old removed, new added), got {}",
        final_count
    );

    processor.abort();
    Ok(())
}

// ============================================================================
// Tests: Hidden directory renamed to visible — files must be indexed
// ============================================================================

/// When a hidden directory (starting with '.') is renamed to a visible name,
/// the watcher must emit Modified events for all files inside so they get indexed.
/// This catches the bug where renaming `.cpp` → `cpp` resulted in no indexing
/// because `notify` does not emit per-file events for directory renames.
#[tokio::test]
async fn test_rename_hidden_dir_to_visible_indexes_files() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    // Initial empty index
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    // Start watcher + processor
    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 1: Create a hidden directory with files (should NOT be indexed)
    let hidden_dir = watch_dir.join(".cpp");
    std::fs::create_dir_all(&hidden_dir)?;
    std::fs::write(hidden_dir.join("main.cpp"), "int main() { return 0; }")?;
    std::fs::write(hidden_dir.join("util.cpp"), "void util() {}")?;
    // Wait for any events to settle
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Files in .cpp should NOT be indexed
    assert!(
        !has_doc_with_path(&index_dir, ".cpp/main.cpp").await?,
        ".cpp/main.cpp should NOT be indexed — hidden directory"
    );
    assert!(
        !has_doc_with_path(&index_dir, ".cpp/util.cpp").await?,
        ".cpp/util.cpp should NOT be indexed — hidden directory"
    );
    let count_before = doc_count(&index_dir, &config).await?;
    assert_eq!(
        count_before, 0,
        "Expected 0 docs — hidden directory files should not be indexed"
    );

    // Step 2: Rename .cpp → cpp (hidden → visible)
    let visible_dir = watch_dir.join("cpp");
    std::fs::rename(&hidden_dir, &visible_dir)?;
    // Wait for debounce (1s) + commit (0.5s) + margin
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Step 3: Files in cpp/ MUST now be indexed
    assert!(
        has_doc_with_path(&index_dir, "cpp/main.cpp").await?,
        "cpp/main.cpp should be indexed after renaming .cpp → cpp"
    );
    assert!(
        has_doc_with_path(&index_dir, "cpp/util.cpp").await?,
        "cpp/util.cpp should be indexed after renaming .cpp → cpp"
    );
    let final_count = doc_count(&index_dir, &config).await?;
    assert_eq!(
        final_count, 2,
        "Expected 2 docs after renaming hidden dir to visible, got {}",
        final_count
    );

    processor.abort();
    Ok(())
}

// ============================================================================
// Tests: Visible directory renamed to hidden — files must be removed from index
// ============================================================================

/// When a visible directory containing indexed files is renamed to start with `.`
/// (e.g., `cpp` → `.cpp`), all previously-indexed files must disappear from the
/// search results.
///
/// This is the inverse of [test_rename_hidden_dir_to_visible_indexes_files].
#[tokio::test]
async fn test_rename_visible_dir_to_hidden_removes_files() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    // Initial index — populate with files in cpp/
    {
        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?;
        writer.commit().await;
    }

    // Start watcher + processor
    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await?,
    );
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let _watcher = spawn_watcher(&config, tx).await?;

    let writer_proc = writer.clone();
    let processor = tokio::spawn(async move {
        let mut commit_timer = tokio::time::interval(Duration::from_millis(500));
        commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(change) = rx.recv() => {
                    apply_change(&writer_proc, change).await;
                }
                _ = commit_timer.tick() => {
                    if writer_proc.buffer_len().await > 0 {
                        let _ = writer_proc.commit().await;
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Step 1: Create visible cpp/ directory with files
    let cpp_dir = watch_dir.join("cpp");
    std::fs::create_dir_all(&cpp_dir)?;
    std::fs::write(cpp_dir.join("main.cpp"), "int main() { return 0; }")?;
    std::fs::write(cpp_dir.join("util.cpp"), "void util() {}")?;
    // Wait for debounce (1s) + commit (0.5s) + margin
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Files in cpp/ MUST be indexed
    assert!(
        has_doc_with_path(&index_dir, "cpp/main.cpp").await?,
        "cpp/main.cpp should be indexed"
    );
    assert!(
        has_doc_with_path(&index_dir, "cpp/util.cpp").await?,
        "cpp/util.cpp should be indexed"
    );
    let count_before = doc_count(&index_dir, &config).await?;
    assert_eq!(count_before, 2, "Expected 2 docs before rename");

    // Step 2: Rename cpp → .cpp (visible → hidden)
    let hidden_dir = watch_dir.join(".cpp");
    std::fs::rename(&cpp_dir, &hidden_dir)?;
    // Deleted(cpp) is immediate + commit (0.5s) + margin
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Step 3: Files from cpp/ MUST no longer be in the index
    assert!(
        !has_doc_with_path(&index_dir, "cpp/main.cpp").await?,
        "cpp/main.cpp should NOT be indexed after renaming cpp → .cpp"
    );
    assert!(
        !has_doc_with_path(&index_dir, "cpp/util.cpp").await?,
        "cpp/util.cpp should NOT be indexed after renaming cpp → .cpp"
    );
    let final_count = doc_count(&index_dir, &config).await?;
    assert_eq!(
        final_count, 0,
        "Expected 0 docs after renaming visible dir to hidden, got {}",
        final_count
    );

    processor.abort();
    Ok(())
}

// ============================================================================
// Tests: Watcher task lifecycle — verifies clean shutdown
// ============================================================================

/// Verify that dropping the watcher causes the internal async task to terminate.
///
/// The task holds the only `tx` sender. When it exits, the channel closes,
/// and `rx.recv()` returns `None`. Without the `None => break` fix in
/// `tokio::select!`, this task would loop forever and the channel would
/// never close.
#[tokio::test]
async fn test_watcher_task_exits_on_drop() -> Result<()> {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(watch_dir.clone(), index_dir.clone()));

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let watcher = spawn_watcher(&config, tx).await?;

    // Let the watcher initialize
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Drop the watcher — this disconnects the notify channel,
    // causing the bridge thread to exit, which drops bridge_tx,
    // which should cause the async task to receive None and break.
    drop(watcher);

    // Wait for the channel to close (task must finish flushing and exit).
    // The debounce flush adds ~1s, so give it 3s total.
    let timeout = tokio::time::timeout(Duration::from_secs(3), async {
        // Drain any remaining flushed events and wait for None
        while rx.recv().await.is_some() {}
    });

    timeout
        .await
        .expect("Watcher task did not terminate within 3 seconds — likely stuck in infinite loop");

    Ok(())
}
