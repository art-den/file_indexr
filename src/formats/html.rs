use std::path::Path;

/// Load HTML file and convert to Markdown.
pub async fn load_from_file_and_convert_to_md(file_name: &Path) -> anyhow::Result<String> {
    let path = file_name.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let buffer = std::fs::read(&path)?;
        to_markdown(&String::from_utf8_lossy(&buffer))
    })
    .await?
}

/// Convert an HTML document to Markdown.
pub(crate) fn to_markdown(html: &str) -> anyhow::Result<String> {
    Ok(html_to_markdown_rs::convert(html, None)?
        .content
        .unwrap_or_default())
}
