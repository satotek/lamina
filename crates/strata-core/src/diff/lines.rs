//! Splitting file contents into lines.
//!
//! The line ending is kept out of `content` (the UI never wants to render a
//! stray `\r`) but is still part of the diff token, so switching CRLF to LF is
//! reported as a change and a missing final newline is not silently ignored.

/// How many leading bytes are inspected before declaring a file binary.
const BINARY_PROBE: usize = 8000;

#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    /// The line including its terminator; used to check that "context" really
    /// is byte-identical on both sides.
    pub raw: &'a str,
    /// The line without `\n` or `\r\n`.
    pub content: &'a str,
    /// True when this is the last line of a file that does not end with `\n`.
    pub no_newline: bool,
}

/// Git calls a file binary when a NUL byte appears early in it. Checking only
/// the head keeps working on a huge file after the answer is already clear.
pub fn is_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(BINARY_PROBE)];
    head.contains(&0)
}

/// Split `text` into lines, keeping the terminator information.
pub fn split(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::with_capacity(text.lines().count().max(1));
    let bytes = text.as_bytes();
    let mut start = 0;
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        let raw = &text[start..=idx];
        let mut content = &text[start..idx];
        if content.ends_with('\r') {
            content = &content[..content.len() - 1];
        }
        out.push(Line {
            raw,
            content,
            no_newline: false,
        });
        start = idx + 1;
    }
    if start < text.len() {
        let raw = &text[start..];
        out.push(Line {
            raw,
            content: raw,
            no_newline: true,
        });
    }
    out
}
