use std::path::Path;

/// Load HTML file and convert to Markdown.
pub async fn load_string_from_file(file_name: &Path) -> anyhow::Result<String> {
    let path = file_name.to_path_buf();
    Ok(tokio::task::spawn_blocking(move || {
        let buffer = std::fs::read(&path)?;
        to_markdown(&String::from_utf8_lossy(&buffer))
    })
    .await??)
}

/// Convert an HTML document to Markdown.
pub(crate) fn to_markdown(html: &str) -> Result<String, html_to_markdown_rs::ConversionError> {
    Ok(html_to_markdown_rs::convert(html, None)?
        .content
        .unwrap_or_default())
}
