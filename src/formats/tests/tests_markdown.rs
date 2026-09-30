use super::*;

#[test]
fn test_structure_basic() {
    let text = "# Title\n\nsome content\n\n## Section 1\n\ntext here\n\n### Subsection\n\nmore\n\n## Section 2\n\nend";
    let structure = structure(text);
    assert_eq!(structure.total_lines, 15);
    assert_eq!(structure.headers.len(), 4);
    assert_eq!(structure.headers[0].text, "Title");
    assert_eq!(structure.headers[0].level, 1);
    assert_eq!(structure.headers[0].start_line, 1);
    assert_eq!(structure.headers[0].end_line, 4);
    assert_eq!(structure.headers[1].text, "Section 1");
    assert_eq!(structure.headers[1].start_line, 5);
    assert_eq!(structure.headers[2].text, "Subsection");
    assert_eq!(structure.headers[2].start_line, 9);
    assert_eq!(structure.headers[3].text, "Section 2");
    assert_eq!(structure.headers[3].end_line, 15);
}

#[test]
fn test_structure_adjacent_headings() {
    let structure = structure("## A\n## B");
    assert_eq!(structure.total_lines, 2);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "A");
    assert_eq!(structure.headers[0].start_line, 1);
    assert_eq!(structure.headers[0].end_line, 1);
    assert_eq!(structure.headers[1].text, "B");
    assert_eq!(structure.headers[1].start_line, 2);
    assert_eq!(structure.headers[1].end_line, 2);
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
    assert_eq!(structure.headers.len(), 0);
}
