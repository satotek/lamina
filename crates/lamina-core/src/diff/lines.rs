//! Splitting file contents into lines.
//!
//! The line ending is kept out of `content` (the UI never wants to render a
//! stray `\r`) but is still part of the diff token, so switching CRLF to LF is
//! reported as a change and a missing final newline is not silently ignored.
//!
//! Splitting works on bytes because a file need not be UTF-8: a Shift_JIS
//! checkout still has one `\n` per line, and patch generation needs the
//! original bytes back. Display decoding happens after the diff.

/// How many leading bytes are inspected before declaring a file binary.
const BINARY_PROBE: usize = 8000;

#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    /// The line without `\n` or `\r\n`.
    pub content: &'a str,
    /// True when this is the last line of a file that does not end with `\n`.
    pub no_newline: bool,
}

/// A line of bytes, as the diff engine sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ByteLine<'a> {
    /// The line including its terminator.
    pub raw: &'a [u8],
    /// The line without `\n` or `\r\n`.
    pub content: &'a [u8],
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
        let mut content = &text[start..idx];
        if content.ends_with('\r') {
            content = &content[..content.len() - 1];
        }
        out.push(Line {
            content,
            no_newline: false,
        });
        start = idx + 1;
    }
    if start < text.len() {
        out.push(Line {
            content: &text[start..],
            no_newline: true,
        });
    }
    out
}

/// [`split`] for bytes that need not be UTF-8.
pub(crate) fn split_bytes(bytes: &[u8]) -> Vec<ByteLine<'_>> {
    let mut out = Vec::with_capacity(bytes.iter().filter(|byte| **byte == b'\n').count() + 1);
    let mut start = 0;
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        let raw = &bytes[start..=idx];
        let mut content = &bytes[start..idx];
        if content.ends_with(b"\r") {
            content = &content[..content.len() - 1];
        }
        out.push(ByteLine {
            raw,
            content,
            no_newline: false,
        });
        start = idx + 1;
    }
    if start < bytes.len() {
        let raw = &bytes[start..];
        out.push(ByteLine {
            raw,
            content: raw,
            no_newline: true,
        });
    }
    out
}
