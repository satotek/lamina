//! Proxy support for the SSH transport.
//!
//! SSH ignores the `http_proxy` family of variables, so a proxy reaches it
//! through `ProxyCommand`: ssh is pointed at this executable in a helper mode,
//! and the helper opens the TCP connection through the proxy and copies bytes
//! between it and stdin/stdout.
//!
//! The app publishes the configured proxy as [`PROXY_URL_ENV`] and the
//! exclusions as [`NO_PROXY_ENV`] in its own environment. This module reads
//! only that contract, so connecting the settings page to it is a one-line
//! change. The helper mode is hidden from the normal command line because only
//! ssh and git start it.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// Proxy URL, e.g. `socks5h://user:pass@proxy:1080` or `http://proxy:8080`.
pub const PROXY_URL_ENV: &str = "LAMINA_PROXY_URL";

/// Comma-separated exclusions in the usual `NO_PROXY` syntax.
pub const NO_PROXY_ENV: &str = "LAMINA_NO_PROXY";

/// Hidden command that ssh runs as its `ProxyCommand`.
pub const CONNECT_FLAG: &str = "--proxy-connect";

/// Establishing the tunnel is interactive, so it may not wait forever; the
/// tunnel itself is cleared of these timeouts before any bytes are pumped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// A proxy response larger than this is not a response any of the supported
/// proxies sends. Bounding the read keeps a broken peer from pinning memory.
const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;

/// Transfer size for the tunnel. SSH pads its packets to the cipher block
/// size, so the buffer only needs to be larger than that.
const PUMP_BUFFER: usize = 32 * 1024;

/// What the proxy speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    /// `CONNECT` over a plain-text HTTP/1.1 connection.
    Http,
    /// `CONNECT` over TLS to the proxy. The standard library has no TLS, so
    /// the connector reports this rather than sending the request in clear.
    Https,
    Socks4,
    /// Host name resolved by this machine.
    Socks5,
    /// Host name resolved by the proxy (`socks5h`).
    Socks5h,
}

impl ProxyScheme {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks4" => Some(Self::Socks4),
            "socks5" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
            Self::Socks4 | Self::Socks5 | Self::Socks5h => 1080,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks4 => "socks4",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }
}

/// A parsed proxy URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub scheme: ProxyScheme,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Parses `scheme://[user[:password]@]host[:port]`. Credentials are
    /// percent-decoded, so a password may contain `@`, `:` or `/` in URL form.
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("missing scheme in {url:?}"))?;
        let scheme =
            ProxyScheme::parse(scheme).ok_or_else(|| format!("unsupported scheme {scheme:?}"))?;
        // A path, query or fragment is not part of the proxy address.
        let rest = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (userinfo, authority) = match rest.rsplit_once('@') {
            Some((userinfo, authority)) => (Some(userinfo), authority),
            None => (None, rest),
        };
        if authority.is_empty() {
            return Err("the URL has no host".to_owned());
        }
        let (host, port) = split_host_port(authority, scheme.default_port())?;
        if host.is_empty() {
            return Err("the URL has no host".to_owned());
        }
        let (username, password) = match userinfo {
            None => (None, None),
            Some(userinfo) => {
                let (user, password) = match userinfo.split_once(':') {
                    Some((user, password)) => (user, Some(password)),
                    None => (userinfo, None),
                };
                let user = percent_decode(user);
                let username = (!user.is_empty()).then_some(user);
                (username, password.map(percent_decode))
            }
        };
        Ok(Self {
            scheme,
            host: host.to_owned(),
            port,
            username,
            password,
        })
    }

    /// `host:port`, with brackets around an IPv6 literal.
    fn authority(&self) -> String {
        format_authority(&self.host, self.port)
    }
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(&str, u16), String> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket
            .split_once(']')
            .ok_or_else(|| "unterminated IPv6 address".to_owned())?;
        let port = match rest.strip_prefix(':') {
            Some(value) => parse_port(value)?,
            None if rest.is_empty() => default_port,
            None => return Err(format!("unexpected text after the address: {rest:?}")),
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        // More than one colon without brackets is an IPv6 literal; it needs
        // brackets so the port can be told apart from the address.
        Some(_) if authority.matches(':').count() > 1 => {
            Err("an IPv6 address must be enclosed in brackets".to_owned())
        }
        Some((host, port)) => Ok((host, parse_port(port)?)),
        None => Ok((authority, default_port)),
    }
}

fn parse_port(value: &str) -> Result<u16, String> {
    match value.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!("invalid port {value:?}")),
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let decoded = (bytes[index] == b'%' && index + 2 < bytes.len())
            .then(|| hex_pair(bytes[index + 1], bytes[index + 2]))
            .flatten();
        match decoded {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(high: u8, low: u8) -> Option<u8> {
    Some(hex_digit(high)? << 4 | hex_digit(low)?)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The configured proxy URL, if the app published one.
pub fn proxy_url() -> Option<String> {
    meaningful_env(PROXY_URL_ENV)
}

fn meaningful_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The parsed proxy. A URL that does not parse is reported by the connector
/// instead of silently falling back to a direct connection.
pub fn configured() -> Option<ProxyConfig> {
    let url = proxy_url()?;
    match ProxyConfig::parse(&url) {
        Ok(config) => Some(config),
        Err(error) => {
            log::warn!("ignoring {PROXY_URL_ENV}: {error}");
            None
        }
    }
}

/// Whether a connection to `host:port` should go through the proxy.
///
/// An unparsable URL still counts as "proxy applies": the connector says why
/// instead of quietly leaving the proxy and reaching the host directly.
pub fn applies_to(host: &str, port: u16) -> bool {
    if proxy_url().is_none() {
        return false;
    }
    let no_proxy = std::env::var(NO_PROXY_ENV).unwrap_or_default();
    !no_proxy_matches(host, Some(port), &no_proxy)
}

/// Matches one host against `NO_PROXY` syntax: `*`, exact names, domain
/// suffixes (`example.com`, `.example.com`, `*.example.com`), IPv4 CIDR
/// blocks, and an optional `:port` that restricts the entry to that port.
pub fn no_proxy_matches(host: &str, port: Option<u16>, no_proxy: &str) -> bool {
    let host = normalize_host(host);
    let parsed_host = host.parse::<IpAddr>().ok();
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry_matches(&host, parsed_host, port, entry))
}

fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn entry_matches(host: &str, parsed_host: Option<IpAddr>, port: Option<u16>, entry: &str) -> bool {
    let entry = entry.trim();
    if entry == "*" {
        return true;
    }
    let (entry_host, entry_port) = split_entry_port(entry);
    if let Some(entry_port) = entry_port
        && port != Some(entry_port)
    {
        return false;
    }
    if let Some((network, prefix)) = entry_host.split_once('/') {
        return match (
            parsed_host,
            network.parse::<Ipv4Addr>(),
            prefix.parse::<u8>(),
        ) {
            (Some(IpAddr::V4(host)), Ok(network), Ok(prefix)) if prefix <= 32 => {
                ipv4_in_network(host, network, prefix)
            }
            _ => false,
        };
    }
    let entry_host = normalize_host(entry_host);
    let entry_host = entry_host
        .strip_prefix("*.")
        .or_else(|| entry_host.strip_prefix('*'))
        .unwrap_or(&entry_host);
    let entry_host = entry_host.trim_start_matches('.');
    if entry_host.is_empty() {
        return false;
    }
    host == entry_host || host.ends_with(&format!(".{entry_host}"))
}

/// Splits `host:port` while keeping `[v6]:port` intact. An unbracketed value
/// with several colons is an IPv6 address, not an address with a port.
fn split_entry_port(entry: &str) -> (&str, Option<u16>) {
    if let Some(after_bracket) = entry.strip_prefix('[')
        && let Some((host, rest)) = after_bracket.split_once(']')
    {
        let port = rest.strip_prefix(':').and_then(|v| v.parse::<u16>().ok());
        return (host, port);
    }
    if entry.matches(':').count() == 1
        && let Some((host, port)) = entry.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return (host, Some(port));
    }
    (entry, None)
}

fn ipv4_in_network(host: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(host) & mask == u32::from(network) & mask
}

fn format_authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The `ProxyCommand` value ssh needs to reach `host` through this executable.
pub fn proxy_command(executable: &Path) -> String {
    let style = if cfg!(windows) {
        QuoteStyle::Windows
    } else {
        QuoteStyle::Posix
    };
    proxy_command_with(executable, style)
}

/// Which shell's quoting rules `ProxyCommand` follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStyle {
    /// ssh runs `ProxyCommand` through `/bin/sh` (and git through its POSIX
    /// shell on Windows as well).
    Posix,
    /// ssh on Windows starts the command directly, so it uses the C runtime's
    /// `argv` quoting rules.
    Windows,
}

/// [`proxy_command`] with an explicit quoting style, so both platforms can be
/// checked from one test run.
pub fn proxy_command_with(executable: &Path, style: QuoteStyle) -> String {
    // ssh expands `%h` in the command even inside quotes, and `%%` is its
    // escape for a literal percent, so a path containing `%` survives.
    let executable = quote_executable(executable, style).replace('%', "%%");
    format!("{executable} {CONNECT_FLAG} %h %p")
}

fn quote_executable(executable: &Path, style: QuoteStyle) -> String {
    let value = executable.to_string_lossy();
    match style {
        QuoteStyle::Posix => quote_posix(&value),
        QuoteStyle::Windows => quote_windows(&value),
    }
}

/// Single quotes for a POSIX shell. Safe tokens stay bare so the ssh command
/// line remains readable.
pub fn quote_posix(value: &str) -> String {
    if is_shell_safe(value) {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn is_shell_safe(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'/' | b'.' | b'_' | b'-' | b'+' | b':' | b'@' | b'%' | b'='
                )
        })
}

/// C runtime `argv` quoting, which is what ssh on Windows applies to a
/// `ProxyCommand`: backslashes are literal unless they precede a quote.
pub fn quote_windows(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for ch in value.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(ch);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote would otherwise escape it.
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// `GIT_SSH_COMMAND` that gives git's ssh the same tunnel. git runs the value
/// through a shell, so it takes POSIX quoting on every platform.
pub fn git_ssh_command() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let command = proxy_command_with(&executable, QuoteStyle::Posix);
    Some(format!(
        "ssh -o {}",
        quote_posix(&format!("ProxyCommand={command}"))
    ))
}

/// Runs the hidden helper ssh starts as `ProxyCommand`, returning its exit
/// code. `None` means the arguments belonged to the application.
pub fn connector_main(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<i32> {
    let mut args = args.into_iter();
    if args.next()?.as_os_str() != OsStr::new(CONNECT_FLAG) {
        return None;
    }
    let (Some(host), Some(port)) = (args.next(), args.next()) else {
        eprintln!("lamina: {CONNECT_FLAG} needs a host and a port");
        return Some(2);
    };
    if args.next().is_some() {
        eprintln!("lamina: {CONNECT_FLAG} takes only a host and a port");
        return Some(2);
    }
    let host = host.to_string_lossy().into_owned();
    let Ok(port) = port.to_string_lossy().parse::<u16>() else {
        eprintln!("lamina: {CONNECT_FLAG}: invalid port");
        return Some(2);
    };
    Some(match connect(&host, port) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("lamina: {host}:{port}: {error}");
            1
        }
    })
}

/// Opens the tunnel for one connection and pumps it until either side closes.
pub fn connect(host: &str, port: u16) -> io::Result<()> {
    let proxy = match proxy_url() {
        None => None,
        Some(url) => Some(
            ProxyConfig::parse(&url)
                .map_err(|error| io::Error::other(format!("{PROXY_URL_ENV}: {error}")))?,
        ),
    };
    let stream = match proxy {
        Some(proxy) if !no_proxy_matches(host, Some(port), &no_proxy()) => {
            connect_through(&proxy, host, port)?
        }
        _ => connect_direct(host, port)?,
    };
    let _ = stream.set_nodelay(true);
    pump(stream)
}

fn no_proxy() -> String {
    std::env::var(NO_PROXY_ENV).unwrap_or_default()
}

fn connect_direct(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last_error = None;
    for address in resolve(host, port)? {
        match TcpStream::connect_timeout(&address, HANDSHAKE_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other(format!("no address for {host}"))))
}

/// Connects to the proxy, performs its handshake, and returns the raw tunnel.
fn connect_through(proxy: &ProxyConfig, host: &str, port: u16) -> io::Result<TcpStream> {
    let mut stream = connect_direct(&proxy.host, proxy.port).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not reach the {} proxy at {}: {error}",
                proxy.scheme.label(),
                proxy.authority()
            ),
        )
    })?;
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
    match proxy.scheme {
        ProxyScheme::Http => http_connect(&mut stream, proxy, host, port)?,
        ProxyScheme::Https => {
            return Err(io::Error::other(
                "an https proxy needs TLS, which the built-in connector does not provide; use an http or socks proxy for SSH",
            ));
        }
        ProxyScheme::Socks4 => socks4_connect(&mut stream, proxy, host, port)?,
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => {
            socks5_connect(&mut stream, proxy, host, port)?
        }
    }
    // The handshake is over; from here the tunnel is as slow or as idle as the
    // session behind it.
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(None);
    Ok(stream)
}

fn resolve(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let addresses: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    if addresses.is_empty() {
        return Err(io::Error::other(format!("{host} did not resolve")));
    }
    Ok(addresses)
}

fn resolve_first(host: &str, port: u16) -> io::Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    resolve(host, port)?
        .first()
        .map(SocketAddr::ip)
        .ok_or_else(|| io::Error::other(format!("{host} did not resolve")))
}

fn resolve_ipv4(host: &str, port: u16) -> io::Result<Ipv4Addr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(ip) => Ok(ip),
            IpAddr::V6(_) => Err(io::Error::other(format!(
                "socks4 cannot carry the IPv6 address {host}"
            ))),
        };
    }
    resolve(host, port)?
        .into_iter()
        .find_map(|address| match address.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .ok_or_else(|| io::Error::other(format!("{host} has no IPv4 address for socks4")))
}

fn http_connect(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> io::Result<()> {
    let authority = format_authority(host, port);
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n"
    );
    if let Some(username) = &proxy.username {
        let password = proxy.password.as_deref().unwrap_or("");
        let token = base64(format!("{username}:{password}").as_bytes());
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;

    let head = read_http_head(stream)?;
    let text = String::from_utf8_lossy(&head);
    let status = text.lines().next().unwrap_or("");
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok());
    match code {
        Some(200..=299) => Ok(()),
        Some(407) => Err(io::Error::other(format!(
            "the proxy requires authentication (407 {})",
            status.trim()
        ))),
        Some(_) => Err(io::Error::other(format!(
            "the proxy refused CONNECT {authority}: {status}"
        ))),
        None => Err(io::Error::other(format!(
            "the proxy sent no HTTP status for CONNECT {authority}: {}",
            text.trim()
        ))),
    }
}

/// Reads response headers one byte at a time so no tunneled byte is consumed
/// along with them.
fn read_http_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while head.len() < MAX_HANDSHAKE_BYTES {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    return Ok(head);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "the proxy closed the connection before sending a complete response",
    ))
}

fn socks5_connect(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> io::Result<()> {
    let credentials = proxy.username.as_ref().map(|username| {
        (
            username.as_bytes().to_vec(),
            proxy.password.clone().unwrap_or_default().into_bytes(),
        )
    });
    let methods: &[u8] = if credentials.is_some() {
        &[0x00, 0x02]
    } else {
        &[0x00]
    };
    let mut greeting = vec![0x05, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting)?;

    let mut choice = [0u8; 2];
    read_exact(stream, &mut choice)?;
    if choice[0] != 0x05 {
        return Err(io::Error::other(format!(
            "the proxy answered with SOCKS version {}",
            choice[0]
        )));
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let Some((username, password)) = credentials else {
                return Err(io::Error::other(
                    "the proxy asks for a username and password, but the URL has none",
                ));
            };
            if username.len() > 255 || password.len() > 255 {
                return Err(io::Error::other(
                    "the proxy username or password is longer than 255 bytes",
                ));
            }
            let mut auth = vec![0x01, username.len() as u8];
            auth.extend_from_slice(&username);
            auth.push(password.len() as u8);
            auth.extend_from_slice(&password);
            stream.write_all(&auth)?;
            let mut reply = [0u8; 2];
            read_exact(stream, &mut reply)?;
            if reply[1] != 0x00 {
                return Err(io::Error::other(
                    "the proxy rejected the username and password",
                ));
            }
        }
        0xff => {
            return Err(io::Error::other(
                "the proxy accepts none of the authentication methods offered",
            ));
        }
        method => {
            return Err(io::Error::other(format!(
                "the proxy chose unsupported authentication method {method:#04x}"
            )));
        }
    }

    let mut request = vec![0x05, 0x01, 0x00];
    match if proxy.scheme == ProxyScheme::Socks5h {
        // The name is sent as-is so the proxy resolves it, which is what `h`
        // means and what keeps split-horizon names working.
        match host.parse::<IpAddr>() {
            Ok(ip) => Socks5Host::Ip(ip),
            Err(_) => Socks5Host::Name(host),
        }
    } else {
        Socks5Host::Ip(resolve_first(host, port)?)
    } {
        Socks5Host::Ip(IpAddr::V4(ip)) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        Socks5Host::Ip(IpAddr::V6(ip)) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
        Socks5Host::Name(name) => {
            let bytes = name.as_bytes();
            if bytes.len() > 255 {
                return Err(io::Error::other("the host name is longer than 255 bytes"));
            }
            request.push(0x03);
            request.push(bytes.len() as u8);
            request.extend_from_slice(bytes);
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;

    let mut reply = [0u8; 4];
    read_exact(stream, &mut reply)?;
    if reply[0] != 0x05 {
        return Err(io::Error::other(format!(
            "the proxy answered with SOCKS version {}",
            reply[0]
        )));
    }
    if reply[1] != 0x00 {
        return Err(io::Error::other(format!(
            "the proxy refused {}: {}",
            format_authority(host, port),
            socks5_error(reply[1])
        )));
    }
    // Consume the bound address and port; the caller is not interested in them
    // but they are part of the reply.
    let bound_length = match reply[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut length = [0u8; 1];
            read_exact(stream, &mut length)?;
            usize::from(length[0])
        }
        other => {
            return Err(io::Error::other(format!(
                "the proxy sent unknown address type {other:#04x}"
            )));
        }
    };
    let mut skip = vec![0u8; bound_length + 2];
    read_exact(stream, &mut skip)?;
    Ok(())
}

enum Socks5Host<'a> {
    Ip(IpAddr),
    Name(&'a str),
}

fn socks5_error(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "connection not allowed by its rules",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

fn socks4_connect(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> io::Result<()> {
    let ip = resolve_ipv4(host, port)?;
    let mut request = vec![0x04, 0x01];
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&ip.octets());
    let user = proxy.username.as_deref().unwrap_or("");
    request.extend_from_slice(user.as_bytes());
    request.push(0x00);
    stream.write_all(&request)?;

    let mut reply = [0u8; 8];
    read_exact(stream, &mut reply)?;
    if reply[1] != 90 {
        return Err(io::Error::other(format!(
            "the proxy refused {}: {}",
            format_authority(host, port),
            socks4_error(reply[1])
        )));
    }
    Ok(())
}

fn socks4_error(code: u8) -> &'static str {
    match code {
        91 => "request rejected or failed",
        92 => "the proxy cannot reach its ident service",
        93 => "the proxy rejected the user id",
        _ => "unknown error",
    }
}

fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> io::Result<()> {
    stream.read_exact(buffer).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::other("the proxy closed the connection during the handshake")
        } else {
            error
        }
    })
}

/// Standard base64, only for the HTTP `Basic` credential.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[usize::from(first >> 2)] as char);
        out.push(ALPHABET[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[usize::from((second & 0x0f) << 2 | third >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[usize::from(third & 0x3f)] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Copies stdin to the tunnel and the tunnel to stdout until either side ends.
///
/// `ProxyCommand` owns stdin/stdout for the life of the connection, so this
/// runs until ssh closes them. The stdin side cannot be joined: when the
/// remote closes, the local pipe may still be open, and a blocked read there
/// must not keep the process alive.
fn pump(mut stream: TcpStream) -> io::Result<()> {
    let mut outbound = stream.try_clone()?;
    let _ = std::thread::Builder::new()
        .name("lamina-proxy-in".to_owned())
        .spawn(move || {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let mut buffer = [0u8; PUMP_BUFFER];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if outbound.write_all(&buffer[..read]).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = outbound.shutdown(Shutdown::Write);
        });

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut buffer = [0u8; PUMP_BUFFER];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                output.write_all(&buffer[..read])?;
                output.flush()?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::thread::{self, JoinHandle};

    use super::*;

    fn parse(url: &str) -> ProxyConfig {
        ProxyConfig::parse(url).expect("proxy url")
    }

    #[test]
    fn a_proxy_url_carries_scheme_credentials_and_port() {
        let config = parse("socks5h://alice:s%40cret@proxy.example:1080");
        assert_eq!(config.scheme, ProxyScheme::Socks5h);
        assert_eq!(config.host, "proxy.example");
        assert_eq!(config.port, 1080);
        assert_eq!(config.username.as_deref(), Some("alice"));
        assert_eq!(config.password.as_deref(), Some("s@cret"));

        let config = parse("http://proxy.example");
        assert_eq!(config.scheme, ProxyScheme::Http);
        assert_eq!(config.port, 80);
        assert_eq!(config.username, None);

        let config = parse("socks5://[::1]:9050");
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 9050);
        assert_eq!(config.authority(), "[::1]:9050");

        assert!(ProxyConfig::parse("proxy.example:1080").is_err());
        assert!(ProxyConfig::parse("ftp://proxy.example").is_err());
        assert!(ProxyConfig::parse("http://proxy.example:0").is_err());
        assert!(ProxyConfig::parse("socks5://").is_err());
    }

    #[test]
    fn no_proxy_understands_names_suffixes_ports_and_ranges() {
        assert!(!no_proxy_matches("example.com", Some(22), ""));
        assert!(no_proxy_matches("example.com", Some(22), "*"));
        assert!(no_proxy_matches("example.com", Some(22), "example.com"));
        assert!(no_proxy_matches("api.example.com", Some(22), "example.com"));
        assert!(no_proxy_matches(
            "api.example.com",
            Some(22),
            ".example.com"
        ));
        assert!(no_proxy_matches(
            "api.example.com",
            Some(22),
            "*.example.com"
        ));
        assert!(!no_proxy_matches("badexample.com", Some(22), "example.com"));
        assert!(no_proxy_matches(
            "api.example.com",
            Some(22),
            "other.test, .example.com"
        ));
        assert!(no_proxy_matches("EXAMPLE.com.", Some(22), "example.COM"));

        assert!(no_proxy_matches(
            "example.com",
            Some(2222),
            "example.com:2222"
        ));
        assert!(!no_proxy_matches(
            "example.com",
            Some(22),
            "example.com:2222"
        ));

        assert!(no_proxy_matches(
            "127.0.0.1",
            Some(22),
            "127.0.0.1, localhost"
        ));
        assert!(no_proxy_matches("10.4.5.6", Some(22), "10.0.0.0/8"));
        assert!(!no_proxy_matches("11.4.5.6", Some(22), "10.0.0.0/8"));
        assert!(no_proxy_matches("::1", Some(22), "[::1]"));
        assert!(no_proxy_matches("::1", Some(22), "[::1]:22"));
        assert!(!no_proxy_matches("::1", Some(22), "[::1]:2222"));
    }

    #[test]
    fn proxy_commands_are_quoted_for_each_platform() {
        let plain = PathBuf::from("/usr/local/bin/lamina");
        assert_eq!(
            proxy_command_with(&plain, QuoteStyle::Posix),
            "/usr/local/bin/lamina --proxy-connect %h %p"
        );

        let spaced = PathBuf::from("/Applications/My Apps/lamina binary");
        assert_eq!(
            proxy_command_with(&spaced, QuoteStyle::Posix),
            "'/Applications/My Apps/lamina binary' --proxy-connect %h %p"
        );
        let quoted = proxy_command_with(&PathBuf::from("/tmp/it's/lamina"), QuoteStyle::Posix);
        assert!(quoted.starts_with("'/tmp/it'\\''s/lamina'"));

        assert_eq!(
            proxy_command_with(&PathBuf::from(r"C:\Lamina\lamina.exe"), QuoteStyle::Windows),
            r"C:\Lamina\lamina.exe --proxy-connect %h %p"
        );
        assert_eq!(
            proxy_command_with(
                &PathBuf::from(r"C:\Program Files\Lamina\lamina.exe"),
                QuoteStyle::Windows
            ),
            r#""C:\Program Files\Lamina\lamina.exe" --proxy-connect %h %p"#
        );
        assert_eq!(
            proxy_command_with(
                &PathBuf::from("C:\\Program Files\\a b\\lamina.exe"),
                QuoteStyle::Windows
            ),
            "\"C:\\Program Files\\a b\\lamina.exe\" --proxy-connect %h %p"
        );

        let percent = PathBuf::from("/opt/100%/lamina");
        assert_eq!(
            proxy_command_with(&percent, QuoteStyle::Posix),
            "/opt/100%%/lamina --proxy-connect %h %p"
        );
    }

    #[test]
    fn windows_quoting_keeps_a_trailing_backslash_literal() {
        assert_eq!(
            quote_windows(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
        assert_eq!(quote_windows("a b"), r#""a b""#);
        assert_eq!(quote_windows("plain"), "plain");
        assert_eq!(quote_windows(""), r#""""#);
        // A backslash before an embedded quote is doubled and the quote is
        // escaped, so ssh reads back the same path.
        assert_eq!(quote_windows("a\\\"b"), r#""a\\\"b""#);
    }

    #[test]
    fn basic_auth_is_standard_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"alice:s3cret"), "YWxpY2U6czNjcmV0");
    }

    #[test]
    fn the_connector_only_claims_its_own_command() {
        use std::ffi::OsString;
        assert_eq!(
            connector_main(Vec::<OsString>::new()),
            None,
            "a normal launch must reach the UI"
        );
        assert_eq!(
            connector_main([OsString::from("--fake")]),
            None,
            "application flags are not the connector's"
        );
        assert_eq!(
            connector_main([OsString::from(CONNECT_FLAG)]),
            Some(2),
            "a missing target is a usage error"
        );
        assert_eq!(
            connector_main([
                OsString::from(CONNECT_FLAG),
                OsString::from("host"),
                OsString::from("not-a-port"),
            ]),
            Some(2)
        );
    }

    /// Starts a fake proxy that handles one connection with `handler`.
    fn fake_proxy(
        handler: impl FnOnce(&mut TcpStream) + Send + 'static,
    ) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
        let address = listener.local_addr().expect("address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            handler(&mut stream);
        });
        (address, handle)
    }

    fn proxy_config(
        address: SocketAddr,
        scheme: ProxyScheme,
        credentials: Option<(&str, &str)>,
    ) -> ProxyConfig {
        ProxyConfig {
            scheme,
            host: address.ip().to_string(),
            port: address.port(),
            username: credentials.map(|(user, _)| user.to_owned()),
            password: credentials.map(|(_, password)| password.to_owned()),
        }
    }

    fn read_request(stream: &mut TcpStream, until: &[u8]) -> Vec<u8> {
        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];
        while buffer.len() < MAX_HANDSHAKE_BYTES {
            stream.read_exact(&mut byte).expect("request byte");
            buffer.push(byte[0]);
            if buffer.ends_with(until) {
                break;
            }
        }
        buffer
    }

    fn round_trip(mut stream: TcpStream) {
        stream.write_all(b"ping").expect("write");
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).expect("read echo");
        assert_eq!(&echoed, b"ping");
    }

    #[test]
    fn an_http_proxy_gets_a_connect_with_basic_auth() {
        let (address, handle) = fake_proxy(|stream| {
            let request = read_request(stream, b"\r\n\r\n");
            let text = String::from_utf8_lossy(&request);
            assert!(
                text.starts_with("CONNECT example.test:22 HTTP/1.1\r\n"),
                "{text}"
            );
            assert!(text.contains("Host: example.test:22\r\n"), "{text}");
            assert!(
                text.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0\r\n"),
                "{text}"
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\nProxy-Agent: fake\r\n\r\n")
                .expect("response");
            let mut echoed = [0u8; 4];
            stream.read_exact(&mut echoed).expect("ping");
            stream.write_all(&echoed).expect("echo");
        });
        let proxy = proxy_config(address, ProxyScheme::Http, Some(("alice", "s3cret")));
        let stream = connect_through(&proxy, "example.test", 22).expect("tunnel");
        round_trip(stream);
        handle.join().expect("proxy thread");
    }

    #[test]
    fn a_407_is_reported_as_an_authentication_failure() {
        let (address, handle) = fake_proxy(|stream| {
            let _ = read_request(stream, b"\r\n\r\n");
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"p\"\r\n\r\n",
                )
                .expect("response");
        });
        let proxy = proxy_config(address, ProxyScheme::Http, None);
        let error = connect_through(&proxy, "example.test", 22).expect_err("407");
        assert!(error.to_string().contains("407"), "{error}");
        assert!(error.to_string().contains("authentication"), "{error}");
        handle.join().expect("proxy thread");
    }

    #[test]
    fn socks5_without_authentication_sends_the_host_name() {
        let (address, handle) = fake_proxy(|stream| {
            let mut greeting = [0u8; 3];
            stream.read_exact(&mut greeting).expect("greeting");
            assert_eq!(greeting, [0x05, 0x01, 0x00]);
            stream.write_all(&[0x05, 0x00]).expect("method");

            let mut header = [0u8; 5];
            stream.read_exact(&mut header).expect("request");
            assert_eq!(&header[..4], &[0x05, 0x01, 0x00, 0x03]);
            let length = usize::from(header[4]);
            let mut rest = vec![0u8; length + 2];
            stream.read_exact(&mut rest).expect("request rest");
            assert_eq!(&rest[..length], b"example.test");
            assert_eq!(&rest[length..], &22u16.to_be_bytes());

            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
                .expect("reply");
            let mut echoed = [0u8; 4];
            stream.read_exact(&mut echoed).expect("ping");
            stream.write_all(&echoed).expect("echo");
        });
        let proxy = proxy_config(address, ProxyScheme::Socks5h, None);
        let stream = connect_through(&proxy, "example.test", 22).expect("tunnel");
        round_trip(stream);
        handle.join().expect("proxy thread");
    }

    #[test]
    fn socks5_authenticates_when_the_url_has_credentials() {
        let (address, handle) = fake_proxy(|stream| {
            let mut greeting = [0u8; 4];
            stream.read_exact(&mut greeting).expect("greeting");
            assert_eq!(greeting, [0x05, 0x02, 0x00, 0x02]);
            stream.write_all(&[0x05, 0x02]).expect("method");

            let mut header = [0u8; 2];
            stream.read_exact(&mut header).expect("auth header");
            assert_eq!(header, [0x01, 5]);
            let mut user = [0u8; 5];
            stream.read_exact(&mut user).expect("user");
            assert_eq!(&user, b"alice");
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).expect("password length");
            let mut password = vec![0u8; usize::from(length[0])];
            stream.read_exact(&mut password).expect("password");
            assert_eq!(password, b"s3cret");
            stream.write_all(&[0x01, 0x00]).expect("auth ok");

            // socks5 resolves locally, so the address arrives as an IP rather
            // than the name the caller passed.
            let mut header = [0u8; 4];
            stream.read_exact(&mut header).expect("request");
            assert_eq!(header[..3], [0x05, 0x01, 0x00]);
            let address_length = match header[3] {
                0x01 => 4,
                0x04 => 16,
                other => panic!("expected an IP address, got type {other:#04x}"),
            };
            let mut request = vec![0u8; address_length + 2];
            stream.read_exact(&mut request).expect("request address");
            assert_eq!(&request[address_length..], &22u16.to_be_bytes());
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
                .expect("reply");
        });
        let proxy = proxy_config(address, ProxyScheme::Socks5, Some(("alice", "s3cret")));
        connect_through(&proxy, "localhost", 22).expect("tunnel");
        handle.join().expect("proxy thread");
    }

    #[test]
    fn socks5_failures_name_the_reason() {
        let (address, handle) = fake_proxy(|stream| {
            let mut greeting = [0u8; 3];
            stream.read_exact(&mut greeting).expect("greeting");
            stream.write_all(&[0x05, 0x00]).expect("method");
            let mut request = [0u8; 10];
            let _ = stream.read(&mut request);
            stream
                .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .expect("reply");
        });
        let proxy = proxy_config(address, ProxyScheme::Socks5h, None);
        let error = connect_through(&proxy, "127.0.0.1", 22).expect_err("refused");
        assert!(error.to_string().contains("connection refused"), "{error}");
        handle.join().expect("proxy thread");
    }

    #[test]
    fn socks4_uses_the_proxy_username_as_the_user_id() {
        let (address, handle) = fake_proxy(|stream| {
            let mut request = [0u8; 8];
            stream.read_exact(&mut request).expect("request");
            assert_eq!(request[0], 0x04);
            assert_eq!(request[1], 0x01);
            assert_eq!(&request[2..4], &22u16.to_be_bytes());
            assert_eq!(&request[4..8], &[127, 0, 0, 1]);
            let mut user = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).expect("user byte");
                if byte[0] == 0x00 {
                    break;
                }
                user.push(byte[0]);
            }
            assert_eq!(user, b"alice");
            stream
                .write_all(&[0x00, 90, 0x00, 0x00, 0, 0, 0, 0])
                .expect("reply");
            let mut echoed = [0u8; 4];
            stream.read_exact(&mut echoed).expect("ping");
            stream.write_all(&echoed).expect("echo");
        });
        let proxy = proxy_config(address, ProxyScheme::Socks4, Some(("alice", "")));
        let stream = connect_through(&proxy, "127.0.0.1", 22).expect("tunnel");
        round_trip(stream);
        handle.join().expect("proxy thread");
    }

    #[test]
    fn an_https_proxy_says_why_it_cannot_carry_ssh() {
        let proxy = ProxyConfig {
            scheme: ProxyScheme::Https,
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: None,
            password: None,
        };
        // The TCP connection happens first, then the scheme check.
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
        let mut proxy = proxy;
        proxy.port = listener.local_addr().expect("address").port();
        let handle = thread::spawn(move || {
            let _ = listener.accept();
        });
        let error = connect_through(&proxy, "example.test", 22).expect_err("https");
        assert!(error.to_string().contains("https"), "{error}");
        handle.join().expect("listener thread");
    }
}
