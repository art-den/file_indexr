use super::*;

/// Assert the headings parsed from `src` are exactly the given `(text, level)` pairs.
#[track_caller]
fn assert_headings(src: &str, expected: &[(&str, u8)]) {
    let result = structure(src);
    assert_eq!(
        result.headers.len(),
        expected.len(),
        "actual headings: {:?}",
        result
            .headers
            .iter()
            .map(|h| (h.text.as_str(), h.level))
            .collect::<Vec<_>>()
    );
    for (i, &(text, level)) in expected.iter().enumerate() {
        assert_eq!(result.headers[i].text, text, "heading #{i} text");
        assert_eq!(result.headers[i].level, level, "heading #{i} level");
    }
}

/// `count_brace_delta` brace delta only (ignores the block-comment state).
#[track_caller]
fn delta(line: &str, in_block_comment: bool) -> isize {
    count_brace_delta(line, in_block_comment).0
}

/// Assert the brace delta of each `(line, expected)` case outside a block comment.
#[track_caller]
fn assert_deltas(cases: &[(&str, isize)]) {
    for &(line, expected) in cases {
        assert_eq!(delta(line, false), expected, "line: {line:?}");
    }
}

#[test]
fn test_free_function_is_level_2() {
    let text = r#"
fn hello() {
println!("world");
}
"#;
    assert_headings(text, &[("hello", 2)]);
}

#[test]
fn test_struct_is_level_2() {
    let text = "struct MyStruct {\n    x: i32,\n}\n";
    assert_headings(text, &[("MyStruct", 2)]);
}

#[test]
fn test_trait_is_level_2() {
    let text = "trait MyTrait {\n    fn do_something();\n}\n";
    // fn inside trait is treated as nested in a Trait block
    assert_headings(text, &[("MyTrait", 2), ("do_something", 3)]);
}

#[test]
fn test_impl_block_is_level_2() {
    let text = "impl MyStruct {\n}\n";
    assert_headings(text, &[("impl MyStruct {", 2)]);
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
    assert_headings(text, &[("impl MyStruct {", 2), ("new", 3), ("run", 3)]);
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
    assert_headings(text, &[("outer", 2), ("inner", 3)]);
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
    assert_headings(
        text,
        &[
            ("standalone", 2),
            ("Foo", 2),
            ("impl Foo {", 2),
            ("method", 3),
        ],
    );
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
    assert_headings(
        text,
        &[
            ("Service", 2),
            ("impl Service {", 2),
            ("start", 3),
            ("impl Drop for Service {", 2),
            ("drop", 3),
        ],
    );
}

#[test]
fn test_mod_is_level_1() {
    let text = "mod utils {}\n";
    assert_headings(text, &[("utils", 1)]);
}

#[test]
fn test_pub_visibility_stripped() {
    let text = "pub fn public_fn() {}\npub struct PublicStruct {}\n";
    assert_headings(text, &[("public_fn", 2), ("PublicStruct", 2)]);
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
    assert_headings(
        text,
        &[
            ("sub", 1),
            ("helper", 2),
            ("Inner", 2),
            ("Local", 2),
            ("fetch", 2),
            ("raw", 2),
            ("spaced", 2),
            ("a", 1),
            // fn directly inside a mod is level 2 (mod is not a level container).
            ("in_path_fn", 2),
        ],
    );
}

#[test]
fn test_pubcrate_struct_methods_are_level_3() {
    let text = r#"
pub(crate) struct Wrapper {
pub fn new() -> Self { Wrapper }
}
"#;
    assert_headings(text, &[("Wrapper", 2), ("new", 3)]);
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
    assert_headings(
        text,
        &[("Foo", 2), ("impl Foo {", 2), ("helper", 3), ("visible", 3)],
    );
}

#[test]
fn test_pubcrate_braceless_struct_fields_not_headings() {
    let text = r#"
pub(crate) struct Config
{
pub(crate) x: i32,
}
"#;
    // Brace-less item: the block opens on the next line (pending path).
    // The field line must not produce a heading.
    assert_headings(text, &[("Config", 2)]);
}

#[test]
fn test_async_and_unsafe_functions() {
    let text = "pub async fn fetch() {}\nunsafe fn raw_access() {}\n";
    assert_headings(text, &[("fetch", 2), ("raw_access", 2)]);
}

#[test]
fn test_generics_preserved_in_heading() {
    let text = "struct Wrapper<T> {}\nfn process<I: Iterator>(items: I) {}\n";
    assert_headings(text, &[("Wrapper<T>", 2), ("process<I: Iterator>", 2)]);
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
    assert_headings(
        text,
        &[("level_one", 2), ("level_two", 3), ("level_three", 4)],
    );
}

#[test]
fn test_empty_file() {
    assert_headings("", &[]);
}

#[test]
fn test_no_headings() {
    let text = "let x = 5;\nlet y = 10;\n";
    assert_headings(text, &[]);
}

#[test]
fn test_braces_in_comment_ignored() {
    let text = r#"
fn foo() {
// this { comment } has braces
}
fn bar() {}
"#;
    assert_headings(text, &[("foo", 2), ("bar", 2)]);
}

#[test]
fn test_trait_methods_are_nested() {
    let text = r#"
trait Processor {
fn process(&self, input: &str) -> String;
fn name(&self) -> &str;
}
"#;
    assert_headings(text, &[("Processor", 2), ("process", 3), ("name", 3)]);
}

#[test]
fn test_impl_trait_for_type() {
    let text = r#"
struct Handler;
impl FromStr for Handler {
fn from_str(s: &str) -> Result<Self, Self::Err> { Ok(Handler) }
}
"#;
    assert_headings(
        text,
        &[
            ("Handler;", 2),
            ("impl FromStr for Handler {", 2),
            ("from_str", 3),
        ],
    );
}

#[test]
fn test_single_line_fn() {
    let text = "fn main() {}\n";
    assert_headings(text, &[("main", 2)]);
}

#[test]
fn test_modifiers_in_any_order() {
    let text = r#"
async pub fn a() {}
pub async fn b() {}
pub const unsafe fn c() {}
const unsafe fn d() {}
"#;
    assert_headings(text, &[("a", 2), ("b", 2), ("c", 2), ("d", 2)]);
}

#[test]
fn test_extra_whitespace_between_tokens() {
    let text = "pub  struct  Foo {}\npub    async    fn  bar() {}\n";
    assert_headings(text, &[("Foo", 2), ("bar", 2)]);
}

#[test]
fn test_unicode_whitespace_between_tokens() {
    // No-break space forces normalise's owned path (split_whitespace on a
    // non-space Unicode whitespace char).
    assert_headings("pub\u{a0}fn foo() {}", &[("foo", 2)]);
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
    assert_headings(
        text,
        &[("impl Service {", 2), ("handle", 3), ("cleanup", 3)],
    );
}

#[test]
fn test_count_brace_delta_basic() {
    assert_deltas(&[
        ("fn foo() {", 1),
        ("}", -1),
        ("{}", 0),
        ("{ let x = {}; }", 0),
    ]);
}

#[test]
fn test_count_brace_delta_ignores_comments() {
    assert_deltas(&[("// { }", 0), ("let x = 1; // {", 0)]);
}

#[test]
fn test_count_brace_delta_ignores_strings() {
    assert_deltas(&[(r#"let s = "{";"#, 0), (r#"let s = "{" + "}";"#, 0)]);
}

#[test]
fn test_count_brace_delta_ignores_char_literals() {
    assert_deltas(&[
        ("let x = '{';", 0),
        ("let x = '}';", 0),
        ("match c { '{' => {}, _ => {} }", 0),
    ]);
}

#[test]
fn test_count_brace_delta_ignores_escaped_char() {
    assert_eq!(delta(r"let x = '\'';", false), 0);
}

#[test]
fn test_count_brace_delta_mixed_string_and_char() {
    assert_eq!(delta(r#"let c = '{'; let s = "}"; {"#, false), 1);
}

#[test]
fn test_count_brace_delta_ignores_lifetimes() {
    assert_deltas(&[
        ("fn foo<'a>(x: &i32) {", 1),
        ("struct Foo<'a> { f: &'a i32 }", 0),
        ("impl<'a> Foo<'a> {", 1),
        ("fn bar<'a, 'b>(a: &'a i32, b: &'b i32) {", 1),
        ("let x: &'a i32 = &5;", 0),
    ]);
}

#[test]
fn test_count_brace_delta_ignores_loop_labels() {
    assert_deltas(&[("for 'label: x in y {", 1), ("'outer: for i in 0..3 {", 1)]);
}

#[test]
fn test_count_brace_delta_char_literal_single_ident() {
    // `'a'` is a char literal, `'a` is a lifetime.
    assert_deltas(&[
        ("match m { 'a' => {}, _ => {} }", 0),
        ("let c = '_';", 0),
        ("let x = b'a';", 0),
    ]);
}

#[test]
fn test_count_brace_delta_escaped_quote_in_string() {
    assert_deltas(&[
        (r#"let s = "a\"b";"#, 0),
        (r#"let s = "a\" } b"; { x }"#, 0),
    ]);
}

#[test]
fn test_count_brace_delta_escaped_backslash_in_char() {
    // `'\\'`: the second backslash is escaped, so the final quote closes the literal.
    assert_eq!(delta("let c = '\\\\'; { x }", false), 0);
}

#[test]
fn test_count_brace_digit_and_escaped_brace_literals() {
    assert_deltas(&[
        ("let c = '1';", 0),
        ("let a = '\\{'; let b = '\\}'; { x }", 0),
        (r"let c = '\u{2018}'; { x }", 0),
    ]);
}

#[test]
fn test_count_brace_delta_apostrophe_at_line_end() {
    // Lifetime split across lines: state resets per line, so it is harmless.
    assert_deltas(&[("fn foo<'", 0), ("a>(x: &i32) {", 1)]);
}

#[test]
fn test_count_brace_delta_non_ascii_lifetime_caveat() {
    // Known byte-scan caveat: a non-ASCII lifetime identifier is misread as
    // an unterminated char literal, swallowing the rest of the line.
    // A non-ASCII char literal, however, is still closed correctly.
    assert_deltas(&[("fn foo<'é>(x: &i32) {", 0), ("let c = 'é'; {", 1)]);
}

#[test]
fn test_fn_with_lifetime_pushes_to_stack() {
    let text = r#"
fn foo<'a>(x: &i32) {
fn inner() {}
}
"#;
    assert_headings(text, &[("foo<'a>", 2), ("inner", 3)]);
}

#[test]
fn test_struct_with_lifetime_fields() {
    let text = "struct Foo<'a> { f: &'a i32 }\nfn after() {}\n";
    // Field body is kept as-is (normalization only strips braces/params).
    assert_headings(text, &[("Foo<'a>  f: &'a i32", 2), ("after", 2)]);
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
    assert_headings(
        text,
        &[
            ("S;", 2),       // struct
            ("impl S {", 2), // impl
            ("m<'a>", 3),    // method
            ("n", 3),        // method
            ("free", 2),     // free fn after impl closed
        ],
    );
}

#[test]
fn test_count_brace_delta_ignores_block_comments() {
    assert_deltas(&[
        ("/* { */", 0),
        ("let x = 1; /* { */", 0),
        ("/* { */ fn f() {", 1),
        ("let c = '{'; /* } */", 0),
    ]);
    // Unclosed on this line: state is returned for the caller.
    assert_eq!(count_brace_delta("/* {", false), (0, true));
    assert_eq!(count_brace_delta("{ */", true), (0, false));
    assert_eq!(delta("*/", true), 0);
    // `/*/` does not close the comment.
    assert_eq!(count_brace_delta("/*/", false), (0, true));
}

#[test]
fn test_block_comment_braces_ignored_in_structure() {
    let text = "fn a() {\n    /* { */\n}\nfn b() {\n}\n";
    assert_headings(text, &[("a", 2), ("b", 2)]);
}

#[test]
fn test_multiline_block_comment_ignored_in_structure() {
    let text = "fn a() {\n    /*\n    fn b() {\n    */\n}\nfn c() {\n}\n";
    assert_headings(text, &[("a", 2), ("c", 2)]); // b is inside the comment
}

#[test]
fn test_item_after_block_comment_close_on_line() {
    let text = "/*\n*/ fn c() {\n}\n";
    assert_headings(text, &[("c", 2)]);
}

#[test]
fn test_multiline_impl_methods_are_level_3() {
    let text = "impl\n    Service\n{\n    fn start(&self) {}\n    fn stop(&self) {}\n}\n";
    assert_headings(text, &[("impl", 2), ("start", 3), ("stop", 3)]);
}

#[test]
fn test_multiline_fn_signature_nested_fn_level() {
    let text = "fn outer(\n    x: i32,\n) {\n    fn inner() {}\n}\n";
    assert_headings(text, &[("outer", 2), ("inner", 3)]); // nested in multiline fn
}

#[test]
fn test_multiline_impl_with_where_clause() {
    let text = "impl Service\nwhere\n    Self: Debug,\n{\n    fn baz() {}\n}\n";
    assert_headings(text, &[("impl Service", 2), ("baz", 3)]);
}

#[test]
fn test_braceless_item_pending_reset_on_next_item() {
    // `struct Foo;` leaves a pending entry; the next item line must reset it,
    // otherwise the closure's `{` would push a false Struct entry and make
    // `inner` level 4 instead of 3.
    let text = "struct Foo;\nfn bar() {\n    let f = || {\n        fn inner() {}\n    };\n}\n";
    assert_headings(text, &[("Foo;", 2), ("bar", 2), ("inner", 3)]);
}

#[test]
fn test_single_line_fn_followed_by_closure_no_false_nesting() {
    // `fn helper() {}` opens and closes its block inline, so it must not
    // leave a pending entry; the closure's `{` must not push a false entry.
    let text = "fn a() {\n    fn helper() {}\n    let f = || {\n        fn inner() {}\n    };\n}\n";
    assert_headings(text, &[("a", 2), ("helper", 3), ("inner", 3)]);
}

#[test]
fn test_multiline_mod_is_tracked() {
    let text = "mod foo\n{\n    fn bar() {}\n}\n";
    // fn in mod is not a container, so `bar` stays level 2.
    assert_headings(text, &[("foo", 1), ("bar", 2)]);
}

#[test]
fn test_braceless_item_pending_cleared_when_context_closes() {
    // `fn b() {}` sets a pending entry; after `}` the depth drops below the
    // pending depth, so a later `{` line must not register a false push.
    let text = "fn a() {\n    fn b() {}\n}\nfn c(\n    x: i32,\n) {\n}\n";
    assert_headings(text, &[("a", 2), ("b", 3), ("c", 2)]);
}
