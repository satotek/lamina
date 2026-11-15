//! Grouping a flat list of repository paths into directory rows.
//!
//! The details pane's changed-file list and the working-tree lists show the
//! same kind of data: paths relative to the repository root. A list mode draws
//! one row per file; a tree mode draws a row per directory and indents what is
//! inside it. The grouping here is plain data so a pane only has to decide how
//! to paint a row, and so the ordering and collapse rules can be tested without
//! a window.

use std::collections::{BTreeMap, HashSet};

/// One row of a grouped path list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    /// A directory. Its contents follow when it is not collapsed.
    Directory {
        /// Repository-relative path, used as the collapse key.
        path: String,
        name: String,
        depth: usize,
        collapsed: bool,
    },
    /// A file. `index` points back into the list the caller grouped.
    File {
        index: usize,
        path: String,
        name: String,
        depth: usize,
    },
}

impl TreeRow {
    /// How many directories sit above this row, for indentation.
    #[cfg(test)]
    pub fn depth(&self) -> usize {
        match self {
            TreeRow::Directory { depth, .. } | TreeRow::File { depth, .. } => *depth,
        }
    }

    /// The repository-relative path of the row.
    #[cfg(test)]
    pub fn path(&self) -> &str {
        match self {
            TreeRow::Directory { path, .. } | TreeRow::File { path, .. } => path,
        }
    }
}

#[derive(Default)]
struct Node<'a> {
    dirs: BTreeMap<&'a str, Node<'a>>,
    files: BTreeMap<&'a str, usize>,
}

/// Rows for `files`, each given as the caller's index and its path.
///
/// Directories come before files at every level and names are in byte order,
/// which keeps a commit's tree stable between frames. A directory whose path is
/// in `collapsed` is a row of its own with nothing beneath it.
pub fn rows(files: &[(usize, &str)], collapsed: &HashSet<String>) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (index, path) in files {
        let mut parts = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".");
        let Some(name) = parts.next_back() else {
            continue;
        };
        let mut node = &mut root;
        for part in parts {
            node = node.dirs.entry(part).or_default();
        }
        node.files.insert(name, *index);
    }

    let mut rows = Vec::new();
    walk(&root, "", 0, collapsed, &mut rows);
    rows
}

fn walk(
    node: &Node<'_>,
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    rows: &mut Vec<TreeRow>,
) {
    for (name, child) in &node.dirs {
        let path = join(prefix, name);
        let is_collapsed = collapsed.contains(&path);
        rows.push(TreeRow::Directory {
            path: path.clone(),
            name: (*name).to_owned(),
            depth,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            walk(child, &path, depth + 1, collapsed, rows);
        }
    }
    for (name, index) in &node.files {
        rows.push(TreeRow::File {
            index: *index,
            path: join(prefix, name),
            name: (*name).to_owned(),
            depth,
        });
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(index: usize, path: &str) -> (usize, &str) {
        (index, path)
    }

    fn labels(rows: &[TreeRow]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let indent = "  ".repeat(row.depth());
                match row {
                    TreeRow::Directory { name, .. } => format!("{indent}{name}/"),
                    TreeRow::File { name, .. } => format!("{indent}{name}"),
                }
            })
            .collect()
    }

    #[test]
    fn directories_group_their_files_and_sort_before_them() {
        let files = [
            file(0, "src/ui/button.rs"),
            file(1, "README.md"),
            file(2, "src/main.rs"),
            file(3, "assets/logo.png"),
        ];
        let grouped = rows(&files, &HashSet::new());

        assert_eq!(
            labels(&grouped),
            [
                "assets/",
                "  logo.png",
                "src/",
                "  ui/",
                "    button.rs",
                "  main.rs",
                "README.md",
            ]
        );
        assert_eq!(grouped[0].path(), "assets");
        assert_eq!(
            grouped[2],
            TreeRow::Directory {
                path: "src".to_owned(),
                name: "src".to_owned(),
                depth: 0,
                collapsed: false,
            }
        );
        assert_eq!(
            grouped[3],
            TreeRow::Directory {
                path: "src/ui".to_owned(),
                name: "ui".to_owned(),
                depth: 1,
                collapsed: false,
            }
        );
        assert_eq!(
            grouped[4],
            TreeRow::File {
                index: 0,
                path: "src/ui/button.rs".to_owned(),
                name: "button.rs".to_owned(),
                depth: 2,
            }
        );
    }

    #[test]
    fn a_collapsed_directory_hides_its_contents_but_keeps_its_own_row() {
        let files = [
            file(0, "src/ui/button.rs"),
            file(1, "src/main.rs"),
            file(2, "README.md"),
        ];
        let collapsed = HashSet::from(["src".to_owned()]);
        let grouped = rows(&files, &collapsed);

        assert_eq!(labels(&grouped), ["src/", "README.md"]);
        assert_eq!(
            grouped[0].path(),
            "src",
            "the row is still the collapse toggle"
        );
        assert_eq!(
            grouped[0],
            TreeRow::Directory {
                path: "src".to_owned(),
                name: "src".to_owned(),
                depth: 0,
                collapsed: true,
            }
        );

        // Collapsing a nested directory leaves its parent's row and siblings.
        let collapsed = HashSet::from(["src/ui".to_owned()]);
        let grouped = rows(&files, &collapsed);
        assert_eq!(
            labels(&grouped),
            ["src/", "  ui/", "  main.rs", "README.md"]
        );
    }

    #[test]
    fn a_file_index_survives_the_grouping() {
        let files = [file(7, "dir/b.txt"), file(4, "dir/a.txt")];
        let grouped = rows(&files, &HashSet::new());
        assert_eq!(
            grouped
                .iter()
                .filter_map(|row| match row {
                    TreeRow::File { index, .. } => Some(*index),
                    TreeRow::Directory { .. } => None,
                })
                .collect::<Vec<_>>(),
            [4, 7],
            "rows sort by name, while each file keeps its caller index"
        );
    }

    #[test]
    fn top_level_files_have_no_directory_rows() {
        let files = [file(0, "main.rs"), file(1, "lib.rs")];
        let grouped = rows(&files, &HashSet::new());
        assert_eq!(labels(&grouped), ["lib.rs", "main.rs"]);
        assert!(grouped.iter().all(|row| row.depth() == 0));
    }

    #[test]
    fn empty_and_odd_paths_are_ignored_rather_than_panicking() {
        let files = [file(0, ""), file(1, "/"), file(2, "dir//./x.txt")];
        let grouped = rows(&files, &HashSet::new());
        assert_eq!(labels(&grouped), ["dir/", "  x.txt"]);
        assert_eq!(
            grouped[1],
            TreeRow::File {
                index: 2,
                path: "dir/x.txt".to_owned(),
                name: "x.txt".to_owned(),
                depth: 1,
            }
        );
    }
}
