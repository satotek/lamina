//! The askpass bridge: how a GUI git command asks for a credential.
//!
//! git asks for a username, a password or an ssh passphrase by running the
//! program named in `GIT_ASKPASS` / `SSH_ASKPASS` with the question as its
//! argument. A window has no terminal to ask on, so Lamina runs itself in a
//! helper mode (`lamina --askpass <prompt>`) that forwards the question to the
//! running app over a local channel and prints the answer on stdout:
//!
//! 1. The app starts an [`AskpassHost`] for one network command. It owns a
//!    private channel (a Unix domain socket in a `0700` directory, or loopback
//!    TCP on Windows) and a random one-time token.
//! 2. The `git` child gets the channel address and token in
//!    [`ENDPOINT_ENV`] / [`TOKEN_ENV`], plus `GIT_ASKPASS` and `SSH_ASKPASS`
//!    pointing at the Lamina executable.
//! 3. The helper sends protocol, token and prompt; the host verifies the token,
//!    hands the prompt to the app, and sends the answer (or a cancel) back.
//!
//! User-configured credential helpers still run first: git only invokes askpass
//! when they had nothing to offer, and `credential.helper` is never overridden.
//! `GIT_TERMINAL_PROMPT` stays `0`, so a failed prompt fails the command instead
//! of waiting on a terminal that does not exist.
//!
//! Answers are kept in [`Answer`], which wipes its bytes when dropped. Nothing
//! is written to disk or to the command log; the helper only writes the answer
//! to its stdout, where git reads it.

use std::collections::hash_map::RandomState;
use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher as _, Hasher as _};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Environment variable carrying the local channel address.
pub const ENDPOINT_ENV: &str = "LAMINA_ASKPASS_ENDPOINT";

/// Environment variable carrying the one-time token that proves the helper was
/// started by this app, not by another local process.
pub const TOKEN_ENV: &str = "LAMINA_ASKPASS_TOKEN";

/// First line of the helper protocol, so a client of another version is refused.
const PROTOCOL: &str = "lamina-askpass/1";

/// Prompts are one line from git; anything larger is a broken or hostile client.
const MAX_PROMPT_BYTES: usize = 4096;

/// An answer longer than this is refused rather than truncated: a silently cut
/// password would fail authentication without saying why.
const MAX_ANSWER_BYTES: usize = 8192;

/// A header line (protocol, token or length) is tiny by comparison.
const MAX_HEADER_BYTES: usize = 512;

/// How long a client may take to send its request, and the host to send its
/// answer. The user's thinking time is not bounded by this: it applies to the
/// protocol exchange, not to the wait for an answer.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A helper that has waited this long for its answer gives up. It is longer
/// than the app's own prompt timeout, so a slow typist is not cut off by the
/// helper first.
const HELPER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(150);

/// How often the accept loop wakes to check whether the host was dropped.
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Distinguishes channel names of hosts created in the same process.
static NEXT_CHANNEL: AtomicU64 = AtomicU64::new(0);

/// A question git asked through the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// git's prompt, e.g. `Password for 'https://user@example.com': `.
    pub text: String,
    /// True when the answer must not be shown while it is typed.
    pub secret: bool,
}

impl Prompt {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            secret: is_secret(&text),
            text,
        }
    }
}

/// Whether a prompt is asking for something that must be masked.
///
/// git has no flag for this, so the words it uses are the only signal. Username
/// and host-key confirmations must stay readable; passwords and passphrases must
/// not be drawn on screen.
pub fn is_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "password",
        "passphrase",
        "passwd",
        "pin ",
        "secret",
        "token",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// A credential that wipes its bytes when dropped.
///
/// The bytes still travel through the git child's stdout, which Lamina cannot
/// wipe, but no copy of them outlives the answer that carries them.
pub struct Answer(Vec<u8>);

impl Answer {
    pub fn new(text: impl AsRef<str>) -> Self {
        Self(text.as_ref().as_bytes().to_vec())
    }

    /// Takes the string's buffer instead of copying it, so no second copy of a
    /// credential is left to drop un-wiped.
    pub fn from_string(text: String) -> Self {
        Self(text.into_bytes())
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the credential into a panic message or a log line.
        formatter.write_str("Answer(<hidden>)")
    }
}

/// The app's side of one prompt. It is called on the host's thread and may
/// block until the user answers; `None` cancels.
pub(crate) type Handler = Arc<dyn Fn(Prompt) -> Option<Answer> + Send + Sync>;

/// Wipes `bytes` with volatile writes, so the zeroing is not optimised away.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, aligned, exclusive reference.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// A random token for one channel.
///
/// `RandomState` is seeded from the OS, and the process id and a counter make
/// two hosts in one process differ even if the seed repeated. The token guards
/// against another local process connecting to the channel; the private socket
/// directory or the loopback port keep it to this machine.
fn random_token() -> String {
    let mut token = String::with_capacity(32);
    for round in 0..2u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(round);
        hasher.write_u32(std::process::id());
        hasher.write_u64(NEXT_CHANNEL.load(Ordering::SeqCst));
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

/// A local listener for one channel.
enum Listener {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
    #[cfg(not(unix))]
    Tcp(std::net::TcpListener),
}

impl Listener {
    /// Binds a channel and returns it with its endpoint and the directory that
    /// has to be removed with it.
    fn bind(unique: &str) -> io::Result<(Self, OsString, Option<std::path::PathBuf>)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;

            let directory = std::env::temp_dir()
                .join(format!("lamina-askpass-{}-{unique}", std::process::id()));
            // Only this user may reach the socket; the token is the second lock.
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&directory)?;
            let path = directory.join("channel.sock");
            match std::os::unix::net::UnixListener::bind(&path) {
                Ok(listener) => Ok((Self::Unix(listener), path.into_os_string(), Some(directory))),
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&directory);
                    Err(error)
                }
            }
        }
        #[cfg(not(unix))]
        {
            // Windows has no portable Unix-socket path here, so the channel is
            // loopback TCP; the token is what keeps other local processes out.
            // An ephemeral port is unique already, so the name is not needed.
            let _ = unique;
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
            let address = listener.local_addr()?;
            Ok((Self::Tcp(listener), address.to_string().into(), None))
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(listener) => listener.set_nonblocking(nonblocking),
            #[cfg(not(unix))]
            Self::Tcp(listener) => listener.set_nonblocking(nonblocking),
        }
    }

    fn accept(&self) -> io::Result<Stream> {
        // The listener is non-blocking so the accept loop can notice a dropped
        // host. BSDs (macOS) hand that flag to the accepted socket, where the
        // protocol below would then read `WouldBlock` when the helper has not
        // sent its request yet and drop a valid prompt; the reads here are
        // timed instead, so the connection has to block.
        match self {
            #[cfg(unix)]
            Self::Unix(listener) => {
                let (stream, _) = listener.accept()?;
                stream.set_nonblocking(false)?;
                Ok(Stream::Unix(stream))
            }
            #[cfg(not(unix))]
            Self::Tcp(listener) => {
                let (stream, _) = listener.accept()?;
                stream.set_nonblocking(false)?;
                Ok(Stream::Tcp(stream))
            }
        }
    }
}

/// One accepted connection.
enum Stream {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(not(unix))]
    Tcp(std::net::TcpStream),
}

impl Stream {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))
            }
            #[cfg(not(unix))]
            Self::Tcp(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))
            }
        }
    }

    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.set_read_timeout(Some(timeout)),
            #[cfg(not(unix))]
            Self::Tcp(stream) => stream.set_read_timeout(Some(timeout)),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buffer),
            #[cfg(not(unix))]
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buffer),
            #[cfg(not(unix))]
            Self::Tcp(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            #[cfg(not(unix))]
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

/// Connects to the channel named by `endpoint`.
fn connect(endpoint: &OsStr) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(endpoint).map(Stream::Unix)
    }
    #[cfg(not(unix))]
    {
        std::net::TcpStream::connect(endpoint.to_string_lossy().as_ref()).map(Stream::Tcp)
    }
}

/// The app's end of one network command's askpass channel.
///
/// Create it before the `git` child and keep it alive until the child exits;
/// dropping it stops the listener and removes the socket.
pub struct AskpassHost {
    endpoint: OsString,
    token: String,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    #[cfg(unix)]
    directory: Option<std::path::PathBuf>,
}

impl AskpassHost {
    /// Starts a host whose prompts `handler` answers.
    pub fn start(
        handler: impl Fn(Prompt) -> Option<Answer> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start_with_handler(Arc::new(handler), IO_TIMEOUT)
    }

    pub(crate) fn start_with_handler(handler: Handler, timeout: Duration) -> io::Result<Self> {
        let unique = NEXT_CHANNEL.fetch_add(1, Ordering::SeqCst).to_string();
        let (listener, endpoint, directory) = Listener::bind(&unique)?;
        #[cfg(not(unix))]
        let _ = directory;
        listener.set_nonblocking(true)?;
        let token = random_token();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let shutdown = Arc::clone(&shutdown);
            let token = token.clone();
            let handler = Arc::clone(&handler);
            thread::Builder::new()
                .name("lamina-askpass".to_owned())
                .spawn(move || serve(listener, &token, &handler, timeout, &shutdown))?
        };
        Ok(Self {
            endpoint,
            token,
            shutdown,
            thread: Some(thread),
            #[cfg(unix)]
            directory,
        })
    }

    /// The channel address and token, for a child process's environment.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        vec![
            (ENDPOINT_ENV.into(), self.endpoint.clone()),
            (TOKEN_ENV.into(), self.token.clone().into()),
        ]
    }

    /// Adds the channel and the askpass programs to a `git` command.
    ///
    /// `program` is the executable that implements [`helper_main`]. `ssh` only
    /// consults `SSH_ASKPASS` without a terminal when `DISPLAY` is set on older
    /// releases, so it is filled in when the user has none; `SSH_ASKPASS_REQUIRE`
    /// makes current `ssh` use the helper regardless.
    pub fn apply(&self, command: &mut Command, program: &std::path::Path) {
        command.env("GIT_ASKPASS", program);
        command.env("SSH_ASKPASS", program);
        command.env("SSH_ASKPASS_REQUIRE", "force");
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let configured = command
                .get_envs()
                .any(|(key, _)| key == OsStr::new("DISPLAY"))
                || std::env::var_os("DISPLAY").is_some();
            if !configured {
                // `ssh` only checks that DISPLAY is non-empty; our helper is not
                // an X client and never opens a display connection. macOS has no
                // X display, and its ssh is new enough for
                // `SSH_ASKPASS_REQUIRE=force`, so it is left alone.
                command.env("DISPLAY", ":0");
            }
        }
        for (key, value) in self.env() {
            command.env(key, value);
        }
    }

    #[cfg(test)]
    fn channel(&self) -> (&OsStr, &str) {
        (&self.endpoint, &self.token)
    }
}

impl Drop for AskpassHost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // The handler polls shutdown (the session and the clone dialog do),
            // so a pending prompt returns instead of holding the operation.
            let _ = thread.join();
        }
        #[cfg(unix)]
        if let Some(directory) = self.directory.take() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

/// Accepts helper connections until the host is dropped.
fn serve(
    listener: Listener,
    token: &str,
    handler: &Handler,
    timeout: Duration,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok(stream) => {
                // A malformed or unauthenticated client is answered with NO by
                // `serve_client` and never reaches the app.
                let _ = serve_client(stream, token, handler, timeout);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
}

fn serve_client(
    stream: Stream,
    token: &str,
    handler: &Handler,
    timeout: Duration,
) -> io::Result<()> {
    stream.set_timeout(timeout)?;
    let mut reader = BufReader::new(stream);
    let prompt = read_request(&mut reader, token);
    let mut stream = reader.into_inner();
    let answer = prompt.ok().and_then(|prompt| handler(prompt));
    write_response(&mut stream, answer.as_ref())
}

/// Reads protocol, token and prompt. Any mismatch is an error, so the handler
/// is not asked for a credential on behalf of a client that guessed wrong.
fn read_request(reader: &mut impl BufRead, token: &str) -> io::Result<Prompt> {
    if read_line(reader)? != PROTOCOL {
        return Err(invalid("unknown askpass protocol"));
    }
    let supplied = read_line(reader)?;
    if !constant_time_eq(supplied.as_bytes(), token.as_bytes()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "askpass token does not match",
        ));
    }
    let length = read_line(reader)?
        .parse::<usize>()
        .map_err(|_| invalid("askpass prompt length is not a number"))?;
    if length > MAX_PROMPT_BYTES {
        return Err(invalid("askpass prompt is too long"));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Prompt::new(String::from_utf8_lossy(&body).into_owned()))
}

fn write_response(stream: &mut Stream, answer: Option<&Answer>) -> io::Result<()> {
    let Some(answer) = answer.filter(|answer| answer.len() <= MAX_ANSWER_BYTES) else {
        return stream.write_all(b"NO\n");
    };
    stream.write_all(b"OK\n")?;
    stream.write_all(format!("{}\n", answer.len()).as_bytes())?;
    stream.write_all(&answer.0)?;
    stream.flush()
}

/// Reads one `\n`-terminated header, refusing anything that is not one.
fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    let mut limited = Read::by_ref(reader).take(MAX_HEADER_BYTES as u64 + 1);
    limited.read_until(b'\n', &mut line)?;
    if line.len() > MAX_HEADER_BYTES || line.last() != Some(&b'\n') {
        return Err(invalid("askpass header is too long or unterminated"));
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// The helper side: asks the app and returns what to print.
fn exchange(endpoint: &OsStr, token: &str, prompt: &str) -> io::Result<Option<Answer>> {
    let mut stream = connect(endpoint)?;
    // Both bounds go on before anything is sent. The answer may take as long
    // as the user needs, and the app, not this timeout, is what bounds that
    // wait; it has to be in place early because the host writes its answer and
    // closes at once, and macOS refuses to set a timeout on a Unix socket
    // whose peer has already closed (EINVAL).
    stream.set_timeout(IO_TIMEOUT)?;
    stream.set_read_timeout(HELPER_RESPONSE_TIMEOUT)?;
    stream.write_all(format!("{PROTOCOL}\n{token}\n{}\n", prompt.len()).as_bytes())?;
    stream.write_all(prompt.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    read_response(&mut reader)
}

/// Reads the host's answer from a channel it may have closed right after
/// writing it: the data is still in the socket buffer, so only the bytes
/// matter, not the connection state.
fn read_response(reader: &mut impl BufRead) -> io::Result<Option<Answer>> {
    match read_line(reader)?.as_str() {
        "OK" => {
            let length = read_line(reader)?
                .parse::<usize>()
                .map_err(|_| invalid("askpass answer length is not a number"))?;
            if length > MAX_ANSWER_BYTES {
                return Err(invalid("askpass answer is too long"));
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body)?;
            Ok(Some(Answer(body)))
        }
        "NO" => Ok(None),
        _ => Err(invalid("unexpected askpass response")),
    }
}

/// Runs helper mode for `prompt`: prints the answer on stdout and returns the
/// process exit code.
///
/// A cancelled or failed prompt exits non-zero, which makes git report its own
/// authentication error; nothing is printed, because a partial answer would be
/// worse than none.
pub fn run_helper(prompt: &str) -> i32 {
    let Some(endpoint) = std::env::var_os(ENDPOINT_ENV) else {
        return 1;
    };
    let Some(token) = std::env::var_os(TOKEN_ENV) else {
        return 1;
    };
    match exchange(&endpoint, &token.to_string_lossy(), prompt) {
        Ok(Some(answer)) => {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(&answer.0);
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
            0
        }
        _ => 1,
    }
}

/// Handles the helper mode git starts, returning its exit code.
///
/// git runs `GIT_ASKPASS` as `program <prompt>` with no extra arguments, so
/// the channel environment is what identifies this as helper mode; the explicit
/// `lamina --askpass <prompt>` form is accepted as well, for tests and for
/// running the helper by hand. `None` means the arguments were the app's.
///
/// This is checked before any window, display or settings work, so git can run
/// the executable without a desktop session.
pub fn helper_main(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    let mut args = args.into_iter().peekable();
    let explicit = args
        .peek()
        .is_some_and(|first| first.as_os_str() == OsStr::new("--askpass"));
    if !explicit && !channel_configured() {
        return None;
    }
    if explicit {
        args.next();
    }
    let Some(prompt) = args.next() else {
        return Some(2);
    };
    if args.next().is_some() {
        return Some(2);
    }
    Some(run_helper(prompt.to_string_lossy().as_ref()))
}

fn channel_configured() -> bool {
    std::env::var_os(ENDPOINT_ENV).is_some() && std::env::var_os(TOKEN_ENV).is_some()
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{Answer, AskpassHost, exchange, helper_main, is_secret};

    /// The channel a host listens on, read from the environment it hands out.
    fn channel(host: &AskpassHost) -> (std::ffi::OsString, String) {
        let (endpoint, token) = host.channel();
        (endpoint.to_os_string(), token.to_owned())
    }

    #[test]
    fn the_helper_gets_the_answer_the_app_gave() {
        let host = AskpassHost::start(|prompt| {
            assert!(prompt.secret, "a password prompt must be masked");
            assert!(prompt.text.contains("Password"), "{}", prompt.text);
            Some(Answer::new("hunter2"))
        })
        .unwrap();

        let (endpoint, token) = channel(&host);
        let answer = exchange(&endpoint, &token, "Password for 'https://x': ")
            .unwrap()
            .expect("the app answered");
        assert_eq!(answer.as_str(), Some("hunter2"));
    }

    #[test]
    fn a_username_prompt_is_not_masked() {
        let host = AskpassHost::start(|prompt| {
            assert!(!prompt.secret, "a username prompt must stay readable");
            Some(Answer::new("alice"))
        })
        .unwrap();

        let (endpoint, token) = channel(&host);
        let answer = exchange(&endpoint, &token, "Username for 'https://x': ")
            .unwrap()
            .expect("the app answered");
        assert_eq!(answer.as_str(), Some("alice"));
    }

    #[test]
    fn cancelling_reaches_the_helper_as_no_answer() {
        let host = AskpassHost::start(|_| None).unwrap();
        let (endpoint, token) = channel(&host);

        let answer = exchange(&endpoint, &token, "Password: ").unwrap();
        assert!(answer.is_none(), "a cancel is not an answer");
    }

    #[test]
    fn a_stale_token_is_refused_without_asking_the_app() {
        let asked = Arc::new(AtomicBool::new(false));
        let host = AskpassHost::start({
            let asked = Arc::clone(&asked);
            move |_| {
                asked.store(true, Ordering::SeqCst);
                Some(Answer::new("secret"))
            }
        })
        .unwrap();
        let (endpoint, _) = channel(&host);

        // The host answers NO rather than the answer; the helper exits non-zero.
        let answer = exchange(&endpoint, "not-the-token", "Password: ").unwrap();
        assert!(answer.is_none());
        assert!(
            !asked.load(Ordering::SeqCst),
            "an unauthenticated client must not reach the prompt handler"
        );
    }

    #[test]
    fn a_client_that_answers_late_is_still_served() {
        // The host accepts before the helper has written its request. On BSDs
        // the accepted socket inherits the listener's non-blocking flag, so a
        // host that trusted it would drop the connection here and the prompt
        // would fail with "unable to read askpass response".
        let host = AskpassHost::start(|_| Some(Answer::new("late"))).unwrap();
        let (endpoint, token) = channel(&host);
        let prompt = "Password: ";

        let mut stream = super::connect(&endpoint).unwrap();
        stream.set_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        stream
            .write_all(
                format!("{}\n{token}\n{}\n{prompt}", super::PROTOCOL, prompt.len()).as_bytes(),
            )
            .unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert_eq!(response, "OK\n4\nlate", "the late request is answered");
    }

    #[cfg(unix)]
    #[test]
    fn an_answer_is_readable_from_a_channel_the_host_already_closed() {
        // The host answers and closes immediately, and macOS refuses a new
        // timeout on a Unix socket whose peer is gone (EINVAL). The answer is
        // already in the socket buffer, so it must be read rather than lost to
        // the connection state.
        let (client, mut host) = std::os::unix::net::UnixStream::pair().unwrap();
        let stream = super::Stream::Unix(client);
        stream.set_timeout(Duration::from_secs(5)).unwrap();
        stream.set_read_timeout(Duration::from_secs(5)).unwrap();
        std::io::Write::write_all(&mut host, b"OK\n4\nlate").unwrap();
        drop(host);

        let answer = super::read_response(&mut std::io::BufReader::new(stream))
            .unwrap()
            .expect("the answer");
        assert_eq!(answer.as_str(), Some("late"));
    }

    #[test]
    fn a_silent_client_times_out_and_is_not_asked() {
        let asked = Arc::new(AtomicBool::new(false));
        let host = AskpassHost::start_with_handler(
            Arc::new({
                let asked = Arc::clone(&asked);
                move |_| {
                    asked.store(true, Ordering::SeqCst);
                    Some(Answer::new("secret"))
                }
            }),
            Duration::from_millis(100),
        )
        .unwrap();
        let (endpoint, _) = channel(&host);

        let mut stream = super::connect(&endpoint).unwrap();
        stream
            .set_timeout(Duration::from_secs(5))
            .expect("client timeout");
        // Say nothing: the host has to close the connection on its own, and the
        // read below then sees either the NO response or EOF.
        let mut received = Vec::new();
        let started = Instant::now();
        stream.read_to_end(&mut received).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the host must not keep a silent client open"
        );
        assert!(
            !asked.load(Ordering::SeqCst),
            "a client that never sent a token must not reach the prompt handler"
        );
    }

    #[test]
    fn a_dropped_host_closes_the_channel_and_removes_the_socket() {
        let host = AskpassHost::start(|_| None).unwrap();
        let (endpoint, token) = channel(&host);
        #[cfg(unix)]
        let socket = PathBuf::from(&endpoint);

        drop(host);

        assert!(
            exchange(&endpoint, &token, "Password: ").is_err(),
            "the helper cannot reach a dropped host"
        );
        #[cfg(unix)]
        assert!(
            !socket.exists(),
            "the socket is removed after the operation"
        );
    }

    #[test]
    fn only_secret_questions_are_masked() {
        assert!(is_secret("Password for 'https://x': "));
        assert!(is_secret(
            "Enter passphrase for key '/home/a/.ssh/id_ed25519': "
        ));
        assert!(is_secret("Enter PIN for key 'yubikey': "));
        assert!(!is_secret("Username for 'https://x': "));
        assert!(!is_secret(
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? "
        ));
    }

    #[test]
    fn helper_main_only_answers_for_the_helper_arguments() {
        assert_eq!(helper_main(Vec::<std::ffi::OsString>::new()), None);
        assert_eq!(
            helper_main([std::ffi::OsString::from("--version")]),
            None,
            "other arguments are the app's"
        );
        assert_eq!(
            helper_main([std::ffi::OsString::from("some/repository")]),
            None,
            "a path is the app's, not a prompt"
        );
        assert_eq!(
            helper_main([
                std::ffi::OsString::from("--askpass"),
                std::ffi::OsString::from("Password: "),
            ]),
            Some(1),
            "without a channel the helper fails instead of prompting a terminal"
        );
        assert_eq!(
            helper_main([std::ffi::OsString::from("--askpass")]),
            Some(2),
            "a prompt argument is required"
        );
    }

    #[test]
    fn writes_a_protocol_error_as_no_answer() {
        // A client that sends a bad protocol is refused without a handler call.
        let host = AskpassHost::start(|_| panic!("must not be asked")).unwrap();
        let (endpoint, _) = channel(&host);
        let mut stream = super::connect(&endpoint).unwrap();
        stream.set_timeout(Duration::from_secs(5)).unwrap();
        stream.write_all(b"not-lamina\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert_eq!(reply.trim(), "NO");
    }
}
