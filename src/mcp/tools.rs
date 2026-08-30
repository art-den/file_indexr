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
                "Read content of a documentation page. This is the ONLY way to read documentation files found by docs_search — do NOT try to use read_file on those paths (they are not local project files). Use start_line and end_line to read a specific section — get these values from docs_headings first. Without line range, returns the beginning of the file only.",
                &[
                    (
                        "path",
                        "string",
                        "Relative path from docs_search or docs_headings results (NOT a local file path — use this tool, not read_file)",
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
        let resolved = validate_doc_path(path, state).await.map_err(mcp_error)?;

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
}

// ---- Helpers ----

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
}
