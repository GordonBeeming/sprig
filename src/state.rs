use crate::{
    git::Repository,
    platform::{self, Fingerprint},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
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
    publish_json(path, value, None)
}
fn publish_json<T: Serialize>(path: &Path, value: &T, stage: Option<&str>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("E_STATE", "state path has no parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temp.write_all(&serde_json::to_vec_pretty(value)?)?;
    temp.as_file().sync_all()?;
    boundary("journal_before_publish")?;
    if let Some(stage) = stage {
        boundary(&format!("journal_before_publish_{stage}"))?;
    }
    temp.persist(path).map_err(|e| Error::from(e.error))?;
    if let Some(stage) = stage {
        boundary(&format!("journal_after_publish_{stage}"))?;
    }
    platform::sync_dir(parent)
}
#[derive(Debug, Serialize, Deserialize)]
pub struct CreationOwnership {
    pub destination_identity: Option<(u64, u64)>,
    pub requested_new_branch: Option<String>,
    pub confirmed_new_branch: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneDirectory {
    pub path: PathBuf,
    pub identity: (u64, u64),
    pub gid: u32,
}
impl CloneDirectory {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(".tool-clones-")
            .tempdir_in(root)?;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))?;
        let m = fs::symlink_metadata(dir.path())?;
        platform::sync_dir(dir.path())?;
        platform::sync_dir(root)?;
        Ok(Self {
            path: dir.keep(),
            identity: (m.dev(), m.ino()),
            gid: m.gid(),
        })
    }
    pub(crate) fn validate(&self, root: &Path) -> Result<bool> {
        if self.path.parent() != Some(root)
            || !self
                .path
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with(".tool-clones-"))
        {
            return Err(Error::new("E_STATE", "unsafe clone directory"));
        }
        let m = match fs::symlink_metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !m.is_dir()
            || (m.dev(), m.ino()) != self.identity
            || m.uid() != unsafe { libc::geteuid() }
            || m.gid() != self.gid
            || m.mode() & 0o077 != 0
        {
            return Err(Error::new(
                "E_CHANGED",
                "clone directory identity or permissions changed; files retained",
            ));
        }
        Ok(true)
    }
    pub(crate) fn remove_if_empty(&self, root: &Path) -> Result<bool> {
        if !self.validate(root)? {
            return Ok(true);
        }
        match fs::remove_dir(&self.path) {
            Ok(()) => {
                platform::sync_dir(root)?;
                Ok(true)
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_directory: Option<CloneDirectory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_clone: Option<Fingerprint>,
}
pub struct Record {
    pub path: PathBuf,
    pub journal: Journal,
    pub entry: Option<usize>,
    _reservation: Option<tempfile::TempPath>,
}
impl Record {
    pub fn new(repo: &Repository, journal: Journal) -> Result<Self> {
        let record = Self::pending(repo, journal)?;
        record.save()?;
        Ok(record)
    }
    pub(crate) fn pending(repo: &Repository, journal: Journal) -> Result<Self> {
        let dir = repo.common.join("tool/operations");
        private_dir(&repo.common.join("tool"))?;
        private_dir(&dir)?;
        for _ in 0..128 {
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
            // The temporary ID lives in a separate namespace from published
            // journals. Never let a repeated random ID replace another record.
            match fs::symlink_metadata(&path) {
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self {
                        path,
                        journal,
                        entry: None,
                        _reservation: Some(file.into_temp_path()),
                    })
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::new(
            "E_STATE",
            "could not allocate a unique journal name",
        ))
    }

    pub(crate) fn batch_entry(&self, entry: usize, journal: Journal) -> Self {
        Self {
            path: self.path.clone(),
            journal,
            entry: Some(entry),
            _reservation: None,
        }
    }
    pub(crate) fn sibling_name(&self) -> Result<String> {
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::new("E_STATE", "invalid journal name"))?;
        Ok(match self.entry {
            Some(entry) => format!(".tool-clone-{stem}-{entry}"),
            None => format!(".tool-clone-{stem}"),
        })
    }
    pub fn document_schema_version(&self) -> u32 {
        if self.entry.is_some() {
            3
        } else {
            self.journal.schema_version
        }
    }
    pub fn save(&self) -> Result<()> {
        atomic_json(&self.path, &self.journal)
    }
    pub(crate) fn finish_pending(self) -> Result<()> {
        fs::remove_file(&self.path)?;
        platform::sync_pending(&File::open(
            self.path
                .parent()
                .ok_or_else(|| Error::new("E_STATE", "invalid journal path"))?,
        )?)
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

pub(crate) fn checkpoint(repo: &Repository) -> Result<()> {
    platform::sync_pending(&File::open(&repo.common)?)?;
    platform::sync_pending(&File::open(repo.common.join("tool"))?)?;
    platform::sync_dir(&repo.common.join("tool/operations"))
}

#[derive(Serialize)]
struct BatchView<'a> {
    schema_version: u32,
    kind: &'static str,
    files: Vec<BatchEntryView<'a>>,
}
#[derive(Serialize)]
struct BatchEntryView<'a> {
    slot: usize,
    journal: &'a Journal,
}
#[derive(Deserialize)]
struct BatchDocument {
    schema_version: u32,
    kind: String,
    files: Vec<BatchEntry>,
}
#[derive(Deserialize)]
struct BatchEntry {
    slot: usize,
    journal: Journal,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredJournal {
    Batch(BatchDocument),
    Single(Box<Journal>),
}

pub(crate) fn save_batch<'a>(
    repo: &Repository,
    records: impl IntoIterator<Item = &'a Record>,
    stage: &str,
) -> Result<()> {
    let records: Vec<_> = records.into_iter().collect();
    let first = records
        .first()
        .ok_or_else(|| Error::new("E_STATE", "empty journal batch"))?;
    let mut slots = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let files = records
        .iter()
        .map(|record| {
            let slot = record
                .entry
                .ok_or_else(|| Error::new("E_STATE", "missing journal entry slot"))?;
            if record.path != first.path
                || record.journal.root != first.journal.root
                || record.journal.clone_directory != first.journal.clone_directory
                || !slots.insert(slot)
                || !targets.insert(&record.journal.target)
            {
                return Err(Error::new("E_STATE", "inconsistent journal batch"));
            }
            Ok(BatchEntryView {
                slot,
                journal: &record.journal,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    platform::sync_pending(&File::open(&repo.common)?)?;
    platform::sync_pending(&File::open(repo.common.join("tool"))?)?;
    publish_json(
        &first.path,
        &BatchView {
            schema_version: 3,
            kind: "convert_batch",
            files,
        },
        Some(stage),
    )
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
            let stored: StoredJournal = serde_json::from_slice(&fs::read(&path)?).map_err(|e| {
                Error::new(
                    "E_STATE",
                    format!("cannot read journal {}: {e}", path.display()),
                )
            })?;
            match stored {
                StoredJournal::Single(journal) => {
                    let journal = *journal;
                    if ![1, 2].contains(&journal.schema_version) {
                        return Err(Error::new("E_STATE", "unknown journal schema"));
                    }
                    records.push(Record {
                        path,
                        journal,
                        entry: None,
                        _reservation: None,
                    });
                }
                StoredJournal::Batch(batch) => {
                    if batch.schema_version != 3
                        || batch.kind != "convert_batch"
                        || batch.files.is_empty()
                    {
                        return Err(Error::new("E_STATE", "invalid batch journal"));
                    }
                    let mut slots = BTreeSet::new();
                    let mut targets = BTreeSet::new();
                    let mut root = None;
                    let mut directory = None;
                    for file in batch.files {
                        let journal = file.journal;
                        if journal.schema_version != 2
                            || journal.kind != "convert"
                            || journal.clone_directory.is_none()
                            || !slots.insert(file.slot)
                            || !targets.insert(journal.target.clone())
                            || root.as_ref().is_some_and(|root| root != &journal.root)
                            || directory
                                .as_ref()
                                .is_some_and(|dir| Some(dir) != journal.clone_directory.as_ref())
                        {
                            return Err(Error::new("E_STATE", "inconsistent batch journal entry"));
                        }
                        root = Some(journal.root.clone());
                        directory = journal.clone_directory.clone();
                        records.push(Record {
                            path: path.clone(),
                            journal,
                            entry: Some(file.slot),
                            _reservation: None,
                        });
                    }
                }
            }
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
