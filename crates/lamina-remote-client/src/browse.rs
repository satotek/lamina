//! Browsing folders on a host before a repository is chosen.
//!
//! The picker keeps one `ssh` connection to a `lamina-remote --browse` helper
//! open while the user walks the tree, so each step is one round trip instead
//! of a fresh SSH handshake. Calls block; the app makes them off the UI thread.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lamina_core::{Error, Result};
use lamina_remote_protocol::{self as proto, DirListingDto, Message, Request, Response};

use crate::session::RemoteTarget;
use crate::ssh::{self, AskpassSession, SshOptions};

/// How long one browse request may wait for its answer.
///
/// The helper answers a listing as fast as the host reads a directory, so a
/// hung mount must fail the request instead of leaving the picker waiting
/// forever. `ssh` keepalives do not help here: the connection is alive, the
/// host is not answering.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The write half of the connection. The read half lives on its own thread so
/// a request never holds this while waiting for an answer.
struct Pipe {
    writer: BufWriter<Box<dyn Write + Send>>,
    next_stream: u16,
}

/// One frame from the reader thread, or why the stream ended.
enum ReadEvent {
    Frame(Box<proto::Frame>),
    Closed(String),
}

/// Where the reader thread hands a frame to the request waiting for it.
///
/// The app runs a request per folder click on the same browser, so several
/// are in flight at once. Each request owns its receiver; the reader owns the
/// sender until it delivers the frame that names that stream.
#[derive(Default)]
struct Inbox {
    waiters: HashMap<u16, Sender<ReadEvent>>,
    /// Why the stream ended, once the reader stopped. A request that arrives
    /// afterwards fails at once instead of waiting out a deadline on a
    /// connection that will never answer.
    closed: Option<String>,
}

/// A live connection to a folder-listing helper on one host.
pub struct RemoteBrowser {
    host: String,
    home: String,
    pipe: Mutex<Pipe>,
    /// One answer slot per request in flight, keyed by stream id.
    inbox: Arc<Mutex<Inbox>>,
    /// Not joined by `Drop`: it can be parked inside a read that only the
    /// killed child's closed pipe would end.
    reader: Mutex<Option<JoinHandle<()>>>,
    child: Mutex<Option<Child>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    timeout: Duration,
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
        let stderr_thread = match thread::Builder::new()
            .name("lamina-browse-ssh-err".to_owned())
            .spawn(move || ssh::capture_stderr(stderr, &capture))
        {
            Ok(handle) => handle,
            Err(error) => {
                // Nothing would drain the pipes without the reader, so the
                // child has to be reaped here or it outlives the failed open.
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::other(format!("could not read ssh stderr: {error}")));
            }
        };

        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let reading = Arc::clone(&inbox);
        let reader_thread = match thread::Builder::new()
            .name("lamina-browse-read".to_owned())
            .spawn(move || read_frames(stdout, reading))
        {
            Ok(handle) => handle,
            Err(error) => {
                // The stderr reader is parked on the child's pipe, so it has
                // to be reaped here too or it outlives the failed open.
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_thread.join();
                return Err(Error::other(format!("could not read from ssh: {error}")));
            }
        };

        let mut browser = Self {
            host: target.host.clone(),
            home: String::new(),
            pipe: Mutex::new(Pipe {
                writer: BufWriter::new(Box::new(stdin) as Box<dyn Write + Send>),
                next_stream: 1,
            }),
            inbox,
            reader: Mutex::new(Some(reader_thread)),
            child: Mutex::new(Some(child)),
            stderr: Mutex::new(Some(stderr_thread)),
            stderr_buf,
            timeout: REQUEST_TIMEOUT,
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

    /// One request and its response.
    ///
    /// Each request waits on its own channel, and the reader thread hands a
    /// frame only to the request whose stream it names. Requests from
    /// different threads share the connection (the app opens one per folder
    /// click), so one must never take an answer that belongs to another.
    ///
    /// The read happens on the reader thread and this waits on a channel, so a
    /// host that stops answering ends the wait at the deadline instead of
    /// holding the pipe forever.
    fn request(&self, request: Request) -> Result<Response> {
        let (answer_tx, answer) = mpsc::channel();
        let stream = self.send(answer_tx, request)?;
        let deadline = Instant::now() + self.timeout;
        match self.wait(stream, &answer, deadline)? {
            ReadEvent::Frame(frame) => match frame.message {
                Message::Response(Response::Error(error)) => Err(Error::other(error.message)),
                Message::Response(response) => Ok(response),
                Message::Request(_) | Message::Event(_) => Err(Error::other(format!(
                    "unexpected message from the remote helper on stream {stream}"
                ))),
            },
            ReadEvent::Closed(what) => Err(self.lost(&what)),
        }
    }

    /// Puts this request's answer slot in the inbox and writes the request.
    ///
    /// The slot goes in before the bytes leave, so an answer that arrives
    /// while the write is still flushing has somewhere to land.
    fn send(&self, answer: Sender<ReadEvent>, request: Request) -> Result<u16> {
        let mut pipe = lock(&self.pipe);
        let stream = pipe.next_stream;
        pipe.next_stream = pipe.next_stream.wrapping_add(1).max(1);
        {
            let mut inbox = lock(&self.inbox);
            if let Some(what) = inbox.closed.clone() {
                drop(inbox);
                drop(pipe);
                return Err(self.lost(&what));
            }
            inbox.waiters.insert(stream, answer);
        }
        let sent = proto::write_frame(&mut pipe.writer, stream, &Message::Request(request))
            .and_then(|()| pipe.writer.flush().map_err(Into::into));
        if let Err(error) = sent {
            // Nothing will answer a request that never left the pipe.
            lock(&self.inbox).waiters.remove(&stream);
            drop(pipe);
            return Err(self.lost(&format!("remote write failed: {error}")));
        }
        Ok(stream)
    }

    /// Waits for this request's one answer, or its deadline.
    ///
    /// Nothing here holds a lock that another request needs: the receiver
    /// belongs to this request, and the inbox is only touched to take the slot
    /// back. A request whose slot was already taken by the reader gets the
    /// answer it left in the channel.
    fn wait(
        &self,
        stream: u16,
        answer: &Receiver<ReadEvent>,
        deadline: Instant,
    ) -> Result<ReadEvent> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            match answer.recv_timeout(remaining) {
                Ok(event) => return Ok(event),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.lost("the connection closed"));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        match self.reclaim(stream, answer) {
            Some(event) => Ok(event),
            None => Err(self.not_answered()),
        }
    }

    /// Takes this request's slot back at the deadline, and reports an answer
    /// the reader had already taken it for.
    ///
    /// A slot still in the inbox cannot receive again, so a late answer is
    /// dropped there instead of leaking. When the reader had already taken it,
    /// it hands the frame over while holding the inbox, so the frame is in the
    /// channel by the time this finds the slot gone.
    fn reclaim(&self, stream: u16, answer: &Receiver<ReadEvent>) -> Option<ReadEvent> {
        if lock(&self.inbox).waiters.remove(&stream).is_some() {
            return None;
        }
        answer.try_recv().ok()
    }

    fn not_answered(&self) -> Error {
        self.lost(&format!(
            "the remote helper did not answer within {:?}",
            self.timeout
        ))
    }

    /// Kills the ssh child and joins the stderr reader. Idempotent: `lost`
    /// disarms both, and `Drop` runs again afterwards.
    fn kill_ssh(&self) {
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = lock(&self.stderr).take() {
            let _ = handle.join();
        }
        // The frame reader is dropped, not joined: it may still be inside a
        // read that only the killed child's pipe would end, and `Drop` must
        // not wait on a read that a broken host never answers.
        drop(lock(&self.reader).take());
    }

    /// An error for a dead connection, carrying what `ssh` printed, since that
    /// is usually the real reason (host key, permission denied, no route).
    fn lost(&self, what: &str) -> Error {
        // Waiting lets the stderr reader reach EOF, so the text is complete.
        self.kill_ssh();
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
        self.kill_ssh();
    }
}

#[cfg(test)]
impl RemoteBrowser {
    /// A browser over streams the test owns, with a deadline short enough to
    /// observe. No ssh child or stderr reader is involved.
    fn with_streams(
        host: &str,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        timeout: Duration,
    ) -> Self {
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let reading = Arc::clone(&inbox);
        let reader = thread::Builder::new()
            .name("lamina-browse-read-test".to_owned())
            .spawn(move || read_frames(reader, reading))
            .expect("reader thread");
        Self {
            host: host.to_owned(),
            home: String::new(),
            pipe: Mutex::new(Pipe {
                writer: BufWriter::new(Box::new(writer) as Box<dyn Write + Send>),
                next_stream: 1,
            }),
            inbox,
            reader: Mutex::new(Some(reader)),
            child: Mutex::new(None),
            stderr: Mutex::new(None),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
            timeout,
            askpass: None,
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

/// Decodes frames for the lifetime of the stream and hands each one to the
/// request waiting on its stream id.
///
/// The thread exists so a request can wait on a channel with a deadline: a
/// pipe has no portable read timeout, and a host that stops answering would
/// otherwise park the caller inside `read` for as long as it likes.
fn read_frames(reader: impl Read, inbox: Arc<Mutex<Inbox>>) {
    let mut reader = BufReader::new(reader);
    loop {
        let event = match proto::read_frame(&mut reader) {
            Ok(Some(frame)) => ReadEvent::Frame(Box::new(frame)),
            Ok(None) => ReadEvent::Closed("the connection closed".to_owned()),
            Err(error) => ReadEvent::Closed(format!("remote read failed: {error}")),
        };
        match event {
            ReadEvent::Frame(frame) => {
                let mut inbox = lock(&inbox);
                // A stream without a slot belongs to a request that gave up
                // at its deadline, so a late answer is dropped here rather
                // than handed to whichever request waits next.
                if let Some(waiter) = inbox.waiters.remove(&frame.stream) {
                    // Sending while the inbox is locked keeps the slot and
                    // the answer in step for `reclaim`.
                    let _ = waiter.send(ReadEvent::Frame(frame));
                }
            }
            ReadEvent::Closed(what) => {
                let mut inbox = lock(&inbox);
                inbox.closed = Some(what.clone());
                let waiters = std::mem::take(&mut inbox.waiters);
                drop(inbox);
                // Every request in flight would otherwise wait out its whole
                // deadline on a stream that can no longer answer.
                for waiter in waiters.into_values() {
                    let _ = waiter.send(ReadEvent::Closed(what.clone()));
                }
                return;
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream that delivers nothing until the test drops the sender, the way
    /// a host stuck in a directory read never writes an answer.
    struct Stall(mpsc::Receiver<()>);

    impl Read for Stall {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    /// The writer side of a fake helper: what the browser writes reaches the
    /// peer thread as chunks.
    struct ChannelWriter(mpsc::Sender<Vec<u8>>);

    impl Write for ChannelWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            // A closed peer surfaces the way a broken pipe would.
            self.0
                .send(buf.to_vec())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The reading side of a fake helper: chunks arrive from the other end of
    /// the connection, and the end of the channel is end of stream.
    struct ChannelReader {
        chunks: mpsc::Receiver<Vec<u8>>,
        chunk: Vec<u8>,
        used: usize,
    }

    impl ChannelReader {
        fn new(chunks: mpsc::Receiver<Vec<u8>>) -> Self {
            Self {
                chunks,
                chunk: Vec::new(),
                used: 0,
            }
        }
    }

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            while self.used == self.chunk.len() {
                match self.chunks.recv() {
                    Ok(chunk) => {
                        self.chunk = chunk;
                        self.used = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let len = buf.len().min(self.chunk.len() - self.used);
            buf[..len].copy_from_slice(&self.chunk[self.used..self.used + len]);
            self.used += len;
            Ok(len)
        }
    }

    /// An answer that names the listing it carries.
    fn answer(listing: &str) -> Message {
        Message::Response(Response::DirListing {
            listing: DirListingDto {
                path: listing.to_owned(),
                parent: None,
                repository: false,
                entries: Vec::new(),
                truncated: false,
            },
        })
    }

    fn write_answer(writer: &mut impl Write, stream: u16, listing: &str) {
        proto::write_frame(writer, stream, &answer(listing))
            .and_then(|()| writer.flush().map_err(Into::into))
            .expect("the peer must write its answers");
    }

    /// Two requests in flight on one browser must each receive the answer for
    /// their own stream.
    ///
    /// The peer answers the second request first, which is the case a shared
    /// answer queue loses: the request already waiting takes that frame,
    /// drops it as not its own, and the second request never hears back. In
    /// the app every folder click runs a request on the same browser, so the
    /// lost answer ended the ssh connection for every later navigation.
    #[test]
    fn an_answer_reaches_the_request_that_asked_for_it() {
        let (to_peer, peer_in) = mpsc::channel();
        let (peer_out, from_peer) = mpsc::channel();
        let browser = RemoteBrowser::with_streams(
            "peer.example",
            ChannelReader::new(from_peer),
            ChannelWriter(to_peer),
            Duration::from_secs(5),
        );
        let peer = thread::spawn(move || {
            let mut requests = BufReader::new(ChannelReader::new(peer_in));
            let first = proto::read_frame(&mut requests)
                .expect("request bytes")
                .expect("a first request");
            let second = proto::read_frame(&mut requests)
                .expect("request bytes")
                .expect("a second request");
            let mut answers = ChannelWriter(peer_out);
            // Both requests are in, so the first is parked on its answer.
            write_answer(&mut answers, second.stream, "/second");
            write_answer(&mut answers, first.stream, "/first");
            let third = proto::read_frame(&mut requests)
                .expect("request bytes")
                .expect("a third request");
            write_answer(&mut answers, third.stream, "/third");
        });

        let (first, second) = thread::scope(|scope| {
            let first = scope.spawn(|| browser.list(None));
            // Starting the second request after the first is parked is what
            // puts them both in flight at once.
            thread::sleep(Duration::from_millis(20));
            let second = scope.spawn(|| browser.list(Some("/second")));
            (
                first.join().expect("first request thread"),
                second.join().expect("second request thread"),
            )
        });
        let third = browser.list(Some("/third"));
        peer.join().expect("peer thread");

        assert_eq!(first.expect("the first answer").path, "/first");
        assert_eq!(second.expect("the second answer").path, "/second");
        assert_eq!(
            third.expect("a connection that stayed alive").path,
            "/third"
        );
    }

    /// The browser plus the sender that later releases its reader thread.
    fn stalling_browser(timeout: Duration) -> (RemoteBrowser, mpsc::Sender<()>) {
        let (release, stalled) = mpsc::channel();
        let browser =
            RemoteBrowser::with_streams("hung.example", Stall(stalled), Vec::new(), timeout);
        (browser, release)
    }

    #[test]
    fn a_hung_listing_returns_after_the_deadline() {
        let (browser, release) = stalling_browser(Duration::from_millis(150));
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let started = Instant::now();
            let error = browser.list(None).expect_err("a host that never answers");
            let _ = done_tx.send((started.elapsed(), error.to_string()));
        });
        let (elapsed, text) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the listing must end at its deadline");
        assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
        assert!(text.contains("did not answer"), "{text}");
        drop(release);
    }

    #[cfg(unix)]
    #[test]
    fn drop_kills_ssh_even_while_a_read_is_stuck() {
        use std::process::Command;

        let (browser, release) = stalling_browser(Duration::from_millis(50));
        let child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("a child to kill");
        let pid = child.id();
        *lock(&browser.child) = Some(child);
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(browser);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Drop must not wait for a stuck read");
        let alive = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(!alive, "the ssh child outlived the browser");
        drop(release);
    }
}
