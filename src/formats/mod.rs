use std::path::{Path, PathBuf};
use std::sync::Arc;

use moka::sync::Cache;
use serde::Serialize;

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

const CACHE_MAX_SIZE: u64 = 1024;

/// LRU cache of normalized file text. The canonical handle lives in `AppState`;
/// `IndexWriterWrapper` holds a clone sharing the same underlying store.
pub type TextDataCache = Cache<PathBuf, FileTextData>;

/// Create a new text data cache with the standard capacity.
pub fn new_text_data_cache() -> TextDataCache {
    Cache::new(CACHE_MAX_SIZE)
}

/// Load file text data with LRU caching.
///
/// Caches up to `CACHE_MAX_SIZE` entries keyed by absolute path.
pub async fn load_file_text_data(
    cache: &TextDataCache,
    file_path: &Path,
) -> anyhow::Result<FileTextData> {
    if let Some(cached) = cache.get(file_path) {
        return Ok(cached);
    }

    let data = FileTextData::load(file_path).await?;
    cache.insert(file_path.to_path_buf(), data.clone());
    Ok(data)
}

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
    async fn load(file_path: &Path) -> anyhow::Result<FileTextData> {
        let format = file_format_by_file_name(file_path)
            .ok_or_else(|| anyhow::anyhow!("Unsupported format"))?;
        let text = match format {
            FileFormat::Html => html::load_from_file_and_convert_to_md(file_path).await?,
            FileFormat::Pdf => pdf::load_from_file_and_convert_to_md(file_path).await?,
            FileFormat::Epub => epub::load_from_file_and_convert_to_md(file_path).await?,
            FileFormat::Rst => rst::load_from_file_and_convert_to_md(file_path).await?,
            _ => text::load_string_from_file(file_path).await?,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_dir() -> PathBuf {
        crate::testutil::unique_temp_dir("formats_test")
    }

    #[tokio::test]
    async fn test_invalidate_text_data_cache() {
        let file = make_temp_dir().join("cache_entry.txt");
        std::fs::write(&file, "version one").unwrap();
        let cache = new_text_data_cache();

        // Warm the cache.
        let first = load_file_text_data(&cache, &file).await.unwrap();
        assert_eq!(first.text.as_str(), "version one");

        // Modify the file on disk; the cache entry is now stale.
        std::fs::write(&file, "version two").unwrap();

        // Without invalidation the cache still serves the stale copy.
        let stale = load_file_text_data(&cache, &file).await.unwrap();
        assert_eq!(stale.text.as_str(), "version one");

        cache.invalidate(&file);

        // After invalidation fresh content is read from disk.
        let fresh = load_file_text_data(&cache, &file).await.unwrap();
        assert_eq!(fresh.text.as_str(), "version two");
    }

    #[tokio::test]
    async fn test_rst_load_and_structure() {
        let file = make_temp_dir().join("doc.rst");
        std::fs::write(
            &file,
            "Title\n=====\n\nBody text.\n\nSub Section\n-----------\n\nMore text.\n",
        )
        .unwrap();
        let cache = new_text_data_cache();

        let data = load_file_text_data(&cache, &file).await.unwrap();
        assert!(matches!(data.format, FileFormat::Rst));
        assert!(data.text.contains("# Title"));
        assert!(data.text.contains("## Sub Section"));

        let structure = data.structure();
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].text, "Title");
        assert_eq!(structure.headers[0].level, 1);
        assert_eq!(structure.headers[1].text, "Sub Section");
        assert_eq!(structure.headers[1].level, 2);
    }
}
