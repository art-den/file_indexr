use std::path::Path;

use tantivy::schema::Field;
use tantivy::{Index, SegmentReader, Term};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::testutil::{unique_temp_dir, unique_temp_path};
use crate::watch::FileChange;

use super::*;

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

/// Index the given `(key, mtime_ns)` docs in a single commit. `None` mtime
/// leaves the `modified` fast-field value absent for that doc. The writer is
/// dropped before returning (tantivy holds an exclusive index lock).
fn add_docs(index: &Index, path_field: Field, docs: &[(&str, Option<i64>)]) {
    let modified_field = index
        .schema()
        .get_field(crate::schema::field::MODIFIED)
        .unwrap();
    let mut writer = index
        .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
        .unwrap();
    for (key, mtime_ns) in docs {
        let mut doc = tantivy::TantivyDocument::default();
        doc.add_field_value(path_field, key);
        if let Some(ns) = mtime_ns {
            doc.add_field_value(
                modified_field,
                &tantivy::DateTime::from_timestamp_nanos(*ns),
            );
        }
        writer.add_document(doc).unwrap();
    }
    writer.commit().unwrap();
}

/// Delete all docs for `key` (the FST keeps the phantom term until a merge)
/// and commit.
fn delete_doc(index: &Index, path_field: Field, key: &str) {
    let mut writer = index
        .writer::<tantivy::TantivyDocument>(TEST_WRITER_HEAP)
        .unwrap();
    writer.delete_term(Term::from_field_text(path_field, key));
    writer.commit().unwrap();
}

/// Segment snapshot of the index; call it after a commit so the searcher is fresh.
fn segments(index: &Index) -> Vec<SegmentReader> {
    let reader = index.reader().unwrap();
    reader.searcher().segment_readers().to_vec()
}

#[test]
fn test_has_hidden_component() {
    let base = Path::new("/home/user/project");

    let cases = [
        // Hidden directory in path
        ("/home/user/project/.git/config", true),
        ("/home/user/project/src/.hidden/file.rs", true),
        // Normal paths
        ("/home/user/project/src/main.rs", false),
        ("/home/user/project/README.md", false),
        // Dot files at top level are NOT hidden (only dirs matter)
        ("/home/user/project/.env", false),
        // Outside base — should return false (not applicable)
        ("/tmp/.secret", false),
    ];
    for (path, expected) in cases {
        assert_eq!(
            has_hidden_component(Path::new(path), base),
            expected,
            "path: {path}"
        );
    }
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
    const NS: i64 = 1_700_000_000_123_456_789;

    // Index a doc for "a.txt" carrying both the path and an mtime.
    add_docs(&index, field, &[("a.txt", Some(NS))]);

    let segs = segments(&index);
    assert_eq!(
        indexed_mtime_in_index(&segs, field, "a.txt").unwrap(),
        Some(NS)
    );
    assert_eq!(indexed_mtime_in_index(&segs, field, "b.txt").unwrap(), None);

    // Delete the doc: the FST keeps the phantom term, only the alive
    // bitset marks the doc as deleted.
    delete_doc(&index, field, "a.txt");

    let segs = segments(&index);
    // The phantom term must NOT count as indexed.
    assert_eq!(indexed_mtime_in_index(&segs, field, "a.txt").unwrap(), None);

    // Re-adding the path (new doc in a new segment) must be found again.
    add_docs(&index, field, &[("a.txt", Some(NS))]);

    let segs = segments(&index);
    assert_eq!(
        indexed_mtime_in_index(&segs, field, "a.txt").unwrap(),
        Some(NS)
    );
}

#[test]
fn test_indexed_mtime_in_index_takes_max_across_segments() {
    let (index, field) = temp_index();
    const X: i64 = 1_700_000_000_123_456_789;

    // Index "a.txt" with mtime X, commit (segment 1).
    add_docs(&index, field, &[("a.txt", Some(X))]);
    // Index "a.txt" again with mtime X + 1, commit (segment 2).
    add_docs(&index, field, &[("a.txt", Some(X + 1))]);

    let segs = segments(&index);
    // Two live docs carry the term across two segments: the MAX mtime wins.
    assert_eq!(
        indexed_mtime_in_index(&segs, field, "a.txt").unwrap(),
        Some(X + 1)
    );
}

#[tokio::test]
async fn test_needs_indexing_mtime_comparison() {
    let (index, field) = temp_index();
    const X_NS: i64 = 1_700_000_000_123_456_789;

    // Empty index: the path is not indexed -> needs indexing.
    let segs = segments(&index);
    assert!(
        needs_indexing(std::path::Path::new("/nonexistent"), &segs, field, "a.txt")
            .await
            .unwrap()
    );

    // Index "a.txt" with mtime exactly X_NS.
    add_docs(&index, field, &[("a.txt", Some(X_NS))]);

    let segs = segments(&index);

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

    let cases = [
        // mtime == indexed mtime: unchanged -> no reindex.
        (X_NS, false),
        // mtime strictly newer -> reindex.
        (X_NS + 1, true),
        // mtime rolled back (older than indexed) -> no reindex (strict >).
        (X_NS - 1, false),
    ];
    for (ns, expected) in cases {
        set_mtime(&path, ns);
        assert_eq!(
            needs_indexing(&path, &segs, field, "a.txt").await.unwrap(),
            expected,
            "file mtime {ns}"
        );
    }
}

#[tokio::test]
async fn test_needs_indexing_reindexes_when_modified_value_missing() {
    let (index, field) = temp_index();

    // A doc carrying the path but NO `modified` value: the fast-field value
    // is absent for that doc, so "cannot verify" must fail in the reindex
    // direction (the path is treated as not indexed).
    add_docs(&index, field, &[("a.txt", None)]);

    let segs = segments(&index);
    assert_eq!(indexed_mtime_in_index(&segs, field, "a.txt").unwrap(), None);
    assert!(
        needs_indexing(std::path::Path::new("/nonexistent"), &segs, field, "a.txt")
            .await
            .unwrap()
    );
}

#[test]
fn test_detect_deletions_from_index_reports_missing_files() {
    let (index, field) = temp_index();

    add_docs(&index, field, &[("gone.txt", None), ("present.txt", None)]);

    let segs = segments(&index);

    let dir = unique_temp_dir("detect_del");
    std::fs::write(dir.join("present.txt"), "hello").unwrap();

    let deleted = detect_deletions_from_index(&segs, field, &dir).unwrap();
    assert_eq!(deleted, vec![dir.join("gone.txt")]);
}

#[test]
fn test_detect_deletions_from_index_ignores_phantom_terms() {
    let (index, field) = temp_index();

    // Index then delete "a.txt": the FST keeps the phantom term.
    add_docs(&index, field, &[("a.txt", None)]);
    delete_doc(&index, field, "a.txt");

    let segs = segments(&index);

    // The term is a phantom (doc deleted), but the file exists on disk —
    // it must NOT be reported as deleted.
    let dir = unique_temp_dir("detect_del_phantom");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    let deleted = detect_deletions_from_index(&segs, field, &dir).unwrap();
    assert!(deleted.is_empty());
}

#[tokio::test]
async fn test_walk_directory_fails_on_missing_directory() {
    let (index, field) = temp_index();
    let segs = segments(&index);

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
    let result = walk_directory(&missing, &missing, &config, &segs, field, &tx).await;
    assert!(result.is_err());
}
