use super::*;

fn make_temp_dir() -> PathBuf {
    crate::testutil::unique_temp_dir("formats_test")
}

#[tokio::test]
async fn test_invalidate_text_data_cache() {
    let file = make_temp_dir().join("cache_entry.txt");
    std::fs::write(&file, "version one").unwrap();
    let cache = new_text_data_cache();

    // Warm the cache.
    let first = load_file_text_data(&cache, &file, u64::MAX).await.unwrap();
    assert_eq!(first.text.as_str(), "version one");

    // Modify the file on disk; the cache entry is now stale.
    std::fs::write(&file, "version two").unwrap();

    // Without invalidation the cache still serves the stale copy.
    let stale = load_file_text_data(&cache, &file, u64::MAX).await.unwrap();
    assert_eq!(stale.text.as_str(), "version one");

    cache.invalidate(&file);

    // After invalidation fresh content is read from disk.
    let fresh = load_file_text_data(&cache, &file, u64::MAX).await.unwrap();
    assert_eq!(fresh.text.as_str(), "version two");
}

#[tokio::test]
async fn test_cache_rejects_entry_heavier_than_budget() {
    // Budget is 16 bytes; the 32-byte text weighs 33 and can never fit.
    let file = make_temp_dir().join("too_big.txt");
    std::fs::write(&file, "01234567890123456789012345678901").unwrap();
    let cache = text_data_cache_with_max_weight(16);

    // Loading itself still succeeds; the text is just not retained.
    let data = load_file_text_data(&cache, &file, u64::MAX).await.unwrap();
    assert_eq!(data.text.len(), 32);

    cache.run_pending_tasks();
    assert_eq!(cache.entry_count(), 0);
    assert!(cache.get(&file).is_none());
}

#[tokio::test]
async fn test_cache_evicts_by_weight_lru() {
    let dir = make_temp_dir();
    let mut files = Vec::new();
    for i in 0..4 {
        let file = dir.join(format!("f{i}.txt"));
        // 31 bytes of content -> weight 32 (len + 1 per entry).
        std::fs::write(&file, format!("abcdefghijklmnopqrstuvwxyz0123{i}")).unwrap();
        files.push(file);
    }
    let cache = text_data_cache_with_max_weight(100);

    for file in &files[..3] {
        load_file_text_data(&cache, file, u64::MAX).await.unwrap();
        cache.run_pending_tasks();
    }
    assert_eq!(cache.entry_count(), 3);
    assert_eq!(cache.weighted_size(), 96);

    // The 4th entry (32) overflows the budget (96 + 32 > 100): the LRU entry
    // (f0) is evicted to make room, the rest stays.
    load_file_text_data(&cache, &files[3], u64::MAX)
        .await
        .unwrap();
    cache.run_pending_tasks();
    assert!(cache.get(&files[0]).is_none());
    assert!(cache.get(&files[1]).is_some());
    assert!(cache.get(&files[2]).is_some());
    assert!(cache.get(&files[3]).is_some());
    assert!(cache.weighted_size() <= 100);
}

#[tokio::test]
async fn test_load_rejects_missing_dir_oversized_and_unknown_format() {
    let dir = make_temp_dir();
    let cache = new_text_data_cache();

    // Missing file -> NotFound.
    let missing = dir.join("missing.txt");
    let Err(err) = load_file_text_data(&cache, &missing, u64::MAX).await else {
        panic!("expected an error for a missing file");
    };
    assert!(matches!(err, AppError::NotFound { .. }), "{err}");

    // Directory -> NotAFile.
    let subdir = dir.join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    let Err(err) = load_file_text_data(&cache, &subdir, u64::MAX).await else {
        panic!("expected an error for a directory");
    };
    assert!(matches!(err, AppError::NotAFile { .. }), "{err}");

    // File over max_bytes -> TooLarge, rejected before reading.
    let large = dir.join("large.txt");
    std::fs::write(&large, [0u8; 16]).unwrap();
    let Err(err) = load_file_text_data(&cache, &large, 8).await else {
        panic!("expected an error for an oversized file");
    };
    assert!(
        matches!(err, AppError::TooLarge { size: 16, limit: 8 }),
        "{err}"
    );

    // Unrecognized extension -> UnsupportedFormat.
    let unknown = dir.join("file.xyz");
    std::fs::write(&unknown, "data").unwrap();
    let Err(err) = load_file_text_data(&cache, &unknown, u64::MAX).await else {
        panic!("expected an error for an unknown format");
    };
    assert!(matches!(err, AppError::UnsupportedFormat { .. }), "{err}");
}

#[tokio::test]
async fn test_rst_load_and_structure() {
    let file = make_temp_dir().join("doc.rst");
    std::fs::write(
        &file,
        "Title\n=====\n\nBody text.\n\nSub Section\n-----------\n\nMore text.\n",
    )
    .unwrap();
    let cache = new_text_data_cache();

    let data = load_file_text_data(&cache, &file, u64::MAX).await.unwrap();
    assert!(matches!(data.format, FileFormat::Rst));
    assert!(data.text.contains("# Title"));
    assert!(data.text.contains("## Sub Section"));

    let structure = data.structure();
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "Title");
    assert_eq!(structure.headers[0].level, 1);
    assert_eq!(structure.headers[1].text, "Sub Section");
    assert_eq!(structure.headers[1].level, 2);
}
