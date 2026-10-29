use crate::{git::Repository, platform, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
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
#[derive(Debug, Serialize)]
pub struct Effective {
    pub time_machine_exclude: bool,
    pub backup_origin: String,
    pub ignored: String,
    pub ignored_origin: String,
}
pub fn user_path() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| Error::new("E_CONFIG", "HOME is unavailable"))?,
    )
    .join("Library/Application Support/tool/config.toml"))
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
    Ok(config)
}
pub fn effective(repo: Option<&Repository>, cli: Option<bool>) -> Result<Effective> {
    let mut value = Effective {
        time_machine_exclude: true,
        backup_origin: "built-in".into(),
        ignored: "none".into(),
        ignored_origin: "built-in".into(),
    };
    let mut layers = vec![("user", user_path()?)];
    if let Some(repo) = repo {
        layers.push(("repository", repo.common.join("tool.toml")));
    }
    for (origin, path) in layers {
        let c = read(&path)?;
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
            .join("tool.toml"),
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
            "another tool operation is updating this configuration",
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
        _ => return Err(Error::new("E_CONFIG", "unknown key or unsupported policy")),
    }
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
