//! `ssh` is a script on `PATH`'s behalf: it ignores the host and runs the
//! remote command locally. Nothing here contacts a real SSH server.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lamina_core::HeadInfo;
use lamina_core::askpass::{Answer, Prompt};
use lamina_core::session::RepositorySession;
use lamina_core::test_support::TestRepo;
use lamina_remote_client::{InstallOptions, RemoteSession, RemoteTarget, SshOptions};

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lamina-ssh-{}-{n}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        Self { root }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Copy)]
enum Lie {
    Never,
    Once,
    Always,
}

struct Fake {
    os: &'static str,
    arch: &'static str,
    lie: Lie,
    /// Exported on the "remote" side as `LAMINA_REMOTE_HELPER`.
    override_path: Option<PathBuf>,
}

struct LaidOut {
    ssh: PathBuf,
    log: PathBuf,
    home: PathBuf,
    helpers: PathBuf,
}

fn layout(scratch: &Scratch, fake: &Fake) -> LaidOut {
    let bin = scratch.root.join("bin");
    let home = scratch.root.join("home");
    let helpers = scratch.root.join("helpers");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&helpers).unwrap();
    let log = scratch.root.join("ssh.log");
    let mark = scratch.root.join("lie-mark");

    let uname = format!(
        "#!/bin/sh\nif [ \"$1\" = \"-s\" ]; then\n  echo {os}\n  exit 0\nfi\nif [ \"$1\" = \"-m\" ]; then\n  echo {arch}\n  exit 0\nfi\necho unknown\nexit 0\n",
        os = fake.os,
        arch = fake.arch,
    );
    write_exec(&bin.join("uname"), &uname);

    let lie = match fake.lie {
        Lie::Never => String::new(),
        Lie::Once => "if [ ! -f \"$MARK\" ]; then\n  printf '%s\\n' 'protocol: 1' 'build: not-this-build'\n  touch \"$MARK\"\n  exit 0\nfi\n".to_owned(),
        Lie::Always => "printf '%s\\n' 'protocol: 1' 'build: not-this-build'\nexit 0\n".to_owned(),
    };
    let override_line = match &fake.override_path {
        Some(path) => format!(
            "export LAMINA_REMOTE_HELPER={}",
            sh_quote(&path.display().to_string())
        ),
        None => String::new(),
    };
    let script = format!(
        r#"#!/bin/sh
export HOME={home}
unset XDG_CACHE_HOME
export PATH={bin}:$PATH
{override_line}
LOG={log}
MARK={mark}
{{
  printf 'SSH_ASKPASS=%s\n' "${{SSH_ASKPASS-}}"
  printf 'SSH_ASKPASS_REQUIRE=%s\n' "${{SSH_ASKPASS_REQUIRE-}}"
  printf 'LAMINA_ASKPASS_ENDPOINT=%s\n' "${{LAMINA_ASKPASS_ENDPOINT-}}"
  printf 'CMD'
  for arg in "$@"; do
    printf ' <%s>' "$arg"
  done
  printf '\n'
}} >> "$LOG"
after=0
cmd=
for arg in "$@"; do
  if [ "$after" = 1 ]; then
    cmd="$cmd $arg"
    continue
  fi
  if [ "$arg" = "--" ]; then
    after=1
  fi
done
cmd=${{cmd# }}
case "$cmd" in
  *--self-check*)
    {lie}
    ;;
esac
if [ -z "$cmd" ]; then
  echo "fake ssh: no remote command" >&2
  exit 2
fi
exec sh -c "$cmd"
"#,
        home = sh_quote(&home.display().to_string()),
        bin = sh_quote(&bin.display().to_string()),
        log = sh_quote(&log.display().to_string()),
        mark = sh_quote(&mark.display().to_string()),
    );
    let ssh = bin.join("ssh");
    write_exec(&ssh, &script);
    LaidOut {
        ssh,
        log,
        home,
        helpers,
    }
}

fn write_exec(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn host_helper(dir: &Path) {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_lamina-remote"));
    let dest = dir.join("lamina-remote-x86_64-unknown-linux-musl");
    fs::copy(&source, &dest).expect("copy helper");
    let mut perms = fs::metadata(&dest).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dest, perms).unwrap();
}

fn copy_exec(source: &Path, dest: &Path) {
    fs::copy(source, dest).unwrap();
    let mut perms = fs::metadata(dest).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(dest, perms).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn options(laid: &LaidOut) -> SshOptions {
    SshOptions {
        program: laid.ssh.clone(),
        helper_dir: Some(laid.helpers.clone()),
        askpass_program: Some(PathBuf::from("lamina-askpass-test")),
        askpass: Some(Arc::new(|_prompt: Prompt| -> Option<Answer> { None })),
    }
}

fn install(laid: &LaidOut, port: Option<u16>) -> Result<String, lamina_core::Error> {
    let mut opts = options(laid);
    lamina_remote_client::install_helper(&InstallOptions {
        program: opts.program.clone(),
        host: "ignored.example".into(),
        port,
        helper_dir: opts.helper_dir.take(),
        askpass_program: opts.askpass_program.clone(),
        askpass: opts.askpass.clone(),
    })
}

#[test]
fn ssh_transport_reads_a_repository_and_installs_mode_0700() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
        },
    );
    host_helper(&laid.helpers);

    let repo = TestRepo::new();
    repo.write("README.md", "alpha\n");
    repo.commit("initial import");

    let (tx, rx) = std::sync::mpsc::channel();
    let session = RemoteSession::connect_ssh(
        RemoteTarget::new("ignored.example").with_port(2222),
        repo.path().display().to_string(),
        move |event| {
            let _ = tx.send(event);
        },
        options(&laid),
    )
    .expect("connect over fake ssh");
    let head = session.reader().head().expect("remote head");
    match head {
        HeadInfo::Branch { short_name, target } => {
            assert_eq!(short_name, "main");
            assert!(target.is_some());
        }
        other => panic!("head: {other:?}"),
    }
    drop(rx);
    drop(session);

    let log = fs::read_to_string(&laid.log).expect("ssh log");
    for needle in [
        "<-T>",
        "<ConnectTimeout=15>",
        "<ServerAliveInterval=15>",
        "<ServerAliveCountMax=3>",
        "<NumberOfPasswordPrompts=1>",
        "<-p>",
        "<2222>",
        "<--stdio>",
        "<--root>",
        "SSH_ASKPASS_REQUIRE=force",
    ] {
        assert!(log.contains(needle), "missing {needle} in {log}");
    }
    assert!(
        !log.contains("StrictHostKeyChecking"),
        "host-key checking must not be disabled: {log}"
    );
    assert!(
        log.contains("LAMINA_ASKPASS_ENDPOINT=") && !log.contains("LAMINA_ASKPASS_ENDPOINT=\n"),
        "askpass endpoint was not passed to ssh: {log}"
    );

    let installed = laid
        .home
        .join(".cache/lamina/remote")
        .join(env!("CARGO_PKG_VERSION"));
    let binary = installed.join("lamina-remote-x86_64-unknown-linux-musl");
    assert!(
        binary.is_file(),
        "helper was not installed at {}",
        binary.display()
    );
    assert_eq!(mode(&binary), 0o700, "helper file mode");
    assert_eq!(mode(&installed), 0o700, "version directory mode");
    assert_eq!(
        mode(&laid.home.join(".cache")),
        0o700,
        "cache directory mode"
    );
}

#[test]
fn a_build_id_mismatch_reinstalls_once() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Once,
            override_path: None,
        },
    );
    host_helper(&laid.helpers);
    let path = install(&laid, None).expect("reinstall succeeds");
    assert!(Path::new(&path).is_file(), "{path}");
    let log = fs::read_to_string(&laid.log).unwrap();
    assert_eq!(
        log.matches("cat >").count(),
        2,
        "upload once, then replace: {log}"
    );
    let check = Command::new(&path)
        .arg("--self-check")
        .output()
        .expect("self-check the installed helper");
    assert!(check.status.success());
    let stdout = String::from_utf8(check.stdout).unwrap();
    assert!(!stdout.contains("not-this-build"), "{stdout}");
}

#[test]
fn a_repeated_build_id_mismatch_is_reported() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Always,
            override_path: None,
        },
    );
    host_helper(&laid.helpers);
    let error = install(&laid, None).expect_err("second mismatch");
    let text = error.to_string();
    assert!(text.contains("not-this-build"), "{text}");
    assert!(text.contains("still failed after reinstall"), "{text}");
    let log = fs::read_to_string(&laid.log).unwrap();
    assert_eq!(
        log.matches("cat >").count(),
        2,
        "do not retry forever: {log}"
    );
}

#[test]
fn an_override_path_skips_the_upload() {
    let scratch = Scratch::new();
    let override_path = scratch.root.join("custom-helper");
    copy_exec(
        Path::new(env!("CARGO_BIN_EXE_lamina-remote")),
        &override_path,
    );
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: Some(override_path.clone()),
        },
    );
    let installed = install(&laid, None).expect("override");
    assert_eq!(installed, override_path.display().to_string());
    let log = fs::read_to_string(&laid.log).unwrap();
    assert!(!log.contains("cat >"), "override must not upload: {log}");
}

#[test]
fn an_override_mismatch_does_not_delete_the_file() {
    let scratch = Scratch::new();
    let override_path = scratch.root.join("custom-helper");
    copy_exec(
        Path::new(env!("CARGO_BIN_EXE_lamina-remote")),
        &override_path,
    );
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Always,
            override_path: Some(override_path.clone()),
        },
    );
    let error = install(&laid, None).expect_err("mismatch");
    let text = error.to_string();
    assert!(text.contains("not-this-build"), "{text}");
    assert!(override_path.is_file(), "the user's helper was deleted");
    let log = fs::read_to_string(&laid.log).unwrap();
    assert!(!log.contains("<rm>"), "override must not be removed: {log}");
}

#[test]
fn an_unwritable_cache_is_a_clear_error() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
        },
    );
    host_helper(&laid.helpers);
    fs::create_dir_all(&laid.home).unwrap();
    fs::write(laid.home.join(".cache"), b"not a directory").unwrap();
    let error = install(&laid, None).expect_err("unwritable cache");
    let text = error.to_string();
    assert!(
        text.contains("could not install the remote helper"),
        "{text}"
    );
}

#[test]
fn a_missing_local_binary_names_the_file() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Linux",
            arch: "x86_64",
            lie: Lie::Never,
            override_path: None,
        },
    );
    let error = install(&laid, None).expect_err("missing binary");
    let text = error.to_string();
    assert!(
        text.contains("lamina-remote-x86_64-unknown-linux-musl"),
        "{text}"
    );
    assert!(text.contains("LAMINA_REMOTE_HELPER_DIR"), "{text}");
}

#[test]
fn an_unsupported_remote_is_refused() {
    let scratch = Scratch::new();
    let laid = layout(
        &scratch,
        &Fake {
            os: "Darwin",
            arch: "arm64",
            lie: Lie::Never,
            override_path: None,
        },
    );
    let error = install(&laid, None).expect_err("darwin");
    let text = error.to_string();
    assert!(text.contains("not available"), "{text}");
    assert!(text.contains("Darwin"), "{text}");
}
