//! Decoding file contents for display.
//!
//! Git stores bytes, not text. A Japanese repository may keep Shift_JIS or
//! EUC-JP blobs, or a UTF-16 file with a BOM, and the UI still has to show
//! readable lines. This module detects the encoding so the diff, blame, file
//! viewer and merge editor can decode for display.
//!
//! Decoding is display-only: writes and patches keep the original bytes (see
//! `diff::diff_bytes` and `patch::build_patch_bytes`), so staging a hunk never
//! re-encodes a file behind the user's back.

use std::borrow::Cow;

use encoding_rs::Encoding;

/// The encoding a body of text was decoded as.
///
/// This wraps a static [`Encoding`], so it is `Copy` and cheap to pass around
/// with a decoded string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextEncoding(&'static Encoding);

impl TextEncoding {
    pub const UTF_8: Self = Self(encoding_rs::UTF_8);
    pub const UTF_16LE: Self = Self(encoding_rs::UTF_16LE);
    pub const UTF_16BE: Self = Self(encoding_rs::UTF_16BE);

    /// The name git and encoding_rs use, such as `UTF-8` or `Shift_JIS`.
    pub fn label(self) -> &'static str {
        self.0.name()
    }

    pub fn is_utf8(self) -> bool {
        self.0 == encoding_rs::UTF_8
    }

    pub fn is_utf16(self) -> bool {
        self.0 == encoding_rs::UTF_16LE || self.0 == encoding_rs::UTF_16BE
    }

    /// Resolve a `working-tree-encoding` value, such as `Shift_JIS`.
    pub fn from_label(label: &str) -> Option<Self> {
        Encoding::for_label(label.trim().as_bytes()).map(Self)
    }

    /// Decode `bytes`, replacing invalid sequences with `U+FFFD`.
    ///
    /// A BOM that matches this encoding is removed, as `encoding_rs` documents.
    pub fn decode(self, bytes: &[u8]) -> Cow<'_, str> {
        let (text, _, _) = self.0.decode(bytes);
        text
    }

    /// Encode text back to this encoding, for a merge editor that writes the
    /// decoded result out again.
    ///
    /// `encoding_rs` treats UTF-16 as decode-only, so the two UTF-16 forms are
    /// encoded here, with the BOM that made them detectable in the first place.
    pub fn encode(self, text: &str) -> Vec<u8> {
        if self == Self::UTF_16LE || self == Self::UTF_16BE {
            let little_endian = self == Self::UTF_16LE;
            let mut bytes = if little_endian {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            for unit in text.encode_utf16() {
                let pair = if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
            return bytes;
        }
        let (bytes, _, _) = self.0.encode(text);
        bytes.into_owned()
    }
}

impl Default for TextEncoding {
    fn default() -> Self {
        Self::UTF_8
    }
}

/// Decode `bytes` for display, preferring the repository's
/// `working-tree-encoding` when the bytes are not already valid UTF-8.
///
/// `attribute` is called only when detection needs it, so the common UTF-8
/// path never reads `.gitattributes`.
pub fn decode_for_display<'a>(
    bytes: &'a [u8],
    attribute: impl FnOnce() -> Option<String>,
) -> (Cow<'a, str>, TextEncoding) {
    if let Some((encoding, skip)) = bom(bytes) {
        return (encoding.decode(&bytes[skip..]), encoding);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (Cow::Borrowed(text), TextEncoding::UTF_8);
    }
    if let Some(encoding) = attribute().as_deref().and_then(TextEncoding::from_label) {
        return (encoding.decode(bytes), encoding);
    }

    let encoding = guess(bytes);
    (encoding.decode(bytes), encoding)
}

/// A legacy encoding, guessed from the bytes.
///
/// `allow_utf8` is false because the caller already proved the bytes are not
/// UTF-8; a UTF-8 answer here would only produce replacement characters.
fn guess(bytes: &[u8]) -> TextEncoding {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Deny);
    TextEncoding(encoding)
}

/// The BOM at the start of `bytes`, with its length.
fn bom(bytes: &[u8]) -> Option<(TextEncoding, usize)> {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        Some((TextEncoding::UTF_8, 3))
    } else if bytes.starts_with(b"\xFF\xFE") {
        Some((TextEncoding::UTF_16LE, 2))
    } else if bytes.starts_with(b"\xFE\xFF") {
        Some((TextEncoding::UTF_16BE, 2))
    } else {
        None
    }
}

/// True when `bytes` are plain ASCII, which every candidate encoding shares.
pub fn is_ascii(bytes: &[u8]) -> bool {
    bytes.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shift_jis(text: &str) -> Vec<u8> {
        TextEncoding::from_label("Shift_JIS")
            .expect("Shift_JIS")
            .encode(text)
    }

    #[test]
    fn valid_utf8_borrows_and_needs_no_attribute() {
        let (text, encoding) = decode_for_display("日本語\n".as_bytes(), || {
            panic!("attributes must not be read for valid UTF-8")
        });
        assert_eq!(text, "日本語\n");
        assert!(matches!(text, Cow::Borrowed(_)));
        assert_eq!(encoding, TextEncoding::UTF_8);
        assert_eq!(encoding.label(), "UTF-8");
    }

    #[test]
    fn detects_and_strips_boms() {
        let utf16 = TextEncoding::UTF_16LE.encode("日本語\n");
        let (text, encoding) = decode_for_display(&utf16, || None);
        assert_eq!(text, "日本語\n");
        assert_eq!(encoding, TextEncoding::UTF_16LE);
        assert_eq!(encoding.label(), "UTF-16LE");

        let be = TextEncoding::UTF_16BE.encode("日本語\n");
        let (text, encoding) = decode_for_display(&be, || None);
        assert_eq!(text, "日本語\n");
        assert_eq!(encoding, TextEncoding::UTF_16BE);
    }

    #[test]
    fn honours_the_working_tree_encoding_attribute() {
        let bytes = shift_jis("日本語のテスト\n");
        assert!(std::str::from_utf8(&bytes).is_err());
        let (text, encoding) = decode_for_display(&bytes, || Some("Shift_JIS".to_owned()));
        assert_eq!(text, "日本語のテスト\n");
        assert_eq!(encoding.label(), "Shift_JIS");
    }

    #[test]
    fn guesses_shift_jis_without_an_attribute() {
        let bytes = shift_jis("これは日本語の文章です。文字コードを判定します。\n");
        let (text, encoding) = decode_for_display(&bytes, || None);
        assert_eq!(text, "これは日本語の文章です。文字コードを判定します。\n");
        assert_eq!(encoding.label(), "Shift_JIS");
    }

    #[test]
    fn an_unknown_attribute_falls_back_to_detection() {
        let bytes = shift_jis("これは日本語の文章です。文字コードを判定します。\n");
        let (text, encoding) = decode_for_display(&bytes, || Some("not-a-charset".to_owned()));
        assert_eq!(text, "これは日本語の文章です。文字コードを判定します。\n");
        assert!(!encoding.is_utf8());
    }

    #[test]
    fn encoding_round_trips_legacy_text() {
        let encoded = shift_jis("日本語");
        let decoded = TextEncoding::from_label("Shift_JIS")
            .unwrap()
            .decode(&encoded);
        assert_eq!(decoded, "日本語");
        assert_eq!(
            TextEncoding::from_label("Shift_JIS")
                .unwrap()
                .encode(&decoded),
            encoded
        );
    }

    #[test]
    fn ascii_is_utf8_and_stays_borrowed() {
        let (text, encoding) = decode_for_display(b"plain ascii\n", || None);
        assert_eq!(text, "plain ascii\n");
        assert_eq!(encoding, TextEncoding::UTF_8);
        assert!(is_ascii(b"plain ascii\n"));
    }
}
