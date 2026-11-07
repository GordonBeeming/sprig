use crate::{git::Repository, platform, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub backup: Backup,
    #[serde(default)]
    pub copy: CopyPolicy,
    #[serde(default)]
    pub cleanup: Cleanup,
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    pub time_machine_exclude: Option<bool>,
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyPolicy {
    pub ignored: Option<String>,
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cleanup {
    pub enabled: Option<bool>,
    pub idle_hours: Option<u64>,
    pub folders: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CleanupPolicy {
    pub enabled: bool,
    pub idle_hours: u64,
    pub folders: Vec<String>,
    pub exclude: Vec<String>,
}
impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_hours: 6,
            folders: ["bin", "obj", "dist", "target", "build", ".next"]
                .map(String::from)
                .to_vec(),
            exclude: Vec::new(),
        }
    }
}
fn validate_cleanup(c: &Cleanup) -> Result<()> {
    if c.idle_hours.is_some_and(|v| v == 0 || v > 8760) {
        return Err(Error::new(
            "E_CONFIG",
            "cleanup.idle_hours must be between 1 and 8760",
        ));
    }
    if let Some(folders) = &c.folders {
        for name in folders {
            if name.is_empty()
                || name.contains('/')
                || name.contains('\\')
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "." | ".." | ".git" | "node_modules" | ".venv" | "venv" | "vendor" | "packages"
                )
            {
                return Err(Error::new("E_CONFIG", "cleanup.folders must contain output directory names, not paths, Git metadata or dependencies"));
            }
        }
    }
    if let Some(exclude) = &c.exclude {
        for name in exclude {
            if name.is_empty()
                || std::path::Path::new(name)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(Error::new(
                    "E_CONFIG",
                    "cleanup.exclude must contain relative paths without traversal",
                ));
            }
        }
    }
    Ok(())
}
#[derive(Debug, Serialize)]
pub struct Effective {
    pub time_machine_exclude: bool,
    pub backup_origin: String,
    pub ignored: String,
    pub ignored_origin: String,
    pub cleanup: CleanupPolicy,
    pub cleanup_origins: BTreeMap<String, String>,
}
pub fn user_path() -> Result<PathBuf> {
    user_path_named("sprig")
}
fn user_path_named(name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| Error::new("E_CONFIG", "HOME is unavailable"))?,
    )
    .join("Library/Application Support")
    .join(name)
    .join("config.toml"))
}
fn read(path: &Path) -> Result<Config> {
    let config: Config = match fs::read_to_string(path) {
        Ok(value) => toml::from_str(&value)
            .map_err(|e| Error::new("E_CONFIG", format!("{}: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e.into()),
    };
    if config.copy.ignored.as_deref().is_some_and(|s| s != "none") {
        return Err(Error::new(
            "E_CONFIG",
            "copy.ignored currently supports only 'none'",
        ));
    }
    validate_cleanup(&config.cleanup)?;
    Ok(config)
}
pub fn effective(repo: Option<&Repository>, cli: Option<bool>) -> Result<Effective> {
    let mut value = Effective {
        time_machine_exclude: true,
        backup_origin: "built-in".into(),
        ignored: "none".into(),
        ignored_origin: "built-in".into(),
        cleanup: CleanupPolicy::default(),
        cleanup_origins: ["enabled", "idle_hours", "folders", "exclude"]
            .into_iter()
            .map(|key| (key.into(), "built-in".into()))
            .collect(),
    };
    let mut layers = vec![("user", user_path_named("tool")?), ("user", user_path()?)];
    if let Some(repo) = repo {
        layers.push(("repository", repo.common.join("tool.toml")));
        layers.push(("repository", repo.common.join("sprig.toml")));
    }
    for (origin, path) in layers {
        let c = read(&path)?;
        if let Some(v) = c.cleanup.enabled {
            value.cleanup.enabled = v;
            value
                .cleanup_origins
                .insert("enabled".into(), origin.into());
        }
        if let Some(v) = c.cleanup.idle_hours {
            value.cleanup.idle_hours = v;
            value
                .cleanup_origins
                .insert("idle_hours".into(), origin.into());
        }
        if let Some(v) = c.cleanup.folders {
            value.cleanup.folders = v;
            value
                .cleanup_origins
                .insert("folders".into(), origin.into());
        }
        if let Some(v) = c.cleanup.exclude {
            for path in v {
                if !value.cleanup.exclude.contains(&path) {
                    value.cleanup.exclude.push(path);
                }
            }
            value
                .cleanup_origins
                .insert("exclude".into(), origin.into());
        }
        if let Some(v) = c.backup.time_machine_exclude {
            value.time_machine_exclude = v;
            value.backup_origin = origin.into();
        }
        if let Some(v) = c.copy.ignored {
            value.ignored = v;
            value.ignored_origin = origin.into();
        }
    }
    if let Some(v) = cli {
        value.time_machine_exclude = v;
        value.backup_origin = "command-line".into();
    }
    Ok(value)
}
pub fn set(repo: Option<&Repository>, scope: &str, key: &str, value: &str) -> Result<PathBuf> {
    let path = match scope {
        "user" => user_path()?,
        "repository" => repo
            .ok_or_else(|| Error::new("E_CONFIG", "repository scope requires a worktree"))?
            .common
            .join("sprig.toml"),
        _ => return Err(Error::new("E_ARGUMENT", "scope must be user or repository")),
    };
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("E_CONFIG", "invalid config path"))?;
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.with_extension("toml.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(Error::new(
            "E_BUSY",
            "another Sprig operation is updating this configuration",
        ));
    }
    let mut config = read(&path)?;
    match key {
        "backup.time_machine_exclude" => {
            config.backup.time_machine_exclude = Some(
                value
                    .parse()
                    .map_err(|_| Error::new("E_CONFIG", "value must be true or false"))?,
            )
        }
        "copy.ignored" if value == "none" => config.copy.ignored = Some(value.into()),
        "cleanup.enabled" => {
            config.cleanup.enabled = Some(
                value
                    .parse()
                    .map_err(|_| Error::new("E_CONFIG", "value must be true or false"))?,
            )
        }
        "cleanup.idle_hours" => {
            config.cleanup.idle_hours = Some(
                value
                    .parse()
                    .map_err(|_| Error::new("E_CONFIG", "value must be an integer"))?,
            )
        }
        "cleanup.folders" => {
            config.cleanup.folders = Some(serde_json::from_str(value).map_err(|_| {
                Error::new("E_CONFIG", "value must be a JSON array of directory names")
            })?)
        }
        "cleanup.exclude" => {
            config.cleanup.exclude = Some(serde_json::from_str(value).map_err(|_| {
                Error::new("E_CONFIG", "value must be a JSON array of relative paths")
            })?)
        }
        _ => return Err(Error::new("E_CONFIG", "unknown key or unsupported policy")),
    }
    validate_cleanup(&config.cleanup)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(
        toml::to_string_pretty(&config)
            .map_err(|e| Error::new("E_CONFIG", e.to_string()))?
            .as_bytes(),
    )?;
    file.as_file().sync_all()?;
    file.persist(&path).map_err(|e| Error::from(e.error))?;
    platform::sync_dir(parent)?;
    Ok(path)
}
