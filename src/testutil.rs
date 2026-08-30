//! Test helpers for unique temp paths.
//!
//! Compiled unconditionally (not behind a `testing` feature) so integration
//! tests in `tests/` can use them without feature-gating. The `tag` should
//! identify the calling module/test so the dirs are distinguishable in the
//! OS temp dir.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Crate-global monotonically increasing id for unique temp names.
pub fn unique_id() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Unique temp path `<env::temp_dir()>/file_indexr_<tag>_<pid>_<id>`.
/// Not created on disk.
pub fn unique_temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "file_indexr_{}_{}_{}",
        tag,
        std::process::id(),
        unique_id()
    ))
}

/// Unique temp dir like [`unique_temp_path`], created via `create_dir`.
///
/// Retries with a fresh id on `AlreadyExists` instead of using
/// `create_dir_all`: the OS recycles pids, so a `<pid>_<id>` name can
/// collide with a directory left over from a previous test run (test dirs
/// are intentionally not cleaned up), and silently reusing it would leak
/// state between runs.
pub fn unique_temp_dir(tag: &str) -> PathBuf {
    loop {
        let path = unique_temp_path(tag);
        match std::fs::create_dir(&path) {
            Ok(()) => return path,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("Failed to create temp dir {}: {}", path.display(), e),
        }
    }
}
