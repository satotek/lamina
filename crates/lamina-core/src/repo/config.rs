//! The git configuration the open repository sees, and the rules a change to
//! it has to satisfy.
//!
//! Values are read with `gix-config`, which parses the same files git does.
//! The parser keeps source paths and line numbers, so a value can be traced to
//! the file and line that set it. Includes are followed here rather than by
//! `gix`'s own resolver: the viewer has to show conditional includes that did
//! *not* match as well, and only this module knows which condition selected
//! which file.
//!
//! Nothing in this module writes a configuration file. Writes live in
//! `git_cli::config`; they call [`validate_key_name`] and [`validate_value`]
//! here so the viewer and the write path agree on what is allowed.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use gix::bstr::{BStr, ByteSlice};

use crate::{Error, Result};

use super::Repo;

/// The precedence layers, lowest first. The order is also the order the view
/// lists them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigLayer {
    System,
    Xdg,
    Global,
    Local,
    Worktree,
    Environment,
    CommandLine,
}

impl ConfigLayer {
    /// Short label for a badge.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Xdg => "XDG",
            Self::Global => "global",
            Self::Local => "local",
            Self::Worktree => "worktree",
            Self::Environment => "environment",
            Self::CommandLine => "command line",
        }
    }
}

/// Where one value came from: the file and line, and the include that pulled
/// the file in when it was not read directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOrigin {
    pub path: Option<PathBuf>,
    /// One-based line number of the key, when the value came from a file.
    pub line: Option<usize>,
    /// Index into [`GitConfig::includes`] for a file reached through an
    /// include directive.
    pub include: Option<usize>,
}

impl ConfigOrigin {
    /// `path:line`, or `(environment)` for values with no file.
    pub fn label(&self) -> String {
        match (&self.path, self.line) {
            (Some(path), Some(line)) => format!("{}:{line}", path.display()),
            (Some(path), None) => path.display().to_string(),
            (None, _) => "(environment)".to_owned(),
        }
    }
}

/// One value of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValue {
    pub value: String,
    pub layer: ConfigLayer,
    pub origin: ConfigOrigin,
}

/// A key with its effective values and the values it overrode.
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    /// Canonical dotted key, e.g. `remote.origin.fetch`.
    pub key: String,
    /// Effective values, in file order. A single-valued key has at most one.
    /// A multi-valued key keeps every value in order.
    pub values: Vec<ConfigValue>,
    /// Earlier values of a single-valued key, in file order.
    pub overridden: Vec<ConfigValue>,
    /// Whether git keeps every value of this key instead of letting the last
    /// one win. The editor uses it to offer "Add value" even when only one
    /// value is set today.
    pub multivalued: bool,
    /// The curated entry this key matches, if any.
    pub well_known: Option<&'static WellKnownKey>,
    /// Set when an effective value is not allowed by the curated entry.
    pub warning: Option<String>,
}

impl ConfigEntry {
    /// The value the UI shows in its collapsed row.
    pub fn effective(&self) -> Option<&ConfigValue> {
        self.values.last()
    }
}

/// `user.name` and `user.email` together, with the file that set each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigIdentity {
    pub name: Option<ConfigValue>,
    pub email: Option<ConfigValue>,
}

/// The includeIf conditions the viewer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeConditionKind {
    /// A plain `include.path`.
    Always,
    GitDir,
    OnBranch,
    HasConfigRemoteUrl,
    /// A condition this build does not evaluate, such as `hasconfig` with a
    /// key other than `remote.*.url`.
    Unsupported,
}

/// One include directive, whether or not its condition matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigInclude {
    /// The directive as written: `include.path` or the `includeIf` subsection.
    pub condition: String,
    pub condition_kind: IncludeConditionKind,
    /// The path after `~` and relative-path expansion, when it could be
    /// resolved.
    pub path: Option<PathBuf>,
    pub layer: ConfigLayer,
    /// File and line of the include directive itself.
    pub origin: ConfigOrigin,
    /// Whether the condition matched for this repository. A missing file can
    /// match and still load nothing; see [`Self::error`].
    pub matched: bool,
    /// Number of values loaded from this file, including nested includes.
    pub values: usize,
    /// Why the file could not be loaded, when it matched but did not.
    pub error: Option<String>,
}

/// The configuration files resolved for this repository.
///
/// [`Self::worktree`] is only present when `extensions.worktreeConfig` is on;
/// without it git ignores `config.worktree` and a write there would silently
/// do nothing. [`Self::system`] is read-only for this process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFilePaths {
    pub local: PathBuf,
    pub worktree: Option<PathBuf>,
    /// The user's global file. `None` when the environment overrode the
    /// sources, as in tests.
    pub global: Option<PathBuf>,
    /// The system file. Shown by the viewer, never written: it needs
    /// administrator rights.
    pub system: Option<PathBuf>,
}

/// The whole configuration the repository sees.
#[derive(Debug, Clone, Default)]
pub struct GitConfig {
    /// Sorted by key. Includes are not entries: they are [`Self::includes`].
    pub entries: Vec<ConfigEntry>,
    pub includes: Vec<ConfigInclude>,
    pub identity: ConfigIdentity,
    /// Where the values came from. The editor may write local, worktree,
    /// global and include files; the system file is listed but read-only.
    pub files: ConfigFilePaths,
}

impl GitConfig {
    /// Read the configuration for the repository containing `path`.
    ///
    /// Paths, environment and the `GIT_CONFIG_*` overrides come from this
    /// process, exactly as they would for a git command started beside it.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let repo = Repo::open(path)?;
        Self::read(&repo, &ConfigSources::from_env())
    }

    /// Read the configuration with explicit base sources. Tests use this so
    /// they never read the machine's real configuration.
    pub fn read(repo: &Repo, sources: &ConfigSources) -> Result<Self> {
        let local = repo.local();
        let git_dir = local.git_dir().to_path_buf();
        let common_dir = local.common_dir().to_path_buf();
        let branch = repo.head().ok().and_then(|head| match head {
            crate::HeadInfo::Branch { short_name, .. } => Some(short_name),
            crate::HeadInfo::Detached { .. } => None,
        });
        let context = Context {
            git_dir: git_dir.clone(),
            branch,
            home: sources.home.clone(),
        };

        let mut reader = Reader {
            context: &context,
            assignments: Vec::new(),
            includes: Vec::new(),
        };

        let mut seen = HashSet::new();
        let base = [
            (sources.system.clone(), ConfigLayer::System),
            (sources.xdg.clone(), ConfigLayer::Xdg),
            (sources.global.clone(), ConfigLayer::Global),
            (Some(common_dir.join("config")), ConfigLayer::Local),
        ];
        for (path, layer) in base {
            let Some(path) = path else { continue };
            if !seen.insert(path.clone()) {
                continue;
            }
            if path.is_file() {
                reader.load_file(&path, layer, None, 0)?;
            }
        }

        let worktree = git_dir.join("config.worktree");
        // Only the repository's own configuration can turn the extension on;
        // git ignores it anywhere else.
        let worktree_enabled = reader
            .assignments
            .iter()
            .rev()
            .find(|assignment| assignment.key == "extensions.worktreeconfig")
            .is_some_and(|assignment| is_git_true(&assignment.value));
        if worktree_enabled && worktree.is_file() && seen.insert(worktree.clone()) {
            reader.load_file(&worktree, ConfigLayer::Worktree, None, 0)?;
        }

        for (key, value) in &sources.environment {
            reader.assignments.push(Assignment {
                key: canonical_key(key),
                value: value.clone(),
                layer: ConfigLayer::Environment,
                origin: ConfigOrigin::default(),
            });
        }

        let files = ConfigFilePaths {
            local: common_dir.join("config"),
            worktree: worktree_enabled.then_some(worktree),
            global: sources.global.clone(),
            system: sources.system.clone(),
        };

        Ok(finish(reader.assignments, reader.includes, files))
    }

    /// The effective value of `key`, if the repository sets it.
    pub fn entry(&self, key: &str) -> Option<&ConfigEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// The identity value a repository-local override would clarify, if any.
    ///
    /// An email inherited through an `includeIf gitdir:…` block belongs to the
    /// directory pattern, not necessarily to this repository. The viewer
    /// offers to set it locally instead of leaving the user to guess which file
    /// wins. A plain global email is not reported: that is the normal setup and
    /// a local copy of it would only duplicate the value.
    pub fn identity_needing_local_override(&self) -> Option<&ConfigValue> {
        let email = self.identity.email.as_ref()?;
        email.origin.include.is_some().then_some(email)
    }

    /// Files the editor may write: local, worktree, global, XDG and includes.
    ///
    /// The system file is absent on purpose. A caller that was handed a path
    /// checks it against this list instead of trusting the path.
    pub fn writable_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut push = |path: &Path| {
            if !files.iter().any(|existing| existing == path) {
                files.push(path.to_path_buf());
            }
        };
        push(&self.files.local);
        if let Some(path) = &self.files.worktree {
            push(path);
        }
        if let Some(path) = &self.files.global {
            push(path);
        }
        for include in &self.includes {
            if include.layer != ConfigLayer::System
                && let Some(path) = &include.path
            {
                push(path);
            }
        }
        for entry in &self.entries {
            for value in entry.values.iter().chain(entry.overridden.iter()) {
                if value.layer == ConfigLayer::Xdg
                    && let Some(path) = &value.origin.path
                {
                    push(path);
                }
            }
        }
        files
    }
}

/// Refuse `file` unless it is absolute, has no `.` or `..`, and is one of the
/// files [`GitConfig::writable_files`] names for `repo`.
///
/// The check runs before `git config --file`, which would otherwise create or
/// edit whatever path it was given.
pub fn require_writable_config_file(repo: &Path, file: &Path) -> Result<()> {
    let Some(text) = file.to_str() else {
        return Err(Error::other("path is not unicode"));
    };
    if text.is_empty() {
        return Err(Error::other("path is empty"));
    }
    if text.chars().any(|character| character.is_control()) {
        return Err(Error::other("path contains a control character"));
    }
    if !file.is_absolute() {
        return Err(Error::other(format!(
            "config path must be absolute: {text}"
        )));
    }
    if file
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::other(format!(
            "config path must not contain . or ..: {text}"
        )));
    }
    let config = GitConfig::load(repo)?;
    if config
        .writable_files()
        .iter()
        .any(|allowed| allowed == file)
    {
        Ok(())
    } else {
        Err(Error::other(format!(
            "config file is not one this repository writes: {text}"
        )))
    }
}

impl Repo {
    /// Read the repository's configuration without touching any file.
    pub fn git_config(&self, sources: &ConfigSources) -> Result<GitConfig> {
        GitConfig::read(self, sources)
    }
}

/// Explicit base paths for [`GitConfig::read`].
///
/// [`ConfigSources::from_env`] fills them the way git does; tests fill them
/// with temporary files so no machine configuration leaks in.
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    pub system: Option<PathBuf>,
    pub xdg: Option<PathBuf>,
    pub global: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_N` and `GIT_CONFIG_VALUE_N`.
    pub environment: Vec<(String, String)>,
}

impl ConfigSources {
    /// The sources a git command started in this process would read.
    pub fn from_env() -> Self {
        let mut env = |name: &str| std::env::var_os(name);
        let system = gix::config::Source::System.storage_location(&mut env);
        let global_override = std::env::var_os("GIT_CONFIG_GLOBAL").map(PathBuf::from);
        let (xdg, global) = match global_override {
            Some(path) => (None, Some(path)),
            None => (
                gix::config::Source::Git.storage_location(&mut env),
                gix::config::Source::User.storage_location(&mut env),
            ),
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);

        let count = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut environment = Vec::new();
        for index in 0..count {
            let key = std::env::var(format!("GIT_CONFIG_KEY_{index}"));
            let value = std::env::var(format!("GIT_CONFIG_VALUE_{index}"));
            if let (Ok(key), Ok(value)) = (key, value) {
                environment.push((key, value));
            }
        }

        Self {
            system,
            xdg,
            global,
            home,
            environment,
        }
    }
}

/// A curated key, its one-line explanation and the values it accepts.
#[derive(Debug, Clone, Copy)]
pub struct WellKnownKey {
    pub key: &'static str,
    pub summary: &'static str,
    pub rule: ValueRule,
}

/// What a curated key accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRule {
    /// Any string, such as a command or a signing key.
    Text,
    /// A git boolean: true/false, yes/no, on/off, 1/0.
    Boolean,
    /// One of the listed words.
    Enum(&'static [&'static str]),
    /// A git boolean or one of the listed words.
    BooleanOrEnum(&'static [&'static str]),
}

impl ValueRule {
    /// True when `value` is one of the accepted spellings.
    pub fn accepts(self, value: &str) -> bool {
        match self {
            Self::Text => true,
            Self::Boolean => is_git_bool(value),
            Self::Enum(allowed) => allowed
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(value)),
            Self::BooleanOrEnum(allowed) => {
                is_git_bool(value)
                    || allowed
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(value))
            }
        }
    }

    /// The allowed values as one line, for the warning and the UI.
    pub fn allowed(self) -> String {
        match self {
            Self::Text => "any text".to_owned(),
            Self::Boolean => "true / false (also yes/no, on/off, 1/0)".to_owned(),
            Self::Enum(allowed) | Self::BooleanOrEnum(allowed) => {
                let mut values = if matches!(self, Self::BooleanOrEnum(_)) {
                    vec!["true", "false"]
                } else {
                    Vec::new()
                };
                values.extend_from_slice(allowed);
                values.join(" / ")
            }
        }
    }
}

/// The curated keys, in the order the phase-one viewer documents them.
pub const WELL_KNOWN_KEYS: &[WellKnownKey] = &[
    WellKnownKey {
        key: "user.name",
        summary: "The name recorded as the author and committer of new commits.",
        rule: ValueRule::Text,
    },
    WellKnownKey {
        key: "user.email",
        summary: "The email recorded as the author and committer of new commits.",
        rule: ValueRule::Text,
    },
    WellKnownKey {
        key: "user.signingkey",
        summary: "The key used to sign commits and tags.",
        rule: ValueRule::Text,
    },
    WellKnownKey {
        key: "commit.gpgSign",
        summary: "Sign every commit automatically.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "tag.gpgSign",
        summary: "Sign every tag automatically.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "gpg.format",
        summary: "The signing format: OpenPGP, SSH or X.509.",
        rule: ValueRule::Enum(&["openpgp", "ssh", "x509"]),
    },
    WellKnownKey {
        key: "pull.rebase",
        summary: "Whether `git pull` rebases instead of merging.",
        rule: ValueRule::BooleanOrEnum(&["merges", "interactive", "preserve"]),
    },
    WellKnownKey {
        key: "pull.ff",
        summary: "Whether `git pull` may create a merge commit.",
        rule: ValueRule::BooleanOrEnum(&["only"]),
    },
    WellKnownKey {
        key: "push.default",
        summary: "What `git push` pushes when no refspec is given.",
        rule: ValueRule::Enum(&["nothing", "current", "upstream", "simple", "matching"]),
    },
    WellKnownKey {
        key: "push.autoSetupRemote",
        summary: "Set an upstream automatically on the first push.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "fetch.prune",
        summary: "Remove remote-tracking branches that no longer exist.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "init.defaultBranch",
        summary: "The branch name a new repository starts on.",
        rule: ValueRule::Text,
    },
    WellKnownKey {
        key: "core.autocrlf",
        summary: "Convert line endings when checking files in and out.",
        rule: ValueRule::BooleanOrEnum(&["input"]),
    },
    WellKnownKey {
        key: "core.eol",
        summary: "The line endings used in the working tree.",
        rule: ValueRule::Enum(&["lf", "crlf", "native"]),
    },
    WellKnownKey {
        key: "core.editor",
        summary: "The editor git starts for commit messages.",
        rule: ValueRule::Text,
    },
    WellKnownKey {
        key: "core.pager",
        summary: "The pager git uses for long output.",
        rule: ValueRule::Text,
    },
    WellKnownKey {
        key: "core.fileMode",
        summary: "Whether the executable bit is tracked.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "core.ignoreCase",
        summary: "Whether path names are compared case-insensitively.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "merge.conflictStyle",
        summary: "How conflict markers in the working tree are written.",
        rule: ValueRule::Enum(&["merge", "diff3", "zdiff3"]),
    },
    WellKnownKey {
        key: "rebase.autoStash",
        summary: "Stash local changes before a rebase and restore them after.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "rerere.enabled",
        summary: "Reuse recorded conflict resolutions.",
        rule: ValueRule::Boolean,
    },
    WellKnownKey {
        key: "diff.algorithm",
        summary: "The diff algorithm git uses.",
        rule: ValueRule::Enum(&["myers", "minimal", "patience", "histogram"]),
    },
    WellKnownKey {
        key: "credential.helper",
        summary: "A program git asks for credentials.",
        rule: ValueRule::Text,
    },
];

/// Look up a curated key. Custom subsection keys never match.
pub fn well_known_key(key: &str) -> Option<&'static WellKnownKey> {
    WELL_KNOWN_KEYS
        .iter()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
}

struct Context {
    git_dir: PathBuf,
    branch: Option<String>,
    home: Option<PathBuf>,
}

struct Assignment {
    key: String,
    value: String,
    layer: ConfigLayer,
    origin: ConfigOrigin,
}

struct Reader<'a> {
    context: &'a Context,
    assignments: Vec<Assignment>,
    includes: Vec<ConfigInclude>,
}

impl Reader<'_> {
    fn load_file(
        &mut self,
        path: &Path,
        layer: ConfigLayer,
        parent_include: Option<usize>,
        depth: usize,
    ) -> Result<usize> {
        if depth > 10 {
            return Err(Error::other(format!(
                "include depth exceeded at {}",
                path.display()
            )));
        }
        let bytes = std::fs::read(path)?;
        let events = gix::config::parse::Events::from_bytes(&bytes, None)
            .map_err(|error| Error::other(format!("cannot parse {}: {error}", path.display())))?;
        let items = parse_items(&events);
        let mut loaded = 0;

        for item in items {
            let origin = ConfigOrigin {
                path: Some(path.to_path_buf()),
                line: Some(item.line),
                include: parent_include,
            };

            if item.section.eq_ignore_ascii_case("include") && item.value_name == "path" {
                loaded += self.include(
                    "include.path".to_owned(),
                    IncludeConditionKind::Always,
                    include_path(&item.value, path, self.context.home.as_deref()),
                    item.value.clone(),
                    layer,
                    origin,
                    depth,
                )?;
                continue;
            }
            if item.section.eq_ignore_ascii_case("includeif") && item.value_name == "path" {
                let condition = item.subsection.clone().unwrap_or_default();
                let parsed = parse_condition(&condition);
                loaded += self.include(
                    condition,
                    parsed.kind,
                    include_path(&item.value, path, self.context.home.as_deref()),
                    item.value.clone(),
                    layer,
                    origin,
                    depth,
                )?;
                continue;
            }

            self.assignments.push(Assignment {
                key: item.key,
                value: item.value,
                layer,
                origin,
            });
            loaded += 1;
        }
        Ok(loaded)
    }

    #[allow(clippy::too_many_arguments)]
    fn include(
        &mut self,
        condition: String,
        kind: IncludeConditionKind,
        path: Option<PathBuf>,
        raw_path: String,
        layer: ConfigLayer,
        origin: ConfigOrigin,
        depth: usize,
    ) -> Result<usize> {
        let matched = match kind {
            IncludeConditionKind::Always => true,
            IncludeConditionKind::GitDir => self.gitdir_condition_matches(&condition, &origin),
            IncludeConditionKind::OnBranch => self.onbranch_condition_matches(&condition),
            IncludeConditionKind::HasConfigRemoteUrl => {
                self.hasconfig_condition_matches(&condition)
            }
            IncludeConditionKind::Unsupported => false,
        };

        // Reserve the slot before reading the file: values from it, and from
        // nested includes, point back at this include's index.
        let index = self.includes.len();
        self.includes.push(ConfigInclude {
            condition: if kind == IncludeConditionKind::Always {
                "include.path".to_owned()
            } else {
                condition
            },
            condition_kind: kind,
            path: path.clone(),
            layer,
            origin,
            matched,
            values: 0,
            error: None,
        });

        if !matched {
            return Ok(0);
        }

        let Some(path) = path else {
            self.includes[index].error = Some(format!("cannot resolve include path `{raw_path}`"));
            return Ok(0);
        };
        if !path.is_file() {
            self.includes[index].error = Some(format!("file does not exist: {}", path.display()));
            return Ok(0);
        }

        match self.load_file(&path, layer, Some(index), depth + 1) {
            Ok(loaded) => {
                self.includes[index].values = loaded;
                Ok(loaded)
            }
            Err(error) => {
                self.includes[index].error = Some(error.to_string());
                Ok(0)
            }
        }
    }

    fn gitdir_condition_matches(&self, condition: &str, origin: &ConfigOrigin) -> bool {
        let Some((pattern, ignore_case)) = condition
            .strip_prefix("gitdir/i:")
            .map(|rest| (rest, true))
            .or_else(|| condition.strip_prefix("gitdir:").map(|rest| (rest, false)))
        else {
            return false;
        };
        let config_path = origin.path.as_deref();
        let expanded =
            match expand_gitdir_pattern(pattern, config_path, self.context.home.as_deref()) {
                Some(path) => path,
                None => return false,
            };
        let mut pattern = slash_path(&expanded);
        if !pattern.starts_with('/') {
            pattern = format!("**/{pattern}");
        }
        // A trailing slash means "this directory and everything under it".
        // Match both spellings: the bare directory itself is a valid target,
        // which `dir/**` alone does not cover in every glob implementation.
        let directory = pattern.trim_end_matches('/').to_owned();
        if pattern.ends_with('/') {
            pattern.push_str("**");
        }

        let mut mode = gix::glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL;
        if ignore_case {
            mode |= gix::glob::wildmatch::Mode::IGNORE_CASE;
        }
        let git_dir = slash_path(&self.context.git_dir);
        let matches = |candidate: &str| {
            gix::glob::wildmatch(
                pattern.as_bytes().as_bstr(),
                candidate.as_bytes().as_bstr(),
                mode,
            ) || (!directory.is_empty()
                && gix::glob::wildmatch(
                    directory.as_bytes().as_bstr(),
                    candidate.as_bytes().as_bstr(),
                    mode,
                ))
        };
        if matches(&git_dir) {
            return true;
        }
        std::fs::canonicalize(&self.context.git_dir)
            .ok()
            .map(|path| slash_path(&path))
            .is_some_and(|path| matches(&path))
    }

    fn onbranch_condition_matches(&self, condition: &str) -> bool {
        let Some(branch) = self.context.branch.as_deref() else {
            return false;
        };
        let Some(pattern) = condition.strip_prefix("onbranch:") else {
            return false;
        };
        let mode = gix::glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL;
        let matches = |pattern: &str| {
            gix::glob::wildmatch(
                pattern.as_bytes().as_bstr(),
                branch.as_bytes().as_bstr(),
                mode,
            )
        };
        if pattern.ends_with('/') {
            matches(&format!("{pattern}**")) || matches(pattern.trim_end_matches('/'))
        } else {
            matches(pattern)
        }
    }

    fn hasconfig_condition_matches(&self, condition: &str) -> bool {
        let Some(rest) = condition.strip_prefix("hasconfig:") else {
            return false;
        };
        let Some((key, value)) = rest.split_once(':') else {
            return false;
        };
        if !key.eq_ignore_ascii_case("remote.*.url") {
            return false;
        }
        self.assignments.iter().any(|assignment| {
            assignment.key.starts_with("remote.")
                && assignment.key.ends_with(".url")
                && gix::glob::wildmatch(
                    value.as_bytes().as_bstr(),
                    assignment.value.as_bytes().as_bstr(),
                    gix::glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
                )
        })
    }
}

struct ParsedItem {
    section: String,
    subsection: Option<String>,
    /// The value name as written, e.g. `name` in `[user] name = …`.
    value_name: String,
    /// Canonical dotted key, e.g. `user.name`.
    key: String,
    value: String,
    line: usize,
}

/// Turn the parser events back into values with their line numbers.
///
/// `gix-config`'s file API deliberately does not expose offsets in its current
/// version, but the event stream covers every byte of the input, so counting
/// newlines while walking it recovers the key's line without a second parser.
fn parse_items(events: &gix::config::parse::Events) -> Vec<ParsedItem> {
    let mut items = Vec::new();
    let mut section = String::new();
    let mut subsection = None;
    let mut line = 1usize;
    let mut pending: Option<Pending> = None;

    for event in events.iter() {
        match event {
            gix::config::parse::EventRef::SectionHeader {
                name,
                subsection_name,
                ..
            } => {
                flush_pending(&mut pending, &mut items);
                section = name.to_str_lossy().into_owned();
                subsection = subsection_name.map(|name| name.to_str_lossy().into_owned());
            }
            gix::config::parse::EventRef::SectionValueName(name) => {
                flush_pending(&mut pending, &mut items);
                pending = Some(Pending {
                    section: section.clone(),
                    subsection: subsection.clone(),
                    key: name.to_str_lossy().to_ascii_lowercase(),
                    line,
                    raw: Vec::new(),
                    has_separator: false,
                    has_value: false,
                    continuing: false,
                });
            }
            gix::config::parse::EventRef::KeyValueSeparator => {
                if let Some(pending) = &mut pending {
                    pending.has_separator = true;
                }
            }
            gix::config::parse::EventRef::Value(value)
            | gix::config::parse::EventRef::ValueDone(value) => {
                if let Some(pending) = &mut pending {
                    pending.raw.extend_from_slice(value);
                    pending.has_value = true;
                    pending.continuing = false;
                }
            }
            gix::config::parse::EventRef::ValueNotDone(value) => {
                if let Some(pending) = &mut pending {
                    pending.raw.extend_from_slice(value);
                    pending.has_value = true;
                    pending.continuing = true;
                }
            }
            gix::config::parse::EventRef::Whitespace(whitespace) => {
                if let Some(pending) = &mut pending
                    && (pending.has_value || pending.continuing)
                {
                    pending.raw.extend_from_slice(whitespace);
                }
            }
            gix::config::parse::EventRef::Newline(newline) => {
                if pending.as_ref().is_some_and(|pending| !pending.continuing) {
                    flush_pending(&mut pending, &mut items);
                }
                line += count_newlines(newline);
                continue;
            }
            gix::config::parse::EventRef::Comment { text, .. } => {
                line += count_newlines(text);
                continue;
            }
        }
        if let Some(newlines) = event_newlines(event) {
            line += newlines;
        }
    }
    flush_pending(&mut pending, &mut items);
    items
}

fn event_newlines(event: gix::config::parse::EventRef<'_>) -> Option<usize> {
    match event {
        gix::config::parse::EventRef::SectionHeader { .. }
        | gix::config::parse::EventRef::SectionValueName(_)
        | gix::config::parse::EventRef::KeyValueSeparator => None,
        gix::config::parse::EventRef::Value(value)
        | gix::config::parse::EventRef::ValueDone(value)
        | gix::config::parse::EventRef::ValueNotDone(value)
        | gix::config::parse::EventRef::Whitespace(value) => Some(count_newlines(value)),
        gix::config::parse::EventRef::Newline(_) | gix::config::parse::EventRef::Comment { .. } => {
            None
        }
    }
}

fn count_newlines(text: &BStr) -> usize {
    text.iter().filter(|byte| **byte == b'\n').count()
}

struct Pending {
    section: String,
    subsection: Option<String>,
    key: String,
    line: usize,
    raw: Vec<u8>,
    has_separator: bool,
    has_value: bool,
    continuing: bool,
}

fn flush_pending(pending: &mut Option<Pending>, items: &mut Vec<ParsedItem>) {
    let Some(pending) = pending.take() else {
        return;
    };
    let value = if pending.has_separator || pending.has_value {
        gix::config::value::normalize(pending.raw.as_slice())
            .to_str_lossy()
            .into_owned()
    } else {
        // A key without `=` is the implicit boolean true.
        "true".to_owned()
    };
    let key = match &pending.subsection {
        Some(subsection) => format!(
            "{}.{}.{}",
            pending.section.to_ascii_lowercase(),
            subsection,
            pending.key
        ),
        None => format!("{}.{}", pending.section.to_ascii_lowercase(), pending.key),
    };
    items.push(ParsedItem {
        section: pending.section,
        subsection: pending.subsection,
        value_name: pending.key,
        key,
        value,
        line: pending.line,
    });
}

fn parse_condition(condition: &str) -> ParsedCondition {
    if condition.starts_with("gitdir:") || condition.starts_with("gitdir/i:") {
        ParsedCondition {
            kind: IncludeConditionKind::GitDir,
        }
    } else if condition.starts_with("onbranch:") {
        ParsedCondition {
            kind: IncludeConditionKind::OnBranch,
        }
    } else if let Some(rest) = condition.strip_prefix("hasconfig:")
        && rest
            .split_once(':')
            .is_some_and(|(key, _)| key.eq_ignore_ascii_case("remote.*.url"))
    {
        ParsedCondition {
            kind: IncludeConditionKind::HasConfigRemoteUrl,
        }
    } else {
        ParsedCondition {
            kind: IncludeConditionKind::Unsupported,
        }
    }
}

struct ParsedCondition {
    kind: IncludeConditionKind,
}

fn include_path(raw: &str, config_path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let raw = raw.trim();
    let expanded = if raw == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home?.join(rest)
    } else if raw.starts_with('~') || raw.starts_with("%(prefix)") {
        return None;
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(config_path.parent()?.join(expanded))
    }
}

fn expand_gitdir_pattern(
    pattern: &str,
    config_path: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if pattern == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = pattern.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    if pattern.starts_with('~') || pattern.starts_with("%(prefix)") {
        return None;
    }
    if let Some(relative) = pattern.strip_prefix("./") {
        return config_path
            .and_then(Path::parent)
            .map(|parent| parent.join(relative));
    }
    Some(PathBuf::from(pattern))
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn canonical_key(key: &str) -> String {
    let mut parts = key.split('.');
    let Some(section) = parts.next() else {
        return key.to_ascii_lowercase();
    };
    let rest: Vec<&str> = parts.collect();
    let mut canonical = section.to_ascii_lowercase();
    for (index, part) in rest.iter().enumerate() {
        canonical.push('.');
        if index + 1 == rest.len() {
            canonical.push_str(&part.to_ascii_lowercase());
        } else {
            canonical.push_str(part);
        }
    }
    canonical
}

/// Validate a configuration key written as `section[.subsection].name`.
///
/// The section and the name are letters, digits and hyphens, starting with a
/// letter. A subsection can hold anything printable, including dots, because
/// git writes it quoted; a key with an empty part (`bad..key`) is refused
/// because git would quietly turn it into an empty subsection.
pub fn validate_key_name(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::other("the key name is empty"));
    }
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() < 2 {
        return Err(Error::other(format!(
            "`{key}` needs a section and a name, like `user.email`"
        )));
    }
    if !is_key_component(parts[0]) || !is_key_component(parts[parts.len() - 1]) {
        return Err(Error::other(format!(
            "`{key}` has a section or name that is not letters, digits and `-`"
        )));
    }
    for subsection in &parts[1..parts.len() - 1] {
        if subsection.is_empty() {
            return Err(Error::other(format!("`{key}` has an empty subsection")));
        }
        if subsection.chars().any(char::is_control) {
            return Err(Error::other(format!(
                "`{key}` contains a control character"
            )));
        }
    }
    Ok(())
}

fn is_key_component(part: &str) -> bool {
    let mut chars = part.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Check a value against the curated rule for `key`, when the key has one.
///
/// Uncurated keys accept any text; git itself decides whether the value makes
/// sense. A curated key with a misspelt value is refused here, before the
/// preview can show a write that git would accept and then misread.
pub fn validate_value(key: &str, value: &str) -> Result<()> {
    let Some(well_known) = well_known_key(key) else {
        return Ok(());
    };
    if well_known.rule.accepts(value) {
        return Ok(());
    }
    Err(Error::other(format!(
        "`{value}` is not a valid value for {} (expected {})",
        well_known.key,
        well_known.rule.allowed()
    )))
}

fn finish(
    assignments: Vec<Assignment>,
    includes: Vec<ConfigInclude>,
    files: ConfigFilePaths,
) -> GitConfig {
    let mut grouped: BTreeMap<String, Vec<Assignment>> = BTreeMap::new();
    for assignment in assignments {
        grouped
            .entry(assignment.key.clone())
            .or_default()
            .push(assignment);
    }

    let mut entries = Vec::with_capacity(grouped.len());
    for (key, assignments) in grouped {
        let multivalued = is_multivalued(&key);
        let values: Vec<ConfigValue> = assignments
            .iter()
            .map(|assignment| ConfigValue {
                value: assignment.value.clone(),
                layer: assignment.layer,
                origin: assignment.origin.clone(),
            })
            .collect();

        let (values, overridden) = if multivalued {
            (values, Vec::new())
        } else {
            let mut values = values;
            let effective = values.pop().into_iter().collect();
            (effective, values)
        };

        let well_known = well_known_key(&key);
        let warning = well_known.and_then(|well_known| {
            values
                .iter()
                .find(|value| !well_known.rule.accepts(&value.value))
                .map(|value| {
                    format!(
                        "`{}` is not a valid value for {} (expected {})",
                        value.value,
                        well_known.key,
                        well_known.rule.allowed()
                    )
                })
        });

        entries.push(ConfigEntry {
            key,
            values,
            overridden,
            multivalued,
            well_known,
            warning,
        });
    }

    let identity = ConfigIdentity {
        name: entries
            .iter()
            .find(|entry| entry.key == "user.name")
            .and_then(ConfigEntry::effective)
            .cloned(),
        email: entries
            .iter()
            .find(|entry| entry.key == "user.email")
            .and_then(ConfigEntry::effective)
            .cloned(),
    };

    GitConfig {
        entries,
        includes,
        identity,
        files,
    }
}

/// Keys git accumulates instead of letting the last value win.
fn is_multivalued(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if lower == "credential.helper" || lower == "safe.directory" {
        return true;
    }
    let parts: Vec<&str> = lower.split('.').collect();
    matches!(
        parts.as_slice(),
        ["remote", _, "fetch" | "push" | "url"]
            | ["url", _, "insteadof" | "pushinsteadof"]
            | ["branch", _, "merge"]
            | ["http", _, "extraheader"]
    )
}

fn is_git_bool(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
    )
}

fn is_git_true(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::{TestRepo, config_path};

    fn sources(global: &Path) -> ConfigSources {
        ConfigSources {
            global: Some(global.to_path_buf()),
            ..ConfigSources::default()
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create config directory");
        }
        fs::write(path, contents).expect("write config file");
    }

    #[test]
    fn later_layers_override_earlier_ones_and_keep_the_overridden_values() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let system = root.path().join("system.gitconfig");
        let global = root.path().join("global.gitconfig");
        write(&system, "[user]\n\tname = System Name\n");
        write(&global, "[user]\n\tname = Global Name\n");
        repo.git(&["config", "user.name", "Local Name"]);

        let config = GitConfig::read(
            &repo.open(),
            &ConfigSources {
                system: Some(system.clone()),
                global: Some(global.clone()),
                ..ConfigSources::default()
            },
        )
        .expect("read config");

        let name = config.entry("user.name").expect("user.name");
        assert_eq!(name.effective().expect("effective").value, "Local Name");
        assert_eq!(
            name.effective().expect("effective").layer,
            ConfigLayer::Local
        );
        assert_eq!(
            name.overridden
                .iter()
                .map(|value| (value.value.as_str(), value.layer))
                .collect::<Vec<_>>(),
            vec![
                ("System Name", ConfigLayer::System),
                ("Global Name", ConfigLayer::Global)
            ]
        );
        assert_eq!(
            name.overridden[0].origin.path.as_deref(),
            Some(system.as_path())
        );
        assert_eq!(
            name.overridden[1].origin.path.as_deref(),
            Some(global.as_path())
        );
    }

    #[test]
    fn include_if_gitdir_reports_matching_and_non_matching_files() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        let matched = root.path().join("matched.gitconfig");
        let unmatched = root.path().join("unmatched.gitconfig");
        write(&matched, "[user]\n\temail = matched@example.invalid\n");
        write(&unmatched, "[user]\n\temail = unmatched@example.invalid\n");
        let git_dir = repo.path().join(".git");
        write(
            &global,
            &format!(
                "[user]\n\tname = Global\n\
                 [includeIf \"gitdir:{}/\"]\n\tpath = {}\n\
                 [includeIf \"gitdir:/definitely/not/this/repository/\"]\n\tpath = {}\n",
                config_path(&git_dir),
                config_path(&matched),
                config_path(&unmatched)
            ),
        );

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");

        let matched_include = config
            .includes
            .iter()
            .find(|include| include.path.as_deref() == Some(matched.as_path()))
            .expect("matched include");
        assert!(matched_include.matched);
        assert_eq!(matched_include.values, 1);
        assert_eq!(matched_include.condition_kind, IncludeConditionKind::GitDir);

        let unmatched_include = config
            .includes
            .iter()
            .find(|include| include.path.as_deref() == Some(unmatched.as_path()))
            .expect("unmatched include");
        assert!(!unmatched_include.matched);
        assert_eq!(unmatched_include.values, 0);

        let email = config.entry("user.email").expect("user.email");
        assert_eq!(
            email.effective().expect("effective").value,
            "matched@example.invalid"
        );
        assert_eq!(
            email.effective().expect("effective").origin.path.as_deref(),
            Some(matched.as_path())
        );
        assert_eq!(
            email.effective().expect("effective").origin.include,
            Some(
                config
                    .includes
                    .iter()
                    .position(|include| include.path.as_deref() == Some(matched.as_path()))
                    .expect("include index")
            )
        );
    }

    #[test]
    fn include_if_onbranch_matches_the_checked_out_branch() {
        let repo = TestRepo::new();
        repo.write("file.txt", "one\n");
        repo.commit("first");
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        let matched = root.path().join("branch.gitconfig");
        write(&matched, "[fetch]\n\tprune = true\n");
        let branch = repo.git(&["branch", "--show-current"]);
        write(
            &global,
            &format!(
                "[includeIf \"onbranch:{}/\"]\n\tpath = {}\n\
                 [includeIf \"onbranch:not-this-branch/\"]\n\tpath = {}\n",
                branch.trim(),
                config_path(&matched),
                config_path(&matched)
            ),
        );

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        let matching: Vec<_> = config
            .includes
            .iter()
            .filter(|include| include.condition_kind == IncludeConditionKind::OnBranch)
            .collect();
        assert_eq!(matching.len(), 2);
        assert!(matching[0].matched);
        assert!(!matching[1].matched);
        assert_eq!(
            config
                .entry("fetch.prune")
                .expect("fetch.prune")
                .values
                .len(),
            1
        );
    }

    #[test]
    fn include_if_hasconfig_matches_a_remote_url_already_seen() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        let matched = root.path().join("remote.gitconfig");
        write(&matched, "[fetch]\n\tprune = true\n");
        write(
            &global,
            &format!(
                "[remote \"origin\"]\n\turl = https://example.com/team/project.git\n\
                 [includeIf \"hasconfig:remote.*.url:https://example.com/**\"]\n\tpath = {}\n\
                 [includeIf \"hasconfig:remote.*.url:https://other.invalid/**\"]\n\tpath = {}\n",
                config_path(&matched),
                config_path(&matched)
            ),
        );

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        let conditions: Vec<_> = config
            .includes
            .iter()
            .filter(|include| include.condition_kind == IncludeConditionKind::HasConfigRemoteUrl)
            .collect();
        assert_eq!(conditions.len(), 2);
        assert!(conditions[0].matched);
        assert!(!conditions[1].matched);
    }

    #[test]
    fn a_nested_include_points_back_at_its_own_directive() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        let outer = root.path().join("outer.gitconfig");
        let inner = root.path().join("inner.gitconfig");
        write(
            &global,
            &format!("[include]\n\tpath = {}\n", config_path(&outer)),
        );
        write(
            &outer,
            &format!("[include]\n\tpath = {}\n", config_path(&inner)),
        );
        write(&inner, "[user]\n\temail = inner@example.invalid\n");

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        assert_eq!(config.includes.len(), 2);
        let outer_index = config
            .includes
            .iter()
            .position(|include| include.path.as_deref() == Some(outer.as_path()))
            .expect("outer include");
        let inner_index = config
            .includes
            .iter()
            .position(|include| include.path.as_deref() == Some(inner.as_path()))
            .expect("inner include");
        assert_eq!(config.includes[outer_index].values, 1);
        assert_eq!(config.includes[inner_index].values, 1);
        let email = config.entry("user.email").expect("user.email");
        assert_eq!(
            email.effective().expect("effective").origin.include,
            Some(inner_index)
        );
        assert_eq!(
            config.includes[inner_index].origin.include,
            Some(outer_index)
        );
    }

    #[test]
    fn multi_valued_keys_keep_every_value_in_order() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        write(
            &global,
            "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\
             \tfetch = +refs/tags/*:refs/tags/*\n",
        );
        repo.git(&[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/notes/*:refs/notes/*",
        ]);

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        let fetch = config.entry("remote.origin.fetch").expect("fetch refspecs");
        assert_eq!(
            fetch
                .values
                .iter()
                .map(|value| value.value.as_str())
                .collect::<Vec<_>>(),
            vec![
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*",
                "+refs/notes/*:refs/notes/*",
            ]
        );
        assert!(fetch.overridden.is_empty());
        assert_eq!(fetch.values[0].layer, ConfigLayer::Global);
        assert_eq!(fetch.values[2].layer, ConfigLayer::Local);
    }

    #[test]
    fn the_entry_says_whether_the_key_keeps_every_value() {
        let repo = TestRepo::new();
        repo.git(&[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/*",
        ]);
        repo.git(&["config", "user.name", "A Name"]);

        let config = GitConfig::read(&repo.open(), &ConfigSources::default()).expect("read config");

        assert!(
            config
                .entry("remote.origin.fetch")
                .expect("fetch")
                .multivalued,
            "a refspec key keeps every value even with one value set"
        );
        assert!(
            !config.entry("user.name").expect("user.name").multivalued,
            "user.name lets the last value win"
        );
    }

    #[test]
    fn an_invalid_well_known_value_is_a_warning() {
        let repo = TestRepo::new();
        repo.git(&["config", "pull.rebase", "ture"]);
        repo.git(&["config", "pull.ff", "only"]);
        repo.git(&["config", "user.name", "A Name"]);

        let config = GitConfig::read(&repo.open(), &ConfigSources::default()).expect("read config");

        let pull = config.entry("pull.rebase").expect("pull.rebase");
        let warning = pull.warning.as_deref().expect("invalid value warns");
        assert!(warning.contains("ture"), "{warning}");
        assert!(warning.contains("true"), "{warning}");
        assert!(config.entry("pull.ff").expect("pull.ff").warning.is_none());
        assert!(
            config
                .entry("user.name")
                .expect("user.name")
                .warning
                .is_none()
        );
    }

    #[test]
    fn the_identity_names_the_file_that_set_each_value() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        write(
            &global,
            "[user]\n\tname = Global Name\n\temail = global@example.invalid\n",
        );
        repo.git(&["config", "user.email", "local@example.invalid"]);

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        let name = config.identity.name.expect("name");
        let email = config.identity.email.expect("email");
        assert_eq!(name.value, "Global Name");
        assert_eq!(name.layer, ConfigLayer::Global);
        assert_eq!(name.origin.path.as_deref(), Some(global.as_path()));
        assert_eq!(name.origin.line, Some(2));
        assert_eq!(email.value, "local@example.invalid");
        assert_eq!(email.layer, ConfigLayer::Local);
    }

    #[test]
    fn key_names_are_checked_as_section_subsection_name() {
        for valid in [
            "user.email",
            "commit.gpgSign",
            "remote.origin.fetch",
            "url.https://example.invalid/.insteadOf",
            "includeIf.gitdir:/tmp/repo/.path",
        ] {
            validate_key_name(valid).unwrap_or_else(|error| panic!("{valid}: {error}"));
        }
        for invalid in [
            "",
            "user",
            "user.",
            ".name",
            "user..name",
            "-user.name",
            "2user.name",
            "bad key.name",
            "user.na me",
            "user.name\nother",
        ] {
            assert!(
                validate_key_name(invalid).is_err(),
                "{invalid:?} is invalid"
            );
        }
    }

    #[test]
    fn curated_values_are_checked_but_other_keys_take_any_text() {
        validate_value("pull.rebase", "merges").expect("curated enum value");
        validate_value("commit.gpgSign", "on").expect("curated boolean value");
        validate_value("user.name", "anything # at all").expect("text keys accept any text");
        validate_value("custom.key", "not curated").expect("unknown keys accept any text");

        let error = validate_value("pull.rebase", "ture").expect_err("misspelt value");
        let text = error.to_string();
        assert!(text.contains("pull.rebase"), "{text}");
        assert!(text.contains("ture"), "{text}");
        assert!(text.contains("merges"), "{text}");
    }

    #[test]
    fn the_file_paths_name_the_local_and_global_files_and_worktree_only_when_enabled() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        write(&global, "");

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        assert_eq!(config.files.local, repo.path().join(".git/config"));
        assert_eq!(config.files.global.as_deref(), Some(global.as_path()));
        assert_eq!(config.files.worktree, None);

        repo.git(&["config", "extensions.worktreeConfig", "true"]);
        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        assert_eq!(
            config.files.worktree.as_deref(),
            Some(repo.path().join(".git/config.worktree").as_path())
        );
    }

    #[test]
    fn an_identity_from_an_include_offers_a_local_override() {
        let repo = TestRepo::new();
        let root = tempfile::TempDir::new().expect("config directory");
        let global = root.path().join("global.gitconfig");
        let work = root.path().join("work.gitconfig");
        write(&work, "[user]\n\temail = work@example.invalid\n");
        write(
            &global,
            &format!(
                "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
                config_path(&repo.path().join(".git")),
                config_path(&work)
            ),
        );

        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        let identity = config
            .identity_needing_local_override()
            .expect("the include identity is worth a local override");
        assert_eq!(identity.value, "work@example.invalid");

        repo.git(&["config", "user.email", "local@example.invalid"]);
        let config = GitConfig::read(&repo.open(), &sources(&global)).expect("read config");
        assert!(config.identity_needing_local_override().is_none());
    }

    #[test]
    fn only_a_config_file_this_repository_lists_may_be_written() {
        let repo = TestRepo::new();
        let local = GitConfig::load(repo.path())
            .expect("read config")
            .files
            .local;
        assert!(
            require_writable_config_file(repo.path(), &local).is_ok(),
            "the repository's own config is writable"
        );

        let outside = repo.path().join("README.md");
        assert!(
            require_writable_config_file(repo.path(), &outside).is_err(),
            "a file the config reader never listed is not writable"
        );
        let escape = PathBuf::from(format!(
            "{}/../config",
            local.parent().expect("config parent").display()
        ));
        assert!(
            require_writable_config_file(repo.path(), &escape).is_err(),
            "a .. component is refused even inside the git directory"
        );
        assert!(require_writable_config_file(repo.path(), Path::new("config")).is_err());
        assert!(require_writable_config_file(repo.path(), Path::new("")).is_err());
        assert!(
            require_writable_config_file(repo.path(), Path::new("/tmp/co\nnfig")).is_err(),
            "a control character is refused"
        );
    }

    #[test]
    fn the_curated_table_explains_every_key_and_rejects_unknown_values() {
        assert_eq!(
            well_known_key("pull.rebase").map(|key| key.key),
            Some("pull.rebase")
        );
        assert!(
            well_known_key("pull.rebase")
                .expect("key")
                .rule
                .accepts("merges")
        );
        assert!(
            !well_known_key("pull.rebase")
                .expect("key")
                .rule
                .accepts("ture")
        );
        assert!(
            well_known_key("commit.gpgSign")
                .expect("key")
                .rule
                .accepts("on")
        );
        assert!(
            !well_known_key("commit.gpgSign")
                .expect("key")
                .rule
                .accepts("maybe")
        );
        assert!(well_known_key("user.name").is_some());
        for key in WELL_KNOWN_KEYS {
            assert!(!key.summary.is_empty(), "{} has a summary", key.key);
        }
    }
}
