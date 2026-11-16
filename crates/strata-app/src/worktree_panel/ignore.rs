//! Ignoring untracked paths from the working-tree file list.
//!
//! None of this is git state: the pattern is a line in the repository's
//! top-level `.gitignore`, exactly what the user would type there, so it can be
//! reviewed, committed or removed like any other file change. The repository's
//! watcher already treats `.gitignore` as a change worth re-reading, so the
//! ignored path leaves the list without another command.

use std::io;
use std::path::{Path, PathBuf};

use gpui::Context;

use super::RepoView;

/// How many parent directories the menu offers, nearest first.
const MAX_DIRECTORY_PATTERNS: usize = 3;

/// One entry of the "Ignore" submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnorePattern {
    /// What the menu row says.
    pub label: String,
    /// The line appended to `.gitignore`.
    pub pattern: String,
}

/// The patterns offered for the untracked `path`, in menu order: the file, its
/// extension, then its parent directories from the nearest outwards.
///
/// Patterns are anchored with a leading `/` where they name a repository path,
/// so ignoring `notes.txt` in the root does not also hide a `notes.txt` deeper
/// in the tree.
pub fn patterns(path: &str) -> Vec<IgnorePattern> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Vec::new();
    }

    let mut patterns = vec![IgnorePattern {
        label: t!("worktree.ignore.file", path = format!("/{path}")).to_string(),
        pattern: format!("/{}", escape(path)),
    }];

    if let Some(extension) = extension(path) {
        patterns.push(IgnorePattern {
            label: t!(
                "worktree.ignore.extension",
                extension = format!(".{extension}")
            )
            .to_string(),
            pattern: format!("*.{extension}"),
        });
    }

    let mut directory = parent(path);
    let mut added = 0;
    while !directory.is_empty() && added < MAX_DIRECTORY_PATTERNS {
        patterns.push(IgnorePattern {
            label: t!("worktree.ignore.folder", path = format!("{directory}/")).to_string(),
            pattern: format!("/{}/", escape(directory)),
        });
        added += 1;
        directory = parent(directory);
    }

    patterns
}

/// Appends `pattern` to the repository's top-level `.gitignore`.
///
/// `Ok(false)` means an identical line is already there, so the menu does not
/// grow duplicates. A missing file is created, and an existing last line
/// without a newline gets one first so the new pattern is not glued to it.
pub fn append(root: &Path, pattern: &str) -> io::Result<bool> {
    let path = root.join(".gitignore");
    let mut contents = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if has_line(&contents, pattern.as_bytes()) {
        return Ok(false);
    }
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    contents.extend_from_slice(pattern.as_bytes());
    contents.push(b'\n');
    std::fs::write(&path, contents)?;
    Ok(true)
}

/// The extension a `*.ext` pattern would ignore, for a path that has one.
fn extension(path: &str) -> Option<&str> {
    let extension = Path::new(path).extension()?.to_str()?;
    let usable = !extension.is_empty()
        && extension
            .chars()
            .all(|character| character.is_alphanumeric() || character == '-' || character == '_');
    usable.then_some(extension)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// Escapes the characters a gitignore pattern reads as wildcards, so a file
/// whose name happens to contain one is still ignored literally.
fn escape(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn has_line(contents: &[u8], pattern: &[u8]) -> bool {
    contents.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line == pattern
    })
}

impl RepoView {
    /// Adds one ignore pattern picked from the working-tree menu.
    pub(super) fn ignore_worktree_path(&mut self, pattern: String, cx: &mut Context<Self>) {
        self.status_note = None;
        let Some(root) = self.worktree_root() else {
            self.status_error = Some(t!("title_bar.open_repository_first").to_string());
            cx.notify();
            return;
        };
        match append(&root, &pattern) {
            Ok(true) => {
                self.status_error = None;
                self.status_note =
                    Some(t!("worktree.ignore.added", pattern = &pattern).to_string());
            }
            Ok(false) => {
                self.status_error = None;
                self.status_note =
                    Some(t!("worktree.ignore.present", pattern = &pattern).to_string());
            }
            Err(error) => {
                self.status_error = Some(format!("{}: {error}", t!("worktree.ignore.failed")));
            }
        }
        cx.notify();
    }

    /// Opens the top-level `.gitignore` in the editor, creating it first so the
    /// editor always has a file to open.
    pub(super) fn edit_gitignore(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.worktree_root() else {
            self.status_error = Some(t!("title_bar.open_repository_first").to_string());
            cx.notify();
            return;
        };
        let file = root.join(".gitignore");
        if !file.exists()
            && let Err(error) = std::fs::write(&file, "")
        {
            self.status_error = Some(format!("{}: {error}", t!("worktree.ignore.edit")));
            cx.notify();
            return;
        }
        self.open_file_in_editor(".gitignore", None, cx);
    }

    /// The working-tree root, resolved the way the editor opener resolves it.
    fn worktree_root(&self) -> Option<PathBuf> {
        let root = self.session.as_ref()?.reader().workdir()?.to_path_buf();
        Some(strata_core::fs_path::canonicalize(&root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(rows: &[IgnorePattern]) -> Vec<&str> {
        rows.iter().map(|entry| entry.label.as_str()).collect()
    }

    fn written(rows: &[IgnorePattern]) -> Vec<&str> {
        rows.iter().map(|entry| entry.pattern.as_str()).collect()
    }

    #[test]
    fn a_file_offers_itself_its_extension_and_its_directories() {
        let offered = patterns("src/ui/button.rs");
        assert_eq!(
            labels(&offered),
            [
                "This file /src/ui/button.rs",
                "Extension .rs",
                "Folder src/ui/",
                "Folder src/",
            ]
        );
        assert_eq!(
            written(&offered),
            ["/src/ui/button.rs", "*.rs", "/src/ui/", "/src/"]
        );
    }

    #[test]
    fn at_most_three_directories_are_offered_nearest_first() {
        let offered = patterns("a/b/c/d/e/file.txt");
        assert_eq!(
            written(&offered),
            [
                "/a/b/c/d/e/file.txt",
                "*.txt",
                "/a/b/c/d/e/",
                "/a/b/c/d/",
                "/a/b/c/",
            ]
        );
    }

    #[test]
    fn a_missing_extension_and_a_root_file_leave_entries_out() {
        let offered = patterns("notes");
        assert_eq!(labels(&offered), ["This file /notes"]);
        assert!(patterns("").is_empty());
    }

    #[test]
    fn wildcards_in_a_name_are_escaped_so_the_file_is_ignored_literally() {
        let offered = patterns("logs/a*b[1].txt");
        assert_eq!(offered[0].pattern, "/logs/a\\*b\\[1\\].txt");
        assert_eq!(offered[2].pattern, "/logs/");
    }

    #[test]
    fn appending_creates_the_file_and_keeps_existing_lines() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        assert!(append(dir.path(), "/build/").expect("create"));
        assert!(append(dir.path(), "/target/").expect("append"));
        let contents = std::fs::read_to_string(dir.path().join(".gitignore")).expect("read");
        assert_eq!(contents, "/build/\n/target/\n");
    }

    #[test]
    fn an_existing_last_line_without_a_newline_still_gets_its_own_line() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        std::fs::write(dir.path().join(".gitignore"), "/build/").expect("write");
        assert!(append(dir.path(), "/target/").expect("append"));
        let contents = std::fs::read_to_string(dir.path().join(".gitignore")).expect("read");
        assert_eq!(contents, "/build/\n/target/\n");
    }

    #[test]
    fn an_identical_line_is_not_added_twice() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        std::fs::write(dir.path().join(".gitignore"), "/build/\r\n/keep.txt\n").expect("write");
        assert!(
            !append(dir.path(), "/build/").expect("first"),
            "CRLF counts"
        );
        assert!(!append(dir.path(), "/keep.txt").expect("second"));
        assert!(append(dir.path(), "/keep.txt.bak").expect("different line"));
        let contents = std::fs::read_to_string(dir.path().join(".gitignore")).expect("read");
        assert_eq!(contents, "/build/\r\n/keep.txt\n/keep.txt.bak\n");
    }
}
