use std::borrow::Cow;
use std::fmt::Write;

use itertools::Itertools;
use serde_json::{Value, json};

use super::jsonrpc::{Error, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use crate::AppState;
use crate::config::PathValidateResult;
use crate::search::{self, SearchParams};

/// Maximum byte length of a snippet shown in `docs_search` results.
const DOCS_SEARCH_SNIPPET_MAX_BYTES: usize = 200;

/// All available tools.
pub struct Tools;

impl Tools {
    /// Return the list of tools for `tools/list`.
    pub fn list() -> Vec<Value> {
        vec![
            Self::describe(
                "docs_search",
                "Search the built-in documentation index by keywords. Use as few keywords in query as possible. Good: `std::vector`, bad: `std::vector features examples how to use properly`. Results contain paths that exist ONLY within this documentation server — they are NOT local project files. To read a result you MUST use docs_get (never read_file). Workflow: docs_search → docs_headings → docs_get.",
                &[
                    ("query", "string", "Search query"),
                    (
                        "max_results",
                        "integer",
                        &format!(
                            "Maximum number of results (default: {}, max: {})",
                            crate::search::MCP_DEFAULT_MAX_RESULTS,
                            crate::search::MAX_RESULTS_LIMIT
                        ),
                    ),
                ],
                &["query"],
            ),
            Self::describe(
                "docs_headings",
                "Get the table of contents for a documentation page: all headings with their line ranges. Call this FIRST before docs_get — it tells you which section covers your topic and gives you exact start_line and end_line values to pass to docs_get so you read only the relevant part instead of dumping the whole file.",
                &[(
                    "path",
                    "string",
                    "Relative path from docs_search results (NOT a local file — you cannot use read_file on it). E.g. 'rust/book/src/ch01-02-hello-world.md'",
                )],
                &["path"],
            ),
            Self::describe(
                "docs_get",
                "Read content of a documentation page. This is the ONLY way to read documentation files found by docs_search — do NOT try to use read_file on those paths (they are not local project files). Text files (txt, md, rs, py, htm, html, pdf, epub, rst, rest) are returned as text — use start_line and end_line to read a specific section, get these values from docs_headings first; without line range, returns the beginning of the file only. Image files (png, jpg, jpeg, gif, webp, bmp, tif, tiff, ico, svg) are returned as image content. Images inside EPUB files are requested with a virtual path '<book.epub>/<path-as-written-in-the-document>', e.g. 'book.epub/images/pic.png'.",
                &[
                    (
                        "path",
                        "string",
                        "Relative path from docs_search or docs_headings results (NOT a local file path — use this tool, not read_file). For images inside an EPUB, use the virtual form '<book.epub>/<path-as-written-in-the-document>'.",
                    ),
                    (
                        "start_line",
                        "integer",
                        "Starting line number from docs_headings (1-based, optional)",
                    ),
                    (
                        "end_line",
                        "integer",
                        "Ending line number from docs_headings (1-based, optional, inclusive)",
                    ),
                ],
                &["path"],
            ),
        ]
    }

    fn describe(
        name: &str,
        description: &str,
        properties: &[(&str, &str, &str)],
        required: &[&str],
    ) -> Value {
        let props: Value = Value::Object(
            properties
                .iter()
                .map(|(pname, ptype, pdesc)| {
                    (
                        pname.to_string(),
                        json!({ "type": ptype, "description": pdesc }),
                    )
                })
                .collect(),
        );
        json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": props, "required": required },
        })
    }

    /// Execute a tool call and return the result as an MCP content array.
    pub async fn call(
        tool_name: &str,
        arguments: &Value,
        state: &AppState,
    ) -> Result<Value, Error> {
        match tool_name {
            "docs_search" => Self::handle_search(arguments, state).await,
            "docs_headings" => Self::handle_headings(arguments, state).await,
            "docs_get" => Self::handle_get(arguments, state).await,
            _ => Err(Error::new(
                METHOD_NOT_FOUND,
                format!("Unknown tool: {tool_name}"),
            )),
        }
    }

    async fn handle_search(args: &Value, state: &AppState) -> Result<Value, Error> {
        let query = extract_str(args, "query")?.trim();
        if query.is_empty() {
            return Ok(text_result("Search query is empty"));
        }
        // `search::search` clamps the limit to [1, MAX_RESULTS_LIMIT] itself.
        let max_results = extract_u64(args, "max_results")
            .map_or(search::MCP_DEFAULT_MAX_RESULTS, |v| {
                usize::try_from(v).unwrap_or(usize::MAX)
            });

        let search_params = SearchParams {
            q: query.to_string(),
            max_results,
            ..SearchParams::default()
        };

        // MCP output must stay plain text — no HTML markup in snippets.
        let response = crate::search::search(
            &state.reader,
            search_params,
            &state.config.directory,
            &state.text_data_cache,
            false,
        )
        .await
        .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

        let mut text = format!("Found {} result(s):\n\n", response.total);
        for (i, r) in response.results.iter().enumerate() {
            let _ = write!(
                text,
                "{}. {} (score: {:.3})\n   {}\n\n",
                i + 1,
                r.path,
                r.score,
                truncate(&r.snippet, DOCS_SEARCH_SNIPPET_MAX_BYTES),
            );
        }

        Ok(text_result(text))
    }

    async fn handle_headings(args: &Value, state: &AppState) -> Result<Value, Error> {
        let path = extract_str(args, "path")?;
        let resolved = validate_doc_path(path, state).await.map_err(mcp_error)?;

        let data = crate::formats::load_file_text_data(&state.text_data_cache, &resolved)
            .await
            .map_err(|e| mcp_error(format!("Failed to read file: {}", e)))?;
        let structure = data.structure();
        let mut out = format!("{} ({} lines)\n\n", path, structure.total_lines);
        for h in &structure.headers {
            let _ = writeln!(
                out,
                "{} {} [L{}-L{}]",
                "  ".repeat(h.level as usize - 1),
                h.text,
                h.start_line,
                h.end_line,
            );
        }

        Ok(text_result(out))
    }

    async fn handle_get(args: &Value, state: &AppState) -> Result<Value, Error> {
        let path = extract_str(args, "path")?;

        // Virtual archive path ("<file.epub>/<inner>") — serve an image
        // stored inside an EPUB; the inner path may be document-relative.
        // Commits only when the outer part is a regular file, so a
        // directory named `x.epub` still falls through to the on-disk flow.
        if let Some((epub_rel, inner)) = split_virtual_epub_path(path) {
            if is_regular_file(epub_rel, state).await {
                return Self::serve_epub_image(epub_rel, inner, state).await;
            }
        }

        let resolved = validate_doc_path(path, state).await.map_err(mcp_error)?;

        // Images skip text normalization and are returned as MCP image content.
        if image_mime_type(&resolved).is_some() {
            return Self::serve_image(&resolved, state).await;
        }

        let start_line = extract_u64(args, "start_line");
        let end_line = extract_u64(args, "end_line");

        // Load and normalize content via formats module.
        let data = crate::formats::load_file_text_data(&state.text_data_cache, &resolved)
            .await
            .map_err(|e| {
                tracing::error!("Failed to read file: {}", e);
                mcp_error(e.to_string())
            })?;

        let body = if start_line.is_some() || end_line.is_some() {
            // Explicit line range (1-based, inclusive) — slice the line iterator.
            let start = u64::max(start_line.unwrap_or(1), 1) as usize;
            let take = end_line.map_or(usize::MAX, |e| (e as usize).saturating_sub(start - 1));
            data.text.lines().skip(start - 1).take(take).join("\n")
        } else {
            // No range — auto-truncate if file is too long.
            let max_lines = crate::formats::AUTO_TRUNCATE_LINES;
            let mut lines = data.text.lines();
            // Take at most `max_lines` lines, then count the rest for the
            // truncation message — one pass over the text instead of two.
            let body = lines.by_ref().take(max_lines).join("\n");
            let remaining = lines.count();
            if remaining == 0 {
                body
            } else {
                let total_lines = max_lines + remaining;
                format!(
                    "--- FILE TRUNCATED: showing lines 1-{max_lines} of {total_lines}+ total. Use start_line and end_line parameters to read more. Example: /file?path={}&start_line={} ---\n\n{body}",
                    encode_url_component(path),
                    max_lines + 1,
                )
            }
        };

        Ok(text_result(format!("{path}\n\n{body}")))
    }

    /// Read an image file from disk and return it as MCP image content (base64).
    async fn serve_image(resolved: &std::path::Path, state: &AppState) -> Result<Value, Error> {
        // Size check before reading so oversized files are not loaded at all.
        let metadata = tokio::fs::metadata(resolved)
            .await
            .map_err(|e| mcp_error(format!("Failed to read file: {e}")))?;
        let max_bytes = state.config.max_file_size_bytes();
        if metadata.len() > max_bytes {
            return Err(mcp_error(format!(
                "File is too large ({} bytes, limit {} bytes)",
                metadata.len(),
                max_bytes
            )));
        }

        let bytes = tokio::fs::read(resolved)
            .await
            .map_err(|e| mcp_error(format!("Failed to read file: {e}")))?;
        image_content(&bytes, resolved, state)
    }

    /// Serve an image stored inside an EPUB archive (virtual path
    /// "<file.epub>/<inner>").
    async fn serve_epub_image(
        epub_rel: &str,
        inner: &str,
        state: &AppState,
    ) -> Result<Value, Error> {
        let resolved = validate_doc_path(epub_rel, state).await.map_err(mcp_error)?;

        let inner = inner.to_string();
        let max_bytes = state.config.max_file_size_bytes();
        let (name, bytes) = tokio::task::spawn_blocking(move || {
            crate::formats::read_image_entry(&resolved, &inner, max_bytes)
        })
        .await
        .map_err(|e| mcp_error(format!("Failed to read EPUB: {e}")))?
        .map_err(|e| mcp_error(e.to_string()))?;

        image_content(&bytes, std::path::Path::new(&name), state)
    }
}

// ---- Helpers ----

/// True if the relative path resolves to a regular file inside the watched directory.
async fn is_regular_file(rel_path: &str, state: &AppState) -> bool {
    let PathValidateResult::Valid(full) = state.config.validate_path(rel_path).await else {
        return false;
    };
    tokio::fs::metadata(&full).await.is_ok_and(|m| m.is_file())
}

/// Validate a docs tool path argument.
async fn validate_doc_path(path: &str, state: &AppState) -> Result<std::path::PathBuf, String> {
    if path.is_empty() {
        return Err("Path is required".into());
    }
    match state.config.validate_path(path).await {
        PathValidateResult::Valid(p) => Ok(p),
        PathValidateResult::OutsideDirectory => {
            Err("File path is outside the watched directory".into())
        }
        PathValidateResult::NotFound => Err(format!("File not found: {}", path)),
    }
}

fn extract_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, Error> {
    args.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        Error::new(
            INVALID_PARAMS,
            format!("Missing or invalid parameter: '{key}'"),
        )
    })
}

fn extract_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

fn mcp_error(msg: String) -> Error {
    Error::new(INTERNAL_ERROR, msg)
}

fn text_result(text: impl Into<String>) -> Value {
    let text = text.into();
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn image_result(data: &str, mime_type: &str) -> Value {
    json!({
        "content": [{ "type": "image", "data": data, "mimeType": mime_type }],
        "isError": false,
    })
}

/// Shared tail of the image-serving paths: MIME check by extension, size
/// limit, base64 encoding, MCP image content.
fn image_content(
    bytes: &[u8],
    file_path: &std::path::Path,
    state: &AppState,
) -> Result<Value, Error> {
    let mime = image_mime_type(file_path).ok_or_else(|| {
        mcp_error(format!(
            "Unsupported image format: {}",
            file_path.display()
        ))
    })?;
    let max_bytes = state.config.max_file_size_bytes();
    if bytes.len() as u64 > max_bytes {
        return Err(mcp_error(format!(
            "File is too large ({} bytes, limit {} bytes)",
            bytes.len(),
            max_bytes
        )));
    }
    let data = base64::engine::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    Ok(image_result(&data, mime))
}

/// Split a virtual archive path "<file.epub>/<inner>" into its parts.
/// The outer part is the leftmost prefix ending in an .epub component;
/// the inner part must be non-empty. Absolute paths are not virtual.
fn split_virtual_epub_path(path: &str) -> Option<(&str, &str)> {
    if path.starts_with('/') {
        return None;
    }
    // Leftmost '/' such that the outer part ends in an .epub component
    // and the inner part is non-empty.
    for (i, c) in path.char_indices() {
        if c != '/' {
            continue;
        }
        let (outer, inner) = path.split_at(i);
        let inner = &inner[1..];
        if inner.is_empty() {
            return None;
        }
        if ends_with_epub(outer) {
            return Some((outer, inner));
        }
    }
    None
}

fn ends_with_epub(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
}

/// MIME type for supported image extensions (case-insensitive), or `None`
/// if the path is not a supported image format.
fn image_mime_type(file_path: &std::path::Path) -> Option<&'static str> {
    let ext = file_path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!(
            "{}...",
            crate::utils::truncate_bytes(s.as_bytes(), max)
        ))
    }
}

/// Percent-encode a string for embedding in a URL query parameter value.
fn encode_url_component(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(b as char);
        } else {
            let _ = write!(encoded, "%{:02X}", b);
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_tools_list_returns_three_tools() {
        let tools = Tools::list();
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name")?.as_str())
            .collect();
        assert!(names.contains(&"docs_search"));
        assert!(names.contains(&"docs_headings"));
        assert!(names.contains(&"docs_get"));
    }

    #[test]
    fn test_extract_str_valid() {
        let args = json!({"query": "hello"});
        let result = extract_str(&args, "query").unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_extract_str_missing() {
        let args = json!({"other": "world"});
        let result = extract_str(&args, "query");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_u64_valid() {
        let args = json!({"max_results": 42});
        assert_eq!(extract_u64(&args, "max_results"), Some(42));
    }

    #[test]
    fn test_extract_u64_missing() {
        let args = json!({"max_results": "not_a_number"});
        assert_eq!(extract_u64(&args, "max_results"), None);
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn test_truncate_cyrillic_boundary() {
        // 'м' is 2 bytes in UTF-8, starts at byte 6
        // max=6 is exactly at char boundary → "Hello ..."
        assert_eq!(truncate("Hello мир", 6), "Hello ...");
    }

    #[test]
    fn test_truncate_emoji_boundary() {
        // '🎉' is 4 bytes in UTF-8, starts at byte 6
        // max=7 is inside emoji → cutoff falls back to 6
        assert_eq!(truncate("Hello 🎉!", 7), "Hello ...");
    }

    #[test]
    fn test_truncate_all_multi_byte() {
        // 'м'=bytes 0-1, 'и'=bytes 2-3, 'р'=bytes 4-5
        // max=3 is inside 'и' → cutoff falls back to 2 (start of 'и')
        assert_eq!(truncate("мир", 3), "м...");
    }

    #[test]
    fn test_image_mime_type_supported() {
        use std::path::Path;
        assert_eq!(image_mime_type(Path::new("a.png")).unwrap(), "image/png");
        assert_eq!(image_mime_type(Path::new("a.jpg")).unwrap(), "image/jpeg");
        assert_eq!(
            image_mime_type(Path::new("a.jpeg")).unwrap(),
            "image/jpeg"
        );
        assert_eq!(
            image_mime_type(Path::new("a.gif")).unwrap(),
            "image/gif"
        );
        assert_eq!(
            image_mime_type(Path::new("a.webp")).unwrap(),
            "image/webp"
        );
        assert_eq!(image_mime_type(Path::new("a.bmp")).unwrap(), "image/bmp");
        assert_eq!(
            image_mime_type(Path::new("a.tiff")).unwrap(),
            "image/tiff"
        );
        assert_eq!(image_mime_type(Path::new("a.tif")).unwrap(), "image/tiff");
        assert_eq!(
            image_mime_type(Path::new("a.ico")).unwrap(),
            "image/x-icon"
        );
        assert_eq!(
            image_mime_type(Path::new("a.svg")).unwrap(),
            "image/svg+xml"
        );
        // Extensions are case-insensitive
        assert_eq!(
            image_mime_type(Path::new("a.PnG")).unwrap(),
            "image/png"
        );
    }

    #[test]
    fn test_image_mime_type_unsupported() {
        use std::path::Path;
        assert!(image_mime_type(Path::new("a.txt")).is_none());
        assert!(image_mime_type(Path::new("a.rs")).is_none());
        assert!(image_mime_type(Path::new("a")).is_none());
    }

    #[test]
    fn test_split_virtual_epub_path() {
        assert_eq!(
            split_virtual_epub_path("book.epub/images/pic.png"),
            Some(("book.epub", "images/pic.png"))
        );
        assert_eq!(
            split_virtual_epub_path("docs/book.EPUB/a.png"),
            Some(("docs/book.EPUB", "a.png"))
        );
        // Not virtual: no inner part, empty inner, wrong extension, no slash.
        assert_eq!(split_virtual_epub_path("book.epub"), None);
        assert_eq!(split_virtual_epub_path("book.epub/"), None);
        assert_eq!(split_virtual_epub_path("book.md/a.png"), None);
        assert_eq!(split_virtual_epub_path("/book.epub/a.png"), None);
        assert_eq!(split_virtual_epub_path("plain.png"), None);
    }
}
