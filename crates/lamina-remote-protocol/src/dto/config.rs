//! The configuration the helper read, and one change the editor wants to make.
//!
//! `well_known` is not on the wire: it is a static table the app attaches
//! again with [`lamina_core::repo::well_known_key`]. Paths are text because a
//! frame cannot carry an `OsString`. A path that is not Unicode is omitted,
//! and the helper then refuses to write it.

use serde::{Deserialize, Serialize};

use crate::codec::ProtocolError;

/// Precedence layer. The order matches [`lamina_core::repo::ConfigLayer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigLayerDto {
    System,
    Xdg,
    Global,
    Local,
    Worktree,
    Environment,
    CommandLine,
}

impl From<lamina_core::repo::ConfigLayer> for ConfigLayerDto {
    fn from(layer: lamina_core::repo::ConfigLayer) -> Self {
        use lamina_core::repo::ConfigLayer;
        match layer {
            ConfigLayer::System => Self::System,
            ConfigLayer::Xdg => Self::Xdg,
            ConfigLayer::Global => Self::Global,
            ConfigLayer::Local => Self::Local,
            ConfigLayer::Worktree => Self::Worktree,
            ConfigLayer::Environment => Self::Environment,
            ConfigLayer::CommandLine => Self::CommandLine,
        }
    }
}

impl From<ConfigLayerDto> for lamina_core::repo::ConfigLayer {
    fn from(layer: ConfigLayerDto) -> Self {
        match layer {
            ConfigLayerDto::System => Self::System,
            ConfigLayerDto::Xdg => Self::Xdg,
            ConfigLayerDto::Global => Self::Global,
            ConfigLayerDto::Local => Self::Local,
            ConfigLayerDto::Worktree => Self::Worktree,
            ConfigLayerDto::Environment => Self::Environment,
            ConfigLayerDto::CommandLine => Self::CommandLine,
        }
    }
}

/// What a write does to one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigChangeDto {
    Set(String),
    Add(String),
    Unset,
    UnsetValue(String),
}

impl From<&lamina_core::git_cli::ConfigChange> for ConfigChangeDto {
    fn from(change: &lamina_core::git_cli::ConfigChange) -> Self {
        use lamina_core::git_cli::ConfigChange;
        match change {
            ConfigChange::Set(value) => Self::Set(value.clone()),
            ConfigChange::Add(value) => Self::Add(value.clone()),
            ConfigChange::Unset => Self::Unset,
            ConfigChange::UnsetValue(value) => Self::UnsetValue(value.clone()),
        }
    }
}

impl From<ConfigChangeDto> for lamina_core::git_cli::ConfigChange {
    fn from(change: ConfigChangeDto) -> Self {
        match change {
            ConfigChangeDto::Set(value) => Self::Set(value),
            ConfigChangeDto::Add(value) => Self::Add(value),
            ConfigChangeDto::Unset => Self::Unset,
            ConfigChangeDto::UnsetValue(value) => Self::UnsetValue(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOriginDto {
    pub path: Option<String>,
    pub line: Option<u64>,
    pub include: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValueDto {
    pub value: String,
    pub layer: ConfigLayerDto,
    pub origin: ConfigOriginDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigEntryDto {
    pub key: String,
    pub values: Vec<ConfigValueDto>,
    pub overridden: Vec<ConfigValueDto>,
    pub multivalued: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IncludeConditionKindDto {
    Always,
    GitDir,
    OnBranch,
    HasConfigRemoteUrl,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIncludeDto {
    pub condition: String,
    pub condition_kind: IncludeConditionKindDto,
    pub path: Option<String>,
    pub layer: ConfigLayerDto,
    pub origin: ConfigOriginDto,
    pub matched: bool,
    pub values: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigIdentityDto {
    pub name: Option<ConfigValueDto>,
    pub email: Option<ConfigValueDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFilePathsDto {
    pub local: String,
    pub worktree: Option<String>,
    pub global: Option<String>,
    pub system: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitConfigDto {
    pub entries: Vec<ConfigEntryDto>,
    pub includes: Vec<ConfigIncludeDto>,
    pub identity: ConfigIdentityDto,
    pub files: ConfigFilePathsDto,
}

fn path_text(path: &std::path::Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

fn text_path(text: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(text)
}

fn origin_to_dto(origin: &lamina_core::repo::ConfigOrigin) -> ConfigOriginDto {
    ConfigOriginDto {
        path: origin.path.as_deref().and_then(path_text),
        line: origin.line.and_then(|line| u64::try_from(line).ok()),
        include: origin.include.and_then(|index| u64::try_from(index).ok()),
    }
}

fn origin_from_dto(
    origin: ConfigOriginDto,
) -> Result<lamina_core::repo::ConfigOrigin, ProtocolError> {
    Ok(lamina_core::repo::ConfigOrigin {
        path: origin.path.map(|path| text_path(&path)),
        line: origin
            .line
            .map(usize::try_from)
            .transpose()
            .map_err(|_| ProtocolError::Malformed("config line does not fit"))?,
        include: origin
            .include
            .map(usize::try_from)
            .transpose()
            .map_err(|_| ProtocolError::Malformed("config include index does not fit"))?,
    })
}

fn value_to_dto(value: &lamina_core::repo::ConfigValue) -> ConfigValueDto {
    ConfigValueDto {
        value: value.value.clone(),
        layer: value.layer.into(),
        origin: origin_to_dto(&value.origin),
    }
}

fn value_from_dto(value: ConfigValueDto) -> Result<lamina_core::repo::ConfigValue, ProtocolError> {
    Ok(lamina_core::repo::ConfigValue {
        value: value.value,
        layer: value.layer.into(),
        origin: origin_from_dto(value.origin)?,
    })
}

impl From<&lamina_core::repo::GitConfig> for GitConfigDto {
    fn from(config: &lamina_core::repo::GitConfig) -> Self {
        use lamina_core::repo::IncludeConditionKind;
        Self {
            entries: config
                .entries
                .iter()
                .map(|entry| ConfigEntryDto {
                    key: entry.key.clone(),
                    values: entry.values.iter().map(value_to_dto).collect(),
                    overridden: entry.overridden.iter().map(value_to_dto).collect(),
                    multivalued: entry.multivalued,
                    warning: entry.warning.clone(),
                })
                .collect(),
            includes: config
                .includes
                .iter()
                .map(|include| ConfigIncludeDto {
                    condition: include.condition.clone(),
                    condition_kind: match include.condition_kind {
                        IncludeConditionKind::Always => IncludeConditionKindDto::Always,
                        IncludeConditionKind::GitDir => IncludeConditionKindDto::GitDir,
                        IncludeConditionKind::OnBranch => IncludeConditionKindDto::OnBranch,
                        IncludeConditionKind::HasConfigRemoteUrl => {
                            IncludeConditionKindDto::HasConfigRemoteUrl
                        }
                        IncludeConditionKind::Unsupported => IncludeConditionKindDto::Unsupported,
                    },
                    path: include.path.as_deref().and_then(path_text),
                    layer: include.layer.into(),
                    origin: origin_to_dto(&include.origin),
                    matched: include.matched,
                    values: include.values as u64,
                    error: include.error.clone(),
                })
                .collect(),
            identity: ConfigIdentityDto {
                name: config.identity.name.as_ref().map(value_to_dto),
                email: config.identity.email.as_ref().map(value_to_dto),
            },
            files: ConfigFilePathsDto {
                local: path_text(&config.files.local).unwrap_or_default(),
                worktree: config.files.worktree.as_deref().and_then(path_text),
                global: config.files.global.as_deref().and_then(path_text),
                system: config.files.system.as_deref().and_then(path_text),
            },
        }
    }
}

impl TryFrom<GitConfigDto> for lamina_core::repo::GitConfig {
    type Error = ProtocolError;

    fn try_from(config: GitConfigDto) -> Result<Self, ProtocolError> {
        use lamina_core::repo::{
            ConfigEntry, ConfigFilePaths, ConfigIdentity, ConfigInclude, IncludeConditionKind,
        };
        let mut entries = Vec::with_capacity(config.entries.len());
        for entry in config.entries {
            let values = entry
                .values
                .into_iter()
                .map(value_from_dto)
                .collect::<Result<Vec<_>, _>>()?;
            let overridden = entry
                .overridden
                .into_iter()
                .map(value_from_dto)
                .collect::<Result<Vec<_>, _>>()?;
            entries.push(ConfigEntry {
                well_known: lamina_core::repo::well_known_key(&entry.key),
                key: entry.key,
                values,
                overridden,
                multivalued: entry.multivalued,
                warning: entry.warning,
            });
        }
        let mut includes = Vec::with_capacity(config.includes.len());
        for include in config.includes {
            let values = usize::try_from(include.values)
                .map_err(|_| ProtocolError::Malformed("config value count does not fit"))?;
            includes.push(ConfigInclude {
                condition: include.condition,
                condition_kind: match include.condition_kind {
                    IncludeConditionKindDto::Always => IncludeConditionKind::Always,
                    IncludeConditionKindDto::GitDir => IncludeConditionKind::GitDir,
                    IncludeConditionKindDto::OnBranch => IncludeConditionKind::OnBranch,
                    IncludeConditionKindDto::HasConfigRemoteUrl => {
                        IncludeConditionKind::HasConfigRemoteUrl
                    }
                    IncludeConditionKindDto::Unsupported => IncludeConditionKind::Unsupported,
                },
                path: include.path.map(|path| text_path(&path)),
                layer: include.layer.into(),
                origin: origin_from_dto(include.origin)?,
                matched: include.matched,
                values,
                error: include.error,
            });
        }
        Ok(Self {
            entries,
            includes,
            identity: ConfigIdentity {
                name: config.identity.name.map(value_from_dto).transpose()?,
                email: config.identity.email.map(value_from_dto).transpose()?,
            },
            files: ConfigFilePaths {
                local: text_path(&config.files.local),
                worktree: config.files.worktree.map(|path| text_path(&path)),
                global: config.files.global.map(|path| text_path(&path)),
                system: config.files.system.map(|path| text_path(&path)),
            },
        })
    }
}
