use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

use std::io::ErrorKind;
use std::path::Path;

/// Checkpoint data stored on disk to support incremental indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Format version for future migration
    pub version: u32,
    /// When the indexing run began
    pub started_at: DateTime<Utc>,
    /// When the indexing run finished successfully; used to determine
    /// which files changed since last run
    pub completed_at: Option<DateTime<Utc>>,
    /// Number of files processed during this run (informational)
    pub files_processed: u64,
}

/// Default path for checkpoint file relative to index directory.
pub const CHECKPOINT_FILENAME: &str = "checkpoint.json";

/// Current checkpoint format version.
const CURRENT_VERSION: u32 = 1;

impl Checkpoint {
    /// Create a new checkpoint marking the start of an indexing run.
    pub fn new_started() -> Self {
        Self {
            version: CURRENT_VERSION,
            started_at: Utc::now(),
            completed_at: None,
            files_processed: 0,
        }
    }

    /// Mark the checkpoint as completed.
    pub fn mark_completed(&mut self, files_processed: u64) {
        self.completed_at = Some(Utc::now());
        self.files_processed = files_processed;
    }
}

/// Save checkpoint to disk.
pub async fn save_checkpoint(index_path: &Path, checkpoint: &Checkpoint) -> Result<(), String> {
    let checkpoint_path = index_path.join(CHECKPOINT_FILENAME);
    let content = serde_json::to_string(checkpoint)
        .map_err(|e| format!("Failed to serialize checkpoint: {}", e))?;

    // Write atomically: write to temp file then rename
    let tmp_path = checkpoint_path.with_extension("tmp");
    fs::write(&tmp_path, &content)
        .await
        .map_err(|e| format!("Failed to write checkpoint temp file: {}", e))?;
    fs::rename(&tmp_path, &checkpoint_path)
        .await
        .map_err(|e| format!("Failed to move checkpoint file: {}", e))?;

    Ok(())
}

/// Load checkpoint from disk. Returns `None` if no checkpoint exists, or if
/// the file is unreadable or corrupted (the scan then falls back to a full
/// reindex, and a successful scan overwrites the damaged file). Returns
/// `Err` only when the checkpoint version is unsupported (needs migration).
pub async fn load_checkpoint(index_path: &Path) -> Result<Option<Checkpoint>, String> {
    let checkpoint_path = index_path.join(CHECKPOINT_FILENAME);

    let content = match fs::read_to_string(&checkpoint_path).await {
        Ok(c) => c,
        // A missing checkpoint is the normal first-run case, not an error.
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            warn!(error = %e, path = %checkpoint_path.display(), "Failed to read checkpoint file; falling back to full scan");
            return Ok(None);
        }
    };

    let checkpoint: Checkpoint = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path = %checkpoint_path.display(), "Failed to parse checkpoint file; falling back to full scan");
            return Ok(None);
        }
    };

    // Validate: accept known versions (<= current), reject only those needing migration.
    if checkpoint.version > CURRENT_VERSION {
        return Err(format!(
            "Unsupported checkpoint version: {} (max supported: {})",
            checkpoint.version, CURRENT_VERSION
        ));
    }

    Ok(Some(checkpoint))
}

/// Delete the checkpoint file (e.g., after full reindex).
pub async fn delete_checkpoint(index_path: &Path) -> Result<(), String> {
    let checkpoint_path = index_path.join(CHECKPOINT_FILENAME);
    match fs::remove_file(&checkpoint_path).await {
        Ok(()) => Ok(()),
        // A missing checkpoint is not an error.
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Failed to delete checkpoint: {}", e)),
    }
}
