use std::borrow::Cow;

use itertools::Itertools;

use crate::formats::{FileStructure, markdown};

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
            raw_headings.push((total_lines, level, normalize_heading_text(item_kind, &name)));

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
fn count_brace_delta(line: &str, mut in_block_comment: bool) -> (isize, bool) {
    let mut delta: isize = 0;
    // Closing quote of the string/char literal being scanned, if any.
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let chars = line.as_bytes();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if in_block_comment {
            if ch == b'*' && i + 1 < chars.len() && chars[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
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
                b'*' => {
                    in_block_comment = true;
                    i += 2;
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
    (delta, in_block_comment)
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
fn normalize_heading_text(kind: ItemKind, text: &str) -> String {
    if kind == ItemKind::Imp {
        // Keep impl line as-is — it's descriptive enough.
        return text.to_string();
    }

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

fn try_parse_rust_item(line: &str) -> Option<(ItemKind, String)> {
    use std::sync::LazyLock;

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
            return Some((kind, rest.to_string()));
        }
    }

    // `impl` — bare, followed by `<` or ` `.
    if stripped == "impl" || stripped.starts_with("impl<") || stripped.starts_with("impl ") {
        return Some((ItemKind::Imp, stripped.to_string()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_free_function_is_level_2() {
        let text = r#"
fn hello() {
    println!("world");
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "hello");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_struct_is_level_2() {
        let text = "struct MyStruct {\n    x: i32,\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "MyStruct");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_trait_is_level_2() {
        let text = "trait MyTrait {\n    fn do_something();\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "MyTrait");
        assert_eq!(result.headers[0].level, 2);
        // fn inside trait is treated as nested in a Trait block
        assert_eq!(result.headers[1].text, "do_something");
        assert_eq!(result.headers[1].level, 3);
    }

    #[test]
    fn test_impl_block_is_level_2() {
        let text = "impl MyStruct {\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "impl MyStruct {");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_method_inside_impl_is_level_3() {
        let text = r#"
impl MyStruct {
    fn new() -> Self {
        Self { x: 0 }
    }

    fn run(&self) {
        println!("{}", self.x);
    }
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].level, 2); // impl
        assert_eq!(result.headers[1].text, "new");
        assert_eq!(result.headers[1].level, 3);
        assert_eq!(result.headers[2].text, "run");
        assert_eq!(result.headers[2].level, 3);
    }

    #[test]
    fn test_nested_function_is_deeper() {
        let text = r#"
fn outer() {
    fn inner() {
        // nested
    }
    inner();
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "outer");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "inner");
        assert_eq!(result.headers[1].level, 3);
    }

    #[test]
    fn test_method_vs_free_fn_distinct_levels() {
        let text = r#"
fn standalone() {}

struct Foo {}

impl Foo {
    fn method(&self) {}
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 4);
        assert_eq!(result.headers[0].text, "standalone");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "Foo");
        assert_eq!(result.headers[1].level, 2);
        assert_eq!(result.headers[2].text, "impl Foo {");
        assert_eq!(result.headers[2].level, 2);
        assert_eq!(result.headers[3].text, "method");
        assert_eq!(result.headers[3].level, 3);
    }

    #[test]
    fn test_multiple_impl_blocks() {
        let text = r#"
struct Service {}

impl Service {
    fn start(&self) {}
}

impl Drop for Service {
    fn drop(&mut self) {}
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 5);
        assert_eq!(result.headers[0].text, "Service");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "impl Service {");
        assert_eq!(result.headers[1].level, 2);
        assert_eq!(result.headers[2].text, "start");
        assert_eq!(result.headers[2].level, 3);
        assert_eq!(result.headers[3].text, "impl Drop for Service {");
        assert_eq!(result.headers[3].level, 2);
        assert_eq!(result.headers[4].text, "drop");
        assert_eq!(result.headers[4].level, 3);
    }

    #[test]
    fn test_mod_is_level_1() {
        let text = "mod utils {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "utils");
        assert_eq!(result.headers[0].level, 1);
    }

    #[test]
    fn test_pub_visibility_stripped() {
        let text = "pub fn public_fn() {}\npub struct PublicStruct {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "public_fn");
        assert_eq!(result.headers[1].text, "PublicStruct");
    }

    #[test]
    fn test_pub_parenthesized_visibility() {
        let text = r#"
pub(crate) mod sub {}
pub(crate) fn helper() {}
pub(super) struct Inner {}
pub(self) trait Local {}
pub(crate) async fn fetch() {}
unsafe pub(crate) fn raw() {}
pub (crate) fn spaced() {}
mod a {
    pub(in crate::a) fn in_path_fn() {}
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 9);
        assert_eq!(result.headers[0].text, "sub");
        assert_eq!(result.headers[0].level, 1);
        assert_eq!(result.headers[1].text, "helper");
        assert_eq!(result.headers[1].level, 2);
        assert_eq!(result.headers[2].text, "Inner");
        assert_eq!(result.headers[2].level, 2);
        assert_eq!(result.headers[3].text, "Local");
        assert_eq!(result.headers[3].level, 2);
        assert_eq!(result.headers[4].text, "fetch");
        assert_eq!(result.headers[4].level, 2);
        assert_eq!(result.headers[5].text, "raw");
        assert_eq!(result.headers[5].level, 2);
        assert_eq!(result.headers[6].text, "spaced");
        assert_eq!(result.headers[6].level, 2);
        assert_eq!(result.headers[7].text, "a");
        assert_eq!(result.headers[7].level, 1);
        assert_eq!(result.headers[8].text, "in_path_fn");
        // fn directly inside a mod is level 2 (mod is not a level container).
        assert_eq!(result.headers[8].level, 2);
    }

    #[test]
    fn test_pubcrate_struct_methods_are_level_3() {
        let text = r#"
pub(crate) struct Wrapper {
    pub fn new() -> Self { Wrapper }
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "Wrapper");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "new");
        assert_eq!(result.headers[1].level, 3);
    }

    #[test]
    fn test_pubcrate_methods_inside_impl_are_level_3() {
        let text = r#"
struct Foo {}
impl Foo {
    pub(crate) fn helper() {}
    pub fn visible() {}
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 4);
        assert_eq!(result.headers[0].text, "Foo");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].level, 2); // impl
        assert_eq!(result.headers[2].text, "helper");
        assert_eq!(result.headers[2].level, 3);
        assert_eq!(result.headers[3].text, "visible");
        assert_eq!(result.headers[3].level, 3);
    }

    #[test]
    fn test_pubcrate_braceless_struct_fields_not_headings() {
        let text = r#"
pub(crate) struct Config
{
    pub(crate) x: i32,
}
"#;
        let result = structure(text);
        // Brace-less item: the block opens on the next line (pending path).
        // The field line must not produce a heading.
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "Config");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_async_and_unsafe_functions() {
        let text = "pub async fn fetch() {}\nunsafe fn raw_access() {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "fetch");
        assert_eq!(result.headers[1].text, "raw_access");
    }

    #[test]
    fn test_generics_preserved_in_heading() {
        let text = "struct Wrapper<T> {}\nfn process<I: Iterator>(items: I) {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "Wrapper<T>");
        assert_eq!(result.headers[1].text, "process<I: Iterator>");
    }

    #[test]
    fn test_deeply_nested_function() {
        let text = r#"
fn level_one() {
    fn level_two() {
        fn level_three() {}
    }
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].level, 3);
        assert_eq!(result.headers[2].level, 4);
    }

    #[test]
    fn test_empty_file() {
        let result = structure("");
        assert_eq!(result.headers.len(), 0);
    }

    #[test]
    fn test_no_headings() {
        let text = "let x = 5;\nlet y = 10;\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 0);
    }

    #[test]
    fn test_braces_in_comment_ignored() {
        let text = r#"
fn foo() {
    // this { comment } has braces
}
fn bar() {}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "foo");
        assert_eq!(result.headers[1].text, "bar");
    }

    #[test]
    fn test_trait_methods_are_nested() {
        let text = r#"
trait Processor {
    fn process(&self, input: &str) -> String;
    fn name(&self) -> &str;
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].level, 2); // trait
        assert_eq!(result.headers[1].level, 3); // method
        assert_eq!(result.headers[2].level, 3); // method
    }

    #[test]
    fn test_impl_trait_for_type() {
        let text = r#"
struct Handler;
impl FromStr for Handler {
    fn from_str(s: &str) -> Result<Self, Self::Err> { Ok(Handler) }
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].level, 2); // struct
        assert_eq!(result.headers[1].level, 2); // impl
        assert_eq!(result.headers[2].level, 3); // method
    }

    #[test]
    fn test_single_line_fn() {
        let text = "fn main() {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "main");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_modifiers_in_any_order() {
        let text = r#"
async pub fn a() {}
pub async fn b() {}
pub const unsafe fn c() {}
const unsafe fn d() {}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 4);
        assert_eq!(result.headers[0].text, "a");
        assert_eq!(result.headers[1].text, "b");
        assert_eq!(result.headers[2].text, "c");
        assert_eq!(result.headers[3].text, "d");
    }

    #[test]
    fn test_extra_whitespace_between_tokens() {
        let text = "pub  struct  Foo {}\npub    async    fn  bar() {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "Foo");
        assert_eq!(result.headers[1].text, "bar");
    }

    #[test]
    fn test_unicode_whitespace_between_tokens() {
        // No-break space forces normalise's owned path (split_whitespace on a
        // non-space Unicode whitespace char).
        let result = structure("pub\u{a0}fn foo() {}");
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "foo");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_nested_brace_blocks_in_method_body() {
        let text = r#"
impl Service {
    fn handle(&self, req: Request) -> Response {
        if self.is_active {
            match req.method() {
                "GET" => {
                    for item in &self.items {
                        if item.visible {
                            println!("{}", item);
                        }
                    }
                }
                _ => {}
            }
        }
        Response::Ok()
    }

    fn cleanup(&self) {
        drop(self.lock());
    }
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].text, "impl Service {");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "handle");
        assert_eq!(result.headers[1].level, 3);
        assert_eq!(result.headers[2].text, "cleanup");
        assert_eq!(result.headers[2].level, 3);
    }

    #[test]
    fn test_count_brace_delta_basic() {
        assert_eq!(count_brace_delta("fn foo() {", false).0, 1);
        assert_eq!(count_brace_delta("}", false).0, -1);
        assert_eq!(count_brace_delta("{}", false).0, 0);
        assert_eq!(count_brace_delta("{ let x = {}; }", false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_ignores_comments() {
        assert_eq!(count_brace_delta("// { }", false).0, 0);
        assert_eq!(count_brace_delta("let x = 1; // {", false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_ignores_strings() {
        assert_eq!(count_brace_delta(r#"let s = "{";"#, false).0, 0);
        assert_eq!(count_brace_delta(r#"let s = "{" + "}";"#, false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_ignores_char_literals() {
        assert_eq!(count_brace_delta("let x = '{';", false).0, 0);
        assert_eq!(count_brace_delta("let x = '}';", false).0, 0);
        assert_eq!(
            count_brace_delta("match c { '{' => {}, _ => {} }", false).0,
            0
        );
    }

    #[test]
    fn test_count_brace_delta_ignores_escaped_char() {
        assert_eq!(count_brace_delta(r#"let x = '\'';"#, false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_mixed_string_and_char() {
        assert_eq!(
            count_brace_delta(r#"let c = '{'; let s = "}"; {"#, false).0,
            1
        );
    }

    #[test]
    fn test_count_brace_delta_ignores_lifetimes() {
        assert_eq!(count_brace_delta("fn foo<'a>(x: &i32) {", false).0, 1);
        assert_eq!(
            count_brace_delta("struct Foo<'a> { f: &'a i32 }", false).0,
            0
        );
        assert_eq!(count_brace_delta("impl<'a> Foo<'a> {", false).0, 1);
        assert_eq!(
            count_brace_delta("fn bar<'a, 'b>(a: &'a i32, b: &'b i32) {", false).0,
            1
        );
        assert_eq!(count_brace_delta("let x: &'a i32 = &5;", false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_ignores_loop_labels() {
        assert_eq!(count_brace_delta("for 'label: x in y {", false).0, 1);
        assert_eq!(count_brace_delta("'outer: for i in 0..3 {", false).0, 1);
    }

    #[test]
    fn test_count_brace_delta_char_literal_single_ident() {
        // `'a'` is a char literal, `'a` is a lifetime.
        assert_eq!(
            count_brace_delta("match m { 'a' => {}, _ => {} }", false).0,
            0
        );
        assert_eq!(count_brace_delta("let c = '_';", false).0, 0);
        assert_eq!(count_brace_delta("let x = b'a';", false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_escaped_quote_in_string() {
        assert_eq!(count_brace_delta(r#"let s = "a\"b";"#, false).0, 0);
        assert_eq!(count_brace_delta(r#"let s = "a\" } b"; { x }"#, false).0, 0);
    }

    #[test]
    fn test_count_brace_delta_escaped_backslash_in_char() {
        // `'\\'`: the second backslash is escaped, so the final quote closes the literal.
        assert_eq!(count_brace_delta("let c = '\\\\'; { x }", false).0, 0);
    }

    #[test]
    fn test_count_brace_digit_and_escaped_brace_literals() {
        assert_eq!(count_brace_delta("let c = '1';", false).0, 0);
        assert_eq!(
            count_brace_delta("let a = '\\{'; let b = '\\}'; { x }", false).0,
            0
        );
        assert_eq!(
            count_brace_delta(r#"let c = '\u{2018}'; { x }"#, false).0,
            0
        );
    }

    #[test]
    fn test_count_brace_delta_apostrophe_at_line_end() {
        // Lifetime split across lines: state resets per line, so it is harmless.
        assert_eq!(count_brace_delta("fn foo<'", false).0, 0);
        assert_eq!(count_brace_delta("a>(x: &i32) {", false).0, 1);
    }

    #[test]
    fn test_count_brace_delta_non_ascii_lifetime_caveat() {
        // Known byte-scan caveat: a non-ASCII lifetime identifier is misread as
        // an unterminated char literal, swallowing the rest of the line.
        assert_eq!(count_brace_delta("fn foo<'é>(x: &i32) {", false).0, 0);
        // A non-ASCII char literal is still closed correctly.
        assert_eq!(count_brace_delta("let c = 'é'; {", false).0, 1);
    }

    #[test]
    fn test_fn_with_lifetime_pushes_to_stack() {
        let text = r#"
fn foo<'a>(x: &i32) {
    fn inner() {}
}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "foo<'a>");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "inner");
        assert_eq!(result.headers[1].level, 3);
    }

    #[test]
    fn test_struct_with_lifetime_fields() {
        let text = "struct Foo<'a> { f: &'a i32 }\nfn after() {}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        // Field body is kept as-is (normalization only strips braces/params).
        assert_eq!(result.headers[0].text, "Foo<'a>  f: &'a i32");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "after");
        assert_eq!(result.headers[1].level, 2);
    }

    #[test]
    fn test_impl_with_lifetime_methods_level() {
        let text = r#"
struct S;
impl S {
    fn m<'a>(&'a self) -> i32 {
        0
    }
    fn n(&self) {}
}
fn free() {}
"#;
        let result = structure(text);
        assert_eq!(result.headers.len(), 5);
        assert_eq!(result.headers[0].level, 2); // struct
        assert_eq!(result.headers[1].level, 2); // impl
        assert_eq!(result.headers[2].text, "m<'a>");
        assert_eq!(result.headers[2].level, 3); // method
        assert_eq!(result.headers[3].text, "n");
        assert_eq!(result.headers[3].level, 3); // method
        assert_eq!(result.headers[4].text, "free");
        assert_eq!(result.headers[4].level, 2); // free fn after impl closed
    }

    #[test]
    fn test_count_brace_delta_ignores_block_comments() {
        assert_eq!(count_brace_delta("/* { */", false).0, 0);
        assert_eq!(count_brace_delta("let x = 1; /* { */", false).0, 0);
        assert_eq!(count_brace_delta("/* { */ fn f() {", false).0, 1);
        assert_eq!(count_brace_delta("let c = '{'; /* } */", false).0, 0);
        // Unclosed on this line: state is returned for the caller.
        assert_eq!(count_brace_delta("/* {", false), (0, true));
        assert_eq!(count_brace_delta("{ */", true), (0, false));
        assert_eq!(count_brace_delta("*/", true).0, 0);
        // `/*/` does not close the comment.
        assert_eq!(count_brace_delta("/*/", false), (0, true));
    }

    #[test]
    fn test_block_comment_braces_ignored_in_structure() {
        let text = "fn a() {\n    /* { */\n}\nfn b() {\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "a");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "b");
        assert_eq!(result.headers[1].level, 2);
    }

    #[test]
    fn test_multiline_block_comment_ignored_in_structure() {
        let text = "fn a() {\n    /*\n    fn b() {\n    */\n}\nfn c() {\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "a");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "c");
        assert_eq!(result.headers[1].level, 2); // b is inside the comment
    }

    #[test]
    fn test_item_after_block_comment_close_on_line() {
        let text = "/*\n*/ fn c() {\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 1);
        assert_eq!(result.headers[0].text, "c");
        assert_eq!(result.headers[0].level, 2);
    }

    #[test]
    fn test_multiline_impl_methods_are_level_3() {
        let text = "impl\n    Service\n{\n    fn start(&self) {}\n    fn stop(&self) {}\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].text, "impl");
        assert_eq!(result.headers[0].level, 2); // impl
        assert_eq!(result.headers[1].text, "start");
        assert_eq!(result.headers[1].level, 3); // method
        assert_eq!(result.headers[2].text, "stop");
        assert_eq!(result.headers[2].level, 3); // method
    }

    #[test]
    fn test_multiline_fn_signature_nested_fn_level() {
        let text = "fn outer(\n    x: i32,\n) {\n    fn inner() {}\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "outer");
        assert_eq!(result.headers[0].level, 2); // free fn
        assert_eq!(result.headers[1].text, "inner");
        assert_eq!(result.headers[1].level, 3); // nested in multiline fn
    }

    #[test]
    fn test_multiline_impl_with_where_clause() {
        let text = "impl Service\nwhere\n    Self: Debug,\n{\n    fn baz() {}\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].level, 2); // impl
        assert_eq!(result.headers[1].text, "baz");
        assert_eq!(result.headers[1].level, 3); // method
    }

    #[test]
    fn test_braceless_item_pending_reset_on_next_item() {
        // `struct Foo;` leaves a pending entry; the next item line must reset it,
        // otherwise the closure's `{` would push a false Struct entry and make
        // `inner` level 4 instead of 3.
        let text = "struct Foo;\nfn bar() {\n    let f = || {\n        fn inner() {}\n    };\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].text, "Foo;");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "bar");
        assert_eq!(result.headers[1].level, 2);
        assert_eq!(result.headers[2].text, "inner");
        assert_eq!(result.headers[2].level, 3); // nested only in `bar`
    }

    #[test]
    fn test_single_line_fn_followed_by_closure_no_false_nesting() {
        // `fn helper() {}` opens and closes its block inline, so it must not
        // leave a pending entry; the closure's `{` must not push a false entry.
        let text =
            "fn a() {\n    fn helper() {}\n    let f = || {\n        fn inner() {}\n    };\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].text, "a");
        assert_eq!(result.headers[0].level, 2);
        assert_eq!(result.headers[1].text, "helper");
        assert_eq!(result.headers[1].level, 3);
        assert_eq!(result.headers[2].text, "inner");
        assert_eq!(result.headers[2].level, 3); // nested only in `a`
    }

    #[test]
    fn test_multiline_mod_is_tracked() {
        let text = "mod foo\n{\n    fn bar() {}\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 2);
        assert_eq!(result.headers[0].text, "foo");
        assert_eq!(result.headers[0].level, 1); // mod
        assert_eq!(result.headers[1].text, "bar");
        assert_eq!(result.headers[1].level, 2); // fn in mod is not a container
    }

    #[test]
    fn test_braceless_item_pending_cleared_when_context_closes() {
        // `fn b() {}` sets a pending entry; after `}` the depth drops below the
        // pending depth, so a later `{` line must not register a false push.
        let text = "fn a() {\n    fn b() {}\n}\nfn c(\n    x: i32,\n) {\n}\n";
        let result = structure(text);
        assert_eq!(result.headers.len(), 3);
        assert_eq!(result.headers[0].level, 2); // a
        assert_eq!(result.headers[1].level, 3); // b
        assert_eq!(result.headers[2].text, "c");
        assert_eq!(result.headers[2].level, 2); // free fn again
    }
}
