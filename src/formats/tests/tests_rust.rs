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
