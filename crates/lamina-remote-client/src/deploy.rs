//! Install `lamina-remote` on a host the app reaches over `ssh`.
//!
//! The helper has to run on the remote machine, and this process may be a
//! macOS or Windows build that cannot execute a Linux binary. Release builds
//! therefore ship prebuilt `lamina-remote` binaries and this crate only
//! uploads the matching file:
//!
//! - `lamina-remote` built for `x86_64-unknown-linux-musl` and
//!   `aarch64-unknown-linux-musl`, with the same `LAMINA_BUILD_ID` as the app
//!   (the id is `version+LAMINA_BUILD_ID`, or `version+dev` when it is unset).
//!   musl is what lets the helper start without the host's libc.
//! - Files named `lamina-remote-<target>` in one directory. The app looks in
//!   `remote-helpers/` next to its own executable when [`HELPER_DIR_ENV`] is
//!   unset. A release archive puts that directory there (on macOS it is a
//!   symlink to the copies under `Contents/Resources`, because an ad-hoc
//!   signature cannot cover a Linux binary placed directly in `Contents/MacOS`).
//!   Otherwise the variable names the directory.
//! - No other operating system or architecture: the client reports an error
//!   instead of compiling on the host.
//!
//! [`HELPER_OVERRIDE_ENV`], set in the remote user's environment, is a full
//! path to a helper. When it is set the client skips the upload, still checks
//! the build id, and does not delete that file.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use lamina_core::{Error, Result};
use lamina_remote_protocol::PROTOCOL_VERSION;

use crate::ssh::{self, AskpassHandler, AskpassSession, SshOutput};

/// Directory of prebuilt `lamina-remote-<target>` binaries on this machine.
pub const HELPER_DIR_ENV: &str = "LAMINA_REMOTE_HELPER_DIR";

/// Remote-side path that replaces the uploaded helper. Not deleted on mismatch.
pub const HELPER_OVERRIDE_ENV: &str = "LAMINA_REMOTE_HELPER";

/// `uname` and the cache directory, plus the override path when the remote
/// user set one. Double quotes only, so the ssh command can wrap it in single
/// quotes without embedding any.
const PROBE_SCRIPT: &str = "printf \"%s\\n\" \"$(uname -s)\" \"$(uname -m)\" \"${LAMINA_REMOTE_HELPER-}\" \"${XDG_CACHE_HOME:-$HOME/.cache}\"";

/// `umask` covers every directory `mkdir -p` creates. The file mode is set
/// explicitly because `cat` follows the umask only until something else
/// changes it, and the rename must not leave a world-readable binary behind.
const UPLOAD_SCRIPT: &str = "set -e; umask 077; mkdir -p -- \"$LAMINA_HELPER_DIR\"; tmp=\"$LAMINA_HELPER_PATH.tmp\"; cat > \"$tmp\"; chmod 700 \"$tmp\"; mv -f \"$tmp\" \"$LAMINA_HELPER_PATH\"";

/// Where to find `ssh`, the host, and the local helper directory.
#[derive(Clone)]
pub struct InstallOptions {
    pub program: PathBuf,
    pub host: String,
    pub port: Option<u16>,
    /// Overrides [`HELPER_DIR_ENV`] when set.
    pub helper_dir: Option<PathBuf>,
    pub askpass_program: Option<PathBuf>,
    pub askpass: Option<AskpassHandler>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::from("ssh"),
            host: String::new(),
            port: None,
            helper_dir: None,
            askpass_program: None,
            askpass: None,
        }
    }
}

/// Linux musl triples this client knows how to upload.
pub fn target_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-musl"),
        _ => Err(Error::other(format!(
            "remote helper is not available for {os} {arch}; supported targets are x86_64-unknown-linux-musl and aarch64-unknown-linux-musl"
        ))),
    }
}

/// Makes sure the host has a helper for this build and returns the remote
/// path to execute: the [`HELPER_OVERRIDE_ENV`] path when the remote side
/// sets it, otherwise the cached copy, uploaded only when it is missing or
/// fails `--self-check` for this protocol and build.
pub fn install_helper(options: &InstallOptions) -> Result<String> {
    let askpass = ssh::start_askpass(options.askpass.as_ref(), options.askpass_program.as_deref())?;
    install_with(options, askpass.as_ref())
}

pub(crate) fn install_with(
    options: &InstallOptions,
    askpass: Option<&AskpassSession>,
) -> Result<String> {
    if options.host.is_empty() {
        return Err(Error::other("the remote host is empty"));
    }
    if options.program.as_os_str().is_empty() {
        return Err(Error::other("the ssh program path is empty"));
    }
    let probe = probe(options, askpass)?;
    if let Some(override_path) = probe.helper_override {
        verify(options, askpass, &override_path, true)?;
        return Ok(override_path);
    }
    let target = target_for(&probe.os, &probe.arch)?;
    if probe.cache.is_empty() {
        return Err(Error::other(
            "the remote host did not report a cache directory",
        ));
    }
    let remote_path = format!(
        "{}/lamina/remote/{}/lamina-remote-{target}",
        probe.cache.trim_end_matches('/'),
        env!("CARGO_PKG_VERSION"),
    );
    let local = local_binary(options, target)?;
    // The helper is tens of megabytes. A copy that already answers for this
    // protocol and build is the one this app would upload, so a reconnect
    // reuses it instead of sending it again.
    if verify(options, askpass, &remote_path, false).is_ok() {
        return Ok(remote_path);
    }
    upload(options, askpass, &local, &remote_path)?;
    if verify(options, askpass, &remote_path, false).is_err() {
        // One replacement covers a partial upload. A second failure is
        // reported as-is: retrying again will not change the binary.
        remove_remote(options, askpass, &remote_path)?;
        upload(options, askpass, &local, &remote_path)?;
        verify(options, askpass, &remote_path, false).map_err(|error| {
            Error::other(format!(
                "remote helper still failed after reinstall: {error}"
            ))
        })?;
    }
    Ok(remote_path)
}

struct Probe {
    os: String,
    arch: String,
    helper_override: Option<String>,
    cache: String,
}

fn probe(options: &InstallOptions, askpass: Option<&AskpassSession>) -> Result<Probe> {
    let output = run(
        options,
        askpass,
        &["sh".to_owned(), "-c".to_owned(), PROBE_SCRIPT.to_owned()],
        None,
    )?;
    ensure_success("could not inspect the remote host", &output)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let os = lines.next().unwrap_or("").trim().to_owned();
    let arch = lines.next().unwrap_or("").trim().to_owned();
    let override_path = lines.next().unwrap_or("").trim().to_owned();
    let cache = lines.next().unwrap_or("").trim().to_owned();
    if os.is_empty() || arch.is_empty() {
        return Err(Error::other(format!(
            "could not inspect the remote host: unexpected probe output: {text}"
        )));
    }
    Ok(Probe {
        os,
        arch,
        helper_override: if override_path.is_empty() {
            None
        } else {
            Some(override_path)
        },
        cache,
    })
}

fn local_binary(options: &InstallOptions, target: &str) -> Result<PathBuf> {
    let dir = match &options.helper_dir {
        Some(dir) => dir.clone(),
        None => match std::env::var(HELPER_DIR_ENV) {
            Ok(value) if !value.is_empty() => PathBuf::from(value),
            _ => {
                return Err(Error::other(format!(
                    "{HELPER_DIR_ENV} is not set; point it at the directory of lamina-remote-<target> release binaries"
                )));
            }
        },
    };
    let path = dir.join(format!("lamina-remote-{target}"));
    if !path.is_file() {
        return Err(Error::other(format!(
            "remote helper binary lamina-remote-{target} was not found in {}; set {HELPER_DIR_ENV} to the directory that contains the release builds",
            dir.display()
        )));
    }
    Ok(path)
}

fn upload(
    options: &InstallOptions,
    askpass: Option<&AskpassSession>,
    local: &Path,
    remote_path: &str,
) -> Result<()> {
    let dir = parent_dir(remote_path);
    let output = run(
        options,
        askpass,
        &[
            "env".to_owned(),
            format!("LAMINA_HELPER_DIR={dir}"),
            format!("LAMINA_HELPER_PATH={remote_path}"),
            "sh".to_owned(),
            "-c".to_owned(),
            UPLOAD_SCRIPT.to_owned(),
        ],
        Some(local),
    )?;
    ensure_success("could not install the remote helper", &output)
}

fn verify(
    options: &InstallOptions,
    askpass: Option<&AskpassSession>,
    remote_path: &str,
    override_path: bool,
) -> Result<()> {
    let output = run(
        options,
        askpass,
        &[remote_path.to_owned(), "--self-check".to_owned()],
        None,
    )?;
    ensure_success("remote helper self-check failed", &output)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut protocol = None;
    let mut build = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("protocol:") {
            protocol = value.trim().parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("build:") {
            build = Some(value.trim().to_owned());
        }
    }
    let (Some(protocol), Some(build)) = (protocol, build) else {
        return Err(Error::other(format!(
            "remote helper self-check was unreadable: {text}"
        )));
    };
    if protocol != PROTOCOL_VERSION {
        return Err(Error::other(format!(
            "remote helper speaks protocol {protocol}, this app speaks {PROTOCOL_VERSION}"
        )));
    }
    let expected = expected_build();
    if build != expected {
        let where_ = if override_path {
            format!("{HELPER_OVERRIDE_ENV}={remote_path}")
        } else {
            remote_path.to_owned()
        };
        return Err(Error::other(format!(
            "remote helper build id mismatch at {where_}: expected {expected}, found {build}"
        )));
    }
    Ok(())
}

fn remove_remote(
    options: &InstallOptions,
    askpass: Option<&AskpassSession>,
    remote_path: &str,
) -> Result<()> {
    let output = run(
        options,
        askpass,
        &[
            "rm".to_owned(),
            "-f".to_owned(),
            "--".to_owned(),
            remote_path.to_owned(),
        ],
        None,
    )?;
    ensure_success("could not replace the remote helper", &output)
}

fn run(
    options: &InstallOptions,
    askpass: Option<&AskpassSession>,
    remote_args: &[String],
    stdin_file: Option<&Path>,
) -> Result<SshOutput> {
    ssh::ssh_run(
        &options.program,
        options.port,
        &options.host,
        remote_args,
        stdin_file,
        askpass,
    )
}

fn ensure_success(purpose: &str, output: &SshOutput) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(Error::other(format!(
        "{purpose}: {}",
        status_detail(&output.status, &output.stderr)
    )))
}

fn status_detail(status: &ExitStatus, stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("exit {status}")
    } else {
        stderr.to_owned()
    }
}

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) if index > 0 => &path[..index],
        _ => ".",
    }
}

fn expected_build() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("LAMINA_BUILD_ID").unwrap_or("dev")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_for_maps_linux_musl_and_rejects_the_rest() {
        assert_eq!(
            target_for("Linux", "x86_64").expect("x86_64"),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            target_for("Linux", "aarch64").expect("aarch64"),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(
            target_for("Linux", "arm64").expect("arm64"),
            "aarch64-unknown-linux-musl"
        );
        let error = target_for("Darwin", "arm64").expect_err("darwin");
        let text = error.to_string();
        assert!(text.contains("Darwin"), "{text}");
        assert!(text.contains("not available"), "{text}");
        assert!(text.contains("x86_64-unknown-linux-musl"), "{text}");
    }
}
