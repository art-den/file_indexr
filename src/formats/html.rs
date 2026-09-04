use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

use html_to_markdown_rs::ConversionOptions;
use regex::bytes::Regex;

/// Maximum number of leading bytes inspected when sniffing a declared charset,
/// as prescribed by the WHATWG encoding specification.
const CHARSET_SNIFF_LIMIT: usize = 1024;

/// Matches the `charset` declaration inside a `<meta>` tag. This covers both
/// `<meta charset="...">` and `<meta http-equiv="content-type" content="...; charset=...">`.
/// The lazy `[^>]*?` makes the first `charset=` in the tag win (the real declaration)
/// while still reaching across the `content` attribute to the `charset=` keyword.
static META_CHARSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<meta[^>]*?charset\s*=\s*["']?([^"'\s;>]+)"#).unwrap());

/// Load HTML file and convert to Markdown.
pub async fn load_from_file_and_convert_to_md(file_name: &Path) -> anyhow::Result<String> {
    let path = file_name.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let buffer = std::fs::read(&path)?;
        to_markdown(&decode_html_to_utf8(&buffer))
    })
    .await?
}

/// Convert an HTML document to Markdown.
pub(crate) fn to_markdown(html: &str) -> anyhow::Result<String> {
    let options = ConversionOptions {
        extract_metadata: false,
        ..ConversionOptions::default()
    };
    Ok(html_to_markdown_rs::convert(html, options)?.content.unwrap_or_default())
}

/// Decode raw HTML bytes into UTF-8, borrowing from `bytes` when it is already
/// valid UTF-8 to avoid a copy.
///
/// HTML is not guaranteed to be UTF-8, so we:
/// 1. take a fast path when the buffer is already valid UTF-8,
/// 2. otherwise honor a byte-order mark or an inline `<meta>` charset declaration,
/// 3. otherwise fall back to Windows-1252, the WHATWG default and the most common
///    non-UTF-8 encoding for real-world HTML.
pub(crate) fn decode_html_to_utf8(bytes: &[u8]) -> Cow<'_, str> {
    // Fast path: already valid UTF-8.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(text);
    }

    let (encoding, payload) = detect_encoding(bytes);
    let (cow, _, _) = encoding.decode(payload);
    cow
}

/// Resolve the encoding for a non-UTF-8 buffer and the byte slice to decode.
/// A BOM takes precedence over any inline declaration and is stripped from the payload.
fn detect_encoding(bytes: &[u8]) -> (&'static encoding_rs::Encoding, &[u8]) {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return (encoding, &bytes[bom_len..]);
    }

    // `_no_replacement` so an unrecognized label hits the Windows-1252 fallback
    // rather than decoding with the replacement (all-U+FFFD) encoding.
    let encoding = declared_charset(bytes)
        .and_then(|label| encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()))
        .unwrap_or(encoding_rs::WINDOWS_1252);
    (encoding, bytes)
}

/// Look up a charset declared in the document head, if any.
fn declared_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(CHARSET_SNIFF_LIMIT)];

    META_CHARSET_RE
        .captures(head)
        .map(|m| String::from_utf8_lossy(&m[1]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `text` into `encoding` (no BOM is added; the BOM test builds its own payload).
    fn encode(text: &str, encoding: &'static encoding_rs::Encoding) -> Vec<u8> {
        let (cow, _, _) = encoding.encode(text);
        cow.into_owned()
    }

    #[test]
    fn test_valid_utf8_passthrough() {
        let html = "<html><body>Caf\u{e9} r\u{e9}sum\u{e9} na\u{ef}ve</body></html>";
        let decoded = decode_html_to_utf8(html.as_bytes());
        assert_eq!(decoded, html);
    }

    #[test]
    fn test_meta_charset_windows_1252() {
        let html = "<html><head><meta charset=\"windows-1252\"></head><body>Caf\u{e9} r\u{e9}sum\u{e9}</body></html>";
        let bytes = encode(html, encoding_rs::WINDOWS_1252);
        assert!(std::str::from_utf8(&bytes).is_err());
        let decoded = decode_html_to_utf8(&bytes);
        assert!(decoded.contains("Caf\u{e9} r\u{e9}sum\u{e9}"));
    }

    #[test]
    fn test_meta_content_type_shift_jis() {
        let marker = "\u{3053}\u{3093}\u{306b}\u{3061}\u{306a}"; // Japanese: "konnichiwa"
        let html = "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=shift_jis\"></head><body>".to_string() + marker + "</body></html>";
        let bytes = encode(&html, encoding_rs::SHIFT_JIS);
        assert!(std::str::from_utf8(&bytes).is_err());
        let decoded = decode_html_to_utf8(&bytes);
        assert!(decoded.contains(marker));
    }

    #[test]
    fn test_utf16le_bom() {
        // This crate exposes no UTF-16LE encoder, so build the payload by hand.
        let html = "<html><body>Caf\u{e9} r\u{e9}sum\u{e9}</body></html>";
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
        bytes.extend(html.encode_utf16().flat_map(u16::to_le_bytes));
        assert!(std::str::from_utf8(&bytes).is_err());
        let decoded = decode_html_to_utf8(&bytes);
        assert!(decoded.contains("Caf\u{e9} r\u{e9}sum\u{e9}"));
    }

    #[test]
    fn test_fallback_to_windows_1252() {
        // No BOM, no declared charset — must fall back to Windows-1252.
        let html = "<html><body>\u{a9} 2026 \u{b0}C</body></html>";
        let bytes = encode(html, encoding_rs::WINDOWS_1252);
        assert!(std::str::from_utf8(&bytes).is_err());
        let decoded = decode_html_to_utf8(&bytes);
        assert!(decoded.contains("\u{a9} 2026 \u{b0}C"));
    }
}
