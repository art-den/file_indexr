use std::path::Path;

use crate::formats::FileStructure;

pub async fn load_string_from_file(file_name: &Path) -> anyhow::Result<String> {
    let buffer = tokio::fs::read(file_name).await?;
    // Valid UTF-8 reuses the buffer directly; only the rare invalid-UTF-8 case falls back to lossy conversion.
    Ok(String::from_utf8(buffer)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

pub fn structure(text: &str) -> FileStructure {
    FileStructure {
        headers: Vec::new(),
        total_lines: text.lines().count(),
    }
}
