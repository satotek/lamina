//! Folder listings for picking a repository on the host.
//!
//! A repository helper is started with the path it serves, but the user has
//! not chosen one yet when the app shows its folder picker. This mode opens no
//! repository: it answers the handshake and lists directories until stdin
//! closes. Requests are answered one at a time, in order, because each one is
//! a single `read_dir` the user is waiting on.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use lamina_remote_protocol::{
    self as proto, DirEntryDto, DirListingDto, ErrorKind, Message, PROTOCOL_VERSION, RemoteError,
    Request, Response,
};

use crate::server::BoxError;

/// A home directory with more subdirectories than this is rare, and a list
/// that long is not something to scroll through anyway.
const MAX_ENTRIES: usize = 5000;

/// Serves folder listings on the given stream until the reader reaches EOF.
pub fn serve_browse(mut reader: impl Read, mut writer: impl Write) -> Result<(), BoxError> {
    let home = home_dir();
    while let Some(frame) = proto::read_frame(&mut reader)? {
        let response = match frame.message {
            Message::Request(request) => answer(&home, request),
            Message::Response(_) | Message::Event(_) => Response::Error(RemoteError::new(
                ErrorKind::Protocol,
                "the helper only accepts requests",
            )),
        };
        proto::write_frame(&mut writer, frame.stream, &Message::Response(response))?;
        writer.flush()?;
    }
    Ok(())
}

fn answer(home: &Path, request: Request) -> Response {
    match request {
        Request::Hello { protocol, .. } if protocol != PROTOCOL_VERSION => {
            Response::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: protocol,
            }
        }
        Request::Hello { .. } => Response::BrowseAck {
            home: home.display().to_string(),
        },
        Request::Ping { nonce } => Response::Pong { nonce },
        Request::ListDir { path } => match list_dir(home, path.as_deref()) {
            Ok(listing) => Response::DirListing { listing },
            Err(error) => Response::Error(error),
        },
        _ => Response::Error(RemoteError::new(
            ErrorKind::NotSupported,
            "this helper lists folders and has no repository open",
        )),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `~` and `~/x` are the user's home, the way they read in a terminal. A
/// relative path is taken from home too: the helper's working directory is
/// whatever `sshd` chose, which means nothing to the user.
fn resolve(home: &Path, path: Option<&str>) -> PathBuf {
    let Some(path) = path.map(str::trim).filter(|path| !path.is_empty()) else {
        return home.to_path_buf();
    };
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(path)
    }
}

pub(crate) fn list_dir(home: &Path, path: Option<&str>) -> Result<DirListingDto, RemoteError> {
    let requested = resolve(home, path);
    let dir = std::fs::canonicalize(&requested).map_err(|error| io_error(&requested, &error))?;
    if !dir.is_dir() {
        return Err(RemoteError::new(
            ErrorKind::NotFound,
            format!("{} is not a folder", dir.display()),
        ));
    }
    let read = std::fs::read_dir(&dir).map_err(|error| io_error(&dir, &error))?;

    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read.flatten() {
        // A work tree's `.git` has the bare layout and would be offered as a
        // repository of its own; the folder that holds it is the one to open.
        if entry.file_name() == ".git" {
            continue;
        }
        let child = entry.path();
        // `metadata` follows symlinks, so a link to a folder is listed like
        // the folder it points at. A dangling link fails and is skipped.
        if !std::fs::metadata(&child).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            truncated = true;
            break;
        }
        entries.push(DirEntryDto {
            name: entry.file_name().to_string_lossy().into_owned(),
            repository: is_repository(&child),
        });
    }
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(DirListingDto {
        path: dir.display().to_string(),
        parent: dir.parent().map(|parent| parent.display().to_string()),
        repository: is_repository(&dir),
        entries,
        truncated,
    })
}

/// A cheap structural check, not a full open: it runs once per listed folder,
/// and a false positive only means opening fails with git's own message.
fn is_repository(dir: &Path) -> bool {
    if dir.join(".git").exists() {
        return true;
    }
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

fn io_error(path: &Path, error: &std::io::Error) -> RemoteError {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => ErrorKind::NotFound,
        _ => ErrorKind::Io,
    };
    RemoteError::new(kind, format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("temp dir")
    }

    #[test]
    fn lists_only_folders_sorted_and_marks_repositories() {
        let root = scratch();
        let base = root.path();
        std::fs::create_dir_all(base.join("beta/.git")).unwrap();
        std::fs::create_dir_all(base.join("Alpha")).unwrap();
        std::fs::create_dir_all(base.join("bare.git/objects")).unwrap();
        std::fs::create_dir_all(base.join("bare.git/refs")).unwrap();
        std::fs::write(base.join("bare.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(base.join("notes.txt"), "file\n").unwrap();

        let listing = list_dir(base, Some(base.to_str().unwrap())).expect("listing");
        let names: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.repository))
            .collect();
        assert_eq!(
            names,
            [("Alpha", false), ("bare.git", true), ("beta", true)]
        );
        assert!(!listing.repository);
        assert!(!listing.truncated);
        assert_eq!(
            listing.path,
            std::fs::canonicalize(base).unwrap().display().to_string()
        );
    }

    #[test]
    fn home_relative_and_tilde_paths_resolve_from_home() {
        let root = scratch();
        let home = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir_all(home.join("src/repo/.git")).unwrap();

        let from_home = list_dir(&home, None).expect("home");
        assert_eq!(from_home.path, home.display().to_string());
        let tilde = list_dir(&home, Some("~/src")).expect("tilde");
        let relative = list_dir(&home, Some("src")).expect("relative");
        assert_eq!(tilde, relative);
        assert_eq!(tilde.entries[0].name, "repo");

        let repo = list_dir(&home, Some("~/src/repo/..//repo")).expect("dot-dot");
        assert!(repo.repository);
        assert!(repo.entries.is_empty(), "{:?}", repo.entries);
        assert_eq!(repo.path, home.join("src/repo").display().to_string());
        assert_eq!(repo.parent, Some(home.join("src").display().to_string()));
    }

    #[test]
    fn a_missing_folder_is_not_found_and_a_file_is_rejected() {
        let root = scratch();
        let base = root.path();
        std::fs::write(base.join("file"), "x").unwrap();

        let missing = list_dir(base, Some("nope")).expect_err("missing");
        assert_eq!(missing.kind, ErrorKind::NotFound);
        let file = list_dir(base, Some("file")).expect_err("file");
        assert_eq!(file.kind, ErrorKind::NotFound);
        assert!(file.message.contains("is not a folder"), "{file:?}");
    }

    #[test]
    fn the_handshake_checks_the_protocol_and_other_requests_are_refused() {
        let home = Path::new("/");
        let mismatch = answer(
            home,
            Request::Hello {
                protocol: PROTOCOL_VERSION - 1,
                app_version: String::new(),
                build: String::new(),
            },
        );
        assert!(matches!(mismatch, Response::VersionMismatch { .. }));
        let ack = answer(
            home,
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                app_version: String::new(),
                build: String::new(),
            },
        );
        assert_eq!(ack, Response::BrowseAck { home: "/".into() });
        let refused = answer(home, Request::Head);
        assert!(
            matches!(&refused, Response::Error(error) if error.kind == ErrorKind::NotSupported)
        );
    }
}
