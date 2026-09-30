use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tantivy::schema::{Field, IndexRecordOption, Schema};
use tantivy::{DocSet, Index, SegmentReader, Term, TERMINATED};
use tokio::fs;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::change::FileChange;
use crate::config::Config;

/// Decode a term key from Tantivy's FST for a STRING field.
/// The FST key stores raw UTF-8 bytes without a type tag prefix.
pub(super) fn decode_term_key(key: &[u8]) -> Option<&str> {
    if key.is_empty() {
        return None;
    }
    std::str::from_utf8(key).ok()
}

/// Resolve the `path_exact` field from the schema.
pub(super) fn resolve_path_field(schema: &Schema) -> Result<Field> {
    schema
        .get_field(crate::schema::field::PATH_EXACT)
        .map_err(|e| anyhow::anyhow!("path_exact field not found in schema: {}", e))
}

/// Look up the mtime (UNIX nanoseconds) stored in the index for `rel_path`.
///
/// Returns `Ok(None)` when the path is not indexed: the term is absent from the FST, or
/// all matching documents have been deleted (phantom terms remain in the FST until a
/// merge; the alive bitset filters them out). When several live documents carry the term
/// (possible before a merge), the MAX mtime is returned — it reflects the latest
/// indexing of the file. A missing fast-field column or value is also `Ok(None)`:
/// "cannot verify" must fail in the reindex direction.
pub(super) fn indexed_mtime_in_index(
    segments: &[SegmentReader],
    path_field: Field,
    rel_path: &str,
) -> Result<Option<i64>> {
    let mut max_ns: Option<i64> = None;
    for segment in segments {
        let Some(ns) = indexed_mtime_in_segment(segment, path_field, rel_path)? else {
            continue;
        };
        max_ns = Some(i64::max(max_ns.unwrap_or(ns), ns));
    }
    Ok(max_ns)
}

/// Per-segment part of [`indexed_mtime_in_index`]. O(log N) FST probe plus a
/// handful of fast-field reads (a single-token STRING term has at most a few
/// docs per segment).
fn indexed_mtime_in_segment(
    segment: &SegmentReader,
    path_field: Field,
    rel_path: &str,
) -> Result<Option<i64>> {
    let term = Term::from_field_text(path_field, rel_path);
    let inv_index = segment.inverted_index(path_field)?;
    let Some(mut postings) = inv_index.read_postings(&term, IndexRecordOption::Basic)? else {
        return Ok(None);
    };
    let Some(column) = segment
        .fast_fields()
        .date(crate::schema::field::MODIFIED)
        .ok()
    else {
        return Ok(None);
    };
    let alive = segment.alive_bitset();
    let mut max_ns: Option<i64> = None;
    while postings.doc() != TERMINATED {
        let doc = postings.doc();
        let is_alive = match alive {
            Some(bitset) => bitset.is_alive(doc),
            None => true,
        };
        if is_alive {
            if let Some(dt) = column.first(doc) {
                let ns = dt.into_timestamp_nanos();
                max_ns = Some(i64::max(max_ns.unwrap_or(ns), ns));
            }
        }
        postings.advance();
    }
    Ok(max_ns)
}

/// Iterate over all indexed paths via term streams and detect deletions
/// (paths in index that no longer exist on disk). Returns the list of
/// deleted file paths.
///
/// Only `NotFound` from `symlink_metadata` counts as deletion; any other
/// error (EACCES, EIO, NFS failure) leaves the path in the index and is
/// reported as a single aggregated warning, so a transient filesystem
/// failure can never wipe the index. The term stream contains phantom
/// terms of deleted docs (no alive-bitset filtering) — harmless: emitting
/// `Deleted` for an already removed path is an idempotent no-op.
pub(super) fn detect_deletions_from_index(
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

/// Returns true if the file needs indexing: either its path is not in the index, or its
/// mtime on disk is strictly newer than the mtime stored in the index. Any missing data
/// (stat failure, unreadable mtime, out-of-range timestamp) means "reindex" — the safe
/// direction is over-indexing.
pub(super) async fn needs_indexing(
    path: &Path,
    segments: &[SegmentReader],
    path_field: Field,
    rel_path: &str,
) -> Result<bool> {
    let indexed_ns = match indexed_mtime_in_index(segments, path_field, rel_path)? {
        // Not indexed (or only phantom terms) — index unconditionally.
        None => return Ok(true),
        Some(ns) => ns,
    };
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Ok(true);
    };
    let Ok(modified) = metadata.modified() else {
        return Ok(true);
    };
    match chrono::DateTime::<chrono::Utc>::from(modified).timestamp_nanos_opt() {
        // Strict `>`: equality is the normal state of an unchanged file (the writer
        // stores the exact filesystem mtime in nanoseconds).
        Some(fs_ns) => Ok(fs_ns > indexed_ns),
        None => Ok(true),
    }
}

/// Recursively walk a directory tree asynchronously, yielding control after
/// each `read_dir` so the tokio runtime can schedule other tasks.
/// A `read_dir` failure at any recursion level returns `Err` and fails the
/// whole scan: an unreadable directory must not look like an empty one.
pub(super) async fn walk_directory(
    dir: &Path,
    base_canonical: &Path,
    config: &Config,
    segments: &[SegmentReader],
    field: Field,
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
        // not silently truncate it: the failure must be surfaced and retried.
        // Per-file mtime makes any missed files self-heal on the next start
        // (their stored mtime stays stale).
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
        if !config.should_index(&p) {
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

        if needs_indexing(&p, segments, field, &canonical_rel).await? {
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
/// - Modified files: exist in index with an mtime newer than the stored one
/// - Deleted files: exist in index but NOT on disk
pub async fn scan_directory(
    config: &Config,
    index: Arc<Index>,
    tx: mpsc::Sender<FileChange>,
) -> Result<u64> {
    info!("Starting directory scan");

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
