use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use tantivy::directory::MmapDirectory;
use tantivy::indexer::IndexWriter;
use tantivy::schema::{Field, Schema};
use tantivy::{Index, TantivyDocument as Document, Term, doc};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::schema;

/// Convert chrono DateTime to tantivy's DateTime (OffsetDateTime).
fn chrono_to_tantivy(dt: chrono::DateTime<chrono::Utc>) -> tantivy::DateTime {
    tantivy::DateTime::from_timestamp_secs(dt.timestamp())
}

/// Like `get_field`, but returns a descriptive error instead of the raw
/// tantivy error.
fn expect_field(schema: &Schema, name: &str) -> Result<Field, String> {
    schema
        .get_field(name)
        .map_err(|_| format!("Missing field '{}' in schema", name))
}

/// Document model that builds Tantivy documents from filesystem metadata.
pub struct DocumentModel;

impl DocumentModel {
    /// Build a Tantivy Document from a filesystem path.
    /// Validates the path via Config to resolve symlinks and ensure it stays inside the watched directory.
    /// The document stores canonical relative path and attributes derived from the resolved file.
    /// This ensures consistency: metadata, content and indexed path all refer to the same physical file.
    pub async fn from_path(
        path: &Path,
        config: &Config,
        schema: &Schema,
        text_data_cache: &crate::formats::TextDataCache,
    ) -> Result<Document, String> {
        // Canonicalize base and path to prevent symlink escape.
        // In production config.directory is already canonical (Config::canonicalize_paths
        // at startup); kept here because tests build Config directly.
        let base_canonical = tokio::fs::canonicalize(&config.directory)
            .await
            .map_err(|e| format!("Failed to canonicalize watched directory: {}", e))?;
        let resolved = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| format!("Failed to canonicalize path {}: {}", path.display(), e))?;
        // Delegate to the canonicalized implementation
        Self::from_resolved_path(&resolved, &base_canonical, config, schema, text_data_cache).await
    }

    /// Build a Tantivy Document from an already canonicalized path.
    /// `resolved_path` must be canonical and under `base_canonical`.
    pub async fn from_resolved_path(
        resolved_path: &Path,
        base_canonical: &Path,
        config: &Config,
        schema: &Schema,
        text_data_cache: &crate::formats::TextDataCache,
    ) -> Result<Document, String> {
        let canonical_rel_path = resolved_path
            .strip_prefix(base_canonical)
            .map_err(|_| format!("Path {} escapes watched directory", resolved_path.display()))?;
        let canonical_rel_path_str = canonical_rel_path
            .to_str()
            .ok_or_else(|| {
                format!(
                    "Canonical path contains invalid UTF-8: {}",
                    resolved_path.display()
                )
            })?
            .to_string();

        let metadata = tokio::fs::metadata(&resolved_path).await.map_err(|e| {
            format!(
                "Failed to read metadata for {}: {}",
                resolved_path.display(),
                e
            )
        })?;

        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", resolved_path.display()));
        }

        let filename = resolved_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // Derive dir from the already-validated relative path to avoid a redundant strip_prefix call.
        let dir = canonical_rel_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let extension = resolved_path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let size = metadata.len();
        let modified = metadata
            .modified()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .map_err(|e| {
                format!(
                    "Cannot read modification time for {}: {}",
                    resolved_path.display(),
                    e
                )
            })?;
        let modified_dt = chrono_to_tantivy(modified);

        let content = if size <= config.max_file_size_bytes() {
            crate::formats::load_file_text_data(text_data_cache, resolved_path)
                .await
                .ok()
                .map(|data| data.text)
        } else {
            None
        };
        let has_content = content.is_some();

        let path_field = expect_field(schema, schema::field::PATH)?;
        let path_exact_field = expect_field(schema, schema::field::PATH_EXACT)?;
        let filename_field = expect_field(schema, schema::field::FILENAME)?;
        let dir_field = expect_field(schema, schema::field::DIR)?;
        let content_field = expect_field(schema, schema::field::CONTENT)?;
        let size_field = expect_field(schema, schema::field::SIZE)?;
        let modified_field = expect_field(schema, schema::field::MODIFIED)?;
        let extension_field = expect_field(schema, schema::field::EXTENSION)?;
        let has_content_field = expect_field(schema, schema::field::HAS_CONTENT)?;

        let mut doc = doc![
            filename_field => filename,
            dir_field => dir,
            size_field => size,
            modified_field => modified_dt,
            extension_field => extension,
            has_content_field => has_content,
        ];
        // Both path fields share the same string; add_text avoids cloning it.
        doc.add_text(path_field, &canonical_rel_path_str);
        doc.add_text(path_exact_field, &canonical_rel_path_str);

        if let Some(content) = content {
            doc.add_text(content_field, content.as_ref());
        }

        Ok(doc)
    }

    /// Build a Term for exact path matching (used for deletions).
    pub fn term_for_path(schema: &Schema, path: &str) -> Result<Term, String> {
        let field = expect_field(schema, schema::field::PATH_EXACT)?;
        Ok(Term::from_field_text(field, path))
    }
}

/// Buffered change pending commit.
#[derive(Debug)]
enum Change {
    Add(Document),
    Delete(String),    // exact relative path for deletion
    DeleteDir(String), // prefix — delete all docs whose path starts with this
}

/// Maximum number of failed commit attempts before a change is dropped.
const MAX_RETRY_COUNT: usize = 2;

/// A buffered change awaiting commit. Retried on failure up to a max attempt limit.
#[derive(Debug)]
struct ChangeItem {
    /// The actual operation to apply.
    data: Change,
    /// Number of failed attempts. Incremented on each retry; dropped when >= MAX_RETRY_COUNT.
    try_count: usize,
}

impl ChangeItem {
    fn new(data: Change) -> Self {
        Self { data, try_count: 0 }
    }
}

/// Increment `try_count` on each failed item and drop those that reached
/// `MAX_RETRY_COUNT`. Returns the survivors to be requeued in the buffer.
fn requeue_failed(mut failed: Vec<ChangeItem>) -> Vec<ChangeItem> {
    failed.retain_mut(|change| {
        change.try_count += 1;
        if change.try_count >= MAX_RETRY_COUNT {
            // The change is lost for this session. Callers must treat a
            // `false` return from `commit()` as "not applied" (e.g. not
            // advancing the checkpoint), so a restart re-applies it via
            // the mtime filter.
            tracing::error!(
                change = ?change,
                "Change dropped after {} failed commit attempts", MAX_RETRY_COUNT
            );
            false
        } else {
            true
        }
    });
    failed
}

/// Index writer wrapper with batching support.
pub struct IndexWriterWrapper {
    inner: Arc<SyncMutex<IndexWriter>>,
    index: Arc<Index>,
    reader: tantivy::IndexReader,
    schema: Schema,
    config: Arc<Config>,
    buffer: Mutex<Vec<ChangeItem>>,
    batch_size: usize,
    base_canonical: PathBuf,
    text_data_cache: crate::formats::TextDataCache,
    /// Test seam: makes the next commit's blocking task panic, to exercise
    /// the JoinError batch-recovery path.
    #[cfg(test)]
    panic_on_commit: Arc<AtomicBool>,
}

impl IndexWriterWrapper {
    /// Create or open an index at the given path and return a wrapped IndexWriter.
    pub async fn new(
        index_path: &Path,
        config: Arc<Config>,
        text_data_cache: crate::formats::TextDataCache,
    ) -> Result<Self> {
        std::fs::create_dir_all(index_path)?;

        let directory = MmapDirectory::open(index_path)
            .map_err(|e| anyhow::anyhow!("Failed to open index directory: {}", e))?;

        let schema = schema::build_schema();

        let index = Index::open_or_create(directory, schema.clone())?;

        // Use 50MB heap for the index writer
        let inner = index.writer(50_000_000)?;

        let reader = index
            .reader()
            .map_err(|e| anyhow::anyhow!("Failed to create index reader: {}", e))?;

        let batch_size = config.batch_size;
        let base_canonical = tokio::fs::canonicalize(&config.directory)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to canonicalize watched directory: {}", e))?;
        Ok(Self {
            inner: Arc::new(SyncMutex::new(inner)),
            index: Arc::new(index),
            reader,
            schema,
            config,
            buffer: Mutex::new(Vec::new()),
            batch_size,
            base_canonical,
            text_data_cache,
            #[cfg(test)]
            panic_on_commit: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Shared text data cache. Cloning the returned handle shares the
    /// same underlying cache held by `AppState`.
    pub fn text_data_cache(&self) -> &crate::formats::TextDataCache {
        &self.text_data_cache
    }

    /// Return canonical relative path.
    /// If the path no longer exists (e.g. deleted file/dir), recovers the path by
    /// canonicalizing the parent and reattaching the relative component.
    /// Returns an error only if the path cannot be resolved or escapes the watched directory.
    async fn canonical_relative_path(&self, path: &Path) -> Result<String> {
        let resolved = match tokio::fs::canonicalize(path).await {
            Ok(r) => r,
            Err(_) => {
                // Path may have been deleted. Recover by canonicalizing the parent
                // and reattaching the non-canonical relative component.
                let parent = path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Path has no parent: {}", path.display()))?;
                let base = tokio::fs::canonicalize(parent).await.map_err(|e| {
                    anyhow::anyhow!("Failed to canonicalize parent of {}: {}", path.display(), e)
                })?;
                if !base.starts_with(&self.base_canonical) {
                    return Err(anyhow::anyhow!(
                        "Parent of {} escapes watched directory",
                        path.display()
                    ));
                }
                let suffix = path.strip_prefix(parent).map_err(|_| {
                    anyhow::anyhow!(
                        "Path {} cannot be reconstructed from parent",
                        path.display()
                    )
                })?;
                base.join(suffix)
            }
        };
        resolved
            .strip_prefix(&self.base_canonical)
            .map(|p| p.to_string_lossy().to_string())
            .map_err(|_| anyhow::anyhow!("Path {} escapes watched directory", path.display()))
    }

    /// Buffer change items, committing if the buffer reaches `batch_size`.
    async fn buffer_changes(&self, items: impl IntoIterator<Item = ChangeItem>) {
        let mut buffer = self.buffer.lock().await;
        buffer.extend(items);
        if buffer.len() >= self.batch_size {
            drop(buffer);
            self.commit().await;
        }
    }

    /// Add a file to the index by reading from disk.
    /// Returns Ok(()) if the file was indexed, or Ok(()) if it was skipped as
    /// non-indexable (`Config::should_index`: extension filter or unrecognized format).
    /// Deletes any existing document with the same path first to avoid duplicates.
    /// Returns an error if the path is outside the watched directory or contains invalid UTF-8.
    pub async fn add_file(&self, path: PathBuf) -> Result<()> {
        // Fast-path: reject non-indexable files without touching the filesystem.
        if !self.config.should_index(&path) {
            return Ok(());
        }

        let resolved_path = tokio::fs::canonicalize(&path).await.map_err(|e| {
            anyhow::anyhow!("Failed to canonicalize path {}: {}", path.display(), e)
        })?;
        let rel_path = resolved_path
            .strip_prefix(&self.base_canonical)
            .map(|p| p.to_string_lossy().to_string())
            .map_err(|_| anyhow::anyhow!("Path {} escapes watched directory", path.display()))?;
        // Drop any stale cached text before re-reading the file for indexing
        self.text_data_cache.invalidate(&resolved_path);
        let doc = DocumentModel::from_resolved_path(
            &resolved_path,
            &self.base_canonical,
            &self.config,
            &self.schema,
            &self.text_data_cache,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build document: {}", e))?;

        tracing::info!(file = ?path, "Buffered file for indexing");
        // Delete existing document with the same path to avoid duplicates on re-index
        self.buffer_changes([
            ChangeItem::new(Change::Delete(rel_path)),
            ChangeItem::new(Change::Add(doc)),
        ])
        .await;

        Ok(())
    }

    /// Delete a file from the index by its exact path.
    /// Returns an error if the path is outside the watched directory or contains invalid UTF-8.
    pub async fn delete_file(&self, path: PathBuf) -> Result<()> {
        // Path must be inside watched directory and valid UTF-8.
        let rel_path = self.canonical_relative_path(&path).await?;

        tracing::info!(path = ?path, "Buffered file for deletion");
        self.buffer_changes([ChangeItem::new(Change::Delete(rel_path))])
            .await;

        Ok(())
    }

    /// Delete all files under a directory from the index using prefix matching.
    /// Used when a directory is removed (e.g., renamed to a hidden name) so that
    /// all documents with paths starting with `dir/` are deleted.
    /// Returns an error if the path is outside the watched directory or contains invalid UTF-8.
    pub async fn delete_dir(&self, path: PathBuf) -> Result<()> {
        let rel_path = self.canonical_relative_path(&path).await?;

        let prefix = format!("{}/", rel_path);
        tracing::info!(prefix = %prefix, "Buffered directory prefix for deletion");
        self.buffer_changes([ChangeItem::new(Change::DeleteDir(prefix))])
            .await;

        Ok(())
    }

    /// Flush pending changes to disk.
    ///
    /// Returns `true` when the buffer was empty or all buffered changes
    /// were committed. Returns `false` when the commit failed: failed
    /// changes are returned to the buffer for retry (or dropped after
    /// `MAX_RETRY_COUNT` attempts). Callers that decide what to persist
    /// (e.g. the startup checkpoint) must treat `false` as "not applied".
    /// Task-level failures (a panic or cancellation of the blocking task)
    /// requeue the whole batch and also return `false` — reapplying is
    /// idempotent, since `Delete`/`DeleteDir` are no-ops when nothing matches
    /// and every `Add` is preceded by a `Delete` of the same path.
    ///
    /// Blocking operations (`commit` / `rollback`) are offloaded to
    /// `spawn_blocking` so the async runtime is not starved during fsync.
    pub async fn commit(&self) -> bool {
        let changes = {
            let mut buffer = self.buffer.lock().await;
            if buffer.is_empty() {
                return true;
            }
            // Keep the batch behind an Arc: the blocking task gets a clone,
            // so if it fails we still own the changes and can requeue them
            // instead of losing the whole batch.
            Arc::new(buffer.drain(..).collect::<Vec<_>>())
        };

        let inner = Arc::clone(&self.inner);
        // Only the PATH_EXACT field is needed by the commit task; extract it up
        // front so the closure moves a cheap `Field` instead of cloning the
        // whole Schema, and skip the per-item field lookup.
        let path_exact_field = match expect_field(&self.schema, schema::field::PATH_EXACT) {
            Ok(field) => Some(field),
            Err(e) => {
                tracing::error!("{e}");
                None
            }
        };
        let task_changes = Arc::clone(&changes);
        #[cfg(test)]
        let panic_on_commit = Arc::clone(&self.panic_on_commit);

        let applied = tokio::task::spawn_blocking(move || {
            // Test seam: one-shot injected panic (swap returns the previous
            // value and resets the flag), converted by tokio into a JoinError
            // on the awaiting side.
            #[cfg(test)]
            if panic_on_commit.swap(false, Ordering::SeqCst) {
                panic!("injected commit failure");
            }

            // A poisoned mutex means a prior blocking task panicked while holding
            // the lock. Recover the writer instead of failing every future commit:
            // tantivy's worker threads and op queue are unaffected by such a panic.
            let mut writer = inner.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut change_idx_applied = Vec::with_capacity(task_changes.len());

            for (i, change) in task_changes.iter().enumerate() {
                match &change.data {
                    Change::Add(doc) => match writer.add_document(doc.clone()) {
                        Ok(_) => change_idx_applied.push(i),
                        Err(e) => tracing::error!("writer.add_document failed: {}", e),
                    },
                    Change::Delete(path_str) => {
                        let Some(field) = path_exact_field else { continue; };
                        let term = Term::from_field_text(field, path_str);
                        match writer.delete_query(Box::new(tantivy::query::TermQuery::new(
                            term,
                            tantivy::schema::IndexRecordOption::Basic,
                        ))) {
                            Ok(_) => change_idx_applied.push(i),
                            Err(e) => tracing::error!("writer.delete_query failed: {}", e),
                        }
                    }
                    Change::DeleteDir(prefix) => {
                        let Some(field) = path_exact_field else { continue; };

                        // tantivy's FST-based regex doesn't support ^/$ anchors.
                        // Pattern `prefix.*` matches the full token since STRING fields
                        // are stored as single tokens, making explicit anchoring unnecessary.
                        let mut regex_pattern = regex::escape(prefix);
                        regex_pattern.push_str(".*");
                        let query = match tantivy::query::RegexQuery::from_pattern(&regex_pattern, field) {
                            Ok(q) => q,
                            Err(e) => {
                                tracing::error!("Failed to compile regex for dir deletion: {}", e);
                                continue;
                            }
                        };

                        match writer.delete_query(Box::new(query)) {
                            Ok(_) => {
                                tracing::info!(pattern = %regex_pattern, "Deleted docs matching directory prefix");
                                change_idx_applied.push(i);
                            }
                            Err(e) => {
                                tracing::error!("delete_query for dir deletion failed: {}", e);
                            }
                        }
                    }
                }
            }

            let commit_result = writer.commit();
            if let Err(e) = commit_result {
                if let Err(rollback_err) = writer.rollback() {
                    tracing::error!("writer.rollback failed: {}", rollback_err);
                }
                change_idx_applied.clear();
                tracing::error!("writer.commit failed: {}", e);
            }

            change_idx_applied
        })
        .await;

        // The task has finished on both `Ok` and `JoinError`, so its Arc clone
        // is dropped and the batch can be recovered here.
        let changes = Arc::try_unwrap(changes)
            .expect("blocking task has finished, its Arc clone must be dropped");

        // Changes to requeue: on `JoinError` (task panic or cancellation)
        // the whole batch, otherwise the items the writer rejected.
        // Reapplication is idempotent (see docs above).
        let failed = match applied {
            Ok(applied) => {
                // `applied` is strictly ascending (pushed in one forward pass
                // over the batch), so a merge scan avoids the HashSet allocation.
                let mut failed = Vec::new();
                let mut next_applied = 0usize;
                for (i, item) in changes.into_iter().enumerate() {
                    if applied.get(next_applied) == Some(&i) {
                        next_applied += 1;
                    } else {
                        failed.push(item);
                    }
                }
                failed
            }
            Err(join_error) => {
                tracing::error!(error = %join_error, "Commit task failed; requeueing batch for retry");
                changes
            }
        };

        if !failed.is_empty() {
            let requeued = requeue_failed(failed);
            self.buffer.lock().await.splice(0..0, requeued);
            return false;
        }

        true
    }

    /// Get the underlying Index for search operations.
    pub fn index(&self) -> Arc<Index> {
        self.index.clone()
    }

    /// Get number of buffered changes.
    pub async fn buffer_len(&self) -> usize {
        let buffer = self.buffer.lock().await;
        buffer.len()
    }

    /// Count documents in the index.
    pub fn doc_count(&self) -> Result<u64> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        Ok(searcher.num_docs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_temp_dir(kind: &str) -> PathBuf {
        crate::testutil::unique_temp_dir(&format!("unit_test_{kind}"))
    }

    fn make_test_config(watch_dir: &Path, index_dir: &Path) -> Config {
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

    /// Create three files in `watch_dir` and buffer them for indexing
    /// (each adds a Delete+Add pair, so 6 buffered items total).
    async fn buffer_three_files(writer: &IndexWriterWrapper, watch_dir: &Path) {
        for name in ["a.txt", "b.txt", "c.txt"] {
            let path = watch_dir.join(name);
            std::fs::write(&path, "content").unwrap();
            writer.add_file(path).await.unwrap();
        }
    }

    #[test]
    fn test_chrono_to_tantivy_conversion() {
        let chrono_ts: i64 = 1_700_000_000;
        let chrono_dt = chrono::DateTime::from_timestamp(chrono_ts, 0).unwrap();
        let tantivy_dt = chrono_to_tantivy(chrono_dt);
        let ts = tantivy_dt.into_timestamp_secs();
        assert_eq!(ts, chrono_ts);
    }

    #[test]
    fn test_term_for_path() {
        let schema = schema::build_schema();
        let _term = DocumentModel::term_for_path(&schema, "src/main.rs").unwrap();
    }

    #[test]
    fn test_requeue_failed_increments_try_count() {
        let items = vec![ChangeItem {
            data: Change::Delete("a.txt".into()),
            try_count: 0,
        }];
        let requeued = requeue_failed(items);
        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].try_count, 1);
    }

    #[test]
    fn test_requeue_failed_drops_at_max_retry_count() {
        let items = vec![
            ChangeItem {
                data: Change::Delete("dropped.txt".into()),
                try_count: MAX_RETRY_COUNT - 1,
            },
            ChangeItem {
                data: Change::Delete("kept.txt".into()),
                try_count: 0,
            },
        ];
        let requeued = requeue_failed(items);
        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].try_count, 1);
        match &requeued[0].data {
            Change::Delete(path) => assert_eq!(path, "kept.txt"),
            _ => panic!("expected Delete change"),
        }
    }

    #[tokio::test]
    async fn test_commit_task_panic_requeues_batch() {
        let watch_dir = make_test_temp_dir("watch");
        let index_dir = make_test_temp_dir("index");
        let config = Arc::new(make_test_config(&watch_dir, &index_dir));

        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            crate::formats::new_text_data_cache(),
        )
        .await
        .unwrap();
        buffer_three_files(&writer, &watch_dir).await;
        assert_eq!(writer.buffer_len().await, 6);

        // Inject a panic into the blocking task: the batch must survive the
        // JoinError and come back to the buffer, not be lost.
        writer.panic_on_commit.store(true, Ordering::SeqCst);
        assert!(!writer.commit().await);
        assert_eq!(writer.buffer_len().await, 6);

        // The follow-up clean commit applies the requeued batch.
        assert!(writer.commit().await);
        assert_eq!(writer.buffer_len().await, 0);
        assert_eq!(writer.doc_count().unwrap(), 3);
    }

    #[tokio::test]
    async fn test_commit_task_panic_drops_batch_after_max_retries() {
        let watch_dir = make_test_temp_dir("watch");
        let index_dir = make_test_temp_dir("index");
        let config = Arc::new(make_test_config(&watch_dir, &index_dir));

        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            crate::formats::new_text_data_cache(),
        )
        .await
        .unwrap();
        buffer_three_files(&writer, &watch_dir).await;

        // Two failed commits: try_count goes 0 -> 1 -> 2 == MAX_RETRY_COUNT,
        // so the batch is dropped per the retry contract.
        writer.panic_on_commit.store(true, Ordering::SeqCst);
        assert!(!writer.commit().await);
        assert_eq!(writer.buffer_len().await, 6);

        writer.panic_on_commit.store(true, Ordering::SeqCst);
        assert!(!writer.commit().await);
        assert_eq!(writer.buffer_len().await, 0);
    }
}
