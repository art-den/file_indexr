//! Shared `FileChange` event type, produced by both the scanner (startup/
//! overflow scans) and the watcher (live events) and consumed by the index
//! coordinator.

use std::path::{Path, PathBuf};

/// Information about a single file change event.
#[derive(Debug)]
pub enum FileChange {
    /// File was created or modified — index it
    Modified(PathBuf),
    /// File was deleted — remove from index
    Deleted(PathBuf),
    /// Directory was removed — delete all docs under this path
    DeletedDir(PathBuf),
    /// The event stream was interrupted (e.g. inotify queue overflow) — rescan the directory
    Rescan,
}

impl FileChange {
    /// Return the path associated with this change.
    /// `Rescan` carries no path.
    pub fn path(&self) -> Option<&Path> {
        match self {
            FileChange::Modified(p) | FileChange::Deleted(p) | FileChange::DeletedDir(p) => Some(p),
            FileChange::Rescan => None,
        }
    }
}
