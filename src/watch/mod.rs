pub mod debouncer;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-existent index dir for tests that use fixed `/tmp` paths — none of
    /// the fixture paths below are skipped with this context.
    const TMP_INDEX: &str = "/tmp/file_indexr_unused_index";

    fn make_event(kind: EventKind, paths: Vec<&Path>) -> Event {
        Event {
            kind,
            paths: paths.into_iter().map(|p| p.to_path_buf()).collect(),
            attrs: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_create_file_emits_modified() {
        let dir = std::env::temp_dir().join("file_indexr_watch_test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("new.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::File),
            vec![&file_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::Modified(p) if p == &file_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_modify_emits_modified() {
        let dir = std::env::temp_dir().join("file_indexr_watch_test2");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("modified.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Any),
            vec![&file_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::Modified(p) if p == &file_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_modify_dir_emits_nothing() {
        let dir = std::env::temp_dir().join("file_indexr_watch_test_modify_dir");
        std::fs::create_dir_all(&dir).unwrap();
        // A file inside the dir must NOT be picked up by a Modify on the dir.
        std::fs::write(dir.join("inner.txt"), "hello").unwrap();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::Any,
            )),
            vec![&dir],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert!(changes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_modify_nonexistent_emits_nothing() {
        let path = std::env::temp_dir().join("file_indexr_watch_test_modify_missing");
        std::fs::remove_file(&path).ok();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Any),
            vec![&path],
        );
        let changes = event_to_changes(
            &event,
            &std::env::temp_dir(),
            &std::env::temp_dir().join("file_indexr_unused_index"),
        )
        .await;
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn test_create_dir_emits_modified_for_files() {
        let dir = std::env::temp_dir().join("file_indexr_watch_test_create_dir_nonempty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("b.txt"), "world").unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::Folder),
            vec![&dir],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        // Create on a directory still walks it and emits Modified per file.
        let mut paths: Vec<_> = changes
            .iter()
            .filter_map(|c| match c {
                FileChange::Modified(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        paths.sort();
        assert_eq!(paths, vec![dir.join("a.txt"), dir.join("b.txt")]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_remove_other_emits_deleted_dir() {
        let path = Path::new("/tmp/deleted.txt");
        let event = make_event(
            EventKind::Remove(notify::event::RemoveKind::Other),
            vec![path],
        );
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::DeletedDir(_)));
    }

    #[tokio::test]
    async fn test_remove_file_emits_deleted() {
        let path = Path::new("/tmp/deleted.txt");
        let event = make_event(
            EventKind::Remove(notify::event::RemoveKind::File),
            vec![path],
        );
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::Deleted(_)));
    }

    #[tokio::test]
    async fn test_remove_folder_emits_deleted_dir() {
        let path = Path::new("/tmp/deleted_dir");
        let event = make_event(
            EventKind::Remove(notify::event::RemoveKind::Folder),
            vec![path],
        );
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::DeletedDir(_)));
    }

    #[tokio::test]
    async fn test_access_skipped() {
        let event = make_event(
            EventKind::Access(notify::event::AccessKind::Any),
            vec![Path::new("/tmp/file.txt")],
        );
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn test_rescan_flag_emits_rescan() {
        let event = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::Rescan));
    }

    #[tokio::test]
    async fn test_other_event_without_rescan_flag_emits_nothing() {
        let event = make_event(EventKind::Other, Vec::new());
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn test_directory_create_skipped() {
        let dir = std::env::temp_dir().join("file_indexr_watch_test3");
        std::fs::create_dir_all(&dir).unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::Folder),
            vec![&dir],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert!(changes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_rename_from_emits_deleted_and_deleted_dir() {
        // RenameMode::From: type is always unknown — emit both variants
        let from_path = Path::new("/tmp/old.txt");
        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::From,
            )),
            vec![from_path],
        );
        let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path.to_path_buf()));
        assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path.to_path_buf()));
    }

    #[tokio::test]
    async fn test_rename_to_emits_modified() {
        // RenameMode::To: new path exists — should emit Modified
        let dir = std::env::temp_dir().join("file_indexr_rename_test");
        std::fs::create_dir_all(&dir).unwrap();
        let to_path = dir.join("new.txt");
        std::fs::write(&to_path, "renamed content").unwrap();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To,
            )),
            vec![&to_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::Modified(p) if p == &to_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_rename_both_file_emits_delete_and_modified() {
        let dir = std::env::temp_dir().join("file_indexr_renameboth_file_test");
        std::fs::create_dir_all(&dir).unwrap();
        let from_path = dir.join("old_name.txt");
        std::fs::write(&from_path, "content").unwrap();
        let to_path = dir.join("new_name.txt");
        std::fs::write(&to_path, "content").unwrap();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            vec![&from_path, &to_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        // Both delete variants are emitted (type is unknown), then Modified for the new path.
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path));
        assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path));
        assert!(matches!(&changes[2], FileChange::Modified(p) if p == &to_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_rename_both_dir_emits_deletedir_and_modified() {
        let dir = std::env::temp_dir().join("file_indexr_renameboth_dir_test");
        std::fs::create_dir_all(&dir).unwrap();
        let from_path = dir.join("old_dir");
        std::fs::create_dir_all(&from_path).unwrap();
        let to_path = dir.join("new_dir");
        std::fs::create_dir_all(&to_path).unwrap();
        // add_as_modified scans the 'to' dir for files — seed it so modified events are emitted.
        std::fs::write(to_path.join("inner.txt"), "data").unwrap();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            vec![&from_path, &to_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        // Both delete variants are emitted (type is unknown), then Modified for the new path.
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path));
        assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path));
        assert!(matches!(&changes[2], FileChange::Modified(p) if p == &to_path.join("inner.txt")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_rename_both_missing_parent_emits_both_deletes() {
        // The old path (and its parent) is gone — emit both Deleted and DeletedDir.
        let dir = std::env::temp_dir().join("file_indexr_renameboth_unk_test");
        std::fs::create_dir_all(&dir).unwrap();
        let from_path = Path::new("/nonexistent/parent/old.txt");
        let to_path = dir.join("actual_new.txt");
        std::fs::write(&to_path, "content").unwrap();

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            vec![from_path, &to_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path.to_path_buf()));
        assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path.to_path_buf()));
        assert!(matches!(&changes[2], FileChange::Modified(p) if p == &to_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_create_hidden_dir_emits_nothing() {
        // A hidden directory as walk root is skipped entirely: its contents
        // are never indexed, so no walk and no changes.
        let dir = std::env::temp_dir().join("file_indexr_watch_hidden_root");
        let hidden = dir.join(".git");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("config"), "data").unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::Folder),
            vec![&hidden],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert!(changes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_create_dir_under_hidden_emits_nothing() {
        // A walk root *under* a hidden directory is caught by the
        // `should_skip_path` branch of `skip_directory_walk` (as opposed to
        // the hidden-root case above, caught by the name check).
        let dir = std::env::temp_dir().join("file_indexr_watch_under_hidden");
        let objects = dir.join(".git").join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::write(objects.join("pack"), "data").unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::Folder),
            vec![&objects],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert!(changes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_create_dir_inside_index_emits_nothing() {
        // A folder created inside the index directory is skipped: no walk, no changes.
        let dir = std::env::temp_dir().join("file_indexr_watch_index_walk");
        let index = dir.join("file_indexr_data");
        let sub = index.join("segments");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("0.seg"), "data").unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::Folder),
            vec![&sub],
        );
        let changes = event_to_changes(&event, &dir, &index).await;
        assert!(changes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_rename_both_to_hidden_dir_keeps_pairing() {
        // 'to' is a hidden directory: its walk is skipped, but the (from, to)
        // pair must stay intact — only the two deletes for 'from' are
        // emitted, and the live 'to' path must not be treated as renamed-from.
        let dir = std::env::temp_dir().join("file_indexr_watch_rename_hidden");
        let to_dir = dir.join(".cache");
        std::fs::create_dir_all(&to_dir).unwrap();
        std::fs::write(to_dir.join("blob.bin"), "data").unwrap();
        let from_path = dir.join("a.txt");

        let event = make_event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            vec![&from_path, &to_dir],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path));
        assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_create_dot_file_emits_modified() {
        // Top-level dot files stay indexable — the hidden-dir-name check in
        // `skip_directory_walk` must not apply to regular files.
        let dir = std::env::temp_dir().join("file_indexr_watch_dot_file");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join(".env");
        std::fs::write(&file_path, "SECRET=1").unwrap();

        let event = make_event(
            EventKind::Create(notify::event::CreateKind::File),
            vec![&file_path],
        );
        let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], FileChange::Modified(p) if p == &file_path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_should_skip_path_inside_index_dir() {
        // Non-hidden index dir: a hidden name (e.g. `.file_indexr`) would also be
        // skipped by the hidden-component rule, masking the index-prefix rule.
        let watched_dir = Path::new("/home/user/docs");
        let index_path = Path::new("/home/user/docs/file_indexr_data");
        // Anti-masking guard: if the fixture name ever becomes hidden, the
        // assertions below would no longer isolate the index-prefix rule.
        assert!(!has_hidden_component(
            Path::new("/home/user/docs/file_indexr_data/data.mfd"),
            watched_dir
        ));
        assert!(should_skip_path(
            Path::new("/home/user/docs/file_indexr_data/data.mfd"),
            watched_dir,
            index_path
        ));
        assert!(should_skip_path(
            Path::new("/home/user/docs/file_indexr_data"),
            watched_dir,
            index_path
        ));
    }

    #[test]
    fn test_should_skip_path_outside_index_dir() {
        let watched_dir = Path::new("/home/user/docs");
        let index_path = Path::new("/home/user/docs/.file_indexr");
        assert!(!should_skip_path(
            Path::new("/home/user/docs/hello.txt"),
            watched_dir,
            index_path
        ));
        assert!(!should_skip_path(
            Path::new("/home/user/docs/sub/file.rs"),
            watched_dir,
            index_path
        ));
    }

    #[test]
    fn test_should_skip_hidden_paths() {
        let watched_dir = Path::new("/home/user/docs");
        let index_path = Path::new("/home/user/docs/.file_indexr");
        assert!(should_skip_path(
            Path::new("/home/user/docs/.git/config"),
            watched_dir,
            index_path
        ));
        assert!(should_skip_path(
            Path::new("/home/user/docs/.venv/lib/site.py"),
            watched_dir,
            index_path
        ));
        assert!(should_skip_path(
            Path::new("/home/user/docs/src/.cache/data.bin"),
            watched_dir,
            index_path
        ));
    }

    #[tokio::test]
    async fn test_collect_files_in_directory() {
        let dir = std::env::temp_dir().join("file_indexr_collect_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
        // Hidden subdirectory should be skipped
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join(".hidden/c.txt"), "c").unwrap();

        let files = collect_files_in_directory(&dir, &dir.join("index")).await;
        let paths: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"b.txt"));
        assert!(
            !paths.contains(&"c.txt"),
            "Hidden dir contents should be excluded"
        );
        assert_eq!(files.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_collect_files_empty_directory() {
        let dir = std::env::temp_dir().join("file_indexr_collect_empty");
        std::fs::create_dir_all(&dir).unwrap();

        let files = collect_files_in_directory(&dir, &dir.join("index")).await;
        assert!(files.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_collect_files_nonexistent_directory() {
        let files =
            collect_files_in_directory(Path::new("/nonexistent/path"), Path::new("/index")).await;
        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn test_collect_files_prunes_index_dir() {
        // An index-directory subdirectory is pruned during the walk.
        let dir = std::env::temp_dir().join("file_indexr_collect_index_prune");
        let index = dir.join("file_indexr_data");
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("0.seg"), "seg").unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let files = collect_files_in_directory(&dir, &index).await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], dir.join("a.txt"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_run_event_loop_propagates_notify_error() {
        let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<Result<Event, notify::Error>>();
        let (tx, _rx) = mpsc::channel::<FileChange>(16);
        let thread = std::thread::spawn(|| ());

        bridge_tx
            .send(Err(notify::Error::generic("test error")))
            .unwrap();
        drop(bridge_tx);

        let result = run_event_loop(
            bridge_rx,
            thread,
            PathBuf::from("/"),
            PathBuf::from("/index"),
            tx,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_event_loop_clean_exit_is_ok() {
        let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<Result<Event, notify::Error>>();
        let (tx, _rx) = mpsc::channel::<FileChange>(16);
        let thread = std::thread::spawn(|| ());

        // Closing the bridge without an error is a clean exit.
        drop(bridge_tx);

        let result = run_event_loop(
            bridge_rx,
            thread,
            PathBuf::from("/"),
            PathBuf::from("/index"),
            tx,
        )
        .await;
        assert!(result.is_ok());
    }
}
