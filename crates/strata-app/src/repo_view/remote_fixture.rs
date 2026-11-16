//! A stand-in for `ssh` so a test can connect a remote tab.
//!
//! The script records its argv, then replaces itself with the remote command,
//! which is enough for the install, the handshake and the protocol stream: the
//! helper runs on this machine and reads a repository from a temporary
//! directory. Unix only, because it is a shell script with a mode bit.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) struct LaidOut {
    pub ssh: PathBuf,
    pub helpers: PathBuf,
    pub log: PathBuf,
    pub pid: PathBuf,
}

/// A directory for one test's fake tooling. Removed by the caller.
pub(crate) fn scratch_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("scratch dir");
    path
}

pub(crate) fn layout_fake_ssh(scratch: &Path) -> LaidOut {
    let bin = scratch.join("bin");
    let home = scratch.join("home");
    let helpers = scratch.join("helpers");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&helpers).unwrap();
    let log = scratch.join("ssh.log");
    let pid = scratch.join("session.pid");
    let uname = "#!/bin/sh\nif [ \"$1\" = \"-s\" ]; then\n  echo Linux\n  exit 0\nfi\nif [ \"$1\" = \"-m\" ]; then\n  echo x86_64\n  exit 0\nfi\necho unknown\nexit 0\n";
    write_exec(&bin.join("uname"), uname);
    let script = format!(
        r#"#!/bin/sh
export HOME={home}
unset XDG_CACHE_HOME
export PATH={bin}:$PATH
LOG={log}
{{
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
if [ -z "$cmd" ]; then
  echo "fake ssh: no remote command" >&2
  exit 2
fi
case "$cmd" in
  *--stdio*)
    # A background command would have its stdin pointed at /dev/null, and the
    # helper would exit before the handshake. Replace this process instead, so
    # the recorded pid is the helper and killing it closes the pipes.
    echo $$ > {pid}
    # The arguments are already shell-quoted by the client. Splitting them
    # here is only safe because this test's paths have no spaces.
    set -- $cmd
    exec "$@"
    ;;
esac
exec sh -c "$cmd"
"#,
        home = sh_quote(&home.display().to_string()),
        bin = sh_quote(&bin.display().to_string()),
        log = sh_quote(&log.display().to_string()),
        pid = sh_quote(&pid.display().to_string()),
    );
    let ssh = bin.join("ssh");
    write_exec(&ssh, &script);
    LaidOut {
        ssh,
        helpers,
        log,
        pid,
    }
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_exec(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// The workspace build places `strata-remote` next to this test binary's
/// `deps` directory. A package test can start before that binary is linked.
pub(crate) fn wait_for_helper() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let mut dir = exe.parent().expect("binary directory").to_path_buf();
    if dir.file_name().is_some_and(|name| name == "deps") {
        dir.pop();
    }
    let helper = dir.join("strata-remote");
    let deadline = Instant::now() + Duration::from_secs(180);
    while !helper.is_file() {
        assert!(
            Instant::now() <= deadline,
            "strata-remote was not built at {}",
            helper.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    helper
}

pub(crate) fn install_host_helper(source: &Path, dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let dest = dir.join("strata-remote-x86_64-unknown-linux-musl");
    std::fs::copy(source, &dest).expect("copy helper");
    let mut perms = std::fs::metadata(&dest).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dest, perms).unwrap();
}
