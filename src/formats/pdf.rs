use std::path::Path;

/// Load PDF file and convert to Markdown.
pub async fn load_string_from_file(file_path: &Path) -> anyhow::Result<String> {
    let path = file_path.to_path_buf();
    Ok(tokio::task::spawn_blocking(move || {
        let options = unpdf::RenderOptions::new()
            .with_frontmatter(true)
            .with_line_breaks(true)
            .with_cleanup_preset(unpdf::CleanupPreset::Aggressive);
        unpdf::to_markdown_with_options(path, &options)
    })
    .await??)
}
