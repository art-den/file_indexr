/// Truncate raw bytes to at most `max_bytes`, returning the longest valid UTF-8
/// prefix not exceeding `max_bytes` (may be empty).
pub fn truncate_bytes(bytes: &[u8], max_bytes: usize) -> &str {
    let limit = usize::min(bytes.len(), max_bytes);
    match std::str::from_utf8(&bytes[..limit]) {
        Ok(s) => s,
        Err(err) => std::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_bytes_ascii() {
        let bytes = b"hello world";
        assert_eq!(truncate_bytes(bytes, 5), "hello");
    }

    #[test]
    fn test_truncate_bytes_no_truncation() {
        let bytes = b"hello";
        assert_eq!(truncate_bytes(bytes, 20), "hello");
    }

    #[test]
    fn test_truncate_bytes_emoji_cut() {
        // '😀' = 4 bytes (0xF0 0x9F 0x98 0x80).
        // "abc" (3B) + 😀 (4B) + "def" (3B) = 10B.
        // max=5 → cuts inside emoji → should keep "abc".
        let data = b"abc\xF0\x9F\x98\x80def";
        assert_eq!(truncate_bytes(data, 5), "abc");
    }

    #[test]
    fn test_truncate_bytes_valid_prefix() {
        // "hello" (5B) + Я (2B, 0xD0 0xAF) + " world" (6B) = 13B.
        // max=7 → reads "helloЯ" → valid UTF-8.
        let data = b"hello\xD0\xAF world";
        assert_eq!(truncate_bytes(data, 7), "helloЯ");
    }

    #[test]
    fn test_truncate_bytes_empty() {
        let data: &[u8] = &[];
        assert_eq!(truncate_bytes(data, 10), "");
    }

    #[test]
    fn test_truncate_bytes_cyrillic_cut() {
        // 'Я' = 2 bytes. File: "hello" (5B) + Я (2B) + "world" (5B).
        // max=6 → cuts inside Я → should keep only "hello".
        let data = b"hello\xD0\xAFworld";
        assert_eq!(truncate_bytes(data, 6), "hello");
    }

    #[test]
    fn test_truncate_bytes_invalid_utf8() {
        // 0xFF is never valid UTF-8: only the longest valid prefix is kept.
        let data = b"ab\xFFcd";
        assert_eq!(truncate_bytes(data, 10), "ab");
        assert_eq!(truncate_bytes(data, 3), "ab");
        assert_eq!(truncate_bytes(&[0xFFu8, 0xFE], 10), "");
    }
}
