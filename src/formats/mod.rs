use std::path::{Path, PathBuf};
use std::sync::Arc;

use moka::sync::Cache;
use serde::Serialize;

use crate::error::AppError;

/// Default number of lines before auto-truncation kicks in.
pub const AUTO_TRUNCATE_LINES: usize = 100;

mod epub;
mod html;
mod markdown;
mod pdf;
mod python;
mod rst;
mod rust;
mod text;

#[cfg(test)]
#[path = "tests/tests_mod.rs"]
mod tests;

/// A single heading extracted from a file.
#[derive(Serialize)]
pub struct HeadingItem {
    pub level: u8,
    pub text: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Copy)]
pub enum FileFormat {
    Text,
    Markdown,
    Html,
    Pdf,
    Epub,
    Python,
    Rust,
    Rst,
}

#[derive(Clone)]
pub struct FileTextData {
    /// Original format of the file.
    pub format: FileFormat,
    /// Normalized text content — plain text for code, Markdown for HTML/PDF/EPUB/MD/RST.
    /// Shared via `Arc` so cache hits and clones don't copy the text.
    pub text: Arc<String>,
}

#[derive(Serialize)]
pub struct FileStructure {
    pub headers: Vec<HeadingItem>,
    /// camelCase key for the web UI.
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
}

/// Read an image entry from an EPUB archive by document-relative path
/// (implemented in the `epub` module). Re-exported for the MCP tools.
pub use epub::read_image_entry;

/// Total weight budget of the text cache: the combined byte length of all
/// cached normalized texts (plus 1 per entry).
const CACHE_MAX_WEIGHT: u64 = 256 * 1024 * 1024; // 256 MiB

/// LRU cache of normalized file text. The canonical handle lives in `AppState`;
/// `IndexWriterWrapper` holds a clone sharing the same underlying store.
///
/// Bounded by the *total weight* of cached texts (see
/// [`text_data_cache_with_max_weight`]), not by the number of entries: a
/// directory full of large PDFs must not be able to grow the cache
/// unboundedly.
pub type TextDataCache = Cache<PathBuf, FileTextData>;

/// Create a new text data cache bounded by `max_weight` — the maximum total
/// byte length of all cached texts.
///
/// With a weigher set, moka interprets `max_capacity` as the total weight
/// limit and evicts by weight in LRU order. The LRU policy (instead of the
/// default TinyLFU) is required so that a newly requested file is always
/// cached: TinyLFU's admission gate would reject a cold candidate while the
/// cache is full until it becomes "popular", forcing redundant re-reads and
/// re-conversions — files are usually read once. A single item heavier than
/// the whole budget is never cached.
pub fn text_data_cache_with_max_weight(max_weight: u64) -> TextDataCache {
    Cache::builder()
        .max_capacity(max_weight)
        // Minimum weight 1 per entry: empty texts still count, so the entry
        // count stays bounded even for files with zero-length text.
        .weigher(|_path, data: &FileTextData| {
            let bytes = u32::try_from(data.text.len()).unwrap_or(u32::MAX);
            bytes.saturating_add(1)
        })
        .eviction_policy(moka::policy::EvictionPolicy::lru())
        .build()
}

/// Create a new text data cache with the standard weight limit.
pub fn new_text_data_cache() -> TextDataCache {
    text_data_cache_with_max_weight(CACHE_MAX_WEIGHT)
}

/// Load file text data with LRU caching.
///
/// Caches texts keyed by absolute path, bounded by the total weight of all
/// cached texts (see [`text_data_cache_with_max_weight`]). Files larger than
/// `max_bytes` are rejected before being read, so oversized files are not
/// loaded into memory (indexing, search snippets, MCP tools, web UI).
pub async fn load_file_text_data(
    cache: &TextDataCache,
    file_path: &Path,
    max_bytes: u64,
) -> Result<FileTextData, AppError> {
    if let Some(cached) = cache.get(file_path) {
        return Ok(cached);
    }

    // Reject missing, non-regular and oversized files before reading them.
    let metadata = tokio::fs::metadata(file_path)
        .await
        .map_err(|e| AppError::from_io(e, file_path))?;
    if !metadata.is_file() {
        return Err(AppError::NotAFile {
            path: file_path.to_path_buf(),
        });
    }
    if metadata.len() > max_bytes {
        return Err(AppError::too_large(metadata.len(), max_bytes));
    }

    let data = FileTextData::load(file_path).await?;
    cache.insert(file_path.to_path_buf(), data.clone());
    Ok(data)
}

/// Detect the file format by extension (case-insensitive), if recognized.
pub fn file_format_by_file_name(file_path: &Path) -> Option<FileFormat> {
    // Lowercase so extensions like "TXT" or "HTML" match case-insensitively.
    let ext = file_path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "txt" => Some(FileFormat::Text),
        "md" => Some(FileFormat::Markdown),
        "rs" => Some(FileFormat::Rust),
        "py" => Some(FileFormat::Python),
        "htm" | "html" => Some(FileFormat::Html),
        "pdf" => Some(FileFormat::Pdf),
        "epub" => Some(FileFormat::Epub),
        "rst" | "rest" => Some(FileFormat::Rst),
        _ => None,
    }
}

impl FileTextData {
    /// Load file, detect format by extension, normalize content.
    ///
    /// HTML, PDF, EPUB and RST files are converted to Markdown; all others are read as-is.
    async fn load(file_path: &Path) -> Result<FileTextData, AppError> {
        let format = file_format_by_file_name(file_path)
            .ok_or_else(|| AppError::unsupported_format(file_path))?;
        let text = match format {
            FileFormat::Html => html::load_from_file_and_convert_to_md(file_path)
                .await
                .map_err(|e| AppError::Conversion {
                    path: file_path.to_path_buf(),
                    detail: e.to_string(),
                })?,
            FileFormat::Pdf => pdf::load_from_file_and_convert_to_md(file_path)
                .await
                .map_err(|e| AppError::Conversion {
                    path: file_path.to_path_buf(),
                    detail: e.to_string(),
                })?,
            FileFormat::Epub => epub::load_from_file_and_convert_to_md(file_path)
                .await
                .map_err(|e| AppError::Conversion {
                    path: file_path.to_path_buf(),
                    detail: e.to_string(),
                })?,
            FileFormat::Rst => rst::load_from_file_and_convert_to_md(file_path)
                .await
                .map_err(|e| AppError::Conversion {
                    path: file_path.to_path_buf(),
                    detail: e.to_string(),
                })?,
            // Plain text cannot fail to "convert"; errors are I/O only.
            _ => text::load_string_from_file(file_path)
                .await
                .map_err(|e| match e.downcast::<std::io::Error>() {
                    Ok(io) => AppError::from_io(io, file_path),
                    Err(anyhow_error) => AppError::Io {
                        path: file_path.to_path_buf(),
                        source: std::io::Error::other(anyhow_error.to_string()),
                    },
                })?,
        };
        Ok(FileTextData {
            format,
            text: Arc::new(text),
        })
    }

    /// Extract file structure (headings + line count) based on format.
    pub fn structure(&self) -> FileStructure {
        match self.format {
            FileFormat::Markdown
            | FileFormat::Html
            | FileFormat::Pdf
            | FileFormat::Epub
            | FileFormat::Rst => markdown::structure(&self.text),
            FileFormat::Text => text::structure(&self.text),
            FileFormat::Python => python::structure(&self.text),
            FileFormat::Rust => rust::structure(&self.text),
        }
    }
}
