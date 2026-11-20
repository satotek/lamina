//! Writes to git configuration files, through the `git` executable.
//!
//! Every change is previewed first: the command runs against a copy of the
//! target file, and the viewer shows the copy's new text beside the original.
//! Saving runs the same command against the real file, but only while the file
//! still has the content and modification time the preview recorded. A file
//! that changed in between is refused instead of overwritten, so the editor
//! never discards a concurrent edit.
//!
//! The system configuration is readable but never writable: changing it needs
//! administrator rights, and this process never elevates.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::diff::{DiffOptions, FileDiff, diff_text};
use crate::repo::{ConfigLayer, validate_key_name, validate_value};
use crate::{Error, Result};

use super::{Git, command_error};

/// Display context lines in the preview diff.
const PREVIEW_CONTEXT: u32 = 3;

/// A file a configuration value is written to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigTarget {
    pub path: PathBuf,
    pub layer: ConfigLayer,
}

impl ConfigTarget {
    pub fn new(path: impl Into<PathBuf>, layer: ConfigLayer) -> Self {
        Self {
            path: path.into(),
            layer,
        }
    }

    /// Whether this layer names a file this process may change.
    pub fn writable(&self) -> bool {
        matches!(
            self.layer,
            ConfigLayer::Xdg | ConfigLayer::Global | ConfigLayer::Local | ConfigLayer::Worktree
        )
    }

    /// Refuse the layers that are not a writable file, with the reason.
    pub fn check_writable(&self) -> Result<()> {
        match self.layer {
            ConfigLayer::System => Err(Error::other(format!(
                "{} needs administrator rights; choose another file",
                self.path.display()
            ))),
            ConfigLayer::Environment | ConfigLayer::CommandLine => Err(Error::other(
                "values from the environment or the command line have no file to edit",
            )),
            ConfigLayer::Xdg | ConfigLayer::Global | ConfigLayer::Local | ConfigLayer::Worktree => {
                Ok(())
            }
        }
    }
}

/// What a write does to the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigChange {
    /// Replace every value of the key with one value.
    Set(String),
    /// Append a value and keep the ones already there.
    Add(String),
    /// Remove the key and every value it has.
    Unset,
    /// Remove one value and keep the others.
    UnsetValue(String),
}

impl ConfigChange {
    /// The value the change writes, if it writes one.
    pub fn value(&self) -> Option<&str> {
        match self {
            Self::Set(value) | Self::Add(value) | Self::UnsetValue(value) => Some(value),
            Self::Unset => None,
        }
    }

    /// A short verb for the dialog's title and the status line.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Set(_) => "Set",
            Self::Add(_) => "Add",
            Self::Unset => "Unset",
            Self::UnsetValue(_) => "Remove value",
        }
    }
}

/// One checked change to one configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWrite {
    pub target: ConfigTarget,
    pub key: String,
    pub change: ConfigChange,
}

impl ConfigWrite {
    pub fn new(target: ConfigTarget, key: impl Into<String>, change: ConfigChange) -> Self {
        Self {
            target,
            key: key.into(),
            change,
        }
    }

    /// Everything that can be refused before git runs.
    pub fn validate(&self) -> Result<()> {
        self.target.check_writable()?;
        if self.target.path.as_os_str().is_empty() {
            return Err(Error::other("no target file was chosen"));
        }
        validate_key_name(&self.key)?;
        if let Some(value) = self.change.value() {
            validate_value(&self.key, value)?;
        }
        Ok(())
    }
}

/// The preview of a write: the target, the command to run and both texts.
#[derive(Debug, Clone)]
pub struct ConfigPreview {
    pub write: ConfigWrite,
    /// The command line the save runs, quoted for display.
    pub command: String,
    pub before: String,
    pub after: String,
    /// Line diff of `before` against `after`, for the viewer's renderer.
    pub diff: FileDiff,
    stamp: FileStamp,
}

impl ConfigPreview {
    /// True when the write changes the file at all.
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

/// What the preview saw, so a save can prove the file did not move under it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    hash: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Result<Self> {
        let (modified, contents) = read_config(path)?;
        Ok(Self::from_bytes(modified, &contents))
    }

    fn from_bytes(modified: Option<SystemTime>, contents: &[u8]) -> Self {
        let mut hasher = DefaultHasher::new();
        contents.hash(&mut hasher);
        Self {
            modified,
            len: contents.len() as u64,
            hash: hasher.finish(),
        }
    }
}

/// The file's modification time and contents. A missing file is an empty one,
/// because `git config --file` creates it.
fn read_config(path: &Path) -> Result<(Option<SystemTime>, Vec<u8>)> {
    match fs::read(path) {
        Ok(contents) => {
            let modified = fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok());
            Ok((modified, contents))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok((None, Vec::new())),
        Err(error) => Err(error.into()),
    }
}

impl Git {
    /// Run `write` against a temporary copy and return both texts and the
    /// command the save would run.
    pub fn config_preview(&self, write: &ConfigWrite) -> Result<ConfigPreview> {
        write.validate()?;
        let target = &write.target.path;
        let (modified, before) = read_config(target)?;
        let stamp = FileStamp::from_bytes(modified, &before);
        let copy = TempConfigFile::new(&before)?;
        let args = config_args(copy.path(), write);
        self.run_config(&args, write)?;
        let after = fs::read(copy.path())?;
        let before_text = String::from_utf8_lossy(&before).into_owned();
        let after_text = String::from_utf8_lossy(&after).into_owned();
        let diff = diff_text(
            &before,
            &after,
            &DiffOptions {
                context_lines: PREVIEW_CONTEXT,
                ignore_whitespace: false,
            },
        );
        let display_args = config_args(target, write);
        Ok(ConfigPreview {
            write: write.clone(),
            command: display_command(&display_args),
            before: before_text,
            after: after_text,
            diff,
            stamp,
        })
    }

    /// Write a previewed change, unless its file changed since the preview.
    pub fn config_apply(&self, preview: &ConfigPreview) -> Result<()> {
        let write = &preview.write;
        write.validate()?;
        if FileStamp::of(&write.target.path)? != preview.stamp {
            return Err(Error::other(format!(
                "{} changed since the preview; preview the write again",
                write.target.path.display()
            )));
        }
        let args = config_args(&write.target.path, write);
        self.run_config(&args, write)
    }

    /// Run one configuration command, treating "key not found" on an unset as
    /// success: the wanted state is already there.
    fn run_config(&self, args: &[String], write: &ConfigWrite) -> Result<()> {
        let output = self.run_output(args, None, false)?;
        if output.status.success() {
            return Ok(());
        }
        let unset = matches!(
            write.change,
            ConfigChange::Unset | ConfigChange::UnsetValue(_)
        );
        if unset && output.status.code() == Some(5) {
            return Ok(());
        }
        Err(command_error(args, &output))
    }
}

/// The `git config --file <file> …` arguments for `write`.
fn config_args(file: &Path, write: &ConfigWrite) -> Vec<String> {
    let mut args = vec![
        "config".to_owned(),
        "--file".to_owned(),
        file.to_string_lossy().into_owned(),
    ];
    match &write.change {
        // A plain set refuses a key that already has several values, so an edit
        // has to say it replaces all of them.
        ConfigChange::Set(value) => {
            args.push("--replace-all".to_owned());
            args.push(write.key.clone());
            args.push(value.clone());
        }
        ConfigChange::Add(value) => {
            args.push("--add".to_owned());
            args.push(write.key.clone());
            args.push(value.clone());
        }
        ConfigChange::Unset => {
            args.push("--unset-all".to_owned());
            args.push(write.key.clone());
        }
        // `--unset` takes the value as a regular expression, so the value is
        // escaped and anchored to match only itself.
        ConfigChange::UnsetValue(value) => {
            args.push("--unset".to_owned());
            args.push(write.key.clone());
            args.push(unset_pattern(value));
        }
    }
    args
}

/// Quote arguments for the status line and the preview. git is started with an
/// argument vector, never a shell; this is only how the command is shown.
fn display_command(args: &[String]) -> String {
    let mut line = String::from("git");
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+".contains(c))
    {
        return arg.to_owned();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("\\$"),
            '`' => quoted.push_str("\\`"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Turn a value into the POSIX regular expression `git config --unset` takes,
/// anchored so it can only match that value and not another one containing it.
///
/// git passes the pattern to the platform's POSIX matcher, so the escaping is
/// the POSIX one: only the ERE metacharacters and the backslash itself get a
/// backslash, and newlines stay literal.
fn unset_pattern(value: &str) -> String {
    let mut pattern = String::with_capacity(value.len() + 2);
    pattern.push('^');
    for c in value.chars() {
        if "\\.^$[]*+?(){}|".contains(c) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('$');
    pattern
}

/// A copy of one configuration file, removed when the preview is done with it.
struct TempConfigFile {
    path: PathBuf,
}

static PREVIEW_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempConfigFile {
    fn new(contents: &[u8]) -> Result<Self> {
        let directory = std::env::temp_dir();
        for _ in 0..64 {
            let count = PREVIEW_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                "lamina-config-{}-{count}.preview",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            // A configuration file can name credentials or private keys; the
            // copy must not be readable while it exists.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    file.write_all(contents)?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::other(
            "could not create a temporary file for the configuration preview",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::repo::GitConfig;
    use crate::test_support::{TestRepo, config_path};

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("config directory");
        }
        fs::write(path, contents).expect("write config");
    }

    /// A git handle that only sees the fixture: its own global file, no system
    /// configuration and a temporary `HOME`.
    fn isolated(repo: &TestRepo, home: &Path) -> Git {
        repo.git_handle()
            .with_env("HOME", home)
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    /// Values as git reports them, split on NUL so a value may contain
    /// newlines.
    fn value_in(path: &Path, key: &str) -> Vec<String> {
        let output = std::process::Command::new("git")
            .args(["config", "--file", path.to_str().expect("utf-8 path")])
            .args(["--get-all", "--null", key])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("run git");
        String::from_utf8_lossy(&output.stdout)
            .split_terminator('\0')
            .map(str::to_owned)
            .collect()
    }

    fn preview_and_apply(
        git: &Git,
        file: &Path,
        layer: ConfigLayer,
        key: &str,
        change: ConfigChange,
    ) -> ConfigPreview {
        let write = ConfigWrite::new(ConfigTarget::new(file, layer), key, change);
        let preview = git.config_preview(&write).expect("preview");
        git.config_apply(&preview).expect("apply");
        assert_eq!(
            read(file),
            preview.after,
            "the real file must match the preview"
        );
        preview
    }

    #[test]
    fn a_local_value_sets_changes_and_unsets() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "user.name",
            ConfigChange::Set("Local Name".into()),
        );
        assert_eq!(value_in(&local, "user.name"), ["Local Name"]);

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "user.name",
            ConfigChange::Set("Renamed".into()),
        );
        assert_eq!(value_in(&local, "user.name"), ["Renamed"]);

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "user.name",
            ConfigChange::Unset,
        );
        assert!(value_in(&local, "user.name").is_empty());
    }

    #[test]
    fn a_global_file_gets_the_same_handling() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let global = home.path().join("gitconfig");
        write(&global, "[core]\n\teditor = vim\n");
        let git = isolated(&repo, home.path());

        let preview = preview_and_apply(
            &git,
            &global,
            ConfigLayer::Global,
            "user.email",
            ConfigChange::Set("global@example.invalid".into()),
        );
        assert_eq!(value_in(&global, "user.email"), ["global@example.invalid"]);
        assert!(preview.command.contains("--file"));
        assert_eq!(value_in(&global, "core.editor"), ["vim"]);

        preview_and_apply(
            &git,
            &global,
            ConfigLayer::Global,
            "user.email",
            ConfigChange::Unset,
        );
        assert!(value_in(&global, "user.email").is_empty());
        assert_eq!(value_in(&global, "core.editor"), ["vim"]);
    }

    #[test]
    fn an_include_file_is_written_without_touching_the_including_file() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let global = home.path().join("gitconfig");
        let include = home.path().join("work.gitconfig");
        write(
            &global,
            &format!("[include]\n\tpath = {}\n", config_path(&include)),
        );
        write(&include, "[user]\n\tname = Included\n");
        let git = isolated(&repo, home.path());

        preview_and_apply(
            &git,
            &include,
            ConfigLayer::Global,
            "user.name",
            ConfigChange::Set("Changed".into()),
        );
        assert_eq!(value_in(&include, "user.name"), ["Changed"]);
        assert!(value_in(&global, "user.name").is_empty());

        // The viewer's read side must agree with what git wrote.
        let config = GitConfig::read(
            &repo.open(),
            &crate::repo::ConfigSources {
                global: Some(global.clone()),
                home: Some(home.path().to_path_buf()),
                ..Default::default()
            },
        )
        .expect("read config");
        assert_eq!(
            config
                .entry("user.name")
                .and_then(|entry| entry.effective())
                .map(|value| value.value.as_str()),
            Some("Changed")
        );

        preview_and_apply(
            &git,
            &include,
            ConfigLayer::Global,
            "user.name",
            ConfigChange::Unset,
        );
        assert!(value_in(&include, "user.name").is_empty());
        assert!(value_in(&global, "user.name").is_empty());
    }

    #[test]
    fn add_and_remove_one_of_several_values_keep_the_others() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");
        repo.git(&[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ]);

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "remote.origin.fetch",
            ConfigChange::Add("+refs/tags/*:refs/tags/*".into()),
        );
        assert_eq!(
            value_in(&local, "remote.origin.fetch"),
            [
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*"
            ]
        );

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "remote.origin.fetch",
            ConfigChange::UnsetValue("+refs/tags/*:refs/tags/*".into()),
        );
        assert_eq!(
            value_in(&local, "remote.origin.fetch"),
            ["+refs/heads/*:refs/remotes/origin/*"]
        );
    }

    #[test]
    fn removing_a_value_never_removes_another_one_that_contains_it() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");
        for value in ["abc", "xabc", "abc2"] {
            repo.git(&["config", "--add", "lamina.list", value]);
        }

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "lamina.list",
            ConfigChange::UnsetValue("abc".into()),
        );
        assert_eq!(value_in(&local, "lamina.list"), ["xabc", "abc2"]);
    }

    #[test]
    fn editing_a_multi_valued_key_replaces_every_value() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");
        repo.git(&["config", "--add", "credential.helper", "store"]);
        repo.git(&["config", "--add", "credential.helper", "cache"]);

        preview_and_apply(
            &git,
            &local,
            ConfigLayer::Local,
            "credential.helper",
            ConfigChange::Set("libsecret".into()),
        );
        assert_eq!(value_in(&local, "credential.helper"), ["libsecret"]);
    }

    #[test]
    fn a_stale_file_is_refused_and_left_alone() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");
        repo.git(&["config", "user.name", "Before"]);

        let write = ConfigWrite::new(
            ConfigTarget::new(&local, ConfigLayer::Local),
            "user.name",
            ConfigChange::Set("After".into()),
        );
        let preview = git.config_preview(&write).expect("preview");

        repo.git(&["config", "user.name", "Someone Else"]);
        let before = read(&local);
        let error = git
            .config_apply(&preview)
            .expect_err("a changed file must be refused");
        assert!(error.to_string().contains("changed since the preview"));
        assert_eq!(read(&local), before, "the refused write must not touch it");
        assert_eq!(value_in(&local, "user.name"), ["Someone Else"]);
    }

    #[test]
    fn the_system_layer_is_rejected_before_any_command_runs() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let system = home.path().join("system.gitconfig");
        write(&system, "[user]\n\tname = System\n");

        let write = ConfigWrite::new(
            ConfigTarget::new(&system, ConfigLayer::System),
            "user.name",
            ConfigChange::Set("Changed".into()),
        );
        let error = git.config_preview(&write).expect_err("system is read-only");
        assert!(error.to_string().contains("administrator rights"));
        assert_eq!(read(&system), "[user]\n\tname = System\n");
        assert!(git.log().is_empty(), "no git command may have run");
    }

    #[test]
    fn invalid_keys_and_curated_values_are_rejected() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");

        for key in [
            "",
            "user",
            "user.",
            ".name",
            "user..name",
            "2user.name",
            "bad key.name",
        ] {
            let write = ConfigWrite::new(
                ConfigTarget::new(&local, ConfigLayer::Local),
                key,
                ConfigChange::Set("value".into()),
            );
            assert!(write.validate().is_err(), "`{key}` must be refused");
            assert!(git.config_preview(&write).is_err());
        }

        let write = ConfigWrite::new(
            ConfigTarget::new(&local, ConfigLayer::Local),
            "pull.rebase",
            ConfigChange::Set("ture".into()),
        );
        let error = write.validate().expect_err("misspelt enum");
        assert!(error.to_string().contains("pull.rebase"), "{error}");
        assert!(
            ConfigWrite::new(
                ConfigTarget::new(&local, ConfigLayer::Local),
                "pull.rebase",
                ConfigChange::Set("merges".into()),
            )
            .validate()
            .is_ok()
        );
        assert!(
            ConfigWrite::new(
                ConfigTarget::new(&local, ConfigLayer::Local),
                "custom.key",
                ConfigChange::Set("anything at all".into()),
            )
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn awkward_values_round_trip_as_arguments_not_through_a_shell() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let local = repo.path().join(".git/config");
        let values = [
            "with spaces",
            "quotes \"inside\" and 'outside'",
            "a # b ; c",
            "back\\slash",
            "line\nbreak",
            "  leading and trailing  ",
        ];

        for (index, value) in values.iter().enumerate() {
            let key = format!("lamina.test{index}");
            let preview = preview_and_apply(
                &git,
                &local,
                ConfigLayer::Local,
                &key,
                ConfigChange::Set((*value).into()),
            );
            assert_eq!(value_in(&local, &key), [*value]);
            assert_eq!(
                preview.after,
                read(&local),
                "the preview matches the file on disk"
            );

            // Unsetting by value matches the literal, not a pattern.
            preview_and_apply(
                &git,
                &local,
                ConfigLayer::Local,
                &key,
                ConfigChange::UnsetValue((*value).into()),
            );
            assert!(value_in(&local, &key).is_empty());
        }
    }

    #[test]
    fn a_new_file_is_created_by_the_preview_too() {
        let repo = TestRepo::new();
        let home = tempfile::TempDir::new().expect("home");
        let git = isolated(&repo, home.path());
        let include = home.path().join("new.gitconfig");
        assert!(!include.exists());

        let write = ConfigWrite::new(
            ConfigTarget::new(&include, ConfigLayer::Global),
            "user.email",
            ConfigChange::Set("new@example.invalid".into()),
        );
        let preview = git.config_preview(&write).expect("preview");
        assert!(preview.changed());
        assert!(preview.before.is_empty());
        git.config_apply(&preview).expect("apply");
        assert_eq!(read(&include), preview.after);
        assert_eq!(value_in(&include, "user.email"), ["new@example.invalid"]);
    }

    #[test]
    fn the_command_line_is_quoted_for_display() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a\nb"), "\"a\\nb\"");
    }
}
