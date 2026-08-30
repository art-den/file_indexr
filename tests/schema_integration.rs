use std::path::{Path, PathBuf};

use file_indexr::config::Config;
use file_indexr::index::writer::DocumentModel;
use file_indexr::schema::{build_schema, field};
use tantivy::schema::Value;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("schema_test")
}

fn make_config(dir: &Path) -> Config {
    Config {
        directory: dir.to_path_buf(),
        index_path: dir.join(".file_indexr").join("index"),
        port: 8080,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,

        allowed_extensions: vec![],
    }
}

// ============================================================================
// DocumentModel tests (filesystem-based)
// ============================================================================

#[tokio::test]
async fn test_document_from_text_file() {
    let dir = make_temp_dir();
    let file_path = dir.join("hello.rs");
    std::fs::write(&file_path, "fn main() { println!(\"Hello\"); }").unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let path_field = schema.get_field(field::PATH).unwrap();
    assert!(doc.get_first(path_field).is_some());

    let content_field = schema.get_field(field::CONTENT).unwrap();
    assert!(doc.get_first(content_field).is_some());

    let has_content_field = schema.get_field(field::HAS_CONTENT).unwrap();
    assert_eq!(
        doc.get_first(has_content_field).unwrap().as_bool(),
        Some(true)
    );

    let ext_field = schema.get_field(field::EXTENSION).unwrap();
    assert_eq!(doc.get_first(ext_field).unwrap().as_str(), Some("rs"));
}

#[tokio::test]
async fn test_document_skipped_extension() {
    // `from_path` does NOT filter by skip_extensions — that's handled
    // by the scanner/writer. It creates a document for any readable file.
    let dir = make_temp_dir();
    let file_path = dir.join("archive.zip");
    std::fs::write(&file_path, "fake zip content").unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let ext_field = schema.get_field(field::EXTENSION).unwrap();
    assert_eq!(doc.get_first(ext_field).unwrap().as_str(), Some("zip"));
}

#[tokio::test]
async fn test_document_binary_extension() {
    let dir = make_temp_dir();
    let file_path = dir.join("image.png");
    std::fs::write(&file_path, [0u8; 100]).unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let has_content_field = schema.get_field(field::HAS_CONTENT).unwrap();
    assert_eq!(
        doc.get_first(has_content_field).unwrap().as_bool(),
        Some(false)
    );
}

#[tokio::test]
async fn test_document_content_size_limit() {
    let dir = make_temp_dir();
    let file_path = dir.join("large.txt");

    let large_content = "x".repeat(3_000_000);
    std::fs::write(&file_path, &large_content).unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let content_field = schema.get_field(field::CONTENT).unwrap();
    assert!(doc.get_first(content_field).is_none());

    let has_content_field = schema.get_field(field::HAS_CONTENT).unwrap();
    assert_eq!(
        doc.get_first(has_content_field).unwrap().as_bool(),
        Some(false)
    );
}

#[tokio::test]
async fn test_document_from_subdirectory() {
    let dir = make_temp_dir();
    let subdir = dir.join("src").join("lib");
    std::fs::create_dir_all(&subdir).unwrap();
    let file_path = subdir.join("main.rs");
    std::fs::write(&file_path, "pub fn hello() {}").unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let path_field = schema.get_field(field::PATH).unwrap();
    let path_value = doc.get_first(path_field).unwrap().as_str().unwrap();
    assert!(path_value.starts_with("src/lib/main.rs"));

    let dir_field = schema.get_field(field::DIR).unwrap();
    let dir_value = doc.get_first(dir_field).unwrap().as_str().unwrap();
    assert!(dir_value.ends_with("lib"));
}

#[tokio::test]
async fn test_document_outside_watched_directory() {
    let watched_dir = make_temp_dir();
    let outside_dir = make_temp_dir();
    let file_path = outside_dir.join("secret.txt");
    std::fs::write(&file_path, "secret").unwrap();

    let config = make_config(&watched_dir);
    let schema = build_schema();
    let result = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_document_no_extension() {
    let dir = make_temp_dir();
    let file_path = dir.join("Makefile");
    std::fs::write(&file_path, "all:\n\techo hello").unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let ext_field = schema.get_field(field::EXTENSION).unwrap();
    assert_eq!(doc.get_first(ext_field).unwrap().as_str(), Some(""));

    // Unsupported format (no recognized extension) → no content indexed.
    let content_field = schema.get_field(field::CONTENT).unwrap();
    assert!(doc.get_first(content_field).is_none());
}

#[tokio::test]
async fn test_document_modified_time() {
    let dir = make_temp_dir();
    let file_path = dir.join("timestamp.rs");
    std::fs::write(&file_path, "fn ts() {}").unwrap();

    let config = make_config(&dir);
    let schema = build_schema();
    let doc = DocumentModel::from_path(
        &file_path,
        &config,
        &schema,
        &file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();

    let modified_field = schema.get_field(field::MODIFIED).unwrap();
    let modified = doc
        .get_first(modified_field)
        .unwrap()
        .as_datetime()
        .unwrap();
    let now = tantivy::DateTime::from_timestamp_secs(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    );
    let diff = now.into_timestamp_secs() - modified.into_timestamp_secs();
    assert!(diff < 60);
}
