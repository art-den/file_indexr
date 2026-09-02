use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

use file_indexr::config::Config;
use file_indexr::index::checkpoint::{Checkpoint, save_checkpoint};
use file_indexr::index::scanner::scan_directory;
use file_indexr::index::writer::IndexWriterWrapper;
use file_indexr::watch::FileChange;

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
// Scanner tests
// ============================================================================

#[tokio::test]
async fn test_scan_empty_directory() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));
    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);

    let count = scan_directory(&config, writer.index(), tx).await.unwrap();
    assert_eq!(count, 0);

    // No events should be sent
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn test_scan_files() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    std::fs::write(watch_dir.join("a.txt"), "content a").unwrap();
    std::fs::write(watch_dir.join("b.txt"), "content b").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    let (tx, mut rx) = mpsc::channel::<FileChange>(100);

    let count = scan_directory(&config, writer.index(), tx).await.unwrap();
    assert_eq!(count, 2);

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
        if events.len() >= 2 {
            break;
        }
    }
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn test_scan_subdirectories() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    std::fs::create_dir_all(watch_dir.join("src")).unwrap();
    std::fs::write(watch_dir.join("src").join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(watch_dir.join("README.md"), "# Hello").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    let (tx, _rx) = mpsc::channel::<FileChange>(100);

    let count = scan_directory(&config, writer.index(), tx).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_incremental_scan_skips_unchanged() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    std::fs::write(watch_dir.join("unchanged.txt"), "old content").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    // First scan — process events through writer so file is in the index
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        let count = scan_directory(&config, writer.index(), tx).await.unwrap();
        assert_eq!(count, 1);

        while let Some(change) = rx.recv().await {
            match change {
                FileChange::Modified(path) => {
                    writer.add_file(path).await.unwrap();
                }
                FileChange::Deleted(_) => {}
                FileChange::DeletedDir(_) => {}
                FileChange::Rescan => panic!("scan must not emit Rescan"),
            }
        }
        writer.commit().await;
    }

    // Save checkpoint (caller's responsibility after commit)
    {
        let mut cp = Checkpoint::new_started();
        cp.mark_completed(1);
        save_checkpoint(&index_dir, &cp).await.unwrap();
    }

    // Wait for filesystem timestamps to settle
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Second scan — file is unchanged, should report 0 changes
    {
        let (tx2, _rx2) = mpsc::channel::<FileChange>(100);
        let count2 = scan_directory(&config, writer.index(), tx2).await.unwrap();
        assert_eq!(count2, 0);
    }
}

#[tokio::test]
async fn test_scan_detects_deleted_files() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    // Create two files
    std::fs::write(watch_dir.join("keep.txt"), "keep me").unwrap();
    std::fs::write(watch_dir.join("remove.txt"), "delete me").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    // First scan — both files indexed, process events through writer
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        let count = scan_directory(&config, writer.index(), tx).await.unwrap();
        assert_eq!(count, 2);

        // Process events through writer so files are actually in the index
        while let Some(change) = rx.recv().await {
            match change {
                FileChange::Modified(path) => {
                    writer.add_file(path).await.unwrap();
                }
                FileChange::Deleted(_) => {}
                FileChange::DeletedDir(_) => {}
                FileChange::Rescan => panic!("scan must not emit Rescan"),
            }
        }
    }

    // Commit so files appear in the index term dictionary
    writer.commit().await;

    // Save checkpoint (caller's responsibility after commit)
    {
        let mut cp = Checkpoint::new_started();
        cp.mark_completed(2);
        save_checkpoint(&index_dir, &cp).await.unwrap();
    }

    // Delete one file while "server is offline"
    std::fs::remove_file(watch_dir.join("remove.txt")).unwrap();

    // Second scan — should detect the deletion from index terms
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        let count = scan_directory(&config, writer.index(), tx).await.unwrap();
        // 0 modified (keep.txt unchanged), but 1 deleted event should arrive
        assert_eq!(count, 0);

        // Collect all events
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
            if events.len() >= 5 {
                break;
            }
        }

        // Should have exactly 1 Deleted event for remove.txt
        let deleted_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, FileChange::Deleted(_)))
            .collect();
        assert_eq!(deleted_events.len(), 1);

        if let FileChange::Deleted(p) = &deleted_events[0] {
            assert_eq!(p.file_name().unwrap(), "remove.txt");
        }
    }
}

#[tokio::test]
async fn test_scan_no_deletion_when_all_files_exist() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    std::fs::write(watch_dir.join("a.txt"), "content").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    // First scan — process events through writer
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        scan_directory(&config, writer.index(), tx).await.unwrap();
        while let Some(change) = rx.recv().await {
            match change {
                FileChange::Modified(path) => {
                    writer.add_file(path).await.unwrap();
                }
                FileChange::Deleted(_) => {}
                FileChange::DeletedDir(_) => {}
                FileChange::Rescan => panic!("scan must not emit Rescan"),
            }
        }
        writer.commit().await;

        // Save checkpoint (caller's responsibility after commit)
        let mut cp = Checkpoint::new_started();
        cp.mark_completed(1);
        save_checkpoint(&index_dir, &cp).await.unwrap();
    }

    // Second scan — no files deleted
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        scan_directory(&config, writer.index(), tx).await.unwrap();

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        let deleted: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, FileChange::Deleted(_)))
            .collect();
        assert!(deleted.is_empty());
    }
}

#[tokio::test]
async fn test_scan_includes_dotfiles_but_skips_hidden_dirs() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    // Dot files at top level — should be indexed. Dot files carry a
    // recognized extension: with the empty default, extensionless files have
    // no recognizable format and are skipped.
    std::fs::write(watch_dir.join(".env.txt"), "SECRET=val").unwrap();
    std::fs::write(watch_dir.join(".gitignore.txt"), "*.log").unwrap();
    std::fs::write(watch_dir.join("normal.txt"), "hello").unwrap();

    // Hidden directory — should be skipped
    std::fs::create_dir_all(watch_dir.join(".git")).unwrap();
    std::fs::write(watch_dir.join(".git/config"), "[core]").unwrap();

    // Normal subdirectory with dotfile — should be indexed
    std::fs::create_dir_all(watch_dir.join("src")).unwrap();
    std::fs::write(watch_dir.join("src/.env.local.txt"), "DEV=true").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    let (tx, _rx) = mpsc::channel::<FileChange>(100);

    let count = scan_directory(&config, writer.index(), tx).await.unwrap();
    // .env.txt, .gitignore.txt, normal.txt, src/.env.local.txt — 4 files
    assert_eq!(count, 4);
}

#[tokio::test]
async fn test_scan_picks_up_new_folder_after_checkpoint() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    // Initial file
    std::fs::write(watch_dir.join("existing.txt"), "old content").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    // First scan — process events through writer
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        let count = scan_directory(&config, writer.index(), tx).await.unwrap();
        assert_eq!(count, 1);

        while let Some(change) = rx.recv().await {
            match change {
                FileChange::Modified(path) => {
                    writer.add_file(path).await.unwrap();
                }
                FileChange::Deleted(_) => {}
                FileChange::DeletedDir(_) => {}
                FileChange::Rescan => panic!("scan must not emit Rescan"),
            }
        }
    }

    writer.commit().await;

    // Save checkpoint
    {
        let mut cp = Checkpoint::new_started();
        cp.mark_completed(1);
        save_checkpoint(&index_dir, &cp).await.unwrap();
    }

    // Wait so filesystem timestamps don't collide with checkpoint time
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Simulate new folder appearing while the app was offline
    let new_folder = watch_dir.join("new_folder");
    std::fs::create_dir_all(&new_folder).unwrap();
    std::fs::write(new_folder.join("a.txt"), "new file a").unwrap();
    std::fs::write(new_folder.join("b.txt"), "new file b").unwrap();

    // Second scan — must pick up the new folder's files
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let count = scan_directory(&config, writer.index(), tx).await.unwrap();
    assert_eq!(count, 2, "Expected 2 new files from new_folder");

    // Collect and verify events
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn test_scan_reindexes_deleted_then_restored_file_with_old_mtime() {
    // Regression: after a file is deleted (doc removed via alive bitset, but
    // the phantom term remains in the FST until merge) and the app restarts,
    // a file restored from backup with an mtime older than the checkpoint
    // must still be re-indexed — the phantom FST term must not hide it.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    std::fs::write(watch_dir.join("a.txt"), "content a").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    // First run — index the file
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        let count = scan_directory(&config, writer.index(), tx).await.unwrap();
        assert_eq!(count, 1);
        while let Some(change) = rx.recv().await {
            match change {
                FileChange::Modified(path) => {
                    writer.add_file(path).await.unwrap();
                }
                FileChange::Deleted(_) => {}
                FileChange::DeletedDir(_) => {}
                FileChange::Rescan => panic!("scan must not emit Rescan"),
            }
        }
    }
    writer.commit().await;

    // Save checkpoint (caller's responsibility after commit)
    {
        let mut cp = Checkpoint::new_started();
        cp.mark_completed(1);
        save_checkpoint(&index_dir, &cp).await.unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Live deletion (as the watcher would do), then commit
    writer.delete_file(watch_dir.join("a.txt")).await.unwrap();
    writer.commit().await;

    // Restore the file from "backup" with an mtime older than the checkpoint
    std::fs::write(watch_dir.join("a.txt"), "content a restored").unwrap();
    let past = chrono::Utc::now() - chrono::Duration::minutes(5);
    let past_std: std::time::SystemTime = past.into();
    filetime::set_file_mtime(watch_dir.join("a.txt"), past_std.into()).unwrap();

    // Second run — the file must be re-indexed despite the phantom FST term
    let (tx, mut rx) = mpsc::channel::<FileChange>(100);
    let count = scan_directory(&config, writer.index(), tx).await.unwrap();
    assert_eq!(count, 1, "Restored file with old mtime must be re-indexed");

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], FileChange::Modified(_)));
}

#[tokio::test]
async fn test_scan_new_folder_files_with_backdated_mtime() {
    // Files whose mtime is in the past (before checkpoint) — must STILL be
    // picked up because they are NOT in the index. The scanner compares
    // filesystem against indexed paths, so new files are detected regardless
    // of mtime.
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = Arc::new(make_config(&watch_dir, &index_dir));

    std::fs::write(watch_dir.join("base.txt"), "base").unwrap();

    let writer = Arc::new(
        IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            file_indexr::formats::new_text_data_cache(),
        )
        .await
        .unwrap(),
    );

    // First scan
    {
        let (tx, mut rx) = mpsc::channel::<FileChange>(100);
        let count = scan_directory(&config, writer.index(), tx).await.unwrap();
        assert_eq!(count, 1);
        while let Some(change) = rx.recv().await {
            match change {
                FileChange::Modified(path) => {
                    writer.add_file(path).await.unwrap();
                }
                FileChange::Deleted(_) => {}
                FileChange::DeletedDir(_) => {}
                FileChange::Rescan => panic!("scan must not emit Rescan"),
            }
        }
    }
    writer.commit().await;

    // Save checkpoint, then wait
    {
        let mut cp = Checkpoint::new_started();
        cp.mark_completed(1);
        save_checkpoint(&index_dir, &cp).await.unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Create new folder with files, but backdate their mtime to before checkpoint
    let new_folder = watch_dir.join("old_stuff");
    std::fs::create_dir_all(&new_folder).unwrap();
    let file_a = new_folder.join("a.txt");
    let file_b = new_folder.join("b.txt");
    std::fs::write(&file_a, "old a").unwrap();
    std::fs::write(&file_b, "old b").unwrap();

    // Touch files with a past timestamp (5 minutes ago)
    let past = chrono::Utc::now() - chrono::Duration::minutes(5);
    let past_std: std::time::SystemTime = past.into();
    filetime::set_file_mtime(&file_a, past_std.into()).unwrap();
    filetime::set_file_mtime(&file_b, past_std.into()).unwrap();

    // Second scan — must detect these files as new (not in index)
    let (tx, _rx) = mpsc::channel::<FileChange>(100);
    let count = scan_directory(&config, writer.index(), tx).await.unwrap();

    assert_eq!(count, 2, "New files detected even with backdated mtime");
}
