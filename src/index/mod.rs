pub mod checkpoint;
pub mod scanner;
pub mod supervisor;
pub mod writer;

use crate::index::checkpoint::{Checkpoint, save_checkpoint};

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::change::FileChange;
use crate::config::Config;
use crate::index::scanner::scan_directory;
use crate::index::writer::IndexWriterWrapper;
use crate::watch::spawn_watcher;
use tokio::sync::{mpsc, watch};

/// Capacity of the channels carrying `FileChange` events from scanner/watcher.
const CHANGE_CHANNEL_CAPACITY: usize = 1000;

/// The IndexCoordinator manages the indexing lifecycle:
/// 1. Creates/opens the index
/// 2. Runs an incremental startup scan
/// 3. Starts a file watcher for real-time updates
/// 4. Processes FileChange events from both sources
pub struct IndexCoordinator {
    writer: Arc<IndexWriterWrapper>,
    config: Arc<Config>,
}

impl IndexCoordinator {
    /// Create a new coordinator, opening (or creating) the index.
    /// The initial scan is run separately via [`Self::startup_scan`].
    pub async fn new(
        config: Arc<Config>,
        text_data_cache: crate::formats::TextDataCache,
    ) -> Result<Self> {
        info!("Creating index coordinator");

        let writer =
            IndexWriterWrapper::new(&config.index_path, config.clone(), text_data_cache).await?;

        Ok(Self {
            writer: Arc::new(writer),
            config,
        })
    }

    /// Create a coordinator with an existing shared writer.
    pub fn with_writer(writer: Arc<IndexWriterWrapper>, config: Arc<Config>) -> Self {
        Self { writer, config }
    }

    /// Get the shared writer.
    pub fn writer_arc(&self) -> Arc<IndexWriterWrapper> {
        self.writer.clone()
    }

    /// Run the full startup sequence: scan, commit, and save the checkpoint.
    /// The checkpoint is only saved when the scan succeeded AND the commit fully
    /// applied all buffered changes; otherwise the previous checkpoint is kept so
    /// a restart re-applies the missed/unapplied changes via the mtime filter.
    pub async fn startup_scan(&self) -> Result<u64> {
        info!("Running startup scan...");

        // Process events from scanner and wait for the scan to finish.
        // `changed` is None if the scan failed.
        let (processed, changed) = run_scan(&self.writer, &self.config).await;

        // Final commit after scan — must happen BEFORE saving checkpoint,
        // otherwise files could be missed if crash occurs between checkpoint and commit.
        // Unapplied changes stay buffered for the event loop to retry. Do not
        // advance the checkpoint on failure: with the previous one intact, the
        // mtime filter re-applies the changes on the next start. Also do not
        // return Err, so a transient disk failure at startup does not kill
        // the service.
        let committed = commit_or_log(&self.writer, "Startup commit failed").await;

        // Save the checkpoint only after a successful commit of a successful
        // scan. A failed scan left some files unprobed, so advancing
        // `completed_at` would make the mtime filter skip files modified since
        // the previous checkpoint. Keep the old checkpoint so a restart
        // re-scans them.
        if committed && changed.is_some() {
            let mut cp = Checkpoint::new_started();
            cp.mark_completed(processed);
            save_checkpoint(&self.config.index_path, &cp)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to save checkpoint: {}", e))?;
        } else {
            warn!("Checkpoint not saved; next start will re-scan changed files");
        }

        let changed = changed.unwrap_or(0);
        info!(files_scanned = changed, processed, "Startup scan complete");
        Ok(changed)
    }

    /// Start the file watcher and run the background event processing loop.
    pub async fn start_watching(self, mut shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        info!("Starting file watcher and event processing loop");

        let (tx, mut rx) = mpsc::channel(CHANGE_CHANNEL_CAPACITY);
        let (watcher, debouncer_handle) = spawn_watcher(&self.config, tx).await?;

        let batch_timeout = Duration::from_millis(self.config.batch_timeout_ms);
        event_loop(
            &self.writer,
            &self.config,
            &mut rx,
            batch_timeout,
            &mut shutdown_rx,
        )
        .await;

        // Release the notify watcher: dropping it disconnects notify's channel,
        // which lets the watcher thread exit, which closes the bridge and makes
        // the debouncer task perform its final flush and drop the sender.
        drop(watcher);

        // Drain remaining events, including the debouncer's final flush.
        // This terminates because the event source exits (or the sender is
        // dropped once the watcher task exits) either way.
        drain_changes(&self.writer, &self.config, &mut rx).await;

        // The watcher task has exited; capture why it did so.
        let watcher_result = debouncer_handle.await;

        // Final commit so buffered changes survive shutdown. A failure here must
        // not return Err, otherwise the supervisor's retry loop would restart
        // the watcher after an explicit shutdown. The last saved checkpoint (if any)
        // still covers these changes: the mtime filter re-applies them on the
        // next start.
        commit_or_log(&self.writer, "Final commit on shutdown failed").await;

        // Propagate the watcher's failure so the supervisor's retry loop can
        // restart it. An explicit shutdown takes precedence: main is already
        // tearing down, and a restarted watcher would never observe the
        // already-set shutdown flag.
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        match watcher_result {
            Ok(inner) => inner.map_err(|e| e.context("File watcher exited with an error")),
            Err(join_error) => Err(anyhow::anyhow!("Watcher task panicked: {join_error}")),
        }
    }

    /// Get a reference to the index writer.
    pub fn writer(&self) -> &IndexWriterWrapper {
        &self.writer
    }
}

/// Run the main event processing loop until shutdown is requested or the
/// event source (watcher task) terminates.
///
/// Drains incoming `FileChange` events and applies them via `process_change`.
/// Periodically commits buffered changes when the batch timeout elapses.
///
/// On exit the loop returns without draining the channel; the caller must
/// release the event source (drop the watcher) and drain `rx` afterwards.
async fn event_loop(
    writer: &IndexWriterWrapper,
    config: &Arc<Config>,
    rx: &mut mpsc::Receiver<FileChange>,
    batch_timeout: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let mut commit_timer = tokio::time::interval(batch_timeout);
    commit_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased; // Prefer draining the channel before timing out

            maybe_change = rx.recv() => {
                // The event source (watcher task) exited. It performs a final
                // flush before dropping the sender, so the remaining events
                // are still in the channel and are drained by the caller.
                let Some(change) = maybe_change else {
                    break;
                };
                process_change_or_log(writer, config, change).await;
            }

            _ = commit_timer.tick() => {
                // commit() no-ops on an empty buffer, so no pre-check is needed.
                commit_or_log(writer, "Commit failed").await;
            }

            _ = shutdown_rx.changed() => {
                info!("Watcher shutdown requested");
                break;
            }
        }
    }
}

/// Commit buffered changes, logging (not propagating) non-applied outcomes.
/// Returns true only if the buffer was empty or fully committed.
async fn commit_or_log(writer: &IndexWriterWrapper, context: &str) -> bool {
    let applied = writer.commit().await;
    if !applied {
        // Failed changes are requeued for retry (or dropped after
        // MAX_RETRY_COUNT), so warn instead of error.
        warn!("{context} — changes not applied");
    }
    applied
}

/// Process a single file change, logging (not propagating) failures.
/// Returns true if the change was applied.
async fn process_change_or_log(
    writer: &IndexWriterWrapper,
    config: &Arc<Config>,
    change: FileChange,
) -> bool {
    match process_change(writer, config, change).await {
        Ok(()) => true,
        Err(e) => {
            error!(error = %e, "Failed to process file change");
            false
        }
    }
}

/// Drain all pending events from the channel, applying them to the index.
/// Returns the number of successfully processed changes.
/// Terminates when all senders are dropped and the channel is empty.
async fn drain_changes(
    writer: &IndexWriterWrapper,
    config: &Arc<Config>,
    rx: &mut mpsc::Receiver<FileChange>,
) -> u64 {
    let mut processed = 0;
    while let Some(change) = rx.recv().await {
        if process_change_or_log(writer, config, change).await {
            processed += 1;
        }
    }
    processed
}

/// Process a single file change event.
async fn process_change(
    writer: &IndexWriterWrapper,
    config: &Arc<Config>,
    change: FileChange,
) -> Result<()> {
    match change {
        FileChange::Modified(path) => writer.add_file(path).await?,
        FileChange::Deleted(path) => writer.delete_file(path).await?,
        FileChange::DeletedDir(path) => writer.delete_dir(path).await?,
        FileChange::Rescan => rescan_directory(writer, config).await?,
    }
    // Clear entire cache on ANY change to minimize stale-data errors.
    // This assumes filesystem changes are very rare.
    // Even unrelated events could affect cached files, and a full reset is simpler
    // than tracking exact dependencies.
    writer.text_data_cache().invalidate_all();
    Ok(())
}

/// Re-run the directory scan after an event-stream interruption (inotify
/// queue overflow): the kernel dropped events, so the index may be missing
/// creates/edits/deletes. The scan is mtime-based and idempotent, so it
/// safely re-applies whatever was lost. Consecutive overflows each trigger
/// their own scan — safe (idempotent), but scans can chain during sustained
/// bulk operations.
async fn rescan_directory(writer: &IndexWriterWrapper, config: &Arc<Config>) -> Result<()> {
    warn!("Inotify queue overflow — some events were dropped; rescanning directory");

    let (processed, Some(changed)) = run_scan(writer, config).await else {
        return Err(anyhow::anyhow!("Overflow rescan failed"));
    };

    info!(processed, changed, "Overflow rescan complete");

    // Make the recovered changes searchable immediately instead of waiting
    // for the next batch commit.
    commit_or_log(writer, "Post-rescan commit failed").await;

    Ok(())
}

/// Spawn `scan_directory` on a dedicated channel and drain its events into
/// the writer. Returns (processed, changed): events applied to the writer,
/// and files changed per the scan — `None` if the scan failed (failures are
/// logged here).
///
/// The drain is Box-pinned: this function is also reachable from
/// `process_change` via `rescan_directory`, and without the indirection the
/// call chain (process_change -> rescan_directory -> run_scan ->
/// drain_changes -> process_change) would be statically recursive. At
/// runtime it terminates because the scanner only emits Modified/Deleted,
/// never Rescan.
async fn run_scan(writer: &IndexWriterWrapper, config: &Arc<Config>) -> (u64, Option<u64>) {
    let (tx, mut rx) = mpsc::channel(CHANGE_CHANNEL_CAPACITY);

    let config_ref = config.clone();
    let index = writer.index();
    let scan_handle = tokio::spawn(async move { scan_directory(&config_ref, index, tx).await });

    let processed = Box::pin(drain_changes(writer, config, &mut rx)).await;

    let changed = match scan_handle.await {
        Ok(Ok(changed)) => Some(changed),
        Ok(Err(e)) => {
            error!(error = %e, "Directory scan failed");
            None
        }
        Err(e) => {
            error!(error = %e, "Scan task panicked");
            None
        }
    };
    (processed, changed)
}

#[cfg(test)]
mod tests {
    use super::checkpoint::{CHECKPOINT_FILENAME, load_checkpoint};
    use super::*;
    use crate::search::{SearchParams, search};

    fn make_temp_dir(tag: &str) -> std::path::PathBuf {
        crate::testutil::unique_temp_dir(&format!("rescan_test_{tag}"))
    }

    fn make_config(watch_dir: &std::path::Path, index_dir: &std::path::Path) -> Arc<Config> {
        Arc::new(Config {
            directory: watch_dir.to_path_buf(),
            index_path: index_dir.to_path_buf(),
            port: 0,
            bind: "127.0.0.1".to_string(),
            max_file_size_mb: 1,
            batch_size: 500,
            batch_timeout_ms: 1000,
            allowed_extensions: vec![],
        })
    }

    async fn found(
        reader: &tantivy::IndexReader,
        watch_dir: &std::path::Path,
        q: &str,
        cache: &crate::formats::TextDataCache,
    ) -> usize {
        search(
            reader,
            SearchParams {
                q: q.to_string(),
                ..Default::default()
            },
            watch_dir,
            cache,
            false,
        )
        .await
        .unwrap()
        .total
    }

    /// Simulate events lost to an inotify queue overflow: change files on
    /// disk without any watcher, then verify `rescan_directory` recovers
    /// them (modified content, new files, deletions) and is idempotent.
    #[tokio::test]
    async fn test_rescan_directory_recovers_lost_changes() {
        let watch_dir = make_temp_dir("watch");
        let index_dir = make_temp_dir("index");
        let config = make_config(&watch_dir, &index_dir);

        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            crate::formats::new_text_data_cache(),
        )
        .await
        .unwrap();

        // Baseline index
        std::fs::write(watch_dir.join("a.txt"), "alpha original").unwrap();
        std::fs::write(watch_dir.join("b.txt"), "beta content").unwrap();
        writer.add_file(watch_dir.join("a.txt")).await.unwrap();
        writer.add_file(watch_dir.join("b.txt")).await.unwrap();
        writer.commit().await;

        // "Lost" changes: modify a, create c, delete b
        std::fs::write(watch_dir.join("a.txt"), "alpha modified").unwrap();
        std::fs::write(watch_dir.join("c.txt"), "gamma content").unwrap();
        std::fs::remove_file(watch_dir.join("b.txt")).unwrap();

        rescan_directory(&writer, &config).await.unwrap();

        let reader = writer.index().reader().unwrap();
        let cache = writer.text_data_cache();
        assert_eq!(
            found(&reader, &watch_dir, "alpha modified", cache).await,
            1,
            "modified content recovered"
        );
        assert_eq!(
            found(&reader, &watch_dir, "gamma", cache).await,
            1,
            "new file indexed"
        );
        assert_eq!(
            found(&reader, &watch_dir, "beta", cache).await,
            0,
            "deleted file removed from index"
        );

        // Second rescan with no further changes: still consistent
        rescan_directory(&writer, &config).await.unwrap();
        assert_eq!(found(&reader, &watch_dir, "gamma", cache).await, 1);
        assert_eq!(found(&reader, &watch_dir, "beta", cache).await, 0);
    }

    /// A corrupted checkpoint file must not block the startup scan: the
    /// scan falls back to a full reindex, and the damaged file is
    /// atomically overwritten once the scan and commit succeed.
    #[tokio::test]
    async fn test_startup_scan_self_heals_corrupted_checkpoint() {
        let watch_dir = make_temp_dir("watch");
        let index_dir = make_temp_dir("index");
        let config = make_config(&watch_dir, &index_dir);

        std::fs::write(watch_dir.join("a.txt"), "alpha content").unwrap();

        // Corrupt the checkpoint before the startup scan
        std::fs::write(index_dir.join(CHECKPOINT_FILENAME), "not valid json {{{").unwrap();

        let writer = IndexWriterWrapper::new(
            &index_dir,
            config.clone(),
            crate::formats::new_text_data_cache(),
        )
        .await
        .unwrap();
        let coordinator = IndexCoordinator::with_writer(Arc::new(writer), config.clone());
        coordinator.startup_scan().await.unwrap();

        // The damaged file is replaced with a valid, completed checkpoint
        let cp = load_checkpoint(&index_dir)
            .await
            .unwrap()
            .expect("checkpoint rewritten after successful scan");
        assert_eq!(cp.version, 1);
        assert!(cp.completed_at.is_some());

        // The full reindex ran: the file is searchable
        let reader = coordinator.writer().index().reader().unwrap();
        let cache = coordinator.writer().text_data_cache();
        assert_eq!(
            found(&reader, &watch_dir, "alpha", cache).await,
            1,
            "file reindexed despite corrupted checkpoint"
        );
    }
}
