use super::{FileStructure, HeadingItem};

pub fn structure(text: &str) -> FileStructure {
    let mut raw_headings = Vec::new();
    let mut in_code_block = false;
    let mut total_lines = 0;

    for line in text.lines() {
        total_lines += 1;
        let trimmed = line.trim();

        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if let Some((level, h_text)) = try_parse_atx_heading(trimmed) {
            raw_headings.push((total_lines, level, h_text.to_string()));
        }
    }
    let headers = build_heading_items(raw_headings, total_lines);

    FileStructure {
        headers,
        total_lines,
    }
}

fn try_parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let level = usize::min(line.bytes().take_while(|&b| b == b'#').count(), 6);
    if level == 0 {
        return None;
    }
    let text = line[level..].trim();
    if text.is_empty() {
        return None;
    }
    Some((level as u8, text))
}

pub fn build_heading_items(
    raw_headings: Vec<(usize, u8, String)>,
    total_lines: usize,
) -> Vec<HeadingItem> {
    let mut result = Vec::with_capacity(raw_headings.len());
    let mut items = raw_headings.into_iter().peekable();
    while let Some((start_line, level, text)) = items.next() {
        let end_line = items.peek().map_or(total_lines, |next| next.0 - 1);
        result.push(HeadingItem {
            level,
            text,
            start_line,
            end_line,
        });
    }
    result
}

#[cfg(test)]
mod tests {
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
}
