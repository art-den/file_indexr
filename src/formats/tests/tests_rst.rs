use crate::formats::rst::*;

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
