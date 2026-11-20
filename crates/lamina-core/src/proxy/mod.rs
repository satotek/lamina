//! App-wide network proxy model, `NO_PROXY` matching, and child-process
//! environment injection.
//!
//! OS proxy detection lives in [`system`]; this module defines the shared
//! [`ProxyScheme`], [`ProxyEndpoint`], and [`ProxyConfig`] contract and maps
//! them onto URL strings, `NO_PROXY` checks, and [`std::process::Command`]
//! environment variables.

pub mod system;

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::Command;

/// Environment variables that curl, git, and `gh` read for proxy routing.
pub const PROXY_ENV_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyScheme {
    #[default]
    Http,
    Https,
    Socks4,
    Socks5,
    Socks5h,
}

impl ProxyScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks4 => "socks4",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyEndpoint {
    pub scheme: ProxyScheme,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyEndpoint {
    /// Formats `scheme://[user[:pass]@]host:port` with percent-encoded userinfo.
    pub fn url(&self) -> String {
        let mut out = String::new();
        out.push_str(self.scheme.as_str());
        out.push_str("://");

        let user = self.username.as_deref().unwrap_or("");
        let pass = self.password.as_deref().unwrap_or("");
        if !user.is_empty() || !pass.is_empty() {
            encode_userinfo(user, &mut out);
            if !pass.is_empty() {
                out.push(':');
                encode_userinfo(pass, &mut out);
            }
            out.push('@');
        }

        let host = self.host.trim();
        if host.contains(':') && !host.starts_with('[') && !host.ends_with(']') {
            let _ = write!(out, "[{host}]");
        } else {
            out.push_str(host);
        }
        let _ = write!(out, ":{}", self.port);
        out
    }
}

/// What a connection to some URL should go through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub endpoint: Option<ProxyEndpoint>,
    /// Hosts/domains/CIDRs that bypass the proxy, NO_PROXY syntax.
    pub no_proxy: Vec<String>,
}

impl ProxyConfig {
    /// Produces `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, and their
    /// lowercase forms when [`Self::endpoint`] is `Some`; returns an empty list
    /// when there is no endpoint.
    pub fn env_vars(&self) -> Vec<(&'static str, String)> {
        let Some(endpoint) = &self.endpoint else {
            return Vec::new();
        };
        let url = endpoint.url();
        let mut vars = vec![
            ("HTTP_PROXY", url.clone()),
            ("http_proxy", url.clone()),
            ("HTTPS_PROXY", url.clone()),
            ("https_proxy", url.clone()),
            ("ALL_PROXY", url.clone()),
            ("all_proxy", url),
        ];
        let no_proxy: Vec<&str> = self
            .no_proxy
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .collect();
        if !no_proxy.is_empty() {
            let joined = no_proxy.join(",");
            vars.push(("NO_PROXY", joined.clone()));
            vars.push(("no_proxy", joined));
        }
        vars
    }
}

/// How a child process's proxy environment should be configured.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProxyPolicy {
    /// Leave inherited environment variables untouched.
    #[default]
    Inherit,
    /// Clear all proxy environment variables so inherited env proxies are not used.
    Clear,
    /// Clear inherited proxy environment variables and inject this config's vars.
    Set(ProxyConfig),
}

impl ProxyPolicy {
    pub fn apply(&self, command: &mut Command) {
        match self {
            Self::Inherit => {}
            Self::Clear => {
                for key in PROXY_ENV_VARS {
                    command.env_remove(key);
                }
            }
            Self::Set(config) => {
                for key in PROXY_ENV_VARS {
                    command.env_remove(key);
                }
                for (key, value) in config.env_vars() {
                    command.env(key, value);
                }
            }
        }
    }
}

/// Splits a comma-separated `NO_PROXY` string into trimmed, non-empty rules.
pub fn parse_no_proxy(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Returns `true` when `url` (or a bare `host[:port]`) matches any `NO_PROXY`
/// rule (wildcards, hostnames, leading-dot domain suffixes, IPs, CIDRs, and
/// optional `:port`).
pub fn should_bypass(url: &str, no_proxy: &[String]) -> bool {
    let Some((target_host, target_port)) = extract_host_port(url) else {
        return false;
    };
    let normalized_target = target_host.trim_end_matches('.');
    let target_ip = normalized_target.parse::<IpAddr>().ok();

    for rule in no_proxy {
        let rule = rule.trim();
        if rule.is_empty() {
            continue;
        }
        if rule == "*" {
            return true;
        }
        let (rule_host, rule_port) = split_rule_host_port(rule);
        if let Some(port) = rule_port
            && target_port != Some(port)
        {
            continue;
        }
        let rule_host = rule_host.trim();
        if rule_host.is_empty() {
            continue;
        }
        if rule_host == "*" {
            return true;
        }
        if let Some((network, prefix)) = parse_cidr(rule_host) {
            if let Some(ip) = target_ip
                && ip_in_cidr(ip, network, prefix)
            {
                return true;
            }
            continue;
        }
        if let (Some(target_ip), Ok(rule_ip)) = (target_ip, rule_host.parse::<IpAddr>()) {
            if target_ip == rule_ip {
                return true;
            }
            continue;
        }
        let suffix = rule_host
            .strip_prefix('.')
            .unwrap_or(rule_host)
            .trim_end_matches('.');
        if suffix.is_empty() {
            continue;
        }
        if normalized_target.eq_ignore_ascii_case(suffix)
            || has_domain_suffix(normalized_target, suffix)
        {
            return true;
        }
    }
    false
}

fn has_domain_suffix(host: &str, suffix: &str) -> bool {
    let Some(prefix_len) = host.len().checked_sub(suffix.len()) else {
        return false;
    };
    if prefix_len == 0 {
        return false;
    }
    host.as_bytes()[prefix_len - 1] == b'.' && host[prefix_len..].eq_ignore_ascii_case(suffix)
}

/// Encodes RFC 3986 userinfo, escaping every byte outside the unreserved set
/// (`A-Za-z0-9-_.~`) so `@`, `:`, `/`, `%`, and spaces cannot corrupt URL parsing.
fn encode_userinfo(value: &str, out: &mut String) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
}

fn extract_host_port(url: &str) -> Option<(&str, Option<u16>)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (scheme_port, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (default_port_for_scheme(scheme), rest),
        None => (None, trimmed),
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, right)| right);
    let (host, explicit_port) = split_host_port(host_port)?;
    if host.is_empty() {
        return None;
    }
    Some((host, explicit_port.or(scheme_port)))
}

fn default_port_for_scheme(scheme: &str) -> Option<u16> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(80),
        "https" => Some(443),
        "ssh" => Some(22),
        "git" => Some(9418),
        _ => None,
    }
}

fn split_host_port(host_port: &str) -> Option<(&str, Option<u16>)> {
    if let Some(after_open) = host_port.strip_prefix('[') {
        let (host, rest) = after_open.split_once(']')?;
        let port = match rest.strip_prefix(':') {
            Some(p) if !p.is_empty() => p.parse::<u16>().ok(),
            _ => None,
        };
        return Some((host, port));
    }
    // Bare IPv6 address has at least two colons and no port.
    if host_port.bytes().filter(|b| *b == b':').count() > 1 {
        return Some((host_port, None));
    }
    match host_port.rsplit_once(':') {
        Some((host, port_str)) if !port_str.is_empty() => {
            if let Ok(port) = port_str.parse::<u16>() {
                Some((host, Some(port)))
            } else {
                Some((host_port, None))
            }
        }
        _ => Some((host_port, None)),
    }
}

fn split_rule_host_port(rule: &str) -> (&str, Option<u16>) {
    if let Some(after_open) = rule.strip_prefix('[')
        && let Some((host, rest)) = after_open.split_once(']')
    {
        let port = rest.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
        return (host, port);
    }
    if rule.contains('/') || rule.bytes().filter(|b| *b == b':').count() > 1 {
        return (rule, None);
    }
    match rule.rsplit_once(':') {
        Some((host, port_str)) => match port_str.parse::<u16>() {
            Ok(port) => (host, Some(port)),
            Err(_) => (rule, None),
        },
        None => (rule, None),
    }
}

fn parse_cidr(rule: &str) -> Option<(IpAddr, u8)> {
    let (addr_str, prefix_str) = rule.split_once('/')?;
    let addr: IpAddr = addr_str.parse().ok()?;
    let prefix: u8 = prefix_str.parse().ok()?;
    let max_prefix = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    (prefix <= max_prefix).then_some((addr, prefix))
}

fn ip_in_cidr(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => ipv4_in_cidr(ip, net, prefix),
        (IpAddr::V6(ip), IpAddr::V6(net)) => ipv6_in_cidr(ip, net, prefix),
        _ => false,
    }
}

fn ipv4_in_cidr(ip: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0u32
    } else {
        !0u32 << (32 - prefix)
    };
    (u32::from(ip) & mask) == (u32::from(net) & mask)
}

fn ipv6_in_cidr(ip: Ipv6Addr, net: Ipv6Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0u128
    } else {
        !0u128 << (128 - prefix)
    };
    (u128::from(ip) & mask) == (u128::from(net) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_url_percent_encodes_credentials_and_brackets_ipv6() {
        let endpoint = ProxyEndpoint {
            scheme: ProxyScheme::Socks5h,
            host: "proxy.example.com".into(),
            port: 1080,
            username: Some("alice@corp".into()),
            password: Some("p:ss/word%1 !".into()),
        };
        assert_eq!(
            endpoint.url(),
            "socks5h://alice%40corp:p%3Ass%2Fword%251%20%21@proxy.example.com:1080"
        );

        let ipv6 = ProxyEndpoint {
            scheme: ProxyScheme::Http,
            host: "::1".into(),
            port: 8080,
            username: None,
            password: None,
        };
        assert_eq!(ipv6.url(), "http://[::1]:8080");

        let user_only = ProxyEndpoint {
            scheme: ProxyScheme::Https,
            host: "proxy.local".into(),
            port: 3128,
            username: Some("bob".into()),
            password: None,
        };
        assert_eq!(user_only.url(), "https://bob@proxy.local:3128");
    }

    #[test]
    fn config_env_vars_are_empty_without_endpoint_and_complete_with_endpoint() {
        let empty = ProxyConfig {
            endpoint: None,
            no_proxy: vec!["localhost".into()],
        };
        assert!(empty.env_vars().is_empty());

        let config = ProxyConfig {
            endpoint: Some(ProxyEndpoint {
                scheme: ProxyScheme::Http,
                host: "proxy.local".into(),
                port: 8080,
                username: Some("u:1".into()),
                password: Some("p@2".into()),
            }),
            no_proxy: vec!["localhost".into(), ".internal.example".into()],
        };
        let vars = config.env_vars();
        let expected_url = "http://u%3A1:p%402@proxy.local:8080";
        assert_eq!(
            vars,
            vec![
                ("HTTP_PROXY", expected_url.to_owned()),
                ("http_proxy", expected_url.to_owned()),
                ("HTTPS_PROXY", expected_url.to_owned()),
                ("https_proxy", expected_url.to_owned()),
                ("ALL_PROXY", expected_url.to_owned()),
                ("all_proxy", expected_url.to_owned()),
                ("NO_PROXY", "localhost,.internal.example".to_owned()),
                ("no_proxy", "localhost,.internal.example".to_owned()),
            ]
        );
    }

    #[test]
    fn no_proxy_parsing_and_matching_cover_hosts_domains_ports_and_cidrs() {
        let rules =
            parse_no_proxy(" localhost , .internal.corp, 10.0.0.0/8, fd00::/8, [::1]:8443 ");
        assert_eq!(
            rules,
            vec![
                "localhost",
                ".internal.corp",
                "10.0.0.0/8",
                "fd00::/8",
                "[::1]:8443",
            ]
        );

        assert!(should_bypass("http://localhost/repo", &rules));
        assert!(should_bypass("https://git.internal.corp/org/repo", &rules));
        assert!(should_bypass("https://internal.corp/org/repo", &rules));
        assert!(!should_bypass("https://notinternal.corp/org/repo", &rules));
        assert!(!should_bypass("https://github.com/satotek/lamina", &rules));

        assert!(should_bypass("http://10.42.0.1:8080/api", &rules));
        assert!(!should_bypass("http://192.168.1.1/api", &rules));
        assert!(should_bypass("https://[fd00::1]/api", &rules));
        assert!(should_bypass("https://[::1]:8443/api", &rules));
        assert!(!should_bypass("https://[::1]:9000/api", &rules));

        assert!(should_bypass(
            "https://github.com/satotek/lamina",
            &["*".to_owned()]
        ));
    }
}
