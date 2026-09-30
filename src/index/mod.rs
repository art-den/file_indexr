pub mod scanner;
pub mod supervisor;
pub mod writer;

#[cfg(test)]
mod tests;

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

    /// Run the full startup sequence: scan and commit.
    ///
    /// Change detection is per-file (filesystem mtime vs the mtime stored in the index),
    /// so a failed scan or commit needs no special handling for correctness: files that
    /// were not (re)indexed keep a stale stored mtime and are picked up again on the
    /// next start.
    pub async fn startup_scan(&self) -> Result<u64> {
        info!("Running startup scan...");

        // Process events from scanner and wait for the scan to finish.
        // `changed` is None if the scan failed.
        let (processed, changed) = run_scan(&self.writer, &self.config).await;

        // Final commit after scan. A failure must not return Err, so a transient
        // disk failure at startup does not kill the service. Unapplied changes
        // stay buffered for the event loop to retry; the file keeps its stale
        // stored mtime, so the next start re-indexes it.
        commit_or_log(&self.writer, "Startup commit failed").await;

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
        // the watcher after an explicit shutdown. Uncommitted changes keep a
        // stale stored mtime in the index and are re-applied on the next start.
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
/// creates/edits/deletes. The scan compares each file's mtime with the mtime
/// stored in the index, so it re-applies only what was lost. Consecutive
/// overflows each trigger their own scan — safe (idempotent), but scans can
/// chain during sustained bulk operations.
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
