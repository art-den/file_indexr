use std::collections::HashMap;
use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use tracing::warn;
use zip::ZipArchive;

#[cfg(test)]
#[path = "tests/tests_epub.rs"]
mod tests;

/// Location of the OPC container descriptor inside an EPUB archive.
const CONTAINER_PATH: &str = "META-INF/container.xml";

/// Maximum number of ambiguous-match candidates listed in an error message.
const MAX_CANDIDATES_SHOWN: usize = 5;

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
}

static OPF_PATTERNS: LazyLock<OpfPatterns> = LazyLock::new(|| OpfPatterns {
    item: Regex::new(r"<item\b[^>]*/?>").unwrap(),
    itemref: Regex::new(r"<itemref\b[^>]*/?>").unwrap(),
    title: Regex::new(r"<(?:[\w-]+:)?title[^>]*>(.*?)</(?:[\w-]+:)?title>").unwrap(),
});

/// Strips XML tags and decodes the five predefined XML entities in a single
/// left-to-right pass; tags are replaced with a space so adjacent words stay
/// separated.
static TITLE_CLEANUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>|&lt;|&gt;|&quot;|&apos;|&amp;").unwrap());

/// Load EPUB file and convert to Markdown.
pub async fn load_from_file_and_convert_to_md(file_path: &Path) -> anyhow::Result<String> {
    let path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let buffer = std::fs::read(&path)?;
        to_markdown(&buffer)
    })
    .await?
}

/// Read an image from an EPUB archive by document-relative path.
///
/// `inner_path` is looked up literally first (a leading '/' and any
/// `#fragment` are stripped). If no entry matches, a suffix match against
/// entry names is attempted: documents reference images relative to their
/// own location, so `images/pic.png` usually resolves to e.g.
/// `Text/images/pic.png`. A unique suffix match is returned; multiple
/// matches are reported together with the candidates.
///
/// Returns the resolved entry name and the image bytes.
pub fn read_image_entry(
    archive_path: &Path,
    inner_path: &str,
    max_bytes: u64,
) -> anyhow::Result<(String, Vec<u8>)> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| anyhow::anyhow!("Failed to open EPUB {}: {e}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file).map_err(|_| {
        anyhow::anyhow!("Not a valid EPUB (zip) archive: {}", archive_path.display())
    })?;

    let target = inner_path.split('#').next().unwrap().trim_start_matches('/');
    if target.is_empty() {
        return Err(anyhow::anyhow!("Empty image path in EPUB archive"));
    }

    // Entry names are opaque zip strings and cannot escape the archive.
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
        .collect();

    // 1) Literal match. 2) Unique suffix match.
    let resolved = if let Some(name) = names.iter().find(|n| n.as_str() == target) {
        name.clone()
    } else {
        let suffix = format!("/{target}");
        let matches: Vec<&str> = names
            .iter()
            .filter(|n| n.ends_with(&suffix))
            .map(|s| s.as_str())
            .collect();
        match matches.len() {
            0 => {
                return Err(anyhow::anyhow!(
                    "Image '{inner_path}' not found in EPUB archive"
                ))
            }
            1 => matches[0].to_string(),
            n => {
                let shown = matches
                    .iter()
                    .take(MAX_CANDIDATES_SHOWN)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(anyhow::anyhow!(
                    "Ambiguous image path '{inner_path}' in EPUB archive: {n} candidates ({shown})"
                ));
            }
        }
    };

    let entry = archive.by_name(&resolved)?;
    // The declared size comes from the (untrusted) central directory: use it
    // only as an early-out and a capacity hint; the actual read is bounded.
    let declared = entry.size();
    if declared > max_bytes {
        return Err(anyhow::anyhow!(
            "Image '{}' is too large ({} bytes, limit {} bytes)",
            resolved,
            declared,
            max_bytes
        ));
    }
    // Here `declared <= max_bytes`, so the capacity stays bounded by the limit.
    let mut buf = Vec::with_capacity(usize::try_from(declared).unwrap_or(usize::MAX));
    entry
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(anyhow::anyhow!(
            "Image '{}' is too large (more than {} bytes)",
            resolved,
            max_bytes
        ));
    }
    Ok((resolved, buf))
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
        .map(|p| p.to_string_lossy().into_owned())
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
    attr(tag, "full-path").ok_or_else(|| anyhow::anyhow!("No full-path attribute in <rootfile>"))
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
    let inner = OPF_PATTERNS.title.captures(opf)?.get(1)?;
    let cleaned = TITLE_CLEANUP.replace_all(inner.as_str(), |c: &regex::Captures| {
        match c.get(0).unwrap().as_str() {
            "&lt;" => "<",
            "&gt;" => ">",
            "&quot;" => "\"",
            "&apos;" => "'",
            "&amp;" => "&",
            _ => " ", // XML tag
        }
    });
    let text = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
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
        absolute.to_string()
    } else if opf_dir.is_empty() {
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
fn find_element_tag<'a>(xml: &'a str, tag: &str) -> anyhow::Result<&'a str> {
    let open = find_element_open(xml, tag)?;
    let end = xml[open..]
        .find('>')
        .ok_or_else(|| anyhow::anyhow!("Unclosed <{tag}> tag in package metadata"))?;
    Ok(&xml[open..open + end + 1])
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
