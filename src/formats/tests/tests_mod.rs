use crate::formats::*;

fn make_temp_dir() -> PathBuf {
    crate::testutil::unique_temp_dir("formats_test")
}

#[tokio::test]
async fn test_invalidate_text_data_cache() {
    let file = make_temp_dir().join("cache_entry.txt");
    std::fs::write(&file, "version one").unwrap();
    let cache = new_text_data_cache();

    // Warm the cache.
    let first = load_file_text_data(&cache, &file).await.unwrap();
    assert_eq!(first.text.as_str(), "version one");

    // Modify the file on disk; the cache entry is now stale.
    std::fs::write(&file, "version two").unwrap();

    // Without invalidation the cache still serves the stale copy.
    let stale = load_file_text_data(&cache, &file).await.unwrap();
    assert_eq!(stale.text.as_str(), "version one");

    cache.invalidate(&file);

    // After invalidation fresh content is read from disk.
    let fresh = load_file_text_data(&cache, &file).await.unwrap();
    assert_eq!(fresh.text.as_str(), "version two");
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

    let data = load_file_text_data(&cache, &file).await.unwrap();
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
