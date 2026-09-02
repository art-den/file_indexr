use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// Punctuation characters allowed for section title underlines/overlines.
const UNDERLINE: &[char] = &[
    '=', '-', '`', ':', '"', '~', '^', '_', '*', '#', '+', '/', '\\', '|', '.', '%', '?', '!',
];

/// Admonition directives rendered as a Markdown blockquote with a bold label.
const ADMONITIONS: &[&str] = &[
    "note",
    "warning",
    "tip",
    "important",
    "caution",
    "danger",
    "attention",
    "hint",
    "admonition",
    "seealso",
];

/// Markdown fenced-code delimiter.
const CODE_FENCE: &str = "```";

static RE_ROLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#":[\w-]+:`([^`]+)`"#).unwrap());
static RE_EXT_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(\S+)\s*<(https?://[^>\s]+)>\s*_*"#).unwrap());
static RE_SUB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\|([\w.-]+)\|"#).unwrap());
static RE_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([A-Za-z0-9_]+)(_{1,2})(\s|$)"#).unwrap());
static RE_DBL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"``([^`]+)``").unwrap());
static RE_ESC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\([`*=_\\|])").unwrap());

/// Load an RST file and convert it to Markdown.
pub async fn load_from_file_and_convert_to_md(file_name: &Path) -> anyhow::Result<String> {
    let path = file_name.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let buffer = std::fs::read(&path)?;
        Ok(to_markdown(&String::from_utf8_lossy(&buffer)))
    })
    .await?
}

/// Convert a reStructuredText document to Markdown (best-effort).
pub fn to_markdown(rst: &str) -> String {
    let lines: Vec<&str> = rst.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut section_levels: Vec<char> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Blank line.
        if line.trim().is_empty() {
            out.push(String::new());
            i += 1;
            continue;
        }

        // Section title: overlined (overline + text + underline) form.
        if let Some((ch, _)) = punct(line) {
            if i + 2 < lines.len()
                && !lines[i + 1].trim().is_empty()
                && matches!(punct(lines[i + 2]), Some((c, _)) if c == ch)
            {
                emit_title(&mut out, lines[i + 1].trim(), ch, &mut section_levels);
                i += 3;
                continue;
            }
            // Not a title: a standalone run of punctuation is a transition.
            out.push(String::new());
            i += 1;
            continue;
        }

        // Section title: underlined form (text + underline).
        if !is_indented(line) && i + 1 < lines.len() {
            if let Some((ch, _)) = punct(lines[i + 1]) {
                emit_title(&mut out, line.trim(), ch, &mut section_levels);
                i += 2;
                continue;
            }
        }

        // Directive or comment (line starting with "..").
        if line.trim_start().starts_with("..") {
            i = handle_directive(&lines, i, &mut out);
            continue;
        }

        // List item (bullet or enumerated) at any indentation.
        if let Some((ordered, number, content)) = parse_list_item(line) {
            let indent = leading_ws(line);
            let prefix = if ordered {
                format!("{}. ", number)
            } else {
                "- ".to_string()
            };
            out.push(format!(
                "{}{}{}",
                indent,
                prefix,
                convert_inline(content.trim())
            ));
            i += 1;
            continue;
        }

        // Indented block: block quote (preceded by a blank line) or literal code block.
        if is_indented(line) {
            let (block, next) = consume_indented(&lines, i);
            let prev = out.last().map(|s| s.trim()).unwrap_or("");
            if prev.is_empty() {
                emit_blockquote(&mut out, &block);
            } else {
                emit_code_fence(&mut out, "", &block);
            }
            i = next;
            continue;
        }

        // Default: paragraph line.
        out.push(convert_inline(line.trim_end()));
        i += 1;
    }

    while out.last().is_some_and(|s| s.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Handle a `..` directive/comment line. Returns the next index to process.
fn handle_directive(lines: &[&str], i: usize, out: &mut Vec<String>) -> usize {
    let rest = lines[i].trim_start().strip_prefix("..").unwrap();
    let trimmed = rest.trim_start();

    // ".." alone or a plain comment (no "::"): drop this line and any
    // following indented continuation lines.
    let (name, arg) = match split_directive(trimmed) {
        Some(v) => v,
        None => {
            let (_, next) = consume_indented(lines, i + 1);
            return next.max(i + 1);
        }
    };

    let name_lower = name.to_ascii_lowercase();

    // Hyperlink target: ".. _name: url".
    if trimmed.starts_with('_') {
        return i + 1;
    }

    // Code / source directives: fence the following indented block.
    if matches!(name_lower.as_str(), "code-block" | "code" | "sourcecode") {
        let (block, next) = consume_indented(lines, i + 1);
        emit_code_fence(out, arg.trim(), &block);
        return next;
    }

    // Admonitions: emit a blockquote with a bold label.
    if ADMONITIONS.contains(&name_lower.as_str()) {
        out.push(format!("> **{}:**", title_case(&name_lower)));
        let (block, next) = consume_indented(lines, i + 1);
        emit_blockquote(out, &block);
        return next;
    }

    // Other directive: skip the marker line; any indented body is handled
    // normally on its next pass.
    i + 1
}

/// Split a directive body into (name, argument) on the first "::".
fn split_directive(s: &str) -> Option<(&str, &str)> {
    let idx = s.find("::")?;
    let name = s[..idx].trim();
    if name.is_empty() {
        return None;
    }
    Some((name, &s[idx + 2..]))
}

/// Collect the run of indented lines starting at `start` and dedent them.
/// Blank lines are kept only while a later indented line follows them.
fn consume_indented(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut j = start;
    let mut block: Vec<&str> = Vec::new();
    while j < lines.len() {
        let l = lines[j];
        if l.trim().is_empty() {
            let mut k = j + 1;
            while k < lines.len() && lines[k].trim().is_empty() {
                k += 1;
            }
            if k < lines.len() && is_indented(lines[k]) {
                block.push(l);
                j += 1;
            } else {
                break;
            }
        } else if is_indented(l) {
            block.push(l);
            j += 1;
        } else {
            break;
        }
    }
    (dedent(&block), j)
}

/// Emit an indented block as a Markdown blockquote.
fn emit_blockquote(out: &mut Vec<String>, block: &[String]) {
    for l in block {
        if l.is_empty() {
            out.push(">".to_string());
        } else {
            out.push(format!("> {}", convert_inline(l)));
        }
    }
}

/// Emit an indented block as a fenced code block.
fn emit_code_fence(out: &mut Vec<String>, lang: &str, block: &[String]) {
    out.push(format!("{}{}", CODE_FENCE, lang));
    out.extend(block.iter().cloned());
    out.push(CODE_FENCE.to_string());
}

/// Strip the common leading indentation from a block of lines.
fn dedent(block: &[&str]) -> Vec<String> {
    let min = block
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    block
        .iter()
        .map(|l| {
            let lead = l.len() - l.trim_start_matches([' ', '\t']).len();
            let skip = min.min(lead);
            l[skip..].to_string()
        })
        .collect()
}

/// Emit an ATX heading, tracking underline characters to derive the level.
fn emit_title(out: &mut Vec<String>, text: &str, ch: char, levels: &mut Vec<char>) {
    let level = match levels.iter().position(|c| *c == ch) {
        Some(pos) => pos + 1,
        None => {
            levels.push(ch);
            levels.len()
        }
    };
    let hashes: String = std::iter::repeat_n('#', usize::min(level, 6)).collect();
    out.push(format!("{} {}", hashes, convert_inline(text.trim())));
}

/// Return (char, length) if `line` is an unindented run of a single underline char.
fn punct(line: &str) -> Option<(char, usize)> {
    if is_indented(line) {
        return None;
    }
    let t = line.trim_end_matches([' ', '\t']);
    let mut chars = t.chars();
    let first = chars.next()?;
    if !UNDERLINE.contains(&first) {
        return None;
    }
    if chars.any(|c| c != first) {
        return None;
    }
    Some((first, t.len()))
}

/// Parse a bullet or enumerated list item. Returns (is_ordered, number, content).
fn parse_list_item(line: &str) -> Option<(bool, usize, &str)> {
    let t = line.trim_start();

    for bullet in ["- ", "+ ", "* "] {
        if let Some(rest) = t.strip_prefix(bullet) {
            return Some((false, 0, rest));
        }
    }

    let mut chars = t.chars().peekable();
    match chars.peek() {
        Some(c) if c.is_ascii_digit() => {
            let digit_count = t.chars().take_while(|c| c.is_ascii_digit()).count();
            // Digits and the terminator are ASCII, so the content starts at
            // byte offset `digit_count + 1` (the terminator is a single byte).
            if matches!(t.chars().nth(digit_count), Some('.') | Some(')')) {
                let rest = &t[digit_count + 1..];
                if rest.starts_with(' ') || rest.starts_with('\t') {
                    let num: usize = t
                        .chars()
                        .take(digit_count)
                        .collect::<String>()
                        .parse()
                        .unwrap_or(1);
                    return Some((true, num, rest));
                }
            }
            None
        }
        Some('#') => {
            // Auto-numbered enumerated list: "#." or "#)".
            if matches!(t.chars().nth(1), Some('.') | Some(')')) {
                let rest = &t[2..];
                if rest.starts_with(' ') || rest.starts_with('\t') {
                    return Some((true, 1, rest));
                }
            }
            None
        }
        _ => None,
    }
}

/// Convert RST inline markup to Markdown.
fn convert_inline(s: &str) -> String {
    let mut out = s.to_string();
    out = RE_ROLE.replace_all(&out, "$1").to_string();
    out = RE_EXT_LINK.replace_all(&out, "[$1]($2)").to_string();
    out = RE_SUB.replace_all(&out, "$1").to_string();
    out = RE_REF
        .replace_all(&out, |caps: &regex::Captures| {
            let word = &caps[1];
            // A token that already contains an underscore is more likely an
            // identifier (e.g. `self._count_`) than a cross-reference, so keep
            // its trailing underscore; otherwise drop the reference marker.
            if word.contains('_') {
                format!("{}{}{}", word, &caps[2], &caps[3])
            } else {
                format!("{}{}", word, &caps[3])
            }
        })
        .to_string();
    out = RE_DBL.replace_all(&out, "`$1`").to_string();
    out = RE_ESC.replace_all(&out, "$1").to_string();
    out
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    let first = chars
        .next()
        .map(|f| f.to_uppercase().to_string())
        .unwrap_or_default();
    format!("{}{}", first, chars.as_str())
}

fn is_indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

fn leading_ws(line: &str) -> String {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_section_titles_become_atx_headings() {
        let rst = "Top Title\n=========\n\nBody text\n\nSection One\n-----------\n\nMore body\n";
        let md = to_markdown(rst);
        assert!(md.starts_with("# Top Title\n"));
        assert!(md.contains("## Section One"));
        assert!(md.contains("Body text"));
    }

    #[test]
    fn test_overlined_title() {
        let rst = "======\nTitle\n======\n\npara\n";
        let md = to_markdown(rst);
        assert!(md.starts_with("# Title\n"));
    }

    #[test]
    fn test_nested_sections() {
        let rst = "A\n=\n\nB\n-\n\nC\n^\n\ntext\n";
        let md = to_markdown(rst);
        assert!(md.contains("# A"));
        assert!(md.contains("## B"));
        assert!(md.contains("### C"));
    }

    #[test]
    fn test_bullet_list() {
        let rst = "Intro\n\n- one\n- two\n- three\n";
        let md = to_markdown(rst);
        assert!(md.contains("- one"));
        assert!(md.contains("- three"));
    }

    #[test]
    fn test_enumerated_list() {
        let rst = "1. first\n2. second\n3. third\n";
        let md = to_markdown(rst);
        assert!(md.contains("1. first"));
        assert!(md.contains("3. third"));
    }

    #[test]
    fn test_code_block_directive() {
        let rst = ".. code-block:: python\n\n    def f():\n        return 1\n\nAfter\n";
        let md = to_markdown(rst);
        assert!(md.contains("```python"));
        assert!(md.contains("def f():"));
        assert!(md.contains("After"));
    }

    #[test]
    fn test_admonition() {
        let rst = ".. note::\n\n    Something important.\n";
        let md = to_markdown(rst);
        assert!(md.contains("> **Note:**"));
        assert!(md.contains("> Something important."));
    }

    #[test]
    fn test_comment_is_dropped() {
        let rst = ".. this is a comment\n\nVisible text\n";
        let md = to_markdown(rst);
        assert!(!md.contains("comment"));
        assert!(md.contains("Visible text"));
    }

    #[test]
    fn test_hyperlink_target_dropped() {
        let rst = ".. _myref: https://example.com\n\nSee myref_ for more.\n";
        let md = to_markdown(rst);
        assert!(!md.contains("example.com"));
        assert!(md.contains("See myref for more."));
    }

    #[test]
    fn test_inline_external_link() {
        let rst = "Visit the <https://example.com> site.\n";
        let md = to_markdown(rst);
        assert!(md.contains("[the](https://example.com)"));
    }

    #[test]
    fn test_reference_marker_stripped_from_plain_word() {
        let md = to_markdown("See intro_ for detail.\n");
        assert!(md.contains("See intro for detail."));
    }

    #[test]
    fn test_trailing_underscore_kept_on_snake_case_identifier() {
        let md = to_markdown("The flag is my_var_ here.\n");
        assert!(md.contains("my_var_"));
    }

    #[test]
    fn test_inline_emphasis_and_code() {
        let rst = "Some *emphasis* and **strong** and `code` here.\n";
        let md = to_markdown(rst);
        assert!(md.contains("*emphasis*"));
        assert!(md.contains("**strong**"));
        assert!(md.contains("`code`"));
    }

    #[test]
    fn test_block_quote() {
        let rst = "Intro para.\n\n    Quoted line.\n    More quote.\n";
        let md = to_markdown(rst);
        assert!(md.contains("> Quoted line."));
        assert!(md.contains("> More quote."));
    }

    #[test]
    fn test_literal_block_after_text() {
        let rst = "Run this command:\n    echo hello\n    echo world\n";
        let md = to_markdown(rst);
        assert!(md.contains("```"));
        assert!(md.contains("echo hello"));
    }

    #[test]
    fn test_empty_input() {
        assert_eq!(to_markdown(""), "");
    }

    #[test]
    fn test_transition_becomes_blank() {
        let rst = "First part\n\n----\n\nSecond part\n";
        let md = to_markdown(rst);
        assert!(md.contains("First part"));
        assert!(md.contains("Second part"));
        assert!(!md.contains("----"));
    }
}
