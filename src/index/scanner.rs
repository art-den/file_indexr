use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::Path;
use std::sync::Arc;
use tantivy::schema::{Field, IndexRecordOption, Schema};
use tantivy::{Index, SegmentReader, Term};
use tokio::fs;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::change::FileChange;
use crate::config::Config;
use crate::index::checkpoint::load_checkpoint;

/// Decode a term key from Tantivy's FST for a STRING field.
/// The FST key stores raw UTF-8 bytes without a type tag prefix.
fn decode_term_key(key: &[u8]) -> Option<&str> {
    if key.is_empty() {
        return None;
    }
    std::str::from_utf8(key).ok()
}

/// Resolve the `path_exact` field from the schema.
fn resolve_path_field(schema: &Schema) -> Result<Field> {
    schema
        .get_field(crate::schema::field::PATH_EXACT)
        .map_err(|e| anyhow::anyhow!("path_exact field not found in schema: {}", e))
}

/// Check if a relative path exists in the segment by constructing a Term
/// and probing the segment's term dictionary. O(log N) per call via FST.
///
/// The term dictionary is immutable and only rebuilt on merge, so it can
/// contain "phantom" terms for documents that have since been deleted —
/// deletions live in the segment's alive bitset, not in the FST. When the
/// term is present, confirm at least one alive document still carries it.
/// `path_exact` is a single-token STRING field, so the posting list holds
/// at most a couple of docs per segment and `doc_freq_given_deletes`
/// (clone + scan) is cheap here.
fn path_in_segment(segment: &SegmentReader, field: Field, rel_path: &str) -> Result<bool> {
    let term = Term::from_field_text(field, rel_path);
    let inv_index = segment.inverted_index(field)?;
    let Some(postings) = inv_index.read_postings(&term, IndexRecordOption::Basic)? else {
        return Ok(false);
    };
    Ok(match segment.alive_bitset() {
        // No deletions in this segment: a term in the FST has at least one doc.
        None => true,
        Some(bitset) => postings.doc_freq_given_deletes(bitset) > 0,
    })
}

/// Check if a relative path exists in any segment of the index.
fn path_exists_in_index(segments: &[SegmentReader], field: Field, rel_path: &str) -> Result<bool> {
    for segment in segments {
        if path_in_segment(segment, field, rel_path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Iterate over all indexed paths via term streams and detect deletions
/// (paths in index that no longer exist on disk). Returns the list of
/// deleted file paths.
///
/// Only `NotFound` from `symlink_metadata` counts as deletion; any other
/// error (EACCES, EIO, NFS failure) leaves the path in the index and is
/// reported as a single aggregated warning, so a transient filesystem
/// failure can never wipe the index. The term stream contains phantom
/// terms of deleted docs (no alive-bitset filtering, unlike
/// [`path_in_segment`]) — harmless: emitting `Deleted` for an already
/// removed path is an idempotent no-op.
fn detect_deletions_from_index(
    segments: &[SegmentReader],
    field: Field,
    directory: &Path,
) -> Result<Vec<std::path::PathBuf>> {
    let mut deleted = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Paths whose state could not be verified (EACCES, EIO, NFS failure, ...).
    // They are NOT treated as deleted: a transient filesystem error must not
    // wipe the index — stale entries are cleared by a later successful scan.
    // Only the count and a first sample are kept: one aggregated warning is
    // enough, and a mass EIO event can involve thousands of paths.
    let mut unverified_count: usize = 0;
    let mut unverified_sample: Option<(std::path::PathBuf, std::io::Error)> = None;

    for segment in segments {
        let inv_index = segment.inverted_index(field)?;
        let mut stream = inv_index.terms().stream()?;

        while let Some((key, _)) = stream.next() {
            let Some(rel) = decode_term_key(key) else {
                continue;
            };
            if !seen.insert(rel.to_string()) {
                continue;
            };
            let full = directory.join(rel);
            // Only NotFound proves deletion; `Path::exists()` conflates it
            // with permission and I/O errors.
            match std::fs::symlink_metadata(&full) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => deleted.push(full),
                Err(e) => {
                    unverified_count += 1;
                    unverified_sample.get_or_insert((full, e));
                }
            }
        }
    }

    if let Some((sample, error)) = unverified_sample {
        warn!(
            count = unverified_count,
            error = %error,
            sample = %sample.display(),
            "Could not verify indexed paths; keeping them in the index"
        );
    }

    Ok(deleted)
}

/// Returns true if the file needs indexing: either it's not in the index,
/// or its mtime indicates it has been modified since the last run. The
/// mtime (a stat syscall) is only read for files already in the index;
/// new files are indexed unconditionally.
async fn needs_indexing(
    path: &Path,
    segments: &[SegmentReader],
    field: Field,
    rel_path: &str,
    last_completed_at: Option<DateTime<Utc>>,
) -> Result<bool> {
    // New file if not present in the index yet
    if !path_exists_in_index(segments, field, rel_path)? {
        return Ok(true);
    }

    // File exists in index — check mtime. Any missing data means "reindex".
    let Some(last_completed) = last_completed_at else {
        return Ok(true);
    };
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Ok(true);
    };
    let Ok(modified) = metadata.modified() else {
        return Ok(true);
    };
    Ok(DateTime::<Utc>::from(modified) >= last_completed)
}

/// Recursively walk a directory tree asynchronously, yielding control after
/// each `read_dir` so the tokio runtime can schedule other tasks.
/// A `read_dir` failure at any recursion level returns `Err` and fails the
/// whole scan: an unreadable directory must not look like an empty one, the
/// checkpoint advancement logic relies on this.
async fn walk_directory(
    dir: &Path,
    base_canonical: &Path,
    config: &Config,
    segments: &[SegmentReader],
    field: Field,
    last_completed_at: Option<DateTime<Utc>>,
    tx: &mpsc::Sender<FileChange>,
) -> Result<(u64, u64)> {
    let base = config.directory.as_path();
    let index_path = config.index_path.as_path();
    let mut files_processed: u64 = 0;
    let mut files_changed: u64 = 0;

    let mut entries = fs::read_dir(dir)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read directory {}: {}", dir.display(), e))?;

    loop {
        // next_entry() returns Result<Option<DirEntry>, io::Error>: Ok(None) is
        // the normal end, but a mid-iteration Err must fail the whole scan,
        // not silently truncate it: the rest of the directory would be skipped
        // while the checkpoint still advanced, leaving those files with stale
        // mtime < completed_at and never re-indexed.
        let entry = match entries.next_entry().await {
            Ok(None) => break,
            Ok(Some(entry)) => entry,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to read entry in directory {}: {}",
                    dir.display(),
                    e
                ));
            }
        };
        let p = entry.path();

        // Get file type without an extra stat — it's already available from readdir.
        let file_type = match entry.file_type().await {
            Ok(ft) => ft,
            Err(e) => {
                warn!(error = %e, "Skipping entry with unreadable type");
                continue;
            }
        };

        if file_type.is_dir() {
            // Skip hidden directories by name. This gates the recursion, so
            // the walk never descends into one and no processed path can lie
            // under a hidden directory (same rule the watcher applies).
            if is_hidden_dir_name(&p) {
                continue;
            }
            let (sub_processed, sub_changed) = Box::pin(walk_directory(
                &p,
                base_canonical,
                config,
                segments,
                field,
                last_completed_at,
                tx,
            ))
            .await?;
            files_processed += sub_processed;
            files_changed += sub_changed;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }
        if p.starts_with(index_path) {
            continue;
        }

        let rel = p.strip_prefix(base).unwrap_or(&p);
        if rel.to_str().is_none() {
            warn!(path = %p.display(), "Skipping file with non-UTF8 path");
            continue;
        }
        if !config.should_index_extension(p.extension()) {
            continue;
        }

        files_processed += 1;

        // Canonicalize once per file, reuse for both lookup and event
        let canonical_path = match tokio::fs::canonicalize(&p).await {
            Ok(cp) => cp,
            Err(_) => {
                // If canonicalization fails, skip the file to avoid mismatched keys
                continue;
            }
        };
        // Canonical relative path for index lookup. Kept as Cow to avoid a
        // per-file String allocation when the path is already valid UTF-8.
        let canonical_rel = match canonical_path.strip_prefix(base_canonical) {
            Ok(rel) => rel.to_string_lossy(),
            Err(_) => continue,
        };

        if needs_indexing(&p, segments, field, &canonical_rel, last_completed_at).await? {
            files_changed += 1;
            tx.send(FileChange::Modified(canonical_path))
                .await
                .map_err(|_| anyhow::anyhow!("Receiver dropped during scan"))?;
        }
    }

    Ok((files_processed, files_changed))
}

/// Scan the watched directory and send FileChange events through the channel.
/// Uses the index's FST term dictionary to probe individual paths rather than
/// loading all indexed paths into memory. Directory traversal is fully async
/// via `tokio::fs::read_dir`, yielding between directories to avoid starving
/// the tokio worker pool:
/// - New files: exist on disk but NOT in the index (regardless of mtime)
/// - Modified files: exist in index with updated mtime
/// - Deleted files: exist in index but NOT on disk
pub async fn scan_directory(
    config: &Config,
    index: Arc<Index>,
    tx: mpsc::Sender<FileChange>,
) -> Result<u64> {
    let checkpoint = load_checkpoint(&config.index_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load checkpoint: {}", e))?;

    let last_completed_at = checkpoint.map(|c| c.completed_at.unwrap_or(c.started_at));

    info!(last_run = ?last_completed_at, "Starting directory scan");

    let reader = index.reader()?;
    let searcher = reader.searcher();
    let segments: Vec<SegmentReader> = searcher.segment_readers().to_vec();
    let field = resolve_path_field(&index.schema())?;

    // Canonicalize base directory once for consistent path handling
    let base_canonical = tokio::fs::canonicalize(&config.directory)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to canonicalize watched directory: {}", e))?;

    // Step 1: Detect deletions (in index but not on disk).
    // Runs in a blocking task to avoid starving the Tokio worker pool.
    let deleted_files = tokio::task::spawn_blocking({
        let segments = segments.clone();
        let dir = base_canonical.to_path_buf();
        move || detect_deletions_from_index(&segments, field, &dir)
    })
    .await
    .expect("blocking task panicked")?;
    let files_deleted = deleted_files.len() as u64;
    for deleted_path in deleted_files {
        tx.send(FileChange::Deleted(deleted_path))
            .await
            .map_err(|_| anyhow::anyhow!("Receiver dropped during scan"))?;
    }

    // Step 2: Async walk filesystem and detect new/modified files.
    let (files_processed, files_changed) = walk_directory(
        &config.directory,
        &base_canonical,
        config,
        &segments,
        field,
        last_completed_at,
        &tx,
    )
    .await?;

    let total_scanned = files_processed + files_deleted;

    info!(
        total_scanned = total_scanned,
        changed = files_changed,
        "Directory scan complete"
    );

    Ok(files_changed)
}

/// Check if any directory component in the path (relative to base) starts with '.'.
///
/// The final component is excluded (treated as a file name), so a hidden
/// directory's own path is NOT caught — check `is_hidden_dir_name` for that.
pub fn has_hidden_component(path: &Path, base: &Path) -> bool {
    let rel = match path.strip_prefix(base) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let Some(parent) = rel.parent() else {
        return false;
    };
    parent
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .any(|name| name.starts_with('.'))
}

/// Check if the path's own name starts with '.' (i.e. it is a hidden
/// directory). Valid only for directories: a top-level dot *file* (e.g.
/// `.env`) must stay indexable.
pub fn is_hidden_dir_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{unique_temp_dir, unique_temp_path};

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
    fn test_path_in_segment_ignores_deleted_docs() {
        let (index, field) = temp_index();

        // Index a doc for "a.txt"
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
        assert!(path_exists_in_index(&segments, field, "a.txt").unwrap());
        assert!(!path_exists_in_index(&segments, field, "b.txt").unwrap());

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
        assert!(!path_exists_in_index(&segments, field, "a.txt").unwrap());

        // Re-adding the path (new doc in a new segment) must be found again.
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
        assert!(path_exists_in_index(&segments, field, "a.txt").unwrap());
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

        // A nonexistent directory must fail the walk: "unreadable" has to be
        // distinguishable from "empty" for the checkpoint logic.
        let missing = unique_temp_path("walk_missing");
        let (tx, _rx) = mpsc::channel::<FileChange>(16);
        let result = walk_directory(&missing, &missing, &config, &segments, field, None, &tx).await;
        assert!(result.is_err());
    }
}
