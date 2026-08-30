use crate::formats::{FileStructure, markdown};

/// Kind of Python heading, used to compute final level from indentation depth.
#[derive(PartialEq)]
enum HeadingKind {
    Class,    // base level 1
    Function, // base level 2
}

/// Parse Python source for class/def headings.
///
/// Uses indentation to determine nesting:
/// - `class` at indent 0 → level 1
/// - `def` at indent 0 (top-level function) → level 2
/// - `def` inside class → level 2
/// - nested `def` inside another `def` → level 3+
pub fn structure(text: &str) -> FileStructure {
    let mut raw_headings = Vec::new();
    // Stack tracks (indent_depth, kind) to compute correct nesting levels.
    let mut stack = Vec::new();
    let mut total_lines = 0;

    for line in text.lines() {
        total_lines += 1;

        if let Some((kind, depth, name)) = try_parse_python_heading(line) {
            // Pop entries at the same or deeper indent (sibling or closed block).
            while stack.last().is_some_and(|&(d, _)| d >= depth) {
                stack.pop();
            }

            let level = match kind {
                HeadingKind::Class => (stack.len() + 1) as u8,
                HeadingKind::Function => {
                    // Level = 2 + number of function ancestors.
                    let func_count = stack
                        .iter()
                        .filter(|(_, k)| *k == HeadingKind::Function)
                        .count();
                    (func_count + 2) as u8
                }
            };

            raw_headings.push((total_lines, level, name));
            stack.push((depth, kind));
        }
    }

    let headings = markdown::build_heading_items(raw_headings, total_lines);

    FileStructure {
        headers: headings,
        total_lines,
    }
}

fn try_parse_python_heading(line: &str) -> Option<(HeadingKind, u8, String)> {
    let trimmed = line.trim_start();

    let (kind, rest) = if let Some(rest) = trimmed.strip_prefix("class ") {
        (HeadingKind::Class, rest)
    } else if let Some(rest) = trimmed.strip_prefix("async def ") {
        (HeadingKind::Function, rest)
    } else {
        (HeadingKind::Function, trimmed.strip_prefix("def ")?)
    };

    let name = normalize_heading_text(rest)?;

    // Count leading whitespace (spaces + tabs) to determine indentation depth.
    let indent_chars = line
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .map(|b| if b == b'\t' { 4 } else { 1 })
        .sum::<usize>();
    // Depth relative to top-level (each 4 spaces = 1 level deeper).
    let depth = (indent_chars / 4) as u8;

    Some((kind, depth, name))
}

/// Normalize heading text: strip comment, parameters, and trailing colon for display.
fn normalize_heading_text(text: &str) -> Option<String> {
    let text = text
        .split_once('#')
        .map_or(text, |(before, _)| before)
        .trim();
    let text = text.split_once('(').map_or(text, |(before, _)| before);
    let text = text.trim_end_matches(':').trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_class_top_level_is_level_1() {
        let text = "class MyClass:\n    pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 1);
        assert_eq!(structure.headers[0].text, "MyClass");
        assert_eq!(structure.headers[0].level, 1);
    }

    #[test]
    fn test_function_top_level_is_level_2() {
        let text = "def my_function():\n    pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 1);
        assert_eq!(structure.headers[0].text, "my_function");
        assert_eq!(structure.headers[0].level, 2);
    }

    #[test]
    fn test_method_inside_class_is_level_2() {
        let text = "class MyClass:\n    def method(self):\n        pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].text, "MyClass");
        assert_eq!(structure.headers[0].level, 1);
        assert_eq!(structure.headers[1].text, "method");
        assert_eq!(structure.headers[1].level, 2);
    }

    #[test]
    fn test_nested_function_is_deeper() {
        let text = "def outer():\n    def inner():\n        pass\n    return inner\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].text, "outer");
        assert_eq!(structure.headers[0].level, 2);
        assert_eq!(structure.headers[1].text, "inner");
        assert_eq!(structure.headers[1].level, 3);
    }

    #[test]
    fn test_async_def_top_level_is_level_2() {
        let text = "async def fetch() -> None:\n    pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 1);
        assert_eq!(structure.headers[0].text, "fetch");
        assert_eq!(structure.headers[0].level, 2);
    }

    #[test]
    fn test_async_method_inside_class_is_level_2() {
        let text = "class Api:\n    async def fetch(self):\n        pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].text, "Api");
        assert_eq!(structure.headers[0].level, 1);
        assert_eq!(structure.headers[1].text, "fetch");
        assert_eq!(structure.headers[1].level, 2);
    }

    #[test]
    fn test_nested_def_inside_async_def_is_level_3() {
        let text = "async def outer():\n    def inner():\n        pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].text, "outer");
        assert_eq!(structure.headers[0].level, 2);
        assert_eq!(structure.headers[1].text, "inner");
        assert_eq!(structure.headers[1].level, 3);
    }

    #[test]
    fn test_nested_class_and_method() {
        let text = "class Outer:\n    def method(self):\n        class Inner:\n            def inner_method(self):\n                pass\n        return Inner\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 4);
        assert_eq!(structure.headers[0].level, 1); // Outer
        assert_eq!(structure.headers[1].level, 2); // method
        assert_eq!(structure.headers[2].level, 3); // Inner
        assert_eq!(structure.headers[3].level, 3); // inner_method (1 func ancestor + 2)
    }

    #[test]
    fn test_empty_structure() {
        let structure = structure("");
        assert_eq!(structure.headers.len(), 0);
    }

    #[test]
    fn test_trailing_comment_stripped_from_name() {
        let text = "class Foo: # a comment\ndef bar(): # does stuff\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].text, "Foo");
        assert_eq!(structure.headers[1].text, "bar");
    }

    #[test]
    fn test_mixed_indentation_depth() {
        // 2 spaces + 1 tab + 2 spaces = 2+4+2 = 8 → depth 2
        let text = "class Outer:\n      def method(self):\n        pass\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].level, 1);
        assert_eq!(structure.headers[1].level, 2);
    }

    #[test]
    fn test_tab_indentation_depth() {
        // 2 tabs = depth 2, nested def inside nested def → level 3
        let text = "def outer():\n\t\tdef inner():\n\t\t\tpass\n    return inner\n";
        let structure = structure(text);
        assert_eq!(structure.headers.len(), 2);
        assert_eq!(structure.headers[0].level, 2); // outer
        assert_eq!(structure.headers[1].level, 3); // inner
    }
}
