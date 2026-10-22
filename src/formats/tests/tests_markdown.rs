use super::*;

/// Asserts every field of `structure.headers[i]`; `i` also labels failure messages.
fn assert_heading(
    structure: &FileStructure,
    i: usize,
    level: u8,
    text: &str,
    start: usize,
    end: usize,
) {
    let h = &structure.headers[i];
    assert_eq!(h.level, level, "level of heading {i}");
    assert_eq!(h.text, text, "text of heading {i}");
    assert_eq!(h.start_line, start, "start_line of heading {i}");
    assert_eq!(h.end_line, end, "end_line of heading {i}");
}

#[test]
fn test_structure_basic() {
    let text = "# Title\n\nsome content\n\n## Section 1\n\ntext here\n\n### Subsection\n\nmore\n\n## Section 2\n\nend";
    let structure = structure(text);
    assert_eq!(structure.total_lines, 15);
    assert_eq!(structure.headers.len(), 4);
    assert_heading(&structure, 0, 1, "Title", 1, 4);
    assert_heading(&structure, 1, 2, "Section 1", 5, 8);
    assert_heading(&structure, 2, 3, "Subsection", 9, 12);
    assert_heading(&structure, 3, 2, "Section 2", 13, 15);
}

#[test]
fn test_structure_adjacent_headings() {
    let structure = structure("## A\n## B");
    assert_eq!(structure.total_lines, 2);
    assert_eq!(structure.headers.len(), 2);
    assert_heading(&structure, 0, 2, "A", 1, 1);
    assert_heading(&structure, 1, 2, "B", 2, 2);
}

#[test]
fn test_structure_skips_code_blocks() {
    let text = "# Title\n\n```\n# not a heading\n```\n\n## Real heading";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "Title");
    assert_eq!(structure.headers[1].text, "Real heading");
}

#[test]
fn test_structure_empty() {
    let structure = structure("");
    assert_eq!(structure.total_lines, 0);
    assert!(structure.headers.is_empty());
}
