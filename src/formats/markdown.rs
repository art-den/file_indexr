use super::{FileStructure, HeadingItem};

#[cfg(test)]
#[path = "tests/tests_markdown.rs"]
mod tests;

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

    FileStructure {
        headers: build_heading_items(raw_headings, total_lines),
        total_lines,
    }
}

fn try_parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let level = usize::min(line.bytes().take_while(|&b| b == b'#').count(), 6);
    let text = line[level..].trim();
    (level > 0 && !text.is_empty()).then_some((level as u8, text))
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
