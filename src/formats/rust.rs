use std::borrow::Cow;
use std::sync::LazyLock;

use itertools::Itertools;

use crate::formats::{FileStructure, markdown};

#[cfg(test)]
#[path = "tests/tests_rust.rs"]
mod tests;

/// Kind of Rust item, used to compute final level from nesting context.
#[derive(PartialEq, Clone, Copy)]
enum ItemKind {
    Mod,
    Struct,
    Trait,
    Imp,
    Fn,
}

/// Stack entry tracking an item's opening brace depth.
struct StackEntry {
    kind: ItemKind,
    braces: usize,
}

/// Parse Rust source for fn/struct/trait/impl/mod headings.
///
/// Uses brace depth to determine nesting:
/// - `mod` at depth 0 → level 1
/// - `struct` / `trait` at depth 0 → level 2
/// - `fn` at depth 0 (free function) → level 2
/// - `impl` at depth 0 → level 2
/// - `fn` inside `impl` (method) → level 3
/// - nested `fn` inside another `fn` → level 3+
pub fn structure(text: &str) -> FileStructure {
    let mut raw_headings = Vec::new();
    let mut stack: Vec<StackEntry> = Vec::new();
    let mut braces: usize = 0;
    let mut in_block_comment = false;
    // A brace-less item (its `{` is on a later line) awaiting its block open.
    let mut pending: Option<(ItemKind, usize)> = None;
    let mut total_lines = 0;

    for line in text.lines() {
        total_lines += 1;
        // A line starting inside a block comment cannot contain an item before
        // the comment ends.
        let line_for_item = if in_block_comment {
            line.find("*/").map_or("", |end| &line[end + 2..])
        } else {
            line
        };

        let pre_braces = braces;
        let (delta, block_comment) = count_brace_delta(line, in_block_comment);
        braces = usize::saturating_add_signed(braces, delta);
        in_block_comment = block_comment;

        if let Some((item_kind, name)) = try_parse_rust_item(line_for_item.trim()) {
            // A new item line means the previous brace-less item was never opened.
            pending = None;

            let level = compute_level(&stack, item_kind);
            raw_headings.push((total_lines, level, name));

            // Only push onto stack if the block opens on this line (more `{` than `}`).
            // Single-line blocks (`fn foo() {}`) still appear in headings but don't
            // contribute to nesting context.
            if braces > pre_braces {
                stack.push(StackEntry {
                    kind: item_kind,
                    braces: pre_braces,
                });
            } else if !line.contains('{') {
                // Brace-less item (no `{` on this line at all): remember it so the
                // block is registered when the `{` appears on a later line. A line
                // with `{` opened and closed its block inline — nothing to wait for.
                pending = Some((item_kind, pre_braces));
            }
        } else {
            // Pop closed blocks.
            while stack.last().is_some_and(|top| braces <= top.braces) {
                stack.pop();
            }
            // Register a brace-less item whose block opens on a later line.
            if let Some((kind, depth)) = pending {
                if braces > depth {
                    stack.push(StackEntry {
                        kind,
                        braces: depth,
                    });
                    pending = None;
                } else if braces < depth {
                    // Enclosing context closed — the block never opened.
                    pending = None;
                }
            }
        }
    }

    let headings = markdown::build_heading_items(raw_headings, total_lines);

    FileStructure {
        headers: headings,
        total_lines,
    }
}

/// Count `{` / `}` on a line, ignoring `//` and `/* … */` comments, strings,
/// and char literals.
///
/// `in_block_comment` carries the block-comment state from the previous line
/// (this function is line-scoped, so the caller tracks multi-line comments).
/// Returns the delta and whether the line ends inside a block comment.
///
/// Apostrophes are disambiguated: a `'` followed by an identifier that is not
/// closed by a second `'` is a lifetime/label, not a char literal. The
/// identifier must be ASCII — a non-ASCII lifetime (e.g. `impl<'é>`) is
/// misread as an unterminated char literal for the rest of the line.
///
/// Line-scoped by design: raw strings `r#"..."#` are not tracked across lines.
fn count_brace_delta(line: &str, in_block_comment: bool) -> (isize, bool) {
    let mut delta: isize = 0;
    // Closing quote of the string/char literal being scanned, if any.
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let chars = line.as_bytes();
    // A block comment started on an earlier line: resume after its `*/`, or
    // bail out if the rest of the line is still comment.
    let mut i = if in_block_comment {
        match line.find("*/") {
            Some(end) => end + 2,
            None => return (0, true),
        }
    } else {
        0
    };

    while i < chars.len() {
        let ch = chars[i];
        if let Some(close) = quote {
            if escaped {
                escaped = false;
            } else if ch == b'\\' {
                // Escape parity: `'\\'` and `"a\\"` must close on the final quote.
                escaped = true;
            } else if ch == close {
                quote = None;
            }
            i += 1;
            continue;
        }
        if ch == b'/' && i + 1 < chars.len() {
            match chars[i + 1] {
                // `//` comments run to end of line — nothing left to count.
                b'/' => break,
                // Block comment: jump straight to its closing `*/`.
                b'*' => {
                    i += 2;
                    match line[i..].find("*/") {
                        Some(end) => i += end + 2,
                        None => return (delta, true),
                    }
                    continue;
                }
                _ => {}
            }
        }
        if ch == b'"' {
            quote = Some(b'"');
            i += 1;
            continue;
        }
        if ch == b'\'' {
            // If the apostrophe starts an identifier, it is a char literal
            // only when that identifier is closed by a second apostrophe.
            let mut j = i + 1;
            if j < chars.len() && (chars[j].is_ascii_alphabetic() || chars[j] == b'_') {
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == b'_') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == b'\'' {
                    // Char literal like `'a'` — skip it entirely.
                    i = j + 1;
                } else {
                    // Lifetime/label like `'a` — not a literal, keep scanning.
                    i += 1;
                }
            } else {
                // Char literal with non-identifier content (e.g. `{`, `!`, digits).
                quote = Some(b'\'');
                i += 1;
            }
            continue;
        }
        match ch {
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    (delta, false)
}

/// Compute heading level based on stack context and item kind.
fn compute_level(stack: &[StackEntry], kind: ItemKind) -> u8 {
    match kind {
        ItemKind::Mod => 1,
        ItemKind::Struct | ItemKind::Trait => {
            // Each parent struct/trait pushes level deeper.
            let st_count = stack
                .iter()
                .filter(|e| matches!(e.kind, ItemKind::Struct | ItemKind::Trait))
                .count();
            (st_count + 2) as u8
        }
        ItemKind::Imp => 2,
        ItemKind::Fn => {
            // Free fn at depth 0 → 2.
            // Method inside impl/trait/struct → 3.
            // Nested fn → 3+.
            let has_container = stack
                .iter()
                .any(|e| matches!(e.kind, ItemKind::Imp | ItemKind::Trait | ItemKind::Struct));
            let fn_count = stack.iter().filter(|e| e.kind == ItemKind::Fn).count();
            let base = if has_container { 3 } else { 2 };
            (base + fn_count) as u8
        }
    }
}

/// Normalize heading text: strip braces, `//` comments, trailing params for display.
fn normalize_heading_text(text: &str) -> String {
    // Drop param lists (everything after `(`, keeping generics) and `//` comments.
    let text = text.split_once('(').map_or(text, |(before, _)| before);
    let text = text.split_once("//").map_or(text, |(before, _)| before);
    let text = text.trim();

    // Remove any braces left, e.g. a trailing `{`.
    // `str::replace` allocates even when nothing matches, so skip it if brace-free.
    if text.contains(['{', '}']) {
        text.replace(['{', '}'], "").trim().to_string()
    } else {
        text.to_string()
    }
}

/// Normalise whitespace: collapse runs of spaces/tabs into a single space.
/// Borrows the input when it needs no normalisation.
fn normalise(s: &str) -> Cow<'_, str> {
    // split_whitespace splits on every Unicode whitespace, not just space/tab,
    // so the borrow fast path must trigger only when no such char is present.
    if s.contains("  ") || s.chars().any(|c| c.is_whitespace() && c != ' ') {
        Cow::Owned(s.split_whitespace().join(" "))
    } else {
        Cow::Borrowed(s)
    }
}

/// Regex that matches any combination of Rust item modifiers at start of line.
/// The parenthesized-visibility alternative must come before the bare `pub\s+`
/// so that `pub (crate)` (space before the paren) is consumed whole.
static MOD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"^(?:(?:pub\s*\(\s*(?:in\s+[\w:]+|crate|super|self)\s*\)\s+|pub\s+|async\s+|const\s+|unsafe\s+|extern\s+"[^"]*"\s+)*)"#).unwrap()
});

fn try_parse_rust_item(line: &str) -> Option<(ItemKind, String)> {
    let line = normalise(line);
    // MOD_RE can match the empty string, so `find` always succeeds.
    let stripped = &line[MOD_RE.find(&line).unwrap().end()..];

    // Try item keywords.
    const PATS: [(&str, ItemKind); 4] = [
        ("struct ", ItemKind::Struct),
        ("trait ", ItemKind::Trait),
        ("mod ", ItemKind::Mod),
        ("fn ", ItemKind::Fn),
    ];

    for &(prefix, kind) in PATS.iter() {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            // `rest` is a suffix of the trimmed, normalised line, so it is
            // non-empty and free of surrounding whitespace.
            return Some((kind, normalize_heading_text(rest)));
        }
    }

    // `impl` — bare, followed by `<` or ` `. Kept as-is, it's descriptive enough.
    if stripped == "impl" || stripped.starts_with("impl<") || stripped.starts_with("impl ") {
        return Some((ItemKind::Imp, stripped.to_string()));
    }

    None
}
