//! Display widths for text that has to fit a column budget.
//!
//! `str::len` counts bytes and `chars().count()` counts code points; neither
//! is how wide a string draws. Japanese and Chinese characters occupy two
//! columns, emoji usually two, and combining marks none. Truncation for a
//! tooltip, a status line or a path has to use the drawing width or a Japanese
//! name overflows while an ASCII one is cut too short.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The number of terminal/font columns `text` occupies.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// The number of columns `text` occupies once tabs expand to the next stop.
///
/// A pane that has to reserve room for its widest line measures the same way
/// the text is drawn: a tab advances to the next multiple of `tab_width`
/// instead of counting as one column.
pub fn display_width_with_tabs(text: &str, tab_width: usize) -> usize {
    let tab_width = tab_width.max(1);
    let mut columns = 0;
    for character in text.chars() {
        if character == '\t' {
            columns += tab_width - columns % tab_width;
        } else {
            columns += UnicodeWidthChar::width(character).unwrap_or(0);
        }
    }
    columns
}

/// `text` cut to at most `max` columns, with `…` marking the cut.
///
/// Never splits a character: a wide character that does not fit is dropped,
/// and zero-width marks directly before the cut stay with their base. A
/// string that already fits is returned unchanged (no ellipsis).
pub fn truncate_width(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    // The ellipsis itself takes a column.
    let budget = max - 1;
    let mut out = String::new();
    let mut width = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > budget {
            break;
        }
        out.push(character);
        width += character_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_and_emoji_are_two_columns_each() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("😀😀"), 4);
        assert_eq!(display_width("aあb"), 4);
        assert_eq!(display_width("é"), 1, "a precomposed accent is one column");
    }

    #[test]
    fn tabs_advance_to_the_next_stop() {
        assert_eq!(display_width_with_tabs("abc", 4), 3);
        assert_eq!(display_width_with_tabs("\tx", 4), 5);
        assert_eq!(display_width_with_tabs("abc\tx", 4), 5);
        assert_eq!(display_width_with_tabs("abcd\tx", 4), 9);
        assert_eq!(display_width_with_tabs("日本\tx", 4), 9);
        assert_eq!(
            display_width_with_tabs("a\tb", 0),
            3,
            "a zero width is a stop"
        );
    }

    #[test]
    fn truncation_counts_columns_and_never_panics() {
        assert_eq!(truncate_width("abcdef", 10), "abcdef");
        assert_eq!(truncate_width("abcdef", 4), "abc…");
        assert_eq!(display_width(&truncate_width("abcdef", 4)), 4);

        let cut = truncate_width("日本語のファイル名", 7);
        assert_eq!(cut, "日本語…");
        assert_eq!(display_width(&cut), 7);

        let cut = truncate_width("a😀b😀", 4);
        assert_eq!(cut, "a😀…");
        assert_eq!(display_width(&cut), 4);

        // A two-column character that would not fit is not half written.
        assert_eq!(truncate_width("日本", 2), "…");
        assert_eq!(truncate_width("日本", 3), "日…");
        assert_eq!(truncate_width("anything", 0), "");
    }

    #[test]
    fn truncation_keeps_combining_marks_with_their_base() {
        // `e` plus a combining acute; the mark has no width of its own.
        let text = "e\u{0301}clair";
        assert_eq!(display_width(text), 6);
        let cut = truncate_width(text, 3);
        assert_eq!(display_width(&cut), 3);
        assert!(cut.starts_with("e\u{0301}"));
    }
}
