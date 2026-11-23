//! OS proxy detection for the "use the system proxy" mode.
//!
//! [`detect`] answers one question: which proxy would the operating system
//! pick for this URL. Sources are consulted in the order a native app would —
//! the platform settings first, environment variables second — and a failing
//! source is logged and skipped, never propagated: a broken settings store
//! can at worst cost the proxy, not the operation.
//!
//! On Windows the answer may come from a PAC script (or WPAD auto-detect),
//! which WinHTTP resolves with DNS and DHCP probes that block for seconds,
//! so callers run [`detect`] off the UI thread.
//!
//! Linux is covered by the environment variables alone: every desktop keeps
//! proxy settings in its own session services with no stable shared API, so
//! there is no meaningful "system" source to read there.

use super::{ProxyConfig, ProxyEndpoint, ProxyScheme};

/// The proxy the OS would use for `url`.
///
/// May block while PAC scripts are downloaded and evaluated; call from a
/// background thread. Never panics: any failing source is logged and the
/// next one is tried, ending at [`ProxyConfig::default`].
pub fn detect(url: &str) -> ProxyConfig {
    let scheme = url_scheme(url);
    match platform_config(url, scheme.as_deref()) {
        Some(config) => config,
        None => env_config(scheme.as_deref()),
    }
}

#[cfg(target_os = "windows")]
fn platform_config(url: &str, scheme: Option<&str>) -> Option<ProxyConfig> {
    windows::detect(url, scheme)
}

#[cfg(target_os = "macos")]
fn platform_config(_url: &str, scheme: Option<&str>) -> Option<ProxyConfig> {
    macos::detect(scheme)
}

// No platform source on Linux: see the module docs.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_config(_url: &str, _scheme: Option<&str>) -> Option<ProxyConfig> {
    None
}

/// Lowercased URL scheme ("https" for `HTTPS://host/…`), or `None` when the
/// input is not a URL-shaped string.
fn url_scheme(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

/// The port an endpoint speaks when none was stated: the IANA registration
/// for each proxy protocol. A corporate proxy on a non-standard port always
/// states it, so the defaults only matter for shorthand values.
fn default_port(scheme: ProxyScheme) -> u16 {
    match scheme {
        ProxyScheme::Http => 80,
        ProxyScheme::Https => 443,
        ProxyScheme::Socks4 | ProxyScheme::Socks5 | ProxyScheme::Socks5h => 1080,
    }
}

/// Parses `host[:port]`, with `[IPv6]` brackets allowed. `scheme` is the
/// protocol the *proxy itself* speaks, which also picks the default port.
/// `None` on anything unparsable.
fn parse_host_port(raw: &str, scheme: ProxyScheme) -> Option<ProxyEndpoint> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        // Bracketed IPv6 literal, optionally followed by `:port`.
        let (address, tail) = rest.split_once(']')?;
        let port = match tail {
            "" => None,
            tail => Some(tail.split_once(':')?.1),
        };
        (address, port)
    } else {
        match raw.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (raw, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None => default_port(scheme),
        Some(port) => port.parse().ok()?,
    };
    Some(ProxyEndpoint {
        scheme,
        host: host.to_owned(),
        port,
        username: None,
        password: None,
    })
}

/// Parses the Internet Options proxy string — `WinHttpGetIEProxyConfigForCurrentUser`
/// and WinHTTP's autoproxy results share this layout: a bare `host:port`
/// default, or per-protocol entries `http=a:1;https=b:2;socks=c:3`.
///
/// The entry keys name the traffic routed through the proxy, not the
/// protocol the proxy itself speaks: `http=`, `https=` and a bare entry all
/// point at an HTTP proxy, so all three map to [`ProxyScheme::Http`]. A
/// `socks=` entry maps to [`ProxyScheme::Socks4`]: the WinINet stack only
/// ever speaks SOCKS4 to it, and guessing SOCKS5 would break setups whose
/// servers tolerate nothing newer.
///
/// `url_specific` marks a string that is already the answer for this one URL
/// (an autoproxy result), where bare and `socks=` entries apply regardless of
/// the URL's scheme. In a static Internet Options string WinINet routes only
/// HTTP and HTTPS through them.
// Unit tests exercise the Windows parsers on every OS, hence the `test` arm.
#[cfg(any(target_os = "windows", test))]
fn parse_windows_proxy_string(
    raw: &str,
    url_scheme: Option<&str>,
    url_specific: bool,
) -> Option<ProxyEndpoint> {
    if !url_specific && !matches!(url_scheme, Some("http" | "https")) {
        return None;
    }
    let mut per_scheme = None;
    let mut bare = None;
    let mut socks = None;
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry.split_once('=') {
            Some((key, value)) => {
                let key = key.trim().to_ascii_lowercase();
                let value = value.trim();
                match key.as_str() {
                    "http" if url_scheme == Some("http") => {
                        per_scheme = parse_host_port(value, ProxyScheme::Http);
                    }
                    "https" if url_scheme == Some("https") => {
                        per_scheme = parse_host_port(value, ProxyScheme::Http);
                    }
                    "socks" => socks = parse_host_port(value, ProxyScheme::Socks4),
                    // `ftp=` and anything else is noise this client never routes.
                    _ => {}
                }
            }
            None => bare = parse_host_port(entry, ProxyScheme::Http),
        }
    }
    per_scheme.or(bare).or(socks)
}

/// Splits a Windows bypass list (`<local>;*.corp;10.0.0.0/8`) into `no_proxy`
/// entries. macOS exception lists use the same wildcard syntax, so they go
/// through the same normalisation.
#[cfg(any(target_os = "windows", test))]
fn parse_windows_bypass(raw: Option<&str>) -> Vec<String> {
    raw.into_iter()
        .flat_map(|raw| raw.split(';'))
        .filter_map(normalize_bypass_entry)
        .collect()
}

/// One bypass-list entry in NO_PROXY spelling, or `None` when it cannot be
/// expressed. `*.corp` becomes `corp`, the suffix-match form NO_PROXY
/// consumers understand; `<local>` ("bypass dotless hostnames") has no
/// NO_PROXY token, so it is dropped rather than mistranslated.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn normalize_bypass_entry(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.eq_ignore_ascii_case("<local>") {
        log::debug!("dropping `<local>` from the bypass list: NO_PROXY syntax cannot express it");
        return None;
    }
    let entry = entry.strip_prefix("*.").unwrap_or(entry);
    if entry.is_empty() {
        None
    } else {
        Some(entry.to_owned())
    }
}

/// The proxy variable candidates for a URL scheme, in curl's precedence
/// order. `http_proxy` goes before `HTTP_PROXY` because CGI servers leak
/// request headers into the uppercase name, a classic injection vector.
fn proxy_var_names(scheme: Option<&str>) -> &'static [&'static str] {
    match scheme {
        Some("https") => &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"],
        Some("http") => &["http_proxy", "HTTP_PROXY", "ALL_PROXY", "all_proxy"],
        _ => &["ALL_PROXY", "all_proxy"],
    }
}

fn env_config(scheme: Option<&str>) -> ProxyConfig {
    match env_endpoint(scheme) {
        Some(endpoint) => ProxyConfig {
            endpoint: Some(endpoint),
            no_proxy: env_no_proxy(),
        },
        None => ProxyConfig::default(),
    }
}

fn env_endpoint(scheme: Option<&str>) -> Option<ProxyEndpoint> {
    for name in proxy_var_names(scheme) {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        match parse_proxy_url(&value) {
            Some(endpoint) => return Some(endpoint),
            // An unparsable value must not hide a later, valid candidate.
            None => log::warn!("ignoring unparsable proxy variable {name}: {value:?}"),
        }
    }
    None
}

fn env_no_proxy() -> Vec<String> {
    let Some(raw) = ["no_proxy", "NO_PROXY"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
    else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses a `*_PROXY` variable: `scheme://user:pass@host:port`, or a bare
/// `host:port` (curl tolerates both; without a scheme the proxy speaks
/// HTTP, as curl also assumes).
fn parse_proxy_url(raw: &str) -> Option<ProxyEndpoint> {
    let raw = raw.trim();
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (parse_proxy_scheme(scheme)?, rest),
        None => (ProxyScheme::Http, raw),
    };
    // Only the authority is ours; a trailing path (rare, but tolerated by
    // curl) ends at the first delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let mut endpoint = parse_host_port(hostport, scheme)?;
    if let Some(userinfo) = userinfo.filter(|userinfo| !userinfo.is_empty()) {
        let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        endpoint.username = Some(percent_decode(user));
        if !pass.is_empty() {
            endpoint.password = Some(percent_decode(pass));
        }
    }
    Some(endpoint)
}

fn parse_proxy_scheme(raw: &str) -> Option<ProxyScheme> {
    match raw.to_ascii_lowercase().as_str() {
        "http" => Some(ProxyScheme::Http),
        "https" => Some(ProxyScheme::Https),
        "socks4" => Some(ProxyScheme::Socks4),
        "socks5" => Some(ProxyScheme::Socks5),
        "socks5h" => Some(ProxyScheme::Socks5h),
        _ => None,
    }
}

/// `%XX`-decodes userinfo, which may carry reserved characters (`@`, `:`)
/// that would otherwise break the `user:pass@host` split. Invalid escapes are
/// kept verbatim, matching the lenient behaviour curl users expect.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(i + 1).copied().and_then(hex_digit),
                bytes.get(i + 2).copied().and_then(hex_digit),
            )
        {
            out.push(high * 16 + low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{ProxyConfig, parse_windows_bypass, parse_windows_proxy_string};
    use core::ffi::c_void;
    use core::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{GetLastError, GlobalFree};
    use windows_sys::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
        WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
        WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS,
        WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO, WinHttpCloseHandle,
        WinHttpGetIEProxyConfigForCurrentUser, WinHttpGetProxyForUrl, WinHttpOpen,
    };
    use windows_sys::core::PWSTR;

    /// WinHTTP wants a user agent for the session that evaluates PAC files.
    const AGENT: &str = concat!("lamina/", env!("CARGO_PKG_VERSION"));

    /// The user's Internet Options, copied out of the WinHTTP buffers they
    /// were allocated in.
    struct IeProxyConfig {
        auto_detect: bool,
        auto_config_url: Option<String>,
        proxy: Option<String>,
        bypass: Option<String>,
    }

    pub fn detect(url: &str, url_scheme: Option<&str>) -> Option<ProxyConfig> {
        let ie = unsafe { read_ie_proxy_config() }?;
        if (ie.auto_detect || ie.auto_config_url.is_some())
            && let Some(config) = autoproxy(url, url_scheme, &ie)
        {
            return Some(config);
        }
        // No autoproxy, or it failed: the static string is the next source.
        static_config(url_scheme, &ie)
    }

    /// Reads the per-user Internet Options. The strings WinHTTP fills in are
    /// `GlobalAlloc`ated and must leave through `GlobalFree`, so they are
    /// copied out and released before returning.
    unsafe fn read_ie_proxy_config() -> Option<IeProxyConfig> {
        let mut raw = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        // SAFETY: `raw` is an owned, zeroed struct WinHTTP only fills on success.
        if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut raw) } == 0 {
            log::debug!("WinHttpGetIEProxyConfigForCurrentUser failed: {}", unsafe {
                GetLastError()
            });
            return None;
        }
        let config = IeProxyConfig {
            auto_detect: raw.fAutoDetect != 0,
            auto_config_url: unsafe { pwstr_to_string(raw.lpszAutoConfigUrl) },
            proxy: unsafe { pwstr_to_string(raw.lpszProxy) },
            bypass: unsafe { pwstr_to_string(raw.lpszProxyBypass) },
        };
        // SAFETY: all three strings were allocated by WinHTTP and are no
        // longer referenced after the copies above.
        unsafe {
            free(raw.lpszAutoConfigUrl);
            free(raw.lpszProxy);
            free(raw.lpszProxyBypass);
        }
        Some(config)
    }

    /// Resolves the URL through the configured PAC URL and/or WPAD
    /// auto-detect. Returns `None` on failure so the caller can fall back to
    /// the static string.
    fn autoproxy(url: &str, url_scheme: Option<&str>, ie: &IeProxyConfig) -> Option<ProxyConfig> {
        // SAFETY: the session handle is null-checked and closed on every
        // path, and the buffers the call fills are copied and freed below.
        let agent = to_utf16(AGENT);
        let session = unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                // The session's own routing is irrelevant: the autoproxy
                // options drive the lookup, so the session starts direct.
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                null(),
                null(),
                0,
            )
        };
        if session.is_null() {
            log::debug!("WinHttpOpen failed: {}", unsafe { GetLastError() });
            return None;
        }
        let result = resolve_via_session(session, url, url_scheme, ie);
        // SAFETY: `session` came from WinHttpOpen and is closed exactly once.
        unsafe { WinHttpCloseHandle(session) };
        result
    }

    fn resolve_via_session(
        session: *mut c_void,
        url: &str,
        url_scheme: Option<&str>,
        ie: &IeProxyConfig,
    ) -> Option<ProxyConfig> {
        let url_wide = to_utf16(url);
        let autoconfig_wide = ie.auto_config_url.as_deref().map(to_utf16);
        let mut options = WINHTTP_AUTOPROXY_OPTIONS {
            dwFlags: 0,
            // DHCP first with DNS as the fallback: the order Windows itself
            // uses for WPAD.
            dwAutoDetectFlags: WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A,
            lpszAutoConfigUrl: autoconfig_wide
                .as_deref()
                .map_or(null(), |wide| wide.as_ptr()),
            lpvReserved: null_mut(),
            dwReserved: 0,
            // Default credentials for a PAC server that answers 401: without
            // this the download fails instead of negotiating silently.
            fAutoLogonIfChallenged: 1,
        };
        if ie.auto_detect {
            options.dwFlags |= WINHTTP_AUTOPROXY_AUTO_DETECT;
        }
        if autoconfig_wide.is_some() {
            options.dwFlags |= WINHTTP_AUTOPROXY_CONFIG_URL;
        }
        let mut info = WINHTTP_PROXY_INFO::default();
        // SAFETY: the wide buffers outlive the call, and `info` is owned
        // here. The options pointer is declared mutable by WinHTTP but only
        // read.
        if unsafe { WinHttpGetProxyForUrl(session, url_wide.as_ptr(), &mut options, &mut info) }
            == 0
        {
            log::debug!(
                "WinHttpGetProxyForUrl failed: {} (PAC/WPAD unreachable?)",
                unsafe { GetLastError() }
            );
            return None;
        }
        let proxy = unsafe { pwstr_to_string(info.lpszProxy) };
        let bypass = unsafe { pwstr_to_string(info.lpszProxyBypass) };
        // SAFETY: both strings were allocated by WinHTTP and are no longer
        // referenced after the copies above.
        unsafe {
            free(info.lpszProxy);
            free(info.lpszProxyBypass);
        }
        if info.dwAccessType != WINHTTP_ACCESS_TYPE_NAMED_PROXY {
            // The resolver answered "direct" for this URL. That answer is
            // final: falling through to the environment would insert a proxy
            // the user's PAC script explicitly declined.
            return Some(ProxyConfig::default());
        }
        // An autoproxy result is the answer for this one URL, so the bare and
        // `socks=` entries apply regardless of the URL's scheme.
        match proxy
            .as_deref()
            .and_then(|proxy| parse_windows_proxy_string(proxy, url_scheme, true))
        {
            Some(endpoint) => Some(ProxyConfig {
                endpoint: Some(endpoint),
                no_proxy: parse_windows_bypass(bypass.as_deref()),
            }),
            None => {
                log::warn!("WinHTTP resolved `{url}` to an unparsable proxy string: {proxy:?}");
                None
            }
        }
    }

    fn static_config(url_scheme: Option<&str>, ie: &IeProxyConfig) -> Option<ProxyConfig> {
        let proxy = ie.proxy.as_deref()?.trim();
        // A proxy configured for other schemes (or not at all) is no answer
        // for this URL: the environment gets the next word.
        let endpoint = parse_windows_proxy_string(proxy, url_scheme, false)?;
        Some(ProxyConfig {
            endpoint: Some(endpoint),
            no_proxy: parse_windows_bypass(ie.bypass.as_deref()),
        })
    }

    fn to_utf16(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(core::iter::once(0)).collect()
    }

    /// SAFETY: `p` must be a NUL-terminated UTF-16 buffer that stays valid.
    unsafe fn pwstr_to_string(p: PWSTR) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees termination and validity.
        let len = unsafe {
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            len
        };
        match String::from_utf16(unsafe { core::slice::from_raw_parts(p, len) }) {
            Ok(string) => Some(string),
            Err(_) => {
                log::warn!("WinHTTP returned a string that is not valid UTF-16");
                None
            }
        }
    }

    /// SAFETY: `p` must have come from a WinHTTP call that allocated it with
    /// `GlobalAlloc`. `GlobalFree` is the documented release for every buffer
    /// those calls hand out.
    unsafe fn free(p: PWSTR) {
        if !p.is_null() {
            // SAFETY: the pointer is released exactly once, here.
            unsafe { GlobalFree(p.cast()) };
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{ProxyConfig, ProxyEndpoint, ProxyScheme, default_port, normalize_bypass_entry};
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;

    pub fn detect(url_scheme: Option<&str>) -> Option<ProxyConfig> {
        let Some(store) = SCDynamicStoreBuilder::new("lamina-proxy-detect").build() else {
            log::debug!("could not open a dynamic store session");
            return None;
        };
        let proxies = match store.get_proxies() {
            Some(proxies) => proxies,
            None => {
                log::debug!("the dynamic store reports no proxy settings");
                return None;
            }
        };
        if flag(&proxies, "ProxyAutoConfigEnable") {
            // PAC evaluation is out of scope here; staying silent would
            // quietly use a direct connection the user's PAC likely forbids,
            // so the environment variables get the last word instead.
            log::info!(
                "a PAC script is configured on macOS but not evaluated; falling back to the environment"
            );
            return None;
        }
        // The per-protocol keys name the traffic, not the wire protocol of
        // the proxy, which speaks HTTP either way.
        let per_protocol = match url_scheme {
            Some("http") => scheme_entry(&proxies, "HTTP", ProxyScheme::Http),
            Some("https") => scheme_entry(&proxies, "HTTPS", ProxyScheme::Http),
            _ => None,
        };
        // The SOCKS entry has no per-protocol scope, so it answers for every
        // scheme the per-protocol keys did not. macOS SOCKS speaks SOCKS5.
        let endpoint =
            per_protocol.or_else(|| scheme_entry(&proxies, "SOCKS", ProxyScheme::Socks5))?;
        if flag(&proxies, "ExcludeSimpleHostnames") {
            log::debug!("`ExcludeSimpleHostnames` has no NO_PROXY equivalent and is dropped");
        }
        Some(ProxyConfig {
            endpoint: Some(endpoint),
            no_proxy: exceptions(&proxies),
        })
    }

    fn scheme_entry(
        proxies: &CFDictionary<CFString, CFType>,
        prefix: &str,
        scheme: ProxyScheme,
    ) -> Option<ProxyEndpoint> {
        if !flag(proxies, &format!("{prefix}Enable")) {
            return None;
        }
        let host = match string(proxies, &format!("{prefix}Proxy")) {
            Some(host) if !host.is_empty() => host,
            _ => {
                log::warn!("macOS proxy `{prefix}Proxy` is enabled without a host; ignoring it");
                return None;
            }
        };
        let port = number(proxies, &format!("{prefix}Port"))
            .and_then(|port| u16::try_from(port).ok())
            .unwrap_or_else(|| default_port(scheme));
        Some(ProxyEndpoint {
            scheme,
            host,
            port,
            username: None,
            password: None,
        })
    }

    fn exceptions(proxies: &CFDictionary<CFString, CFType>) -> Vec<String> {
        let Some(array) = value(proxies, "ExceptionsList").and_then(|entry| {
            // Only the untyped array is downcastable; the element type is
            // recovered per entry below.
            entry.downcast_into::<CFArray>()
        }) else {
            return Vec::new();
        };
        // Downcast per element: the list is plain strings in practice, but a
        // type check per entry is cheap insurance against a corrupt store.
        array
            .iter()
            .filter_map(|entry| {
                // SAFETY: the pointer is an element of an array we own; the
                // retain keeps it alive while the downcast type-checks it,
                // and either outcome releases it again on drop.
                let entry = unsafe { CFType::wrap_under_get_rule(*entry) };
                entry
                    .downcast_into::<CFString>()
                    .map(|string| string.to_string())
            })
            .filter_map(|entry| normalize_bypass_entry(&entry))
            .collect()
    }

    fn value(proxies: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFType> {
        let key = CFString::new(key);
        // find() only borrows, but downcasting consumes, so retain once.
        proxies.find(&key).map(|found| (*found).clone())
    }

    fn flag(proxies: &CFDictionary<CFString, CFType>, key: &str) -> bool {
        let Some(entry) = value(proxies, key) else {
            return false;
        };
        // Stored as a number (0/1); some tools write a boolean instead.
        if let Some(number) = entry.clone().downcast_into::<CFNumber>() {
            return number.to_i32().map(|number| number != 0).unwrap_or(false);
        }
        match entry.downcast_into::<CFBoolean>() {
            Some(boolean) => boolean == CFBoolean::true_value(),
            None => {
                log::warn!("proxy key {key:?} holds an unreadable value");
                false
            }
        }
    }

    fn string(proxies: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
        value(proxies, key)?
            .downcast_into::<CFString>()
            .map(|s| s.to_string())
    }

    fn number(proxies: &CFDictionary<CFString, CFType>, key: &str) -> Option<i32> {
        value(proxies, key)?
            .downcast_into::<CFNumber>()
            .and_then(|number| number.to_i32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(host: &str, port: u16) -> Option<ProxyEndpoint> {
        Some(ProxyEndpoint {
            scheme: ProxyScheme::Http,
            host: host.to_owned(),
            port,
            username: None,
            password: None,
        })
    }

    #[test]
    fn scheme_is_lowercased_and_validated() {
        assert_eq!(
            url_scheme("HTTPS://Example.com/x"),
            Some("https".to_owned())
        );
        assert_eq!(
            url_scheme("git+ssh://host/repo"),
            Some("git+ssh".to_owned())
        );
        assert_eq!(url_scheme("host.example.com"), None);
        assert_eq!(url_scheme("scp-style:path"), None);
        assert_eq!(url_scheme("://host"), None);
    }

    #[test]
    fn windows_static_string_routes_by_url_scheme() {
        assert_eq!(
            parse_windows_proxy_string("proxy:8080", Some("http"), false),
            http("proxy", 8080)
        );
        // The bare entry is an HTTP proxy whatever the URL scheme, so its
        // default port stays 80.
        assert_eq!(
            parse_windows_proxy_string("proxy", Some("https"), false),
            http("proxy", 80)
        );
        // A per-scheme entry beats the bare default.
        assert_eq!(
            parse_windows_proxy_string("proxy:9;https=b:2", Some("https"), false),
            http("b", 2)
        );
        // The bare entry is not an answer for other URL schemes.
        assert_eq!(
            parse_windows_proxy_string("proxy:9", Some("git"), false),
            None
        );
    }

    #[test]
    fn windows_socks_entry_maps_to_socks4() {
        let socks = || {
            Some(ProxyEndpoint {
                scheme: ProxyScheme::Socks4,
                host: "s".to_owned(),
                port: 3,
                username: None,
                password: None,
            })
        };
        assert_eq!(
            parse_windows_proxy_string("socks=s:3", Some("http"), false),
            socks()
        );
        // In a per-URL answer the socks entry applies to any scheme.
        assert_eq!(
            parse_windows_proxy_string("socks=s:3", Some("git"), true),
            socks()
        );
        // In a static string it does not.
        assert_eq!(
            parse_windows_proxy_string("socks=s:3", Some("git"), false),
            None
        );
    }

    #[test]
    fn windows_string_garbage_is_rejected() {
        assert_eq!(parse_windows_proxy_string("", Some("http"), false), None);
        assert_eq!(
            parse_windows_proxy_string("  ; ;", Some("https"), false),
            None
        );
        assert_eq!(
            parse_windows_proxy_string("http=a:x", Some("http"), false),
            None
        );
        assert_eq!(parse_windows_proxy_string(":::", Some("http"), false), None);
    }

    #[test]
    fn windows_bypass_list_normalises_to_no_proxy() {
        assert_eq!(
            parse_windows_bypass(Some("<local>; *.corp ;10.0.0.0/8;;")),
            vec!["corp".to_owned(), "10.0.0.0/8".to_owned()]
        );
        assert_eq!(parse_windows_bypass(None), Vec::<String>::new());
    }

    #[test]
    fn env_value_accepts_url_and_bare_forms() {
        assert_eq!(parse_proxy_url("http://proxy:8080"), http("proxy", 8080));
        assert_eq!(
            parse_proxy_url(" proxy.example.com:3128 "),
            http("proxy.example.com", 3128)
        );
        // Without a scheme the proxy speaks HTTP and uses port 80.
        assert_eq!(
            parse_proxy_url("proxy.example.com"),
            http("proxy.example.com", 80)
        );
        // A trailing path is tolerated and dropped.
        assert_eq!(parse_proxy_url("http://p:1/path?q#f"), http("p", 1));
    }

    #[test]
    fn env_value_covers_every_proxy_scheme() {
        let endpoint = |scheme| {
            parse_proxy_url(match scheme {
                ProxyScheme::Https => "https://p",
                ProxyScheme::Socks4 => "socks4://p",
                ProxyScheme::Socks5 => "socks5://p",
                ProxyScheme::Socks5h => "socks5h://p",
                ProxyScheme::Http => unreachable!(),
            })
            .unwrap()
        };
        assert_eq!(endpoint(ProxyScheme::Https).port, 443);
        assert_eq!(endpoint(ProxyScheme::Socks4).port, 1080);
        assert_eq!(endpoint(ProxyScheme::Socks4).scheme, ProxyScheme::Socks4);
        assert_eq!(endpoint(ProxyScheme::Socks5).scheme, ProxyScheme::Socks5);
        assert_eq!(endpoint(ProxyScheme::Socks5h).scheme, ProxyScheme::Socks5h);
    }

    #[test]
    fn env_value_percent_decodes_credentials() {
        let endpoint = parse_proxy_url("socks5://u%40ser:p%3Ass@h:1080").unwrap();
        assert_eq!(endpoint.scheme, ProxyScheme::Socks5);
        assert_eq!(endpoint.username.as_deref(), Some("u@ser"));
        assert_eq!(endpoint.password.as_deref(), Some("p:ss"));
        // No password, and an empty userinfo, both stay out of the endpoint.
        let endpoint = parse_proxy_url("http://user@h:2").unwrap();
        assert_eq!(endpoint.username.as_deref(), Some("user"));
        assert_eq!(endpoint.password, None);
        let endpoint = parse_proxy_url("http://h:2").unwrap();
        assert_eq!(endpoint.username, None);
    }

    #[test]
    fn env_value_handles_ipv6_and_rejects_garbage() {
        let endpoint = parse_proxy_url("http://[2001:db8::1]:8123").unwrap();
        assert_eq!(endpoint.host, "2001:db8::1");
        assert_eq!(endpoint.port, 8123);
        assert_eq!(parse_proxy_url("http://[2001:db8::1]").unwrap().port, 80);
        assert_eq!(parse_proxy_url("ftp://p:1"), None);
        assert_eq!(parse_proxy_url(""), None);
        assert_eq!(parse_proxy_url("not a proxy!"), None);
    }

    #[test]
    fn proxy_variables_follow_curl_precedence() {
        assert_eq!(
            proxy_var_names(Some("https")),
            &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        );
        // Lowercase first for HTTP: CGI leaks request headers into
        // `HTTP_PROXY`, so the uppercase name cannot be trusted there.
        assert_eq!(
            proxy_var_names(Some("http")),
            &["http_proxy", "HTTP_PROXY", "ALL_PROXY", "all_proxy"]
        );
        assert_eq!(proxy_var_names(Some("git")), &["ALL_PROXY", "all_proxy"]);
        assert_eq!(proxy_var_names(None), &["ALL_PROXY", "all_proxy"]);
    }

    #[test]
    fn percent_decode_keeps_invalid_escapes_verbatim() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("%e6%97%a5"), "日");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn detect_never_panics_on_any_input() {
        for url in [
            "",
            "https://example.com/repo.git",
            "git://example.com/repo",
            "not a url at all",
            "http://[::1]:9999/x",
        ] {
            let _ = detect(url);
        }
    }
}
