use std::path::PathBuf;

use file_indexr::index::checkpoint::{
    CHECKPOINT_FILENAME, Checkpoint, delete_checkpoint, load_checkpoint, save_checkpoint,
};

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("checkpoint_test")
}

// ============================================================================
// Checkpoint tests
// ============================================================================

#[test]
fn test_new_started_checkpoint() {
    let cp = Checkpoint::new_started();
    assert_eq!(cp.version, 1);
    assert!(cp.completed_at.is_none());
    assert_eq!(cp.files_processed, 0);
    // started_at should be recent
    let diff = chrono::Utc::now().signed_duration_since(cp.started_at);
    assert!(diff.num_seconds() < 5);
}

#[test]
fn test_mark_completed() {
    let mut cp = Checkpoint::new_started();
    cp.mark_completed(42);
    assert!(cp.completed_at.is_some());
    assert_eq!(cp.files_processed, 42);
}

#[tokio::test]
async fn test_save_and_load_checkpoint() {
    let dir = make_temp_dir();
    let mut cp = Checkpoint::new_started();
    cp.mark_completed(100);

    save_checkpoint(&dir, &cp).await.unwrap();

    let loaded = load_checkpoint(&dir).await.unwrap().unwrap();
    assert_eq!(loaded.version, 1);
    assert_eq!(loaded.files_processed, 100);
    assert!(loaded.completed_at.is_some());
}

#[tokio::test]
async fn test_load_checkpoint_missing() {
    let dir = make_temp_dir();
    let result = load_checkpoint(&dir).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_load_checkpoint_empty_file() {
    let dir = make_temp_dir();
    let path = dir.join(CHECKPOINT_FILENAME);
    std::fs::write(&path, "").unwrap();

    let result = load_checkpoint(&dir).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_load_checkpoint_corrupted() {
    let dir = make_temp_dir();
    let path = dir.join(CHECKPOINT_FILENAME);
    std::fs::write(&path, "not valid json {{{").unwrap();

    // A corrupted checkpoint must not block scanning: fall back to a full
    // scan (Ok(None)); a successful scan overwrites the damaged file.
    let result = load_checkpoint(&dir).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn test_load_checkpoint_wrong_version() {
    let dir = make_temp_dir();
    let path = dir.join(CHECKPOINT_FILENAME);
    std::fs::write(&path, r#"{"version": 99, "started_at": "2026-01-01T00:00:00Z", "completed_at": null, "files_processed": 0}"#).unwrap();

    let result = load_checkpoint(&dir).await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("Unsupported checkpoint version")
    );
}

#[tokio::test]
async fn test_delete_checkpoint() {
    let dir = make_temp_dir();
    let cp = Checkpoint::new_started();
    save_checkpoint(&dir, &cp).await.unwrap();
    assert!(load_checkpoint(&dir).await.unwrap().is_some());

    delete_checkpoint(&dir).await.unwrap();
    assert!(load_checkpoint(&dir).await.unwrap().is_none());
}

#[tokio::test]
async fn test_delete_checkpoint_nonexistent() {
    let dir = make_temp_dir();
    // Should not error when file doesn't exist
    delete_checkpoint(&dir).await.unwrap();
}

#[tokio::test]
async fn test_checkpoint_persists_across_reloads() {
    let dir = make_temp_dir();
    let mut cp1 = Checkpoint::new_started();
    cp1.mark_completed(50);
    save_checkpoint(&dir, &cp1).await.unwrap();

    // Simulate reload
    let loaded = load_checkpoint(&dir).await.unwrap().unwrap();
    assert_eq!(loaded.files_processed, 50);

    // Update and save again
    let mut cp2 = Checkpoint::new_started();
    cp2.mark_completed(75);
    save_checkpoint(&dir, &cp2).await.unwrap();

    let reloaded = load_checkpoint(&dir).await.unwrap().unwrap();
    assert_eq!(reloaded.files_processed, 75);
}

#[tokio::test]
async fn test_save_checkpoint_creates_parent_dir() {
    let dir = make_temp_dir();
    let subdir = dir.join("sub").join("index");
    let cp = Checkpoint::new_started();

    // save_checkpoint does NOT create parent dirs — it expects them to exist
    std::fs::create_dir_all(&subdir).unwrap();
    save_checkpoint(&subdir, &cp).await.unwrap();
    assert!(load_checkpoint(&subdir).await.unwrap().is_some());
}
