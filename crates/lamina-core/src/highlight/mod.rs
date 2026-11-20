//! Syntax highlighting with tree-sitter.
//!
//! The UI paints single diff lines, so the API is line-oriented: [`Highlighter::highlight`]
//! returns, for every line of a file, the byte ranges that carry a [`HighlightKind`]. Ranges are
//! relative to the line start so a line can be rendered without knowing the rest of the file.
//!
//! Grammars and their bundled highlight queries come from the `tree-sitter-*` grammar crates;
//! capture names are translated to the small [`HighlightKind`] set the UI knows about.
//! Highlighting never panics: a grammar whose query fails to compile, a parse error or an
//! over-sized input all degrade to "no spans" rather than to a broken diff view.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

use tree_sitter_highlight::{
    Highlight, HighlightConfiguration, HighlightEvent, Highlighter as TreeSitterHighlighter,
};

/// Inputs larger than this are not highlighted; a diff view never shows a file that big and the
/// cost of parsing one would stall the UI thread.
pub const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// Highlighted spans of a single line, as byte offsets relative to the line start.
pub type LineHighlights = Vec<(Range<usize>, HighlightKind)>;

/// A syntax class the diff view can render differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Constant,
    Number,
    Variable,
    Property,
    Operator,
    Punctuation,
    Attribute,
    Tag,
    Label,
    Escape,
    Embedded,
}

/// Languages with a bundled grammar and an ABI-compatible `tree-sitter` build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
    Json,
    Toml,
    Yaml,
    Css,
    Html,
    Bash,
    C,
    Cpp,
    Markdown,
}

/// Everything needed to lazily build one language's [`HighlightConfiguration`].
struct Grammar {
    /// Identifier passed to tree-sitter; it only shows up in error messages.
    name: &'static str,
    language: fn() -> tree_sitter::Language,
    highlights_query: Cow<'static, str>,
}

/// Joins several `highlights.scm` overlays into one query.
///
/// Some grammar crates ship their query in layers -- the TypeScript crate only adds TypeScript
/// scopes on top of the JavaScript base query, and JSX has its own file -- so a language may need
/// more than one source.
fn join_queries(queries: &[&'static str]) -> Cow<'static, str> {
    Cow::Owned(queries.join("\n"))
}

impl Language {
    fn grammar(self) -> Grammar {
        match self {
            Language::Rust => Grammar {
                name: "rust",
                language: || tree_sitter_rust::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_rust::HIGHLIGHTS_QUERY),
            },
            Language::JavaScript => Grammar {
                name: "javascript",
                language: || tree_sitter_javascript::LANGUAGE.into(),
                highlights_query: join_queries(&[
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                ]),
            },
            Language::TypeScript => Grammar {
                name: "typescript",
                language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                highlights_query: join_queries(&[
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                ]),
            },
            Language::Tsx => Grammar {
                name: "tsx",
                language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
                highlights_query: join_queries(&[
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                ]),
            },
            Language::Python => Grammar {
                name: "python",
                language: || tree_sitter_python::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_python::HIGHLIGHTS_QUERY),
            },
            Language::Go => Grammar {
                name: "go",
                language: || tree_sitter_go::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_go::HIGHLIGHTS_QUERY),
            },
            Language::Json => Grammar {
                name: "json",
                language: || tree_sitter_json::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_json::HIGHLIGHTS_QUERY),
            },
            Language::Toml => Grammar {
                name: "toml",
                language: || tree_sitter_toml_ng::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_toml_ng::HIGHLIGHTS_QUERY),
            },
            Language::Yaml => Grammar {
                name: "yaml",
                language: || tree_sitter_yaml::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_yaml::HIGHLIGHTS_QUERY),
            },
            Language::Css => Grammar {
                name: "css",
                language: || tree_sitter_css::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_css::HIGHLIGHTS_QUERY),
            },
            Language::Html => Grammar {
                name: "html",
                language: || tree_sitter_html::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_html::HIGHLIGHTS_QUERY),
            },
            Language::Bash => Grammar {
                name: "bash",
                language: || tree_sitter_bash::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_bash::HIGHLIGHT_QUERY),
            },
            Language::C => Grammar {
                name: "c",
                language: || tree_sitter_c::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_c::HIGHLIGHT_QUERY),
            },
            Language::Cpp => Grammar {
                name: "cpp",
                language: || tree_sitter_cpp::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_cpp::HIGHLIGHT_QUERY),
            },
            // Markdown ships two grammars: the block grammar covers headings, fences and lists,
            // and the inline grammar covers emphasis and links inside them. Block-level spans
            // already include the inline content, so the block grammar alone is enough here and
            // saves parsing every document twice.
            Language::Markdown => Grammar {
                name: "markdown",
                language: || tree_sitter_md::LANGUAGE.into(),
                highlights_query: Cow::Borrowed(tree_sitter_md::HIGHLIGHT_QUERY_BLOCK),
            },
        }
    }
}

/// Resolves a file path to one of the bundled grammars.
///
/// Matching is extension-based with a few extension-less shell files (`.bashrc`, `.zshrc`)
/// handled by name. Case is ignored so `.RS` and `.Rs` work on case-insensitive filesystems.
pub fn language_for_path(path: &str) -> Option<Language> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = name.to_ascii_lowercase();

    match name.as_str() {
        ".bashrc" | ".bash_profile" | ".bash_login" | ".bash_logout" | ".bash_aliases"
        | ".zshrc" | ".zshenv" | ".zprofile" | ".zlogin" | ".profile" => {
            return Some(Language::Bash);
        }
        _ => {}
    }

    let (stem, extension) = name.rsplit_once('.')?;
    // Dotfiles such as `.gitignore` have no extension, only a leading dot.
    if stem.is_empty() {
        return None;
    }

    Some(match extension {
        "rs" => Language::Rust,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "py" | "pyi" | "pyw" => Language::Python,
        "go" => Language::Go,
        "json" => Language::Json,
        "toml" => Language::Toml,
        "yml" | "yaml" => Language::Yaml,
        "css" => Language::Css,
        "html" | "htm" | "xhtml" => Language::Html,
        "sh" | "bash" | "zsh" | "ksh" => Language::Bash,
        "c" | "h" => Language::C,
        "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" => Language::Cpp,
        "md" | "markdown" => Language::Markdown,
        _ => return None,
    })
}

/// Capture-name prefixes mapped to a [`HighlightKind`], most specific first is not required:
/// [`kind_for_capture`] picks the longest prefix that matches on a scope boundary.
///
/// The names come from the grammars' `highlights.scm` files, which follow the conventional
/// tree-sitter/neovim scope vocabulary. Anything not listed here is dropped instead of guessed
/// at, so grammar updates cannot silently paint unrelated tokens.
const CAPTURE_KINDS: &[(&str, HighlightKind)] = &[
    ("keyword", HighlightKind::Keyword),
    ("string.special.key", HighlightKind::Property),
    ("string.escape", HighlightKind::Escape),
    ("string", HighlightKind::String),
    ("comment", HighlightKind::Comment),
    ("function", HighlightKind::Function),
    ("constructor", HighlightKind::Type),
    ("type", HighlightKind::Type),
    ("constant", HighlightKind::Constant),
    ("boolean", HighlightKind::Constant),
    ("number", HighlightKind::Number),
    ("variable", HighlightKind::Variable),
    ("property", HighlightKind::Property),
    ("operator", HighlightKind::Operator),
    ("punctuation", HighlightKind::Punctuation),
    ("delimiter", HighlightKind::Punctuation),
    ("attribute", HighlightKind::Attribute),
    ("tag", HighlightKind::Tag),
    ("label", HighlightKind::Label),
    ("escape", HighlightKind::Escape),
    ("embedded", HighlightKind::Embedded),
    // Markdown scopes (`text.title`, `text.literal`, ...) have no code equivalent; they are
    // mapped onto the closest kind the UI renders rather than left unhighlighted.
    ("text.title", HighlightKind::Keyword),
    ("text.literal", HighlightKind::String),
    ("text.uri", HighlightKind::String),
    ("text.reference", HighlightKind::Label),
];

/// Maps a capture name to a kind by longest matching prefix on a `.` boundary.
fn kind_for_capture(capture: &str) -> Option<HighlightKind> {
    let mut best: Option<(&str, HighlightKind)> = None;
    for &(prefix, kind) in CAPTURE_KINDS {
        let matches = capture == prefix
            || capture
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'));
        if matches && best.is_none_or(|(current, _)| prefix.len() > current.len()) {
            best = Some((prefix, kind));
        }
    }
    best.map(|(_, kind)| kind)
}

/// A compiled highlight query plus the kind for each capture name tree-sitter recognises.
struct CompiledGrammar {
    config: HighlightConfiguration,
    /// Indexed by the capture index tree-sitter reports in [`Highlight`].
    kinds: Vec<HighlightKind>,
}

/// Highlights source files, compiling each language's query at most once.
///
/// Compiling a query is the expensive part of highlighting, so configurations are cached for the
/// lifetime of the value. The type is `Send`; the parser itself is created per call because the
/// UI may move the cache between threads but highlights from only one at a time.
pub struct Highlighter {
    grammars: HashMap<Language, Option<CompiledGrammar>>,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            grammars: HashMap::new(),
        }
    }

    /// Highlights a whole file; returns spans per line (line split on '\n'), byte ranges relative
    /// to the line start, non-overlapping and sorted.
    pub fn highlight(&mut self, lang: Language, source: &str) -> Vec<LineHighlights> {
        let lines = line_ranges(source);
        if source.len() > MAX_SOURCE_BYTES {
            return empty_spans(lines.len());
        }

        let Some(compiled) = self.compiled(lang) else {
            return empty_spans(lines.len());
        };

        // A malformed file must not break the diff view, so an aborted pass yields no spans at
        // all instead of a half-built vector.
        collect_spans(compiled, source, &lines).unwrap_or_else(|| empty_spans(lines.len()))
    }

    /// Returns the compiled grammar for `lang`, compiling and caching it on first use.
    fn compiled(&mut self, lang: Language) -> Option<&CompiledGrammar> {
        self.grammars
            .entry(lang)
            .or_insert_with(|| compile(lang))
            .as_ref()
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the query configuration for `lang`, or `None` if the grammar or query is unusable.
fn compile(lang: Language) -> Option<CompiledGrammar> {
    let grammar = lang.grammar();
    let mut config = HighlightConfiguration::new(
        (grammar.language)(),
        grammar.name,
        &grammar.highlights_query,
        // Injections and locals are skipped on purpose: injections would pull in every embedded
        // language (HTML in JSX, SQL in strings, ...) and locals need the extra `locals.scm`
        // query, while a diff line only needs token colours.
        "",
        "",
    )
    .ok()?;

    // Owned names: `configure` needs `&mut config` while `capture_names()` borrows the query.
    let mut names: Vec<String> = Vec::new();
    let mut kinds: Vec<HighlightKind> = Vec::new();
    for capture in config.query.capture_names() {
        if let Some(kind) = kind_for_capture(capture) {
            names.push((*capture).to_owned());
            kinds.push(kind);
        }
    }
    config.configure(&names);

    Some(CompiledGrammar { config, kinds })
}

/// Span vector shaped like a file with `line_count` lines and nothing highlighted.
fn empty_spans(line_count: usize) -> Vec<LineHighlights> {
    vec![Vec::new(); line_count]
}

/// Splits `source` into line contents, excluding the `\n` separators (and keeping the empty
/// final line that `"a\n".split('\n')` produces).
fn line_ranges(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            ranges.push(start..index);
            start = index + 1;
        }
    }
    ranges.push(start..source.len());
    ranges
}

/// Runs the highlighter and distributes the emitted spans over the lines they touch.
///
/// `None` means the pass failed part-way (parse or highlight error).
fn collect_spans(
    compiled: &CompiledGrammar,
    source: &str,
    lines: &[Range<usize>],
) -> Option<Vec<LineHighlights>> {
    let mut parser = TreeSitterHighlighter::new();
    let events = parser
        .highlight(&compiled.config, source.as_bytes(), None, None, |_| None)
        .ok()?;

    let mut spans: Vec<LineHighlights> = vec![Vec::new(); lines.len()];
    // tree-sitter emits nested HighlightStart/HighlightEnd pairs around the Source ranges they
    // cover, so the innermost kind on the stack is the one that applies to a token.
    let mut stack: Vec<Option<HighlightKind>> = Vec::new();

    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(Highlight(index)) => {
                stack.push(compiled.kinds.get(index).copied());
            }
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let Some(Some(kind)) = stack.last() else {
                    continue;
                };
                push_span(&mut spans, lines, start..end, *kind);
            }
        }
    }

    Some(spans)
}

/// Appends `span` to every line it covers, rebasing the offsets onto each line start and letting
/// the newline separators (which belong to no line) end a piece.
fn push_span(
    spans: &mut [LineHighlights],
    lines: &[Range<usize>],
    span: Range<usize>,
    kind: HighlightKind,
) {
    if span.start >= span.end {
        return;
    }

    let mut index = lines
        .partition_point(|line| line.start <= span.start)
        .saturating_sub(1);
    while let Some(line) = lines.get(index) {
        if line.start >= span.end {
            break;
        }
        let start = span.start.max(line.start);
        let end = span.end.min(line.end);
        if start < end {
            spans[index].push((start - line.start..end - line.start, kind));
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte ranges of the kinds found on `line`, as text slices of that line.
    fn line_tokens<'a>(
        lines: &[LineHighlights],
        source: &'a str,
        line: usize,
    ) -> Vec<(&'a str, HighlightKind)> {
        let text = source.split('\n').nth(line).expect("line exists");
        lines[line]
            .iter()
            .map(|(range, kind)| (&text[range.clone()], *kind))
            .collect()
    }

    fn kinds_for(source: &str, lang: Language) -> Vec<LineHighlights> {
        let mut highlighter = Highlighter::new();
        highlighter.highlight(lang, source)
    }

    #[test]
    fn language_for_path_maps_extensions_and_shell_rc_files() {
        let cases = [
            ("src/main.rs", Some(Language::Rust)),
            ("Cargo.toml", Some(Language::Toml)),
            ("a/b/c.tsx", Some(Language::Tsx)),
            ("a/b/c.ts", Some(Language::TypeScript)),
            ("a/b/c.mjs", Some(Language::JavaScript)),
            ("a/b/c.jsx", Some(Language::JavaScript)),
            ("pkg/mod.py", Some(Language::Python)),
            ("cmd/main.go", Some(Language::Go)),
            ("package.json", Some(Language::Json)),
            ("ci/config.yml", Some(Language::Yaml)),
            ("ci/config.yaml", Some(Language::Yaml)),
            ("web/site.css", Some(Language::Css)),
            ("web/index.html", Some(Language::Html)),
            ("scripts/build.sh", Some(Language::Bash)),
            ("src/legacy.c", Some(Language::C)),
            ("src/legacy.h", Some(Language::C)),
            ("src/app.cpp", Some(Language::Cpp)),
            ("src/app.hpp", Some(Language::Cpp)),
            ("README.md", Some(Language::Markdown)),
            ("/home/me/.bashrc", Some(Language::Bash)),
            ("/home/me/.zshrc", Some(Language::Bash)),
            ("/home/me/.profile", Some(Language::Bash)),
            ("Makefile", None),
            ("Dockerfile", None),
            ("LICENSE", None),
            (".gitignore", None),
            ("notes.txt", None),
            ("", None),
        ];

        for (path, expected) in cases {
            assert_eq!(language_for_path(path), expected, "path {path:?}");
        }

        // Extensions are matched case-insensitively.
        assert_eq!(language_for_path("SRC/MAIN.RS"), Some(Language::Rust));
    }

    #[test]
    fn rust_snippet_highlights_keyword_string_and_comment() {
        let source = "// note\nfn main() {\n    let s = \"hi\";\n}\n";
        let lines = kinds_for(source, Language::Rust);
        assert_eq!(lines.len(), 5, "trailing newline keeps an empty last line");

        assert_eq!(
            line_tokens(&lines, source, 0),
            vec![("// note", HighlightKind::Comment)]
        );

        let first = line_tokens(&lines, source, 1);
        assert!(
            first.contains(&("fn", HighlightKind::Keyword)),
            "got {first:?}"
        );
        // The function name is its own span.
        assert!(
            first
                .iter()
                .any(|(text, kind)| *text == "main" && *kind == HighlightKind::Function),
            "got {first:?}"
        );

        let second = line_tokens(&lines, source, 2);
        assert!(
            second.contains(&("\"hi\"", HighlightKind::String)),
            "got {second:?}"
        );
    }

    #[test]
    fn multiline_block_comment_is_split_per_line() {
        let source = "/*\n * inner\n */\nfn main() {}\n";
        let lines = kinds_for(source, Language::Rust);

        for line in 0..3 {
            let tokens = line_tokens(&lines, source, line);
            assert!(
                tokens
                    .iter()
                    .any(|(_, kind)| *kind == HighlightKind::Comment),
                "line {line} has no comment span: {tokens:?}"
            );
        }

        // Spans stay inside their line: the newline itself is never covered.
        for (index, line_spans) in lines.iter().enumerate() {
            let len = source.split('\n').nth(index).unwrap().len();
            for (range, _) in line_spans {
                assert!(
                    range.end <= len,
                    "line {index} span {range:?} past line end"
                );
            }
        }
    }

    #[test]
    fn non_ascii_source_keeps_char_boundaries() {
        let source = "// 日本語のコメント\nfn main() {\n    let s = \"こんにちは\";\n}\n";
        let lines = kinds_for(source, Language::Rust);

        let mut saw_comment = false;
        let mut saw_string = false;
        for (index, line_spans) in lines.iter().enumerate() {
            let text = source.split('\n').nth(index).unwrap();
            for (range, kind) in line_spans {
                assert!(range.start <= range.end, "reversed range {range:?}");
                assert!(
                    text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
                    "span {range:?} splits a char on line {index}"
                );
                assert!(!range.is_empty(), "empty span on line {index}");
                match kind {
                    HighlightKind::Comment => saw_comment = true,
                    HighlightKind::String => saw_string = true,
                    _ => {}
                }
            }
        }
        assert!(saw_comment, "no comment span");
        assert!(saw_string, "no string span");
    }

    #[test]
    fn spans_are_sorted_and_non_overlapping() {
        let source = "fn main() { let x = foo(1) + 2; } // done\n";
        let lines = kinds_for(source, Language::Rust);
        let spans = &lines[0];
        assert!(!spans.is_empty());
        for pair in spans.windows(2) {
            assert!(
                pair[0].0.end <= pair[1].0.start,
                "spans overlap: {:?}",
                pair
            );
        }
        assert!(
            spans
                .windows(2)
                .all(|pair| pair[0].0.start <= pair[1].0.start)
        );
    }

    #[test]
    fn every_language_highlights_a_sample() {
        let samples = [
            (Language::Rust, "fn main() { let x = 1; } // c\n"),
            (Language::JavaScript, "const x = 1; // c\n"),
            (Language::TypeScript, "const x: number = 1;\n"),
            (Language::Tsx, "const el = <div className=\"x\" />;\n"),
            (Language::Python, "def f():\n    return 1  # c\n"),
            (Language::Go, "package main\n\nfunc main() {}\n"),
            (Language::Json, "{\"a\": 1}\n"),
            (Language::Toml, "key = 1\n"),
            (Language::Yaml, "key: value\n"),
            (Language::Css, "a { color: red; }\n"),
            (Language::Html, "<div class=\"x\">hi</div>\n"),
            (Language::Bash, "echo hello\n"),
            (Language::C, "int main(void) { return 0; }\n"),
            (Language::Cpp, "class Widget { public: int size; };\n"),
            (Language::Markdown, "# Title\n\nSome text\n"),
        ];

        let mut highlighter = Highlighter::new();
        for (lang, sample) in samples {
            let lines = highlighter.highlight(lang, sample);
            assert_eq!(
                lines.len(),
                sample.split('\n').count(),
                "{lang:?} line count"
            );
            let total: usize = lines.iter().map(Vec::len).sum();
            assert!(total > 0, "{lang:?} produced no spans");
        }
    }

    #[test]
    fn caches_one_configuration_per_language() {
        let mut highlighter = Highlighter::new();
        let first = highlighter.highlight(Language::Rust, "fn a() {}\n");
        let second = highlighter.highlight(Language::Rust, "fn b() {}\n");
        assert_eq!(first, second, "same shape of source, same spans");
        assert_eq!(highlighter.grammars.len(), 1);
        assert!(highlighter.grammars.get(&Language::Rust).unwrap().is_some());
    }

    #[test]
    fn oversized_input_yields_no_spans() {
        let mut source = String::with_capacity(MAX_SOURCE_BYTES + 16);
        source.push_str("fn main() {\n");
        while source.len() < MAX_SOURCE_BYTES + 1 {
            source.push('\n');
        }
        let mut highlighter = Highlighter::new();
        let lines = highlighter.highlight(Language::Rust, &source);
        assert_eq!(lines.len(), source.split('\n').count());
        assert!(lines.iter().all(Vec::is_empty));
    }

    #[test]
    fn highlighter_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Highlighter>();
    }

    #[test]
    #[ignore = "timing test; run with `cargo test --release -- --ignored`"]
    fn highlights_5k_line_rust_file_within_budget() {
        let mut source = String::new();
        for index in 0..5_000 {
            source.push_str(&format!(
                "fn fun_{index}(v: u32) -> u32 {{ v + {index} }} // note {index}\n"
            ));
        }

        let mut highlighter = Highlighter::new();
        // Compiling the query is a one-off cost; a running UI only pays for parses.
        let _ = highlighter.highlight(Language::Rust, &source);

        // Best of several runs: a shared CI machine can deschedule us mid-parse, and that
        // preemption says nothing about the highlighter's own cost.
        let mut best = std::time::Duration::MAX;
        for _ in 0..20 {
            let start = std::time::Instant::now();
            let lines = highlighter.highlight(Language::Rust, &source);
            best = best.min(start.elapsed());
            assert_eq!(lines.len(), 5_001);
            assert!(lines.iter().any(|line| !line.is_empty()));
        }

        println!("5k lines highlighted in {best:?}");
        assert!(
            best < std::time::Duration::from_millis(50),
            "highlighting 5k lines took {best:?}"
        );
    }
}
