use serde_json::json;

use super::*;

#[test]
fn test_tools_list_returns_three_tools() {
    let tools = Tools::list();
    assert_eq!(tools.len(), 3);
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name")?.as_str())
        .collect();
    assert!(names.contains(&"docs_search"));
    assert!(names.contains(&"docs_headings"));
    assert!(names.contains(&"docs_get"));
}

#[test]
fn test_extract_str_valid() {
    let args = json!({"query": "hello"});
    let result = extract_str(&args, "query").unwrap();
    assert_eq!(result, "hello");
}

#[test]
fn test_extract_str_missing() {
    let args = json!({"other": "world"});
    let result = extract_str(&args, "query");
    assert!(result.is_err());
}

#[test]
fn test_extract_u64_valid() {
    let args = json!({"max_results": 42});
    assert_eq!(extract_u64(&args, "max_results"), Some(42));
}

#[test]
fn test_extract_u64_missing() {
    let args = json!({"max_results": "not_a_number"});
    assert_eq!(extract_u64(&args, "max_results"), None);
}

#[test]
fn test_truncate_short_string() {
    assert_eq!(truncate("hello", 10), "hello");
}

#[test]
fn test_truncate_exact_length() {
    assert_eq!(truncate("hello", 5), "hello");
}

#[test]
fn test_truncate_ascii() {
    assert_eq!(truncate("hello world", 5), "hello...");
}

#[test]
fn test_truncate_cyrillic_boundary() {
    // 'м' is 2 bytes in UTF-8, starts at byte 6
    // max=6 is exactly at char boundary → "Hello ..."
    assert_eq!(truncate("Hello мир", 6), "Hello ...");
}

#[test]
fn test_truncate_emoji_boundary() {
    // '🎉' is 4 bytes in UTF-8, starts at byte 6
    // max=7 is inside emoji → cutoff falls back to 6
    assert_eq!(truncate("Hello 🎉!", 7), "Hello ...");
}

#[test]
fn test_truncate_all_multi_byte() {
    // 'м'=bytes 0-1, 'и'=bytes 2-3, 'р'=bytes 4-5
    // max=3 is inside 'и' → cutoff falls back to 2 (start of 'и')
    assert_eq!(truncate("мир", 3), "м...");
}

#[test]
fn test_image_mime_type_supported() {
    use std::path::Path;
    assert_eq!(image_mime_type(Path::new("a.png")).unwrap(), "image/png");
    assert_eq!(image_mime_type(Path::new("a.jpg")).unwrap(), "image/jpeg");
    assert_eq!(
        image_mime_type(Path::new("a.jpeg")).unwrap(),
        "image/jpeg"
    );
    assert_eq!(
        image_mime_type(Path::new("a.gif")).unwrap(),
        "image/gif"
    );
    assert_eq!(
        image_mime_type(Path::new("a.webp")).unwrap(),
        "image/webp"
    );
    assert_eq!(image_mime_type(Path::new("a.bmp")).unwrap(), "image/bmp");
    assert_eq!(
        image_mime_type(Path::new("a.tiff")).unwrap(),
        "image/tiff"
    );
    assert_eq!(image_mime_type(Path::new("a.tif")).unwrap(), "image/tiff");
    assert_eq!(
        image_mime_type(Path::new("a.ico")).unwrap(),
        "image/x-icon"
    );
    assert_eq!(
        image_mime_type(Path::new("a.svg")).unwrap(),
        "image/svg+xml"
    );
    // Extensions are case-insensitive
    assert_eq!(
        image_mime_type(Path::new("a.PnG")).unwrap(),
        "image/png"
    );
}

#[test]
fn test_image_mime_type_unsupported() {
    use std::path::Path;
    assert!(image_mime_type(Path::new("a.txt")).is_none());
    assert!(image_mime_type(Path::new("a.rs")).is_none());
    assert!(image_mime_type(Path::new("a")).is_none());
}

#[test]
fn test_split_virtual_epub_path() {
    assert_eq!(
        split_virtual_epub_path("book.epub/images/pic.png"),
        Some(("book.epub", "images/pic.png"))
    );
    assert_eq!(
        split_virtual_epub_path("docs/book.EPUB/a.png"),
        Some(("docs/book.EPUB", "a.png"))
    );
    // Not virtual: no inner part, empty inner, wrong extension, no slash.
    assert_eq!(split_virtual_epub_path("book.epub"), None);
    assert_eq!(split_virtual_epub_path("book.epub/"), None);
    assert_eq!(split_virtual_epub_path("book.md/a.png"), None);
    assert_eq!(split_virtual_epub_path("/book.epub/a.png"), None);
    assert_eq!(split_virtual_epub_path("plain.png"), None);
}
