use crate::{
    git::Repository,
    platform::{self, Fingerprint},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

pub struct Lock {
    _file: File,
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(Error::new(
            "E_STATE",
            "Sprig state directory has unsafe type, owner or permissions",
        ));
    }
    Ok(())
}
impl Lock {
    pub fn acquire(repo: &Repository) -> Result<Self> {
        let dir = repo.common.join("tool");
        private_dir(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::new(
                "E_BUSY",
                "another Sprig operation holds the repository lock; retry when it exits",
            ));
        }
        Ok(Self { _file: file })
    }
}
pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("E_STATE", "state path has no parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temp.write_all(&serde_json::to_vec_pretty(value)?)?;
    temp.as_file().sync_all()?;
    boundary("journal_before_publish")?;
    temp.persist(path).map_err(|e| Error::from(e.error))?;
    platform::sync_dir(parent)
}
#[derive(Debug, Serialize, Deserialize)]
pub struct CreationOwnership {
    pub destination_identity: Option<(u64, u64)>,
    pub requested_new_branch: Option<String>,
    pub confirmed_new_branch: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Journal {
    pub schema_version: u32,
    pub kind: String,
    pub root: PathBuf,
    pub target: PathBuf,
    pub sibling: Option<PathBuf>,
    pub original: Option<Fingerprint>,
    pub clone: Option<Fingerprint>,
    pub commit: Option<String>,
    pub stage: String,
    #[serde(default)]
    pub creation: Option<CreationOwnership>,
}
pub struct Record {
    pub path: PathBuf,
    pub journal: Journal,
}
impl Record {
    pub fn new(repo: &Repository, journal: Journal) -> Result<Self> {
        let dir = repo.common.join("tool/operations");
        private_dir(&repo.common.join("tool"))?;
        private_dir(&dir)?;
        let file = tempfile::Builder::new()
            .prefix(".journal-id-")
            .tempfile_in(&dir)?;
        let name = file
            .path()
            .file_name()
            .ok_or_else(|| Error::new("E_STATE", "journal name missing"))?
            .to_string_lossy();
        let path = dir.join(format!(
            "operation-{}.json",
            name.trim_start_matches(".journal-id-")
        ));
        drop(file);
        let record = Self { path, journal };
        record.save()?;
        Ok(record)
    }
    pub fn save(&self) -> Result<()> {
        atomic_json(&self.path, &self.journal)
    }
    pub fn finish(self) -> Result<()> {
        fs::remove_file(&self.path)?;
        platform::sync_dir(
            self.path
                .parent()
                .ok_or_else(|| Error::new("E_STATE", "invalid journal path"))?,
        )
    }
}
pub fn records(repo: &Repository) -> Result<Vec<Record>> {
    let dir = repo.common.join("tool/operations");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut records = vec![];
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json") {
            let m = fs::symlink_metadata(&path)?;
            if !m.is_file() || m.uid() != unsafe { libc::geteuid() } {
                return Err(Error::new("E_STATE", "unsafe operation journal"));
            }
            let journal: Journal = serde_json::from_slice(&fs::read(&path)?)?;
            if journal.schema_version != 1 {
                return Err(Error::new("E_STATE", "unknown journal schema"));
            }
            records.push(Record { path, journal });
        }
    }
    Ok(records)
}
pub fn require_recovered(repo: &Repository) -> Result<()> {
    let records = records(repo)?;
    if let Some(record) = records.first() {
        return Err(Error::new(
            "E_RECOVERY",
            format!(
                "unfinished operation {}; run sprig doctor and sprig recover",
                record.path.display()
            ),
        ).with_data(serde_json::json!({"retained_workspace":record.journal.root,"journal":record.path,"recovery_command":["sprig","recover",if record.journal.kind == "create" {&repo.root} else {&record.journal.root}],"recovery_requires_inspection":record.journal.kind == "create"})));
    }
    Ok(())
}

pub fn receipt(repo: &Repository) -> Result<Option<serde_json::Value>> {
    let path = repo.admin.join("tool-workspace.json");
    let m = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } {
        return Err(Error::new("E_STATE", "unsafe workspace receipt"));
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    if value["namespace"] != "tool.copy-on-write-workspace" || value["schema_version"] != 1 {
        return Err(Error::new(
            "E_STATE",
            "unknown workspace receipt; refusing to overwrite it",
        ));
    }
    Ok(Some(value))
}
pub fn write_receipt(repo: &Repository, kind: &str, files: u64, logical_bytes: u64) -> Result<()> {
    receipt(repo)?;
    atomic_json(
        &repo.admin.join("tool-workspace.json"),
        &serde_json::json!({"schema_version":1,"namespace":"tool.copy-on-write-workspace","last_operation":kind,"files_at_operation":files,"shared_logical_bytes_at_operation":logical_bytes,"historical_only":true,"exclusive_savings_bytes":null}),
    )
}

// Failure injection is compiled only in debug builds and requires an explicit opt-in.
pub fn boundary(name: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    if std::env::var("SPRIG_TEST_FAULTS").as_deref() == Ok("1") {
        if std::env::var("SPRIG_TEST_PAUSE").as_deref() == Ok(name) {
            if let Ok(path) = std::env::var("SPRIG_TEST_READY") {
                fs::write(path, name)?;
            }
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(30) {
                if std::env::var_os("SPRIG_TEST_RELEASE").is_some_and(|p| Path::new(&p).exists()) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        if std::env::var("SPRIG_TEST_CRASH").as_deref() == Ok(name) {
            std::process::exit(91);
        }
        if std::env::var("SPRIG_TEST_FAIL").as_deref() == Ok(name) {
            return Err(Error::new(
                "E_IO",
                format!("injected disk/permission/cleanup error at {name}"),
            ));
        }
    }
    let _ = name;
    Ok(())
}
