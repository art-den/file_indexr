pub mod debouncer;

#[cfg(test)]
mod tests;

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Debounce window for create/modify events — deletion is immediate.
const DEBOUNCE_DELAY: Duration = Duration::from_secs(1);
/// Interval for draining expired debounced events.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Type alias for the notify bridge channel receiver.
type NotifyBridgeRx = mpsc::UnboundedReceiver<Result<Event, notify::Error>>;

use crate::config::Config;
use crate::index::scanner::{has_hidden_component, is_hidden_dir_name};
use crate::watch::debouncer::Debouncer;

/// Re-export of the shared `FileChange` event type (defined in `crate::change`)
/// so the public path `file_indexr::watch::FileChange` keeps resolving.
pub use crate::change::FileChange;

/// Returns true if the path should be ignored by the file watcher.
fn should_skip_path(path: &Path, watched_dir: &Path, index_path: &Path) -> bool {
    // Skip paths inside the index directory and paths containing hidden
    // directory components (consistent with scanner).
    path.starts_with(index_path) || has_hidden_component(path, watched_dir)
}

/// Returns true if `path` is a directory subtree whose contents are never
/// indexed, so walking it would be pure waste: the subtree is inside the
/// index directory, or it is a hidden directory / under a hidden one.
///
/// `should_skip_path` alone does not catch a hidden directory's own path —
/// `has_hidden_component` excludes the final component, treating it as a
/// file name — so the directory name is checked via `is_hidden_dir_name`.
fn skip_directory_walk(path: &Path, watched_dir: &Path, index_path: &Path) -> bool {
    should_skip_path(path, watched_dir, index_path) || is_hidden_dir_name(path)
}

/// Setup the notify watcher and a background bridge thread that forwards
/// blocking `recv` calls to an async channel.
fn setup_notify_watcher(
    watched_dir: &Path,
) -> Result<(
    RecommendedWatcher,
    std::thread::JoinHandle<()>,
    NotifyBridgeRx,
)> {
    let (notify_tx, notify_rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(notify_tx, notify::Config::default())?;
    watcher.watch(watched_dir, RecursiveMode::Recursive)?;

    info!("File watcher registered for recursive monitoring");

    // Spawn a blocking thread to receive from notify (`recv` blocks OS thread).
    // Use unbounded_channel to bridge std thread → async runtime.
    let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<Result<Event, notify::Error>>();
    let watcher_thread = std::thread::spawn(move || {
        loop {
            match notify_rx.recv() {
                Ok(Ok(event)) => {
                    if bridge_tx.send(Ok(event)).is_err() {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    error!(error = %e, "File watcher error");
                    // Forward the error so the consumer can restart the watcher;
                    // a plain break would look like a clean exit.
                    let _ = bridge_tx.send(Err(e));
                    break;
                }
                Err(_) => {
                    info!("Notify channel disconnected, stopping watcher");
                    break;
                }
            }
        }
    });

    Ok((watcher, watcher_thread, bridge_rx))
}

/// Process a single notify event: convert to FileChanges, filter, and apply debounce.
/// Returns `false` if the sender channel was closed.
async fn process_event(
    event: Event,
    watched_dir: &Path,
    index_path: &Path,
    delayed: &mut Debouncer,
    tx: &mpsc::Sender<FileChange>,
) -> bool {
    let changes = event_to_changes(&event, watched_dir, index_path).await;

    for change in changes {
        // Rescan is pathless, so this is a no-op for it and it falls through
        // to the immediate-send arm below.
        if change
            .path()
            .is_some_and(|p| should_skip_path(p, watched_dir, index_path))
        {
            continue;
        }

        match change {
            // Deletion and Rescan are processed immediately — no debounce.
            FileChange::Deleted(_) | FileChange::DeletedDir(_) | FileChange::Rescan => {
                if tx.send(change).await.is_err() {
                    return false;
                }
            }
            FileChange::Modified(p) => {
                let now = Instant::now();
                if let Some(ev) = delayed.add(p, now)
                    && tx.send(ev).await.is_err()
                {
                    return false;
                }
            }
        }
    }
    true
}

/// Run the main event loop: receive bridged notify events, process them,
/// and periodically tick the debouncer to emit expired events.
///
/// Returns `Err` when the bridge reported a notify error (the watcher died and
/// should be restarted); a plain bridge-channel close is a clean exit.
async fn run_event_loop(
    mut bridge_rx: NotifyBridgeRx,
    watcher_thread: std::thread::JoinHandle<()>,
    watched_dir: PathBuf,
    index_path: PathBuf,
    tx: mpsc::Sender<FileChange>,
) -> Result<()> {
    let mut delayed_debouncer = Debouncer::new(DEBOUNCE_DELAY);
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    let mut bridge_error: Option<notify::Error> = None;

    'outer: loop {
        tokio::select! {
            result = bridge_rx.recv() => {
                match result {
                    Some(Ok(event)) => {
                        if !process_event(event, &watched_dir, &index_path, &mut delayed_debouncer, &tx).await {
                            break 'outer;
                        }
                    }
                    Some(Err(e)) => {
                        bridge_error = Some(e);
                        break 'outer;
                    }
                    None => break 'outer, // Bridge channel closed — watcher thread exited.
                }
            }
            _ = interval.tick() => {
                // Drain expired debounced events.
                let now = Instant::now();
                let events = delayed_debouncer.tick(now);
                for ev in events {
                    if tx.send(ev).await.is_err() {
                        // Consumer dropped — exit without reporting an error.
                        break 'outer;
                    }
                }
            }
        }
    }

    // Final flush of pending debounced events.
    let remaining = delayed_debouncer.flush();
    for ev in remaining {
        let _ = tx.send(ev).await;
    }

    // Join the watcher thread.
    let _ = tokio::task::spawn_blocking(move || {
        let _ = watcher_thread.join();
    })
    .await;

    match bridge_error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// Spawn a file watcher that monitors the directory for changes and sends
/// FileChange events through the channel.
///
/// The returned task handle resolves to `Err` when the underlying notify
/// watcher fails (the caller should restart the watcher), and `Ok(())` on a
/// clean stop.
pub async fn spawn_watcher(
    config: &Config,
    tx: mpsc::Sender<FileChange>,
) -> Result<(RecommendedWatcher, tokio::task::JoinHandle<Result<()>>)> {
    info!(directory = ?config.directory, "Starting file watcher");

    let (watcher, watcher_thread, bridge_rx) = setup_notify_watcher(&config.directory)?;

    let handle = tokio::spawn(run_event_loop(
        bridge_rx,
        watcher_thread,
        config.directory.clone(),
        config.index_path.clone(),
        tx,
    ));

    Ok((watcher, handle))
}

/// Collect all file paths inside a directory, recursively.
///
/// Prunes the index directory and hidden subdirectories during the walk:
/// their contents are never indexed (see `should_skip_path`). The caller is
/// expected to have checked the root itself via `skip_directory_walk` — a
/// root under a hidden directory is caught there, so only the by-name
/// hidden check is needed for subdirectories here.
async fn collect_files_in_directory(start: &Path, index_path: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![start.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(ft) = entry.file_type().await else {
                continue; // File may have been deleted between read_dir and file_type().
            };
            let path = entry.path();
            if ft.is_file() {
                result.push(path);
            } else if ft.is_dir() {
                // Skip hidden subdirectories and the index directory during walk
                let excluded = path.starts_with(index_path) || is_hidden_dir_name(&path);
                if !excluded {
                    stack.push(path);
                }
            }
        }
    }

    result
}

/// Convert a single path from a non-rename event to FileChange(s),
/// appending them to `changes`. `Create` on a directory produces multiple
/// changes (scans its contents); `Modify` on a directory is skipped (see
/// the arm below).
async fn path_to_change(
    kind: &EventKind,
    path: &Path,
    watched_dir: &Path,
    index_path: &Path,
    changes: &mut Vec<FileChange>,
) {
    match kind {
        EventKind::Create(_) => {
            add_as_modified(path, watched_dir, index_path, changes).await;
        }
        EventKind::Modify(_) => {
            // Emit only for regular files. A Modify on a directory (touch,
            // chmod, rsync/tar/unzip restoring directory mtimes — inotify
            // IN_ATTRIB) must not trigger a recursive walk: under a
            // recursive watch child files generate their own events, and
            // reindexing the whole subtree on a metadata-only change is
            // wasteful. Rescan (inotify queue overflow) is the safety net
            // for dropped events.
            if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file()) {
                changes.push(FileChange::Modified(path.to_path_buf()));
            }
        }
        EventKind::Remove(remove_kind) => match remove_kind {
            notify::event::RemoveKind::Folder | notify::event::RemoveKind::Other => {
                changes.push(FileChange::DeletedDir(path.to_path_buf()));
            }
            notify::event::RemoveKind::File | notify::event::RemoveKind::Any => {
                changes.push(FileChange::Deleted(path.to_path_buf()));
            }
        },
        // Ignore access events and other events. The rescan flag on `Other`
        // events (inotify queue overflow) is handled in `event_to_changes`.
        EventKind::Access(_) | EventKind::Other | EventKind::Any => {}
    }
}

/// Add `FileChange::Modified` for each file inside `path`, or a single
/// `Modified(path)` if it's a regular file. Ignores inaccessible paths.
///
/// Directories inside excluded subtrees (index dir, hidden) are not walked:
/// the per-file filter in `process_event` would discard everything anyway.
async fn add_as_modified(
    path: &Path,
    watched_dir: &Path,
    index_path: &Path,
    changes: &mut Vec<FileChange>,
) {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return;
    };
    if meta.is_dir() {
        if skip_directory_walk(path, watched_dir, index_path) {
            return;
        }
        let files = collect_files_in_directory(path, index_path).await;
        if !files.is_empty() {
            debug!(dir = %path.display(), files = files.len(), "Scanning directory");
        }
        changes.extend(files.into_iter().map(FileChange::Modified));
    } else if meta.is_file() {
        changes.push(FileChange::Modified(path.to_path_buf()));
    }
}

/// Emit delete changes for a renamed-from path.
///
/// The old path no longer exists, so its type is unknown: emit both variants.
/// Each is a no-op for the wrong type — `Deleted` matches nothing for a
/// directory (no document at that exact path), and `DeletedDir`'s `dir/`
/// prefix never matches a file path.
fn emit_delete_for_rename(path: &Path, changes: &mut Vec<FileChange>) {
    changes.push(FileChange::Deleted(path.to_path_buf()));
    changes.push(FileChange::DeletedDir(path.to_path_buf()));
}

/// Convert a notify event into FileChange(s).
///
/// For rename events (ModifyKind::Name), emits Deleted + Modified pairs so that
/// the old index entry is removed and the new one is added. Without this,
/// RenameMode::From is silently ignored (old path no longer exists → is_file()=false)
/// leaving a ghost record in the index.
///
/// The renamed-from path is already gone, so for `RenameMode::From` and
/// `RenameMode::Both` both delete variants are emitted (`Deleted` and
/// `DeletedDir`); each is a no-op for the wrong type.
///
/// `watched_dir`/`index_path` are passed down so that expensive directory
/// walks are skipped for excluded paths *before* they happen. Rename pairs
/// are consumed as (from, to) via `chunks_exact(2)`, so the skip check must
/// never remove paths from `event.paths` itself — that would break the
/// pairing and could delete a live `to` path.
async fn event_to_changes(event: &Event, watched_dir: &Path, index_path: &Path) -> Vec<FileChange> {
    let mut changes = Vec::new();

    // Inotify queue overflow (or equivalent backend signal): the kernel
    // dropped events, so changes may be missing from the index. Request an
    // idempotent rescan; regular path handling below still runs.
    if event.need_rescan() {
        warn!("File event stream interrupted (rescan flag set) — some events were dropped");
        changes.push(FileChange::Rescan);
    }

    // Handle renames specially — we need both from/to paths to emit Delete+Modified
    if let EventKind::Modify(notify::event::ModifyKind::Name(mode)) = event.kind {
        match mode {
            notify::event::RenameMode::Both => {
                // Paths come as (from, to) pairs per notify docs.
                let mut pairs = event.paths.chunks_exact(2);
                for pair in pairs.by_ref() {
                    emit_delete_for_rename(&pair[0], &mut changes);
                    add_as_modified(&pair[1], watched_dir, index_path, &mut changes).await;
                }
                // Handle odd trailing path (unlikely, but safe)
                if let Some(odd) = pairs.remainder().first() {
                    path_to_change(&event.kind, odd, watched_dir, index_path, &mut changes).await;
                }
            }
            notify::event::RenameMode::From => {
                // Old path is gone — delete from index regardless of is_file().
                for path in &event.paths {
                    emit_delete_for_rename(path, &mut changes);
                }
            }
            _ => {
                // To, Any, Other — treat as modify (new path exists).
                for path in &event.paths {
                    add_as_modified(path, watched_dir, index_path, &mut changes).await;
                }
            }
        }
    } else {
        // Non-rename events: process each path independently
        for path in &event.paths {
            path_to_change(&event.kind, path, watched_dir, index_path, &mut changes).await;
        }
    }

    changes
}
