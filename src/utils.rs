#[cfg(test)]
#[path = "tests/tests_utils.rs"]
mod tests;

/// Truncate raw bytes to at most `max_bytes`, returning the longest valid UTF-8
/// prefix not exceeding `max_bytes` (may be empty).
pub fn truncate_bytes(bytes: &[u8], max_bytes: usize) -> &str {
    let limit = usize::min(bytes.len(), max_bytes);
    match std::str::from_utf8(&bytes[..limit]) {
        Ok(s) => s,
        Err(err) => std::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap(),
    }
}
