use std::path::Path;

use tantivy::{Index, SegmentReader, Term};
use tantivy::schema::Field;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::testutil::{unique_temp_dir, unique_temp_path};
use crate::index::scanner::*;
use crate::watch::FileChange;

/// Heap budget (bytes) for the test `IndexWriter`s.
const TEST_WRITER_HEAP: usize = 50_000_000;

/// In-memory index with the production schema and the resolved `path_exact` field.
fn temp_index() -> (Index, Field) {
    use tantivy::directory::RamDirectory;

    let schema = crate::schema::build_schema();
    let index = Index::open_or_create(RamDirectory::default(), schema).unwrap();
    let field = resolve_path_field(&index.schema()).unwrap();
    (index, field)
}

#[test]
fn test_has_hidden_component() {
    let base = Path::new("/home/user/project");

    // Hidden directory in path
    assert!(has_hidden_component(
        Path::new("/home/user/project/.git/config"),
        base
    ));
    assert!(has_hidden_component(
        Path::new("/home/user/project/src/.hidden/file.rs"),
        base
    ));

    // Normal paths
    assert!(!has_hidden_component(
        Path::new("/home/user/project/src/main.rs"),
        base
    ));
    assert!(!has_hidden_component(
        Path::new("/home/user/project/README.md"),
        base
    ));

    // Dot files at top level are NOT hidden (only dirs matter)
    assert!(!has_hidden_component(
        Path::new("/home/user/project/.env"),
        base
    ));

    // Outside base — should return false (not applicable)
    assert!(!has_hidden_component(Path::new("/tmp/.secret"), base));
}

#[test]
fn test_decode_term_key() {
    // FST keys are raw UTF-8, no type prefix
    assert_eq!(decode_term_key(b"a.txt"), Some("a.txt"));
    assert_eq!(decode_term_key(b"src/main.rs"), Some("src/main.rs"));

    // Invalid: empty key
    assert!(decode_term_key(&[]).is_none());

    // Invalid: non-UTF-8 bytes
    assert!(decode_term_key(&[0xFF, 0xFE]).is_none());
}

#[test]
fn test_indexed_mtime_in_index_ignores_deleted_docs() {
    let (index, field) = temp_index();
    let modified_field = index
        .schema()
        .get_field(crate::schema::field::MODIFIED)
        .unwrap();
    const NS: i64 = 1_700_000_000_123_456_789;

    // Index a doc for "a.txt" carrying both the path and an mtime.
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        let doc = tantivy::doc! {
            field => "a.txt",
            modified_field => tantivy::DateTime::from_timestamp_nanos(NS)
        };
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();
    assert_eq!(
        indexed_mtime_in_index(&segments, field, "a.txt").unwrap(),
        Some(NS)
    );
    assert_eq!(indexed_mtime_in_index(&segments, field, "b.txt").unwrap(), None);

    // Delete the doc: the FST keeps the phantom term, only the alive
    // bitset marks the doc as deleted.
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        writer.delete_term(Term::from_field_text(field, "a.txt"));
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();
    // The phantom term must NOT count as indexed.
    assert_eq!(indexed_mtime_in_index(&segments, field, "a.txt").unwrap(), None);

    // Re-adding the path (new doc in a new segment) must be found again.
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        let doc = tantivy::doc! {
            field => "a.txt",
            modified_field => tantivy::DateTime::from_timestamp_nanos(NS)
        };
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();
    assert_eq!(
        indexed_mtime_in_index(&segments, field, "a.txt").unwrap(),
        Some(NS)
    );
}

#[test]
fn test_indexed_mtime_in_index_takes_max_across_segments() {
    let (index, field) = temp_index();
    let modified_field = index
        .schema()
        .get_field(crate::schema::field::MODIFIED)
        .unwrap();
    const X: i64 = 1_700_000_000_123_456_789;

    // Index "a.txt" with mtime X, commit (segment 1).
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        let doc = tantivy::doc! {
            field => "a.txt",
            modified_field => tantivy::DateTime::from_timestamp_nanos(X)
        };
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }
    // Index "a.txt" again with mtime X + 1, commit (segment 2).
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        let doc = tantivy::doc! {
            field => "a.txt",
            modified_field => tantivy::DateTime::from_timestamp_nanos(X + 1)
        };
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();
    // Two live docs carry the term across two segments: the MAX mtime wins.
    assert_eq!(
        indexed_mtime_in_index(&segments, field, "a.txt").unwrap(),
        Some(X + 1)
    );
}

#[tokio::test]
async fn test_needs_indexing_mtime_comparison() {
    let (index, field) = temp_index();
    let modified_field = index
        .schema()
        .get_field(crate::schema::field::MODIFIED)
        .unwrap();
    const X_NS: i64 = 1_700_000_000_123_456_789;

    // Empty index: the path is not indexed -> needs indexing.
    {
        let reader = index.reader().unwrap();
        let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();
        assert!(needs_indexing(
            std::path::Path::new("/nonexistent"),
            &segments,
            field,
            "a.txt"
        )
        .await
        .unwrap());
    }

    // Index "a.txt" with mtime exactly X_NS.
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        let doc = tantivy::doc! {
            field => "a.txt",
            modified_field => tantivy::DateTime::from_timestamp_nanos(X_NS)
        };
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();

    // Real file whose mtime we control precisely.
    let dir = unique_temp_dir("needs_indexing_mtime");
    let path = dir.join("a.txt");
    std::fs::write(&path, "content").unwrap();

    let set_mtime = |path: &std::path::Path, ns: i64| {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_unix_time(ns / 1_000_000_000, (ns % 1_000_000_000) as u32),
        )
        .unwrap();
    };

    // mtime == indexed mtime: unchanged -> no reindex.
    set_mtime(&path, X_NS);
    assert!(!needs_indexing(&path, &segments, field, "a.txt").await.unwrap());

    // mtime strictly newer -> reindex.
    set_mtime(&path, X_NS + 1);
    assert!(needs_indexing(&path, &segments, field, "a.txt").await.unwrap());

    // mtime rolled back (older than indexed) -> no reindex (strict >).
    set_mtime(&path, X_NS - 1);
    assert!(!needs_indexing(&path, &segments, field, "a.txt").await.unwrap());
}

#[tokio::test]
async fn test_needs_indexing_reindexes_when_modified_value_missing() {
    let (index, field) = temp_index();

    // A doc carrying the path but NO `modified` value: the fast-field value
    // is absent for that doc, so "cannot verify" must fail in the reindex
    // direction (the path is treated as not indexed).
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        let doc = tantivy::doc! { field => "a.txt" };
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();
    assert_eq!(indexed_mtime_in_index(&segments, field, "a.txt").unwrap(), None);
    assert!(needs_indexing(
        std::path::Path::new("/nonexistent"),
        &segments,
        field,
        "a.txt"
    )
    .await
    .unwrap());
}

#[test]
fn test_detect_deletions_from_index_reports_missing_files() {
    let (index, field) = temp_index();

    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        writer
            .add_document(tantivy::doc! { field => "gone.txt" })
            .unwrap();
        writer
            .add_document(tantivy::doc! { field => "present.txt" })
            .unwrap();
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();

    let dir = unique_temp_dir("detect_del");
    std::fs::write(dir.join("present.txt"), "hello").unwrap();

    let deleted = detect_deletions_from_index(&segments, field, &dir).unwrap();
    assert_eq!(deleted, vec![dir.join("gone.txt")]);
}

#[test]
fn test_detect_deletions_from_index_ignores_phantom_terms() {
    let (index, field) = temp_index();

    // Index then delete "a.txt": the FST keeps the phantom term.
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        writer
            .add_document(tantivy::doc! { field => "a.txt" })
            .unwrap();
        writer.commit().unwrap();
    }
    {
        let mut writer = index
            .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
            .unwrap();
        writer.delete_term(Term::from_field_text(field, "a.txt"));
        writer.commit().unwrap();
    }

    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();

    // The term is a phantom (doc deleted), but the file exists on disk —
    // it must NOT be reported as deleted.
    let dir = unique_temp_dir("detect_del_phantom");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    let deleted = detect_deletions_from_index(&segments, field, &dir).unwrap();
    assert!(deleted.is_empty());
}

#[tokio::test]
async fn test_walk_directory_fails_on_missing_directory() {
    let (index, field) = temp_index();
    let reader = index.reader().unwrap();
    let segments: Vec<SegmentReader> = reader.searcher().segment_readers().to_vec();

    let config = Config {
        directory: std::path::PathBuf::new(),
        index_path: std::path::PathBuf::new(),
        port: 0,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 1,
        batch_size: 500,
        batch_timeout_ms: 1000,
        allowed_extensions: vec![],
    };

    // A nonexistent directory must fail the walk: 'unreadable' has to be
    // distinguishable from 'empty' so a partial walk is not silently accepted.
    let missing = unique_temp_path("walk_missing");
    let (tx, _rx) = mpsc::channel::<FileChange>(16);
    let result = walk_directory(&missing, &missing, &config, &segments, field, &tx).await;
    assert!(result.is_err());
}
