//! Word-level emphasis for modified lines.
//!
//! A removed line followed by an added line is usually the same line with a
//! few words changed. Highlighting just those words is far easier to read than
//! painting both lines, so each pair is tokenised and diffed once more at word
//! granularity.

use std::collections::HashMap;
use std::ops::Range;

use imara_diff::{Algorithm, Diff, Token};

use super::{DiffLine, LineKind};

/// Longer lines are not tokenised: the diff is still correct but the emphasis
/// becomes unreadable and the extra work is not worth it.
const MAX_LINE_BYTES: usize = 1000;

/// Above this share of changed bytes the emphasis covers nearly the whole
/// line, which is just noise; the plain `-`/`+` colouring says it better.
const MAX_CHANGED_SHARE: f64 = 0.6;

/// Lines in a run that do not pair up positionally are only emphasised when
/// they are at least this similar, otherwise they are different lines that
/// merely happen to sit next to each other.
const MIN_PAIRED_SIMILARITY: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Whitespace,
    Punctuation,
}

fn classify(ch: char) -> TokenKind {
    if ch.is_whitespace() {
        TokenKind::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        TokenKind::Word
    } else {
        TokenKind::Punctuation
    }
}

/// Byte ranges of the tokens in `text`: runs of word characters, runs of
/// whitespace, and single punctuation characters.
fn tokenize(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut current: Option<(usize, TokenKind)> = None;
    for (idx, ch) in text.char_indices() {
        let kind = classify(ch);
        match current {
            // A run continues only for word and whitespace tokens; punctuation
            // is always a token of its own, so the transition below handles it.
            Some((_, prev)) if prev == kind && kind != TokenKind::Punctuation => {}
            Some((start, _)) => {
                if start < idx {
                    out.push(start..idx);
                }
                current = Some((idx, kind));
            }
            None => current = Some((idx, kind)),
        }
    }
    if let Some((start, _)) = current
        && start < text.len()
    {
        out.push(start..text.len());
    }
    out
}

/// Byte ranges of the tokens flagged as changed, merged into one range per run
/// of adjacent changed tokens.
fn changed_spans(tokens: &[Range<usize>], is_changed: impl Fn(usize) -> bool) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for (idx, token) in tokens.iter().enumerate() {
        if is_changed(idx) {
            match &mut open {
                Some((_, end)) => *end = token.end,
                None => open = Some((token.start, token.end)),
            }
        } else if let Some((start, end)) = open.take() {
            spans.push(start..end);
        }
    }
    if let Some((start, end)) = open {
        spans.push(start..end);
    }
    spans
}

/// Collects the changed byte ranges of `old` and `new` at word granularity.
fn changed_bytes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);

    let mut ids: HashMap<&str, u32> = HashMap::new();
    let before: Vec<Token> = old_tokens
        .iter()
        .map(|range| intern(&mut ids, &old[range.clone()]))
        .collect();
    let after: Vec<Token> = new_tokens
        .iter()
        .map(|range| intern(&mut ids, &new[range.clone()]))
        .collect();
    let num_tokens = ids.len() as u32;

    let mut diff = Diff::default();
    diff.compute_with(Algorithm::Histogram, &before, &after, num_tokens);

    (
        changed_spans(&old_tokens, |idx| diff.is_removed(idx as u32)),
        changed_spans(&new_tokens, |idx| diff.is_added(idx as u32)),
    )
}

fn intern<'a>(ids: &mut HashMap<&'a str, u32>, text: &'a str) -> Token {
    let next = ids.len() as u32;
    Token(*ids.entry(text).or_insert(next))
}

fn changed_share(changed: &[Range<usize>], len: usize) -> f64 {
    if len == 0 {
        return 0.0;
    }
    changed.iter().map(Range::len).sum::<usize>() as f64 / len as f64
}

/// Adds emphasis to every removed/added pair in `lines`.
pub fn emphasize(lines: &mut [DiffLine]) {
    let mut idx = 0;
    while idx < lines.len() {
        if lines[idx].kind != LineKind::Removed {
            idx += 1;
            continue;
        }
        let run_start = idx;
        while idx < lines.len() && lines[idx].kind == LineKind::Removed {
            idx += 1;
        }
        let removed_end = idx;
        while idx < lines.len() && lines[idx].kind == LineKind::Added {
            idx += 1;
        }
        let run_len = removed_end - run_start;
        let (removed, added) = lines[run_start..idx].split_at_mut(run_len);
        if !added.is_empty() {
            emphasize_run(removed, added);
        }
    }
}

fn emphasize_run(removed: &mut [DiffLine], added: &mut [DiffLine]) {
    let equal_size = removed.len() == added.len();
    for (old_line, new_line) in removed.iter_mut().zip(added.iter_mut()) {
        if old_line.content.len() > MAX_LINE_BYTES || new_line.content.len() > MAX_LINE_BYTES {
            continue;
        }
        if old_line.content == new_line.content {
            // Possible when only the line ending changed.
            continue;
        }
        let (old_spans, new_spans) = changed_bytes(&old_line.content, &new_line.content);
        let share = changed_share(&old_spans, old_line.content.len())
            .max(changed_share(&new_spans, new_line.content.len()));
        if share > MAX_CHANGED_SHARE {
            continue;
        }
        if !equal_size && 1.0 - share < MIN_PAIRED_SIMILARITY {
            continue;
        }
        old_line.emphasis = old_spans;
        new_line.emphasis = new_spans;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_words_whitespace_and_punctuation() {
        let text = "let x = f(a, b)";
        let tokens: Vec<&str> = tokenize(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(
            tokens,
            vec![
                "let", " ", "x", " ", "=", " ", "f", "(", "a", ",", " ", "b", ")"
            ]
        );
    }

    #[test]
    fn tokenizes_multibyte_text_without_broken_ranges() {
        let text = "値 = 1";
        let tokens = tokenize(text);
        for range in &tokens {
            assert!(text.get(range.clone()).is_some());
        }
    }
}
