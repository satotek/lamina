//! Canonical paths in the plain form people and child processes expect.
//!
//! `std::fs::canonicalize` on Windows returns the verbatim form
//! `\\?\C:\work\repo` (or `\\?\UNC\server\share\repo`). Most Win32 programs do
//! not accept it: `cmd.exe` started in such a directory reports "UNC paths are
//! not supported", falls back to the Windows directory, and every relative
//! path in a batch file then misses. So every path Strata hands to a person or
//! a child process goes through [`canonicalize`] instead.

use std::path::{Path, PathBuf};

/// The canonical form of `path` without Windows' verbatim prefix, or `path`
/// itself when it cannot be resolved (a removed directory still needs a name).
pub fn canonicalize(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => strip_verbatim(canonical),
        Err(_) => path.to_path_buf(),
    }
}

/// Drops the `\\?\` prefix: `\\?\C:\a` becomes `C:\a` and `\\?\UNC\srv\a`
/// becomes `\\srv\a`. Other paths, and every path off Windows, are unchanged.
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    match strip_verbatim_text(&path.to_string_lossy()) {
        Some(plain) => PathBuf::from(plain),
        None => path,
    }
}

fn strip_verbatim_text(text: &str) -> Option<String> {
    let rest = text.strip_prefix(r"\\?\")?;
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    // Only a drive path has a plain spelling; `\\?\Volume{…}` and the like do
    // not, so they keep the prefix.
    let bytes = rest.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        .then(|| rest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_drive_and_unc_paths_lose_the_prefix() {
        assert_eq!(
            strip_verbatim_text(r"\\?\C:\Users\me\repo").as_deref(),
            Some(r"C:\Users\me\repo")
        );
        assert_eq!(
            strip_verbatim_text(r"\\?\UNC\server\share\repo").as_deref(),
            Some(r"\\server\share\repo")
        );
        assert_eq!(strip_verbatim_text(r"\\?\Volume{abc}\repo"), None);
        assert_eq!(strip_verbatim_text(r"C:\Users\me"), None);
        assert_eq!(strip_verbatim_text("/home/me"), None);
    }

    #[test]
    fn a_missing_path_is_returned_as_spelled() {
        let missing = Path::new("/definitely/not/here/strata");
        assert_eq!(canonicalize(missing), missing);
    }
}
