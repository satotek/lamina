//! In-app update checking, cryptographic verification, and platform installers.
//!
//! Pure manifest parsing, signature/hash verification, and platform selection
//! live in [`core`]. This module orchestrates background HTTP requests via
//! `ureq` on GPUI's background executor, non-blocking notifications, and
//! platform-specific install/relaunch steps.

pub mod core;

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext as _, BorrowAppContext as _, Context, Global, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window, actions, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::h_flex;
use gpui_component::notification::Notification;
use gpui_component::v_flex;

use crate::settings::{self, Settings};
use crate::style::Design;
use crate::ui::{ButtonKind, ButtonSize, button, spinner};

use self::core::{
    Arch, AvailableUpdate, CheckTrigger, InstallKind, InstallMode, MANIFEST_SIG_URL, MANIFEST_URL,
    ManifestAsset, Platform, UPDATE_PUBLIC_KEY, UpdateDecision, UpdateError, asset_download_url,
    detect_install_kind, evaluate_update, has_release_version, is_check_due, is_placeholder_key,
    parse_public_key, verify_manifest_and_parse, verify_reader_against_asset,
};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(4);
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(120);
const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIG_BYTES: usize = 64 * 1024;
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

actions!(
    lamina,
    [
        /// Checks GitHub Releases for a newer Lamina release.
        CheckForUpdates,
    ]
);

/// Notification identity so any new updater notification replaces the previous one.
struct UpdaterNotification;

#[derive(Default, Clone, PartialEq, Eq)]
enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    Downloading {
        version: String,
        received: u64,
        total: Option<u64>,
    },
    Installing {
        version: String,
    },
}

#[derive(Default)]
pub(crate) struct UpdaterState {
    phase: UpdatePhase,
    outcome: Option<Outcome>,
}

impl Global for UpdaterState {}

/// What the About page draws beside its check-now button while a check or
/// install runs, as one line. Once idle it shows the last [`Outcome`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Checking,
    Downloading { version: String },
    Installing { version: String },
}

impl Status {
    /// The localized one-line status.
    pub fn label(&self) -> String {
        match self {
            Status::Checking => t!("updater.checking").to_string(),
            Status::Downloading { version } => {
                t!("updater.downloading", version = version.clone()).to_string()
            }
            Status::Installing { version } => {
                t!("updater.installing", version = version.clone()).to_string()
            }
        }
    }
}

/// How the last check this session ended. The notification that reports it
/// can be dismissed or missed, so the About page keeps showing it until the
/// next check starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Available(AvailableUpdate),
    Failed(String),
}

/// The last check's outcome this session, or `None` before any check ends.
pub fn outcome(cx: &App) -> Option<Outcome> {
    cx.try_global::<UpdaterState>()
        .and_then(|state| state.outcome.clone())
}

/// Installs an update the last check found, as the notification's install
/// button does.
pub fn install(update: AvailableUpdate, cx: &mut App) {
    start_install(update, cx);
}

/// The updater's in-flight phase, or `None` when no check or install runs.
pub fn status(cx: &App) -> Option<Status> {
    let phase = cx
        .try_global::<UpdaterState>()
        .map(|state| state.phase.clone())
        .unwrap_or_default();
    match phase {
        UpdatePhase::Idle => None,
        UpdatePhase::Checking => Some(Status::Checking),
        UpdatePhase::Downloading { version, .. } => Some(Status::Downloading { version }),
        UpdatePhase::Installing { version } => Some(Status::Installing { version }),
    }
}

/// Registers updater state and the `CheckForUpdates` action. Call once at startup.
pub fn bind(cx: &mut App) {
    cx.set_global(UpdaterState::default());
    cx.on_action(|_: &CheckForUpdates, cx: &mut App| {
        cx.defer(|cx| check_now(CheckTrigger::Manual, cx));
    });
}

/// Schedules an automatic update check a few seconds after startup when
/// enabled, running a release build, and at least 24 hours since the last check.
pub fn schedule_startup_check(cx: &mut App) {
    let updates = &Settings::get(cx).updates;
    if !updates.auto_check {
        return;
    }
    if !has_release_version(cfg!(debug_assertions), CURRENT_VERSION) {
        return;
    }
    if !is_check_due(updates.last_checked_at, unix_now()) {
        return;
    }

    cx.spawn(async move |cx| {
        cx.background_executor().timer(STARTUP_CHECK_DELAY).await;
        cx.update(|cx| {
            if Settings::get(cx).updates.auto_check {
                check_now(CheckTrigger::Automatic, cx);
            }
        });
    })
    .detach();
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn check_now(trigger: CheckTrigger, cx: &mut App) {
    let phase = cx
        .try_global::<UpdaterState>()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    match phase {
        UpdatePhase::Idle => {}
        UpdatePhase::Checking => {
            if trigger == CheckTrigger::Manual {
                push_checking_notification(cx);
            }
            return;
        }
        UpdatePhase::Downloading { .. } | UpdatePhase::Installing { .. } => {
            if trigger == CheckTrigger::Manual {
                push_progress_notification(cx);
            }
            return;
        }
    }

    let skipped_version = Settings::get(cx).updates.skipped_version.clone();
    let proxy = Settings::get(cx).proxy.clone();
    let exe = std::env::current_exe().ok();
    let install_kind =
        detect_install_kind(Platform::current(), exe.as_deref(), cfg!(debug_assertions));

    set_phase(UpdatePhase::Checking, cx);
    set_outcome(None, cx);
    if trigger == CheckTrigger::Manual {
        push_checking_notification(cx);
    }

    let task = cx.background_executor().spawn(async move {
        fetch_and_evaluate(trigger, skipped_version.as_deref(), install_kind, &proxy)
    });

    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            set_phase(UpdatePhase::Idle, cx);
            match result {
                Ok(decision) => {
                    settings::update(cx, |s| s.updates.last_checked_at = Some(unix_now()));
                    set_outcome(
                        match &decision {
                            UpdateDecision::Available(update) => {
                                Some(Outcome::Available(update.clone()))
                            }
                            // The newest release is one the user chose to
                            // skip, so for them this build is current.
                            UpdateDecision::UpToDate | UpdateDecision::Skipped(_) => {
                                Some(Outcome::UpToDate)
                            }
                        },
                        cx,
                    );
                    handle_decision(trigger, decision, cx);
                }
                Err(err) => {
                    log::warn!("update check failed: {err}");
                    set_outcome(Some(Outcome::Failed(error_message(&err))), cx);
                    if trigger == CheckTrigger::Manual {
                        push_error_notification(&err, cx);
                    }
                }
            }
        });
    })
    .detach();
}

fn fetch_and_evaluate(
    trigger: CheckTrigger,
    skipped_version: Option<&str>,
    install_kind: InstallKind,
    proxy: &settings::ProxySettings,
) -> Result<UpdateDecision, CheckError> {
    if let Err(err) = parse_public_key(UPDATE_PUBLIC_KEY) {
        return Err(CheckError::Update(err));
    }

    let agent = crate::proxy::http_agent(MANIFEST_URL, proxy, Duration::from_secs(30));
    let manifest_bytes = fetch_bytes(&agent, MANIFEST_URL, MAX_MANIFEST_BYTES)?;
    let sig_bytes = fetch_bytes(&agent, MANIFEST_SIG_URL, MAX_SIG_BYTES)?;
    let sig_text = String::from_utf8(sig_bytes)
        .map_err(|err| CheckError::Update(UpdateError::SignatureDecode(err.to_string())))?;

    let manifest = verify_manifest_and_parse(UPDATE_PUBLIC_KEY, &manifest_bytes, &sig_text)
        .map_err(CheckError::Update)?;

    evaluate_update(
        &manifest,
        CURRENT_VERSION,
        skipped_version,
        trigger,
        Platform::current(),
        Arch::current(),
        install_kind,
    )
    .map_err(CheckError::Update)
}

#[derive(Debug)]
enum CheckError {
    Network(String),
    Update(UpdateError),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(msg) => write!(f, "{msg}"),
            Self::Update(err) => write!(f, "{err}"),
        }
    }
}

fn handle_decision(trigger: CheckTrigger, decision: UpdateDecision, cx: &mut App) {
    match decision {
        UpdateDecision::UpToDate => {
            if trigger == CheckTrigger::Manual {
                push_notification(cx, || {
                    Notification::info(t!("updater.up_to_date").to_string())
                        .id::<UpdaterNotification>()
                });
            }
        }
        UpdateDecision::Skipped(_) => {}
        UpdateDecision::Available(update) => {
            push_available_notification(update, cx);
        }
    }
}

fn start_install(update: AvailableUpdate, cx: &mut App) {
    if let Err(err) = parse_public_key(UPDATE_PUBLIC_KEY) {
        push_error_notification(&CheckError::Update(err), cx);
        return;
    }

    let current_phase = cx
        .try_global::<UpdaterState>()
        .map(|s| s.phase.clone())
        .unwrap_or_default();
    if current_phase != UpdatePhase::Idle {
        return;
    }

    match update.mode.clone() {
        InstallMode::NotifyOnly { .. } => {
            cx.open_with_system(Path::new(&update.notes_url));
        }
        InstallMode::MacosDmg { bundle_path, asset } => {
            if !is_bundle_parent_writable(&bundle_path) {
                log::warn!(
                    "cannot update bundle at {}: parent directory is not writable; opening release page instead",
                    bundle_path.display()
                );
                cx.open_with_system(Path::new(&update.notes_url));
                let version = update.version.clone();
                push_notification(cx, move || {
                    Notification::info(t!("updater.bundle_not_writable").to_string())
                        .title(t!("updater.available_title", version = version).to_string())
                        .id::<UpdaterNotification>()
                });
                return;
            }
            launch_download_and_install(
                update.version,
                update.tag,
                asset,
                InstallTarget::MacosBundle(bundle_path),
                cx,
            );
        }
        InstallMode::WindowsSetup { asset } => {
            launch_download_and_install(
                update.version,
                update.tag,
                asset,
                InstallTarget::WindowsSetup,
                cx,
            );
        }
    }
}

enum InstallTarget {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    MacosBundle(std::path::PathBuf),
    WindowsSetup,
}

#[derive(Default)]
struct DownloadProgress {
    received: AtomicU64,
    total: AtomicU64,
    installing: AtomicBool,
    done: AtomicBool,
}

fn launch_download_and_install(
    version: String,
    tag: String,
    asset: ManifestAsset,
    target: InstallTarget,
    cx: &mut App,
) {
    set_phase(
        UpdatePhase::Downloading {
            version: version.clone(),
            received: 0,
            total: None,
        },
        cx,
    );
    push_progress_notification(cx);

    let progress = Arc::new(DownloadProgress::default());
    let poll_progress = Arc::clone(&progress);
    let poll_version = version.clone();

    cx.spawn(async move |cx| {
        let mut last_received = u64::MAX;
        let mut last_total = u64::MAX;
        let mut was_installing = false;
        while !poll_progress.done.load(Ordering::Relaxed) {
            cx.background_executor().timer(PROGRESS_POLL_INTERVAL).await;
            if poll_progress.done.load(Ordering::Relaxed) {
                break;
            }
            let installing = poll_progress.installing.load(Ordering::Relaxed);
            let received = poll_progress.received.load(Ordering::Relaxed);
            let total_raw = poll_progress.total.load(Ordering::Relaxed);
            if installing != was_installing || received != last_received || total_raw != last_total
            {
                was_installing = installing;
                last_received = received;
                last_total = total_raw;
                let phase = if installing {
                    UpdatePhase::Installing {
                        version: poll_version.clone(),
                    }
                } else {
                    UpdatePhase::Downloading {
                        version: poll_version.clone(),
                        received,
                        total: (total_raw > 0).then_some(total_raw),
                    }
                };
                cx.update(|cx| set_phase(phase, cx));
            }
        }
    })
    .detach();

    let proxy = Settings::get(cx).proxy.clone();
    let bg_progress = Arc::clone(&progress);
    let task = cx.background_executor().spawn(async move {
        let result = download_verify_and_install(&tag, &asset, target, &proxy, &bg_progress);
        bg_progress.done.store(true, Ordering::Relaxed);
        result
    });

    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            set_phase(UpdatePhase::Idle, cx);
            match result {
                Ok(InstallOutcome::QuitToComplete) => {
                    cx.quit();
                }
                Err(err) => {
                    log::error!("update install failed: {err}");
                    push_notification(cx, move || {
                        Notification::error(
                            t!("updater.install_failed", error = err.clone()).to_string(),
                        )
                        .title(t!("updater.error_title").to_string())
                        .id::<UpdaterNotification>()
                        .autohide(false)
                    });
                }
            }
        });
    })
    .detach();
}

// Only the macOS and Windows installers finish by quitting; elsewhere the
// updater just points at the release page and never builds this.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
enum InstallOutcome {
    QuitToComplete,
}

fn download_verify_and_install(
    tag: &str,
    asset: &ManifestAsset,
    target: InstallTarget,
    proxy: &settings::ProxySettings,
    progress: &DownloadProgress,
) -> Result<InstallOutcome, String> {
    if let Err(err) = parse_public_key(UPDATE_PUBLIC_KEY) {
        return Err(err.to_string());
    }
    if is_placeholder_key(UPDATE_PUBLIC_KEY) {
        return Err(UpdateError::PlaceholderKey.to_string());
    }

    let url = asset_download_url(tag, &asset.name).map_err(|err| err.to_string())?;
    let agent = crate::proxy::http_agent(&url, proxy, Duration::from_secs(30));

    let staging_dir = tempfile::Builder::new()
        .prefix("lamina-update-")
        .tempdir()
        .map_err(|err| format!("failed to create temp directory: {err}"))?;
    let download_path = staging_dir.path().join(&asset.name);

    download_and_verify(&agent, &url, &download_path, asset, progress)?;
    progress.installing.store(true, Ordering::Relaxed);

    match target {
        InstallTarget::MacosBundle(bundle_path) => {
            #[cfg(target_os = "macos")]
            {
                install_macos_dmg(&download_path, &bundle_path, staging_dir)?;
                Ok(InstallOutcome::QuitToComplete)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = bundle_path;
                Err("macOS bundle updates are only supported on macOS".to_owned())
            }
        }
        InstallTarget::WindowsSetup => {
            #[cfg(target_os = "windows")]
            {
                spawn_windows_installer(&download_path)?;
                // Keep the staging directory so the setup executable stays on
                // disk while the installer runs after Lamina quits.
                let _ = staging_dir.keep();
                Ok(InstallOutcome::QuitToComplete)
            }
            #[cfg(not(target_os = "windows"))]
            {
                Err("Windows setup updates are only supported on Windows".to_owned())
            }
        }
    }
}

fn download_and_verify(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    asset: &ManifestAsset,
    progress: &DownloadProgress,
) -> Result<(), String> {
    let response = agent
        .get(url)
        .call()
        .map_err(|err| format!("failed to download update: {err}"))?;

    if let Some(len) = response
        .headers()
        .get("Content-Length")
        .and_then(|val| val.to_str().ok())
        .and_then(|val| val.parse::<u64>().ok())
    {
        if len > MAX_ASSET_BYTES {
            return Err("update asset exceeds maximum supported size".to_owned());
        }
        progress.total.store(len, Ordering::Relaxed);
    }

    let mut file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|err| format!("failed to create download file: {err}"))?;

    let mut reader = response.into_body().into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|err| format!("download read error: {err}"))?;
        if n == 0 {
            break;
        }
        received = received.saturating_add(n as u64);
        if received > MAX_ASSET_BYTES {
            return Err("update asset exceeds maximum supported size".to_owned());
        }
        file.write_all(&buf[..n])
            .map_err(|err| format!("download write error: {err}"))?;
        progress.received.store(received, Ordering::Relaxed);
    }

    file.flush()
        .map_err(|err| format!("failed to flush download file: {err}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|err| format!("failed to rewind download file: {err}"))?;

    verify_reader_against_asset(UPDATE_PUBLIC_KEY, &mut file, &asset.minisig, &asset.sha256)
        .map_err(|err| err.to_string())
}

fn fetch_bytes(agent: &ureq::Agent, url: &str, limit: usize) -> Result<Vec<u8>, CheckError> {
    let response = agent
        .get(url)
        .call()
        .map_err(|err| CheckError::Network(err.to_string()))?;
    let mut reader = response.into_body().into_reader().take((limit as u64) + 1);
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(|err| CheckError::Network(err.to_string()))?;
    if buf.len() > limit {
        return Err(CheckError::Network(format!(
            "response from {url} exceeded {limit} bytes"
        )));
    }
    Ok(buf)
}

fn is_bundle_parent_writable(bundle_path: &Path) -> bool {
    let Some(parent) = bundle_path.parent() else {
        return false;
    };
    tempfile::Builder::new()
        .prefix(".lamina-write-probe-")
        .tempfile_in(parent)
        .is_ok()
}

#[cfg(target_os = "macos")]
struct HdiutilMount {
    mount_point: PathBuf,
}

#[cfg(target_os = "macos")]
impl Drop for HdiutilMount {
    fn drop(&mut self) {
        let status = std::process::Command::new("hdiutil")
            .arg("detach")
            .arg(&self.mount_point)
            .arg("-quiet")
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            let _ = std::process::Command::new("hdiutil")
                .arg("detach")
                .arg("-force")
                .arg(&self.mount_point)
                .arg("-quiet")
                .status();
        }
    }
}

#[cfg(target_os = "macos")]
fn install_macos_dmg(
    dmg_path: &Path,
    bundle_path: &Path,
    staging_dir: tempfile::TempDir,
) -> Result<(), String> {
    let mount_dir = staging_dir.path().join("mount");
    std::fs::create_dir_all(&mount_dir)
        .map_err(|err| format!("failed to create DMG mount point: {err}"))?;

    let attach = std::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount_dir)
        .arg(dmg_path)
        .output()
        .map_err(|err| format!("failed to run hdiutil attach: {err}"))?;
    if !attach.status.success() {
        let stderr = String::from_utf8_lossy(&attach.stderr);
        return Err(format!("hdiutil attach failed: {}", stderr.trim()));
    }
    let mount_guard = HdiutilMount {
        mount_point: mount_dir.clone(),
    };

    let source_app = find_app_in_mount(&mount_dir)?;
    let parent = bundle_path
        .parent()
        .ok_or_else(|| "invalid bundle path: missing parent directory".to_owned())?;
    let pid = std::process::id();
    let staged_app = parent.join(format!(".Lamina.app.new.{pid}"));
    let backup_app = parent.join(format!(".Lamina.app.old.{pid}"));

    let _ = std::fs::remove_dir_all(&staged_app);
    let _ = std::fs::remove_dir_all(&backup_app);

    let ditto = std::process::Command::new("ditto")
        .arg(&source_app)
        .arg(&staged_app)
        .output()
        .map_err(|err| format!("failed to run ditto: {err}"))?;
    if !ditto.status.success() {
        let _ = std::fs::remove_dir_all(&staged_app);
        let stderr = String::from_utf8_lossy(&ditto.stderr);
        return Err(format!("ditto failed: {}", stderr.trim()));
    }

    let codesign = std::process::Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&staged_app)
        .output()
        .map_err(|err| format!("failed to run codesign: {err}"))?;
    if !codesign.status.success() {
        let _ = std::fs::remove_dir_all(&staged_app);
        let stderr = String::from_utf8_lossy(&codesign.stderr);
        return Err(format!("codesign verification failed: {}", stderr.trim()));
    }

    // Detach the DMG before swapping and relaunching so no background mount
    // holds open handles.
    drop(mount_guard);
    drop(staging_dir);

    if let Err(err) = std::fs::rename(bundle_path, &backup_app) {
        let _ = std::fs::remove_dir_all(&staged_app);
        return Err(format!("failed to back up current bundle: {err}"));
    }

    if let Err(err) = std::fs::rename(&staged_app, bundle_path) {
        if let Err(rollback_err) = std::fs::rename(&backup_app, bundle_path) {
            log::error!("failed to restore backup bundle after rename error: {rollback_err}");
        }
        let _ = std::fs::remove_dir_all(&staged_app);
        return Err(format!("failed to move updated bundle into place: {err}"));
    }

    if let Err(err) = std::fs::remove_dir_all(&backup_app) {
        log::warn!("failed to remove old bundle backup: {err}");
    }

    // Wait briefly for the current process to exit, then launch the replacement.
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "pid=\"$1\"; app=\"$2\"; i=0; \
             while [ \"$i\" -lt 100 ] && kill -0 \"$pid\" 2>/dev/null; do \
               i=$((i+1)); sleep 0.1; \
             done; \
             exec open -n \"$app\"",
        )
        .arg("lamina-relaunch")
        .arg(pid.to_string())
        .arg(bundle_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("failed to spawn relaunch helper: {err}"))?;

    Ok(())
}

#[cfg(target_os = "macos")]
fn find_app_in_mount(mount_dir: &Path) -> Result<PathBuf, String> {
    let preferred = mount_dir.join("Lamina.app");
    if preferred.is_dir() {
        return Ok(preferred);
    }
    let entries = std::fs::read_dir(mount_dir)
        .map_err(|err| format!("failed to read mounted DMG directory: {err}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("app") && path.is_dir() {
            return Ok(path);
        }
    }
    Err("mounted DMG does not contain an application bundle".to_owned())
}

#[cfg(target_os = "windows")]
fn spawn_windows_installer(setup_exe: &Path) -> Result<(), String> {
    std::process::Command::new(setup_exe)
        .args([
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/CLOSEAPPLICATIONS",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("failed to launch installer: {err}"))?;
    Ok(())
}

fn set_outcome(outcome: Option<Outcome>, cx: &mut App) {
    if cx.has_global::<UpdaterState>() {
        cx.update_global::<UpdaterState, _>(|state, _| state.outcome = outcome);
    }
}

fn set_phase(phase: UpdatePhase, cx: &mut App) {
    if cx.has_global::<UpdaterState>() {
        cx.update_global::<UpdaterState, _>(|state, _| state.phase = phase);
    }
}

fn push_notification(cx: &mut App, build: impl FnOnce() -> Notification + 'static) {
    let Some(window) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        window.push_notification(build(), cx);
    });
}

fn push_checking_notification(cx: &mut App) {
    push_notification(cx, || {
        Notification::info(t!("updater.checking").to_string())
            .id::<UpdaterNotification>()
            .autohide(false)
    });
}

fn error_message(err: &CheckError) -> String {
    match err {
        CheckError::Update(UpdateError::PlaceholderKey) => {
            t!("updater.placeholder_key").to_string()
        }
        other => t!("updater.check_failed", error = other.to_string()).to_string(),
    }
}

fn push_error_notification(err: &CheckError, cx: &mut App) {
    let message = error_message(err);
    push_notification(cx, move || {
        Notification::error(message)
            .title(t!("updater.error_title").to_string())
            .id::<UpdaterNotification>()
            .autohide(false)
    });
}

fn push_available_notification(update: AvailableUpdate, cx: &mut App) {
    let version = update.version.clone();
    let title = t!("updater.available_title", version = version).to_string();
    let body = if update.can_install() {
        t!("updater.available_body", version = version).to_string()
    } else {
        t!("updater.available_manual_body", version = version).to_string()
    };

    push_notification(cx, move || {
        Notification::new()
            .id::<UpdaterNotification>()
            .autohide(false)
            .title(title)
            .content(move |_, _, cx| {
                let note = cx.entity().downgrade();
                let notes_url = update.notes_url.clone();
                let skip_version = update.version.clone();

                let primary_button = if update.can_install() {
                    let install_update = update.clone();
                    let note_for_install = note.clone();
                    button(
                        "updater-install",
                        t!("updater.install_and_restart").to_string(),
                    )
                    .kind(ButtonKind::Primary)
                    .size(ButtonSize::Small)
                    .on_click(move |_, window, cx| {
                        let _ = note_for_install.update(cx, |n, cx| n.dismiss(window, cx));
                        let install_update = install_update.clone();
                        cx.defer(move |cx| start_install(install_update, cx));
                    })
                } else {
                    let download_url = notes_url.clone();
                    let note_for_download = note.clone();
                    button("updater-download", t!("updater.download").to_string())
                        .kind(ButtonKind::Primary)
                        .size(ButtonSize::Small)
                        .on_click(move |_, window, cx| {
                            cx.open_with_system(Path::new(&download_url));
                            let _ = note_for_download.update(cx, |n, cx| n.dismiss(window, cx));
                        })
                };

                v_flex()
                    .gap_2()
                    .child(div().child(body.clone()))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(primary_button)
                            .child(
                                button(
                                    "updater-release-notes",
                                    t!("updater.release_notes").to_string(),
                                )
                                .kind(ButtonKind::Normal)
                                .size(ButtonSize::Small)
                                .on_click(move |_, _, cx| {
                                    cx.open_with_system(Path::new(&notes_url));
                                }),
                            )
                            .child(
                                button("updater-skip", t!("updater.skip_version").to_string())
                                    .kind(ButtonKind::Ghost)
                                    .size(ButtonSize::Small)
                                    .on_click(move |_, window, cx| {
                                        let skip_version = skip_version.clone();
                                        settings::update(cx, |s| {
                                            s.updates.skipped_version = Some(skip_version);
                                        });
                                        let _ = note.update(cx, |n, cx| n.dismiss(window, cx));
                                    }),
                            ),
                    )
                    .into_any_element()
            })
    });
}

fn push_progress_notification(cx: &mut App) {
    let Some(window) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        let view = cx.new(InstallProgressView::new);
        let note = Notification::new()
            .id::<UpdaterNotification>()
            .autohide(false)
            .title(t!("updater.updating_title").to_string())
            .content(move |_, _, _| div().child(view.clone()).into_any_element());
        window.push_notification(note, cx);
    });
}

struct InstallProgressView {
    _subscription: Subscription,
}

impl InstallProgressView {
    fn new(cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_global::<UpdaterState>(|_, cx| cx.notify());
        Self {
            _subscription: subscription,
        }
    }
}

impl Render for InstallProgressView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let design = Design::new(cx.theme(), Settings::get(cx));
        let phase = cx
            .try_global::<UpdaterState>()
            .map(|s| s.phase.clone())
            .unwrap_or_default();

        let status = match phase {
            UpdatePhase::Downloading {
                version,
                received,
                total: Some(total),
            } if total > 0 => {
                let percent = ((received.saturating_mul(100)) / total).min(100);
                t!(
                    "updater.downloading_percent",
                    version = version,
                    percent = percent
                )
                .to_string()
            }
            UpdatePhase::Downloading { version, .. } => {
                t!("updater.downloading", version = version).to_string()
            }
            UpdatePhase::Installing { version } => {
                t!("updater.installing", version = version).to_string()
            }
            UpdatePhase::Checking => t!("updater.checking").to_string(),
            UpdatePhase::Idle => t!("updater.installing_done").to_string(),
        };

        h_flex()
            .items_center()
            .gap(design.px(8.0))
            .child(spinner("updater-progress-spinner", &design, 14.0))
            .child(
                div()
                    .text_size(px(design.small()))
                    .text_color(design.text)
                    .child(status),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_parent_writability_probe_succeeds_in_tempdir_and_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bundle = dir.path().join("Lamina.app");
        assert!(is_bundle_parent_writable(&fake_bundle));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[gpui::test]
    fn the_last_outcome_is_kept_while_idle(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(UpdaterState::default()));
        cx.update(|cx| assert_eq!(outcome(cx), None));

        cx.update(|cx| set_outcome(Some(Outcome::Failed("offline".to_owned())), cx));
        cx.update(|cx| set_phase(UpdatePhase::Idle, cx));
        assert_eq!(
            cx.update(|cx| outcome(cx)),
            Some(Outcome::Failed("offline".to_owned())),
            "returning to idle does not drop what the check found"
        );
    }

    #[gpui::test]
    fn status_reports_only_the_phase_in_flight(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(UpdaterState::default());
        });
        cx.update(|cx| assert_eq!(status(cx), None));

        for phase in [
            UpdatePhase::Checking,
            UpdatePhase::Downloading {
                version: "0.2.0".to_owned(),
                received: 10,
                total: Some(100),
            },
            UpdatePhase::Installing {
                version: "0.2.0".to_owned(),
            },
        ] {
            cx.update(|cx| set_phase(phase.clone(), cx));
            let reported = cx.update(|cx| status(cx));
            assert_eq!(
                reported,
                Some(match &phase {
                    UpdatePhase::Checking => Status::Checking,
                    UpdatePhase::Downloading { version, .. } => {
                        Status::Downloading {
                            version: version.clone(),
                        }
                    }
                    UpdatePhase::Installing { version } => {
                        Status::Installing {
                            version: version.clone(),
                        }
                    }
                    UpdatePhase::Idle => unreachable!("the phase was just set"),
                })
            );
            cx.update(|_| {
                assert!(
                    reported
                        .as_ref()
                        .is_some_and(|status| !status.label().is_empty()),
                    "every phase draws a line"
                );
            });
        }
    }
}
