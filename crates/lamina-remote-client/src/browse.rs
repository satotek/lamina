//! Browsing folders on a host before a repository is chosen.
//!
//! The picker keeps one `ssh` connection to a `lamina-remote --browse` helper
//! open while the user walks the tree, so each step is one round trip instead
//! of a fresh SSH handshake. Calls block; the app makes them off the UI thread.

use std::io::{BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use lamina_core::{Error, Result};
use lamina_remote_protocol::{self as proto, DirListingDto, Message, Request, Response};

use crate::session::RemoteTarget;
use crate::ssh::{self, AskpassSession, SshOptions};

/// One subdirectory of a listed folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDirEntry {
    pub name: String,
    /// The folder holds a repository (a `.git` entry, or a bare layout).
    pub repository: bool,
}

/// A folder on the host and the subdirectories in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDirListing {
    /// Absolute and canonical on the host.
    pub path: String,
    /// `None` at the filesystem root.
    pub parent: Option<String>,
    /// Whether `path` itself is a repository.
    pub repository: bool,
    /// Sorted by name; files are not listed.
    pub entries: Vec<RemoteDirEntry>,
    /// The helper stopped listing at its limit.
    pub truncated: bool,
}

impl From<DirListingDto> for RemoteDirListing {
    fn from(dto: DirListingDto) -> Self {
        Self {
            path: dto.path,
            parent: dto.parent,
            repository: dto.repository,
            entries: dto
                .entries
                .into_iter()
                .map(|entry| RemoteDirEntry {
                    name: entry.name,
                    repository: entry.repository,
                })
                .collect(),
            truncated: dto.truncated,
        }
    }
}

struct Pipe {
    reader: BufReader<ChildStdout>,
    writer: BufWriter<ChildStdin>,
    next_stream: u16,
}

/// A live connection to a folder-listing helper on one host.
pub struct RemoteBrowser {
    host: String,
    home: String,
    pipe: Mutex<Pipe>,
    child: Mutex<Option<Child>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    /// Keeps the askpass listener alive for the ssh process. Not read.
    #[allow(dead_code)]
    askpass: Option<AskpassSession>,
}

impl RemoteBrowser {
    /// Starts `helper --browse` on the host. `helper` is the path
    /// [`install_helper`](crate::install_helper) returned, so the binary is
    /// already this build and the handshake only confirms it.
    pub fn open(
        target: &RemoteTarget,
        helper: impl Into<String>,
        options: &SshOptions,
    ) -> Result<Self> {
        let helper = helper.into();
        if target.host.is_empty() {
            return Err(Error::other("the remote host is empty"));
        }
        if helper.is_empty() {
            return Err(Error::other("the remote helper path is empty"));
        }
        let askpass =
            ssh::start_askpass(options.askpass.as_ref(), options.askpass_program.as_deref())?;
        let remote_args = vec![helper, "--stdio".to_owned(), "--browse".to_owned()];
        let mut child = ssh::spawn(
            &options.program,
            target.port,
            &target.host,
            &remote_args,
            askpass.as_ref(),
        )?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::other("ssh did not provide its pipes"));
        };
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&stderr_buf);
        let stderr_thread = thread::Builder::new()
            .name("lamina-browse-ssh-err".to_owned())
            .spawn(move || ssh::capture_stderr(stderr, &capture))
            .map_err(|error| Error::other(format!("could not read ssh stderr: {error}")))?;

        let mut browser = Self {
            host: target.host.clone(),
            home: String::new(),
            pipe: Mutex::new(Pipe {
                reader: BufReader::new(stdout),
                writer: BufWriter::new(stdin),
                next_stream: 1,
            }),
            child: Mutex::new(Some(child)),
            stderr: Mutex::new(Some(stderr_thread)),
            stderr_buf,
            askpass,
        };
        let home = match browser.request(Request::Hello {
            protocol: proto::PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            build: option_env!("LAMINA_BUILD_ID").unwrap_or("dev").to_owned(),
        })? {
            Response::BrowseAck { home } => home,
            Response::VersionMismatch { expected, got } => {
                return Err(Error::other(format!(
                    "remote protocol version mismatch: helper speaks {expected}, app speaks {got}"
                )));
            }
            other => return Err(unexpected(&other)),
        };
        browser.home = home;
        Ok(browser)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// The remote user's home directory, where a picker starts.
    pub fn home(&self) -> &str {
        &self.home
    }

    /// Subdirectories of `path`. `None` lists home; `~/x` and relative paths
    /// are taken from home.
    pub fn list(&self, path: Option<&str>) -> Result<RemoteDirListing> {
        match self.request(Request::ListDir {
            path: path.map(str::to_owned),
        })? {
            Response::DirListing { listing } => Ok(listing.into()),
            other => Err(unexpected(&other)),
        }
    }

    /// One request and its response. The helper answers in order and sends
    /// no events, so the next frame on this stream is the answer.
    fn request(&self, request: Request) -> Result<Response> {
        let mut pipe = lock(&self.pipe);
        let stream = pipe.next_stream;
        pipe.next_stream = pipe.next_stream.wrapping_add(1).max(1);
        let sent = proto::write_frame(&mut pipe.writer, stream, &Message::Request(request))
            .and_then(|()| pipe.writer.flush().map_err(Into::into));
        if let Err(error) = sent {
            return Err(self.lost(&format!("remote write failed: {error}")));
        }
        loop {
            let frame = match proto::read_frame(&mut pipe.reader) {
                Ok(Some(frame)) => frame,
                Ok(None) => return Err(self.lost("the connection closed")),
                Err(error) => return Err(self.lost(&format!("remote read failed: {error}"))),
            };
            if frame.stream != stream {
                continue;
            }
            return match frame.message {
                Message::Response(Response::Error(error)) => Err(Error::other(error.message)),
                Message::Response(response) => Ok(response),
                Message::Request(_) | Message::Event(_) => continue,
            };
        }
    }

    /// An error for a dead connection, carrying what `ssh` printed, since that
    /// is usually the real reason (host key, permission denied, no route).
    fn lost(&self, what: &str) -> Error {
        // Waiting lets the stderr reader reach EOF, so the text is complete.
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = lock(&self.stderr).take() {
            let _ = handle.join();
        }
        let stderr = String::from_utf8_lossy(&lock(&self.stderr_buf))
            .trim()
            .to_owned();
        if stderr.is_empty() {
            Error::other(format!("{}: {what}", self.host))
        } else {
            Error::other(format!("{}: {what}: {stderr}", self.host))
        }
    }
}

impl Drop for RemoteBrowser {
    fn drop(&mut self) {
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = lock(&self.stderr).take() {
            let _ = handle.join();
        }
    }
}

impl std::fmt::Debug for RemoteBrowser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteBrowser")
            .field("host", &self.host)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

fn unexpected(response: &Response) -> Error {
    let name = format!("{response:?}");
    let name = name.split([' ', '(', '{']).next().unwrap_or("response");
    Error::other(format!("unexpected answer from the remote helper: {name}"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
