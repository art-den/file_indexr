use std::io::Write;
use std::path::Path;

use crate::formats::{FileFormat, FileTextData};

use super::*;

fn image_test_epub(dir: &Path, name: &str, entries: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
    let path = dir.join(name);
    write_zip_bytes(&path, entries);
    path
}

fn image_epub_entries(png_name: &str) -> Vec<(&str, Vec<u8>)> {
    vec![
        ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
        ("OEBPS/content.opf", b"<package/>".to_vec()),
        ("OEBPS/Text/ch1.xhtml", b"<html/>".to_vec()),
        (png_name, b"PNGDATA".to_vec()),
    ]
}

#[test]
fn test_epub_read_image_entry() {
    let dir = crate::testutil::unique_temp_dir("epub_img_test");
    let path = image_test_epub(&dir, "book.epub", &image_epub_entries("OEBPS/Text/images/pic.png"));

    // Document-relative path resolves via the unique suffix.
    let (name, bytes) = read_image_entry(&path, "images/pic.png", 1024).unwrap();
    assert_eq!(name, "OEBPS/Text/images/pic.png");
    assert_eq!(bytes.as_slice(), b"PNGDATA");

    // The literal archive entry path also works.
    let (name, _) = read_image_entry(&path, "OEBPS/Text/images/pic.png", 1024).unwrap();
    assert_eq!(name, "OEBPS/Text/images/pic.png");

    // Leading '/' and '#fragment' are stripped.
    let (name, _) =
        read_image_entry(&path, "/OEBPS/Text/images/pic.png#fig", 1024).unwrap();
    assert_eq!(name, "OEBPS/Text/images/pic.png");

    // Not found.
    let err = read_image_entry(&path, "nope.png", 1024).unwrap_err();
    assert!(err.to_string().contains("not found"));

    // Empty target after stripping.
    assert!(read_image_entry(&path, "#frag", 1024).is_err());
    assert!(read_image_entry(&path, "/", 1024).is_err());
}

#[test]
fn test_epub_read_image_entry_ambiguous_and_limits() {
    let dir = crate::testutil::unique_temp_dir("epub_img_test2");
    let path = image_test_epub(
        &dir,
        "ambig.epub",
        &[
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            ("a/images/pic.png", b"A".to_vec()),
            ("b/images/pic.png", b"B".to_vec()),
        ],
    );

    // Two suffix matches → ambiguity with candidates.
    let msg = read_image_entry(&path, "images/pic.png", 1024).unwrap_err().to_string();
    assert!(msg.contains("Ambiguous"));
    assert!(msg.contains("a/images/pic.png"));
    assert!(msg.contains("b/images/pic.png"));

    // Literal paths still work.
    let (name, bytes) = read_image_entry(&path, "b/images/pic.png", 1024).unwrap();
    assert_eq!(name, "b/images/pic.png");
    assert_eq!(bytes.as_slice(), b"B");

    // Size limit (entry is 1 byte, limit 0).
    let msg = read_image_entry(&path, "a/images/pic.png", 0).unwrap_err().to_string();
    assert!(msg.contains("too large"));

    // Not a zip archive.
    let not_zip = dir.join("notzip.epub");
    std::fs::write(&not_zip, b"no zip here").unwrap();
    assert!(read_image_entry(&not_zip, "x.png", 1024).is_err());
}

const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles>
<rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
</rootfiles>
</container>"#;

fn write_zip(path: &Path, entries: &[(&str, &str)]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(name, options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// Like `write_zip`, but takes raw bytes so entries can hold non-UTF-8 content.
fn write_zip_bytes(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(name, options).unwrap();
        zip.write_all(content).unwrap();
    }
    zip.finish().unwrap();
}

fn build_epub(path: &Path) {
    // The spine intentionally lists ch2 before ch1 to verify reading order.
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
<metadata>
<dc:title>Test &amp; Demo Book</dc:title>
</metadata>
<manifest>
<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
<item id="ch2" href="Text/ch2.xhtml" media-type="application/xhtml+xml"/>
<item id="img" href="Text/pic.png" media-type="image/png"/>
</manifest>
<spine>
<itemref idref="ch2"/>
<itemref idref="ch1"/>
<itemref idref="img"/>
</spine>
</package>"#;
    let ch1 = r#"<html><body><h1>Chapter One</h1><p>First chapter text.</p></body></html>"#;
    let ch2 = r#"<html><body><h1>Chapter Two</h1><p>Second chapter text.</p></body></html>"#;
    write_zip(
        path,
        &[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            ("OEBPS/Text/ch1.xhtml", ch1),
            ("OEBPS/Text/ch2.xhtml", ch2),
        ],
    );
}

#[tokio::test]
async fn test_epub_non_utf8_html_entry() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("latin1.epub");

    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
<metadata>
<dc:title>Latin Book</dc:title>
</metadata>
<manifest>
<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine>
<itemref idref="ch1"/>
</spine>
</package>"#;

    // An XHTML entry stored in Windows-1252 (not valid UTF-8).
    let ch1 = "<html><head><meta charset=\"windows-1252\"/></head><body><p>Caf\u{e9} r\u{e9}sum\u{e9}</p></body></html>";
    let ch1_bytes = encoding_rs::WINDOWS_1252.encode(ch1).0.into_owned();

    write_zip_bytes(
        &path,
        &[
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            ("OEBPS/content.opf", opf.as_bytes().to_vec()),
            ("OEBPS/Text/ch1.xhtml", ch1_bytes),
        ],
    );

    let data = FileTextData::load(&path).await.unwrap();
    assert!(matches!(data.format, FileFormat::Epub));
    assert!(
        data.text.contains("Caf\u{e9} r\u{e9}sum\u{e9}"),
        "got: {}",
        data.text
    );
}

#[tokio::test]
async fn test_epub_to_markdown() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    build_epub(&path);

    let data = FileTextData::load(&path).await.unwrap();
    assert!(matches!(data.format, FileFormat::Epub));
    assert!(data.text.contains("# Test & Demo Book"));
    assert!(data.text.contains("First chapter text."));
    assert!(data.text.contains("Second chapter text."));
    // Spine order: ch2 comes before ch1 despite the manifest listing ch1 first.
    assert!(
        data.text
            .find("Chapter Two")
            .unwrap()
            .lt(&data.text.find("Chapter One").unwrap())
    );
}

#[tokio::test]
async fn test_epub_absolute_and_fragment_hrefs() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    // Package-absolute href with a leading '/', a '#fragment' suffix,
    // a dangling idref, and a valid idref whose file is absent from the
    // archive — all must be skipped without failing the book.
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
<metadata>
<dc:title>Paths Book</dc:title>
</metadata>
<manifest>
<item id="abs" href="/OEBPS/ch_abs.xhtml" media-type="application/xhtml+xml"/>
<item id="frag" href="ch_frag.xhtml#intro" media-type="application/xhtml+xml"/>
<item id="gone" href="ch_gone.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine>
<itemref idref="missing"/>
<itemref idref="abs"/>
<itemref idref="gone"/>
<itemref idref="frag"/>
</spine>
</package>"#;
    write_zip(
        &path,
        &[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            (
                "OEBPS/ch_abs.xhtml",
                r#"<html><body><h1>Absolute</h1></body></html>"#,
            ),
            (
                "OEBPS/ch_frag.xhtml",
                r#"<html><body><h1>Fragment</h1></body></html>"#,
            ),
        ],
    );

    let data = FileTextData::load(&path).await.unwrap();
    assert!(data.text.contains("Absolute"));
    assert!(data.text.contains("Fragment"));
}

#[tokio::test]
async fn test_epub_missing_container_errors() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    write_zip(
        &path,
        &[(
            "OEBPS/ch1.xhtml",
            r#"<html><body><p>No container here.</p></body></html>"#,
        )],
    );

    let err = match FileTextData::load(&path).await {
        Ok(_) => panic!("expected loading an EPUB without container.xml to fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("container.xml"));
}

#[tokio::test]
async fn test_epub_root_level_opf() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    // The OPF sits at the archive root, so relative hrefs resolve directly.
    let container = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles>
<rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
</rootfiles>
</container>"#;
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
<metadata>
<dc:title>Root Book</dc:title>
</metadata>
<manifest>
<item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine>
<itemref idref="ch1"/>
</spine>
</package>"#;
    write_zip(
        &path,
        &[
            ("META-INF/container.xml", container),
            ("content.opf", opf),
            (
                "ch1.xhtml",
                r#"<html><body><h1>Root Chapter</h1></body></html>"#,
            ),
        ],
    );

    let data = FileTextData::load(&path).await.unwrap();
    assert!(data.text.contains("Root Chapter"));
}

#[tokio::test]
async fn test_epub_empty_spine_errors() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
<manifest>
<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine></spine>
</package>"#;
    write_zip(
        &path,
        &[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
        ],
    );

    let err = match FileTextData::load(&path).await {
        Ok(_) => panic!("expected loading an EPUB with an empty spine to fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("spine"));
}

#[tokio::test]
async fn test_epub_title_with_nested_markup() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    // The title may contain nested markup and entities; both must be
    // stripped/decoded when the H1 is prepended.
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
<metadata>
<dc:title><span>Part One</span> &amp; More</dc:title>
</metadata>
<manifest>
<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine>
<itemref idref="ch1"/>
</spine>
</package>"#;
    write_zip(
        &path,
        &[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            (
                "OEBPS/Text/ch1.xhtml",
                r#"<html><body><p>Body text.</p></body></html>"#,
            ),
        ],
    );

    let data = FileTextData::load(&path).await.unwrap();
    assert!(data.text.contains("# Part One & More"));
    let structure = data.structure();
    assert_eq!(structure.headers[0].text, "Part One & More");
}

#[tokio::test]
async fn test_epub_missing_title() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    // No <metadata> at all: the book title H1 must not be prepended.
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
<manifest>
<item id="ch1" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
</manifest>
<spine>
<itemref idref="ch1"/>
</spine>
</package>"#;
    write_zip(
        &path,
        &[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            (
                "OEBPS/Text/ch1.xhtml",
                r#"<html><body><h1>Only Chapter</h1></body></html>"#,
            ),
        ],
    );

    let data = FileTextData::load(&path).await.unwrap();
    // No book title in the structure: only the chapter heading.
    let structure = data.structure();
    let texts: Vec<&str> = structure.headers.iter().map(|h| h.text.as_str()).collect();
    assert_eq!(texts, vec!["Only Chapter"]);
}

#[tokio::test]
async fn test_epub_missing_opf_errors() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    // The container points to an OPF that is absent from the archive.
    write_zip(&path, &[("META-INF/container.xml", CONTAINER)]);

    let err = match FileTextData::load(&path).await {
        Ok(_) => panic!("expected loading an EPUB without an OPF file to fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("content.opf"));
}

#[tokio::test]
async fn test_epub_structure() {
    let dir = crate::testutil::unique_temp_dir("epub_test");
    let path = dir.join("book.epub");
    build_epub(&path);

    let data = FileTextData::load(&path).await.unwrap();
    let structure = data.structure();
    let texts: Vec<&str> = structure.headers.iter().map(|h| h.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["Test & Demo Book", "Chapter Two", "Chapter One"]
    );
    assert!(structure.total_lines > 3);
}
