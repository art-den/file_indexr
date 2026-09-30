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
