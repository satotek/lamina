//! App-wide proxy resolution, credential-store password persistence, and
//! `ureq` agent configuration.
//!
//! Manual proxy passwords are kept in the OS credential store via [`keyring`]
//! instead of `settings.json`. Resolution can invoke OS proxy/PAC detection in
//! [`lamina_core::proxy::system`], which may block on network I/O, so callers
//! should run [`resolve`] / [`resolve_policy`] on a background thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lamina_core::proxy::{
    ProxyConfig, ProxyEndpoint, ProxyPolicy, ProxyScheme, parse_no_proxy, should_bypass, system,
};
use ureq::{Proxy, ProxyProtocol};

use crate::settings::{ProxyMode, ProxySettings};

const KEYRING_SERVICE: &str = "lamina";
const KEYRING_ACCOUNT: &str = "proxy";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

static KEYRING_LOCK: Mutex<()> = Mutex::new(());
static PASSWORD_REVISION: AtomicU64 = AtomicU64::new(0);

/// Monotonic counter incremented whenever [`store_password`] writes or clears
/// the OS credential store entry, so open repositories can re-resolve proxy
/// policies even when `settings.json` did not change.
pub fn password_revision() -> u64 {
    PASSWORD_REVISION.load(Ordering::Relaxed)
}

/// Loads the manual proxy password from the OS credential store.
///
/// Missing entries return `None`. An unavailable or unreadable credential store
/// is logged and treated as no password so network requests still proceed
/// without failing startup.
pub fn load_password() -> Option<String> {
    let _guard = KEYRING_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
        Ok(entry) => entry,
        Err(error) => {
            log::warn!("credential store unavailable for reading proxy password: {error}");
            return None;
        }
    };
    match entry.get_password() {
        Ok(password) => (!password.is_empty()).then_some(password),
        Err(keyring::Error::NoEntry) => take_legacy_password(&entry),
        Err(error) => {
            log::warn!("failed to read proxy password from credential store: {error}");
            None
        }
    }
}

/// Moves a password saved under the former app name to the current entry.
/// Called with the lock held, from the read that found no current entry.
fn take_legacy_password(entry: &keyring::Entry) -> Option<String> {
    let legacy = keyring::Entry::new(crate::legacy::KEYRING_SERVICE, KEYRING_ACCOUNT).ok()?;
    let password = legacy.get_password().ok().filter(|p| !p.is_empty())?;
    match entry.set_password(&password) {
        Ok(()) => {
            if let Err(error) = legacy.delete_credential() {
                log::warn!("could not remove the former proxy password entry: {error}");
            }
        }
        // Keep the old entry so the next read tries again.
        Err(error) => log::warn!("could not move the proxy password: {error}"),
    }
    Some(password)
}

/// Saves or clears the manual proxy password in the OS credential store.
///
/// An empty string removes any stored password. Returns an error message only
/// when the store fails unexpectedly (missing entries during clear are ignored).
pub fn store_password(password: &str) -> Result<(), String> {
    let _guard = KEYRING_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
        Ok(entry) => entry,
        Err(error) => {
            log::warn!("credential store unavailable for saving proxy password: {error}");
            return Err(error.to_string());
        }
    };
    if password.is_empty() {
        return match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                PASSWORD_REVISION.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(error) => {
                log::warn!("failed to delete proxy password from credential store: {error}");
                Err(error.to_string())
            }
        };
    }
    entry
        .set_password(password)
        .map(|()| {
            PASSWORD_REVISION.fetch_add(1, Ordering::Relaxed);
        })
        .map_err(|error| {
            log::warn!("failed to store proxy password in credential store: {error}");
            error.to_string()
        })
}

/// Resolves the [`ProxyConfig`] to use for `url` under `settings`.
///
/// Runs OS detection for [`ProxyMode::System`] and reads the OS credential store
/// for [`ProxyMode::Manual`], so callers should invoke this off the UI thread.
pub fn resolve(url: &str, settings: &ProxySettings) -> ProxyConfig {
    resolve_with(url, settings, load_password)
}

/// Pure testable core of [`resolve`], accepting an injected password loader so
/// unit tests never touch the host keychain.
pub fn resolve_with<F>(url: &str, settings: &ProxySettings, load_password: F) -> ProxyConfig
where
    F: FnOnce() -> Option<String>,
{
    match settings.mode {
        ProxyMode::None => ProxyConfig::default(),
        ProxyMode::System => system::detect(url),
        ProxyMode::Manual => {
            let no_proxy = parse_no_proxy(&settings.no_proxy);
            let host = settings.host.trim();
            if host.is_empty() || settings.port == 0 {
                return ProxyConfig {
                    endpoint: None,
                    no_proxy,
                };
            }
            let username = trimmed_non_empty(&settings.username);
            let password = load_password().and_then(|value| (!value.is_empty()).then_some(value));
            ProxyConfig {
                endpoint: Some(ProxyEndpoint {
                    scheme: settings.scheme,
                    host: host.to_owned(),
                    port: settings.port,
                    username,
                    password,
                }),
                no_proxy,
            }
        }
    }
}

/// Resolves the child-process [`ProxyPolicy`] for `git` / `gh` targeting `url`.
pub fn resolve_policy(url: &str, settings: &ProxySettings) -> ProxyPolicy {
    resolve_policy_with(url, settings, load_password)
}

pub fn resolve_policy_with<F>(url: &str, settings: &ProxySettings, load_password: F) -> ProxyPolicy
where
    F: FnOnce() -> Option<String>,
{
    match settings.mode {
        ProxyMode::None => ProxyPolicy::Clear,
        ProxyMode::Manual => {
            let config = resolve_with(url, settings, load_password);
            if config.endpoint.is_some() {
                ProxyPolicy::Set(config)
            } else {
                // Manual mode with an incomplete endpoint should not silently
                // inherit ambient environment variables.
                ProxyPolicy::Clear
            }
        }
        ProxyMode::System => {
            let config = resolve_with(url, settings, load_password);
            if config.endpoint.is_some() {
                ProxyPolicy::Set(config)
            } else {
                ProxyPolicy::Inherit
            }
        }
    }
}

/// Converts a resolved [`ProxyConfig`] for `target_url` into a `ureq::Proxy`.
///
/// Returns `None` when there is no endpoint or when `target_url` matches
/// `config.no_proxy`. Also attaches `config.no_proxy` rules to the builder so
/// redirects follow bypass rules as well.
pub fn ureq_proxy(target_url: &str, config: &ProxyConfig) -> Option<Proxy> {
    let endpoint = config.endpoint.as_ref()?;
    if should_bypass(target_url, &config.no_proxy) {
        return None;
    }

    let protocol = match endpoint.scheme {
        ProxyScheme::Http => ProxyProtocol::Http,
        ProxyScheme::Https => ProxyProtocol::Https,
        ProxyScheme::Socks4 => ProxyProtocol::Socks4,
        ProxyScheme::Socks5 => ProxyProtocol::Socks5,
        ProxyScheme::Socks5h => ProxyProtocol::Socks5h,
    };
    let host = endpoint.host.trim();
    let formatted_host = if host.contains(':') && !host.starts_with('[') && !host.ends_with(']') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };

    let mut builder = Proxy::builder(protocol)
        .host(&formatted_host)
        .port(endpoint.port);
    if let Some(username) = endpoint.username.as_deref() {
        builder = builder.username(username);
    } else if endpoint.password.is_some() {
        // ureq's ProxyBuilder only writes the password when `username` is `Some`.
        builder = builder.username("");
    }
    if let Some(password) = endpoint.password.as_deref() {
        builder = builder.password(password);
    }
    for entry in &config.no_proxy {
        builder = builder.no_proxy(entry);
    }

    match builder.build() {
        Ok(proxy) => Some(proxy),
        Err(error) => {
            log::warn!("ignoring invalid proxy configuration for {formatted_host}: {error}");
            None
        }
    }
}

/// Builds a `ureq::Agent` configured with platform TLS verification and the
/// resolved proxy for `target_url`.
///
/// Runs proxy resolution synchronously, so callers must invoke this from a
/// background thread.
pub fn http_agent(target_url: &str, settings: &ProxySettings, timeout: Duration) -> ureq::Agent {
    let config = resolve(target_url, settings);
    http_agent_with_config(target_url, settings.mode, &config, timeout)
}

fn http_agent_with_config(
    target_url: &str,
    mode: ProxyMode,
    config: &ProxyConfig,
    timeout: Duration,
) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let mut builder = ureq::Agent::config_builder()
        .tls_config(tls)
        .user_agent(format!("Lamina/{APP_VERSION}"))
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout));

    match mode {
        ProxyMode::None => {
            builder = builder.proxy(None);
        }
        ProxyMode::Manual => {
            builder = builder.proxy(ureq_proxy(target_url, config));
        }
        ProxyMode::System => {
            if config.endpoint.is_some() {
                builder = builder.proxy(ureq_proxy(target_url, config));
            } else if should_bypass(target_url, &config.no_proxy) {
                builder = builder.proxy(None);
            }
            // Otherwise leave ureq's default (`Proxy::try_from_env()`) active
            // when OS detection returns no explicit endpoint.
        }
    }

    builder.build().into()
}

/// Verifies connectivity to `target_url` (e.g. `https://github.com`) through
/// the resolved proxy settings on a blocking background thread.
pub fn test_connection(target_url: &str, settings: &ProxySettings) -> Result<(), String> {
    let agent = http_agent(target_url, settings, Duration::from_secs(10));
    agent
        .head(target_url)
        .call()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn trimmed_non_empty(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual_settings() -> ProxySettings {
        ProxySettings {
            mode: ProxyMode::Manual,
            scheme: ProxyScheme::Socks5h,
            host: " 127.0.0.1 ".into(),
            port: 1080,
            username: " alice ".into(),
            no_proxy: "localhost, 10.0.0.0/8 , .internal.corp".into(),
        }
    }

    #[test]
    fn none_mode_resolves_to_empty_and_clears_child_env() {
        let settings = ProxySettings {
            mode: ProxyMode::None,
            ..manual_settings()
        };
        let config = resolve_with("https://github.com", &settings, || Some("secret".into()));
        assert_eq!(config, ProxyConfig::default());
        assert_eq!(
            resolve_policy_with("https://github.com", &settings, || Some("secret".into())),
            ProxyPolicy::Clear
        );
    }

    #[test]
    fn manual_mode_builds_endpoint_with_keyring_password_and_no_proxy() {
        let settings = manual_settings();
        let config = resolve_with("https://github.com", &settings, || Some("p@ss:word".into()));
        assert_eq!(
            config,
            ProxyConfig {
                endpoint: Some(ProxyEndpoint {
                    scheme: ProxyScheme::Socks5h,
                    host: "127.0.0.1".into(),
                    port: 1080,
                    username: Some("alice".into()),
                    password: Some("p@ss:word".into()),
                }),
                no_proxy: vec![
                    "localhost".into(),
                    "10.0.0.0/8".into(),
                    ".internal.corp".into(),
                ],
            }
        );
        assert_eq!(
            resolve_policy_with("https://github.com", &settings, || None),
            ProxyPolicy::Set(ProxyConfig {
                endpoint: Some(ProxyEndpoint {
                    scheme: ProxyScheme::Socks5h,
                    host: "127.0.0.1".into(),
                    port: 1080,
                    username: Some("alice".into()),
                    password: None,
                }),
                no_proxy: vec![
                    "localhost".into(),
                    "10.0.0.0/8".into(),
                    ".internal.corp".into(),
                ],
            })
        );
    }

    #[test]
    fn manual_mode_without_host_or_zero_port_clears_policy() {
        let empty_host = ProxySettings {
            mode: ProxyMode::Manual,
            host: "   ".into(),
            port: 8080,
            no_proxy: "localhost".into(),
            ..ProxySettings::default()
        };
        let config = resolve_with("https://github.com", &empty_host, || Some("secret".into()));
        assert_eq!(config.endpoint, None);
        assert_eq!(config.no_proxy, vec!["localhost"]);
        assert_eq!(
            resolve_policy_with("https://github.com", &empty_host, || None),
            ProxyPolicy::Clear
        );

        let zero_port = ProxySettings {
            mode: ProxyMode::Manual,
            host: "proxy.local".into(),
            port: 0,
            ..ProxySettings::default()
        };
        assert_eq!(
            resolve_with("https://github.com", &zero_port, || None).endpoint,
            None
        );
    }

    #[test]
    fn system_mode_uses_stub_and_inherits_when_unset() {
        let settings = ProxySettings::default();
        assert_eq!(
            resolve_with("https://github.com", &settings, || unreachable!(
                "system mode must not read manual password"
            )),
            ProxyConfig::default()
        );
        assert_eq!(
            resolve_policy_with("https://github.com", &settings, || unreachable!()),
            ProxyPolicy::Inherit
        );
    }

    #[test]
    fn ureq_proxy_respects_no_proxy_and_builds_socks_and_http_proxies() {
        let config = resolve_with("https://github.com", &manual_settings(), || {
            Some("secret".into())
        });
        assert!(ureq_proxy("https://github.com/satotek/lamina", &config).is_some());
        assert!(ureq_proxy("http://localhost:3000/api", &config).is_none());
        assert!(ureq_proxy("http://10.1.2.3/api", &config).is_none());
        assert!(ureq_proxy("https://git.internal.corp/repo", &config).is_none());
    }

    #[test]
    fn ureq_proxy_supports_ipv6_and_password_without_username() {
        let config = ProxyConfig {
            endpoint: Some(ProxyEndpoint {
                scheme: ProxyScheme::Http,
                host: "::1".into(),
                port: 8888,
                username: None,
                password: Some("token".into()),
            }),
            no_proxy: vec![],
        };
        assert!(ureq_proxy("https://github.com", &config).is_some());
    }
}
