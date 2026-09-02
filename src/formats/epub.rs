use std::collections::HashMap;
use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use tracing::warn;
use zip::ZipArchive;

/// Location of the OPC container descriptor inside an EPUB archive.
const CONTAINER_PATH: &str = "META-INF/container.xml";

/// Compiled regexes for the fixed set of OPF attributes we extract.
static ATTR_REGEXES: LazyLock<HashMap<&'static str, Regex>> = LazyLock::new(|| {
    ["id", "href", "media-type", "idref", "full-path"]
        .iter()
        .map(|name| {
            (
                *name,
                Regex::new(&format!(r#"\b{name}\s*=\s*(?:"([^"]*)"|'([^']*)')"#)).unwrap(),
            )
        })
        .collect()
});

/// Compiled regexes for the fixed OPF structural patterns.
struct OpfPatterns {
    item: Regex,
    itemref: Regex,
    title: Regex,
    tag: Regex,
}

static OPF_PATTERNS: LazyLock<OpfPatterns> = LazyLock::new(|| OpfPatterns {
    item: Regex::new(r"<item\b[^>]*/?>").unwrap(),
    itemref: Regex::new(r"<itemref\b[^>]*/?>").unwrap(),
    title: Regex::new(r"<(?:[\w-]+:)?title[^>]*>(.*?)</(?:[\w-]+:)?title>").unwrap(),
    tag: Regex::new(r"<[^>]+>").unwrap(),
});

/// Load EPUB file and convert to Markdown.
pub async fn load_from_file_and_convert_to_md(file_path: &Path) -> anyhow::Result<String> {
    let path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let buffer = std::fs::read(&path)?;
        to_markdown(&buffer)
    })
    .await?
}

/// Extract the EPUB spine documents in reading order and convert them to Markdown.
fn to_markdown(buffer: &[u8]) -> anyhow::Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(buffer))?;

    // OPC container points to the OPF package file (usually "OEBPS/content.opf").
    let container = read_entry(&mut archive, CONTAINER_PATH)?;
    let opf_path = container_rootfile(&container)?;
    let opf = read_entry(&mut archive, &opf_path)?;
    let (manifest, spine) = parse_opf(&opf)?;
    let opf_dir = Path::new(&opf_path)
        .parent()
        .map(Path::to_string_lossy)
        .map(|s| s.into_owned())
        .unwrap_or_default();

    let mut out = String::new();
    if let Some(title) = parse_title(&opf) {
        out.push_str("# ");
        out.push_str(&title);
        out.push_str("\n\n");
    }
    for idref in spine {
        let Some(item) = manifest.get(&idref) else {
            warn!("EPUB spine references unknown item id '{idref}'");
            continue;
        };
        if !item.html {
            continue;
        }
        let entry = resolve_entry_path(&opf_dir, &item.href);
        let Ok(html_bytes) = read_entry_bytes(&mut archive, &entry) else {
            warn!("EPUB spine entry '{entry}' is missing from the archive");
            continue;
        };
        let html = super::html::decode_html_to_utf8(&html_bytes);
        let Ok(markdown) = super::html::to_markdown(&html) else {
            warn!("Failed to convert EPUB entry '{entry}' to Markdown, skipping");
            continue;
        };
        out.push_str(&markdown);
        out.push_str("\n\n");
    }
    Ok(out)
}

/// Read an archive entry as raw bytes.
fn read_entry_bytes<R>(archive: &mut ZipArchive<R>, name: &str) -> anyhow::Result<Vec<u8>>
where
    R: Read + Seek,
{
    let mut entry = archive
        .by_name(name)
        .map_err(|e| anyhow::anyhow!("Entry '{name}' not found in EPUB archive: {e}"))?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read an archive entry as a UTF-8 string (OPF/container documents are UTF-8 per spec).
fn read_entry<R>(archive: &mut ZipArchive<R>, name: &str) -> anyhow::Result<String>
where
    R: Read + Seek,
{
    let buf = read_entry_bytes(archive, name)?;
    Ok(String::from_utf8(buf).unwrap_or_else(|e| {
        // Fall back to lossy decoding for non-UTF-8 entries.
        String::from_utf8_lossy(e.as_bytes()).into_owned()
    }))
}

/// `full-path` attribute of the `<rootfile>` element in `META-INF/container.xml`.
fn container_rootfile(xml: &str) -> anyhow::Result<String> {
    let tag = find_element_tag(xml, "rootfile")?;
    attr(&tag, "full-path").ok_or_else(|| anyhow::anyhow!("No full-path attribute in <rootfile>"))
}

/// Parse the OPF package file into manifest items keyed by id and the spine reading order.
fn parse_opf(xml: &str) -> anyhow::Result<(HashMap<String, OpfItem>, Vec<String>)> {
    let manifest_region = element_region(xml, "manifest")?;
    let mut manifest: HashMap<String, OpfItem> = HashMap::new();
    for tag in OPF_PATTERNS.item.find_iter(manifest_region) {
        let tag = tag.as_str();
        let (Some(id), Some(href)) = (attr(tag, "id"), attr(tag, "href")) else {
            continue;
        };
        let html = matches!(
            attr(tag, "media-type").as_deref(),
            Some("application/xhtml+xml" | "text/html")
        );
        manifest.insert(id, OpfItem { href, html });
    }

    let spine_region = element_region(xml, "spine")?;
    let spine: Vec<String> = OPF_PATTERNS
        .itemref
        .find_iter(spine_region)
        .filter_map(|m| attr(m.as_str(), "idref"))
        .collect();

    if spine.is_empty() {
        return Err(anyhow::anyhow!("No spine entries in the OPF package file"));
    }
    Ok((manifest, spine))
}

/// Book title from the OPF `<metadata>` element, if present.
fn parse_title(opf: &str) -> Option<String> {
    let inner = OPF_PATTERNS.title.captures(opf)?.get(1)?.as_str();
    let unescaped = unescape_xml(&OPF_PATTERNS.tag.replace_all(inner, " "));
    let text = unescaped.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

struct OpfItem {
    href: String,
    /// True when the manifest item is an HTML/XHTML document.
    html: bool,
}

/// Resolve a manifest `href` against the directory containing the OPF file.
/// A leading '/' marks a package-absolute path, resolved against the archive
/// root; any '#fragment' suffix is stripped.
fn resolve_entry_path(opf_dir: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap();
    if let Some(absolute) = href.strip_prefix('/') {
        return absolute.to_string();
    }
    if opf_dir.is_empty() {
        href.to_string()
    } else {
        format!("{opf_dir}/{href}")
    }
}

/// Index of the `<` of the first opening tag for `tag` (namespace prefix ignored).
fn find_element_open(xml: &str, tag: &str) -> anyhow::Result<usize> {
    for (i, c) in xml.char_indices() {
        if c != '<' {
            continue;
        }
        let rest = &xml[i + 1..];
        let name = rest
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != ':')
            .next()
            .unwrap_or_default();
        let local = name.rsplit(':').next().unwrap_or_default();
        if local == tag {
            return Ok(i);
        }
    }
    Err(anyhow::anyhow!("No <{tag}> element in package metadata"))
}

/// The first opening tag for `tag`, e.g. `<item id="a" ... />`.
fn find_element_tag(xml: &str, tag: &str) -> anyhow::Result<String> {
    let open = find_element_open(xml, tag)?;
    let tag_text = &xml[open..];
    let end = tag_text
        .find('>')
        .ok_or_else(|| anyhow::anyhow!("Unclosed <{tag}> tag in package metadata"))?;
    Ok(tag_text[..end + 1].to_string())
}

/// The opening tag and inner content of the first `<tag>...</tag>` element
/// (up to but excluding the closing tag), searching from the start of `xml`.
fn element_region<'a>(xml: &'a str, tag: &str) -> anyhow::Result<&'a str> {
    let open = find_element_open(xml, tag)?;
    let close_rel = xml[open..]
        .find(&format!("</{tag}>"))
        .ok_or_else(|| anyhow::anyhow!("Unclosed <{tag}> element in package metadata"))?;
    Ok(&xml[open..open + close_rel])
}

/// Extract an attribute value from an opening tag, handling both quote styles
/// and unescaping XML entities in the value.
fn attr(tag: &str, name: &str) -> Option<String> {
    let re = ATTR_REGEXES.get(name)?;
    re.captures(tag)
        .and_then(|c| c.get(1).or(c.get(2)))
        .map(|m| unescape_xml(m.as_str()))
}

/// The five predefined XML entities, decoded left-to-right in a single pass.
static XML_ENTITIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&lt;|&gt;|&quot;|&apos;|&amp;").unwrap());

fn unescape_xml(s: &str) -> String {
    XML_ENTITIES
        .replace_all(s, |c: &regex::Captures| match c.get(0).unwrap().as_str() {
            "&lt;" => "<",
            "&gt;" => ">",
            "&quot;" => "\"",
            "&apos;" => "'",
            _ => "&",
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{FileFormat, FileTextData};
    use std::io::Write;

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
}
