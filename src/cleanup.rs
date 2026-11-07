//! Opt-in removal of ignored build outputs from receipted linked worktrees.
use crate::{
    config,
    git::{self, Repository},
    state, Error, Output, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const STATE: &str = "sprig-cleanup.json";
const PIN: &str = "sprig-cleanup-pin";
const LEASE: &str = "sprig-cleanup-lease";
const SKIP: &[&str] = &[".git", "node_modules", ".venv", "venv"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    fn new(m: &fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            mode: m.mode(),
            uid: m.uid(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
    fn directory(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
    }
    fn regular(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
    }
}
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema_version: u32,
    root: PathBuf,
    device: u64,
    inode: u64,
    fingerprint: String,
    policy: String,
    stable_since: u64,
    last_scan: u64,
    #[serde(default)]
    history: Vec<Value>,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
struct Candidate {
    path: PathBuf,
    logical_bytes: u64,
    reason: Option<String>,
}
struct Scan {
    stamps: BTreeMap<PathBuf, Stamp>,
    fingerprint: String,
    candidates: Vec<Candidate>,
}
fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .map_err(|e| Error::new("E_STATE", e.to_string()))
}
fn boundary_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| SKIP.iter().any(|s| name.eq_ignore_ascii_case(s)))
}
fn git_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(".git"))
}
fn folded(path: &Path) -> PathBuf {
    PathBuf::from(path.as_os_str().to_ascii_lowercase())
}
fn cstring(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| Error::new("E_STATE", "path contains NUL"))
}
fn directory_at(parent: &File, name: &OsStr) -> Result<File> {
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn open_root(repo: &Repository) -> Result<File> {
    crate::platform::validate_workspace_parent(&repo.root)?;
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&repo.root)?)
}
fn relative_dir(root: &File, path: &Path) -> Result<File> {
    let mut dir = root.try_clone()?;
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(Error::new("E_STATE", "invalid relative directory"));
        }
        dir = directory_at(&dir, component.as_os_str())?;
    }
    Ok(dir)
}
fn names(dir: &File) -> Result<Vec<OsString>> {
    // A fresh description avoids sharing readdir's offset with the pinned descriptor.
    let copy = directory_at(dir, OsStr::new("."))?;
    use std::os::fd::IntoRawFd;
    let fd = copy.into_raw_fd();
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(std::io::Error::last_os_error().into());
    }
    let result = (|| {
        let mut result = Vec::new();
        loop {
            // POSIX distinguishes end-of-directory from failure through errno.
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(0) {
                    return Err(e.into());
                }
                break;
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes != b"." && bytes != b".." {
                result.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        result.sort();
        Ok(result)
    })();
    unsafe {
        libc::closedir(stream);
    }
    result
}
fn stamp_at(dir: &File, name: &OsStr) -> Result<Stamp> {
    let name = cstring(name)?;
    let mut raw = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            raw.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let s = unsafe { raw.assume_init() };
    #[cfg(target_os = "macos")]
    let (mtime, ctime) = ((s.st_mtime, s.st_mtime_nsec), (s.st_ctime, s.st_ctime_nsec));
    #[cfg(not(target_os = "macos"))]
    let (mtime, ctime) = ((s.st_mtime, s.st_mtime_nsec), (s.st_ctime, s.st_ctime_nsec));
    Ok(Stamp {
        dev: s.st_dev as u64,
        ino: s.st_ino,
        mode: s.st_mode as u32,
        uid: s.st_uid,
        len: s.st_size as u64,
        mtime,
        ctime,
    })
}
fn walk(
    dir: &File,
    relative: &Path,
    device: u64,
    stamps: &mut BTreeMap<PathBuf, Stamp>,
    submodules: &BTreeSet<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > 128 {
        return Err(Error::new(
            "E_STATE",
            "cleanup scan exceeds maximum directory depth",
        ));
    }
    for name in names(dir)? {
        if relative.as_os_str().is_empty() && git_name(&name) {
            continue;
        }
        let path = relative.join(&name);
        let stamp = stamp_at(dir, &name)?;
        let descend = stamp.directory()
            && stamp.dev == device
            && !boundary_name(&name)
            && !submodules.contains(&path);
        let before = stamp.clone();
        stamps.insert(path.clone(), stamp);
        if descend {
            let child = directory_at(dir, &name)?;
            if Stamp::new(&child.metadata()?) != before {
                return Err(Error::new(
                    "E_CHANGED",
                    "directory changed during cleanup scan",
                ));
            }
            // Nested repositories are boundaries, even when their .git is ignored.
            if let Some(marker) = names(&child)?.iter().find(|s| git_name(s)) {
                stamps.insert(path.join(marker), stamp_at(&child, marker)?);
                continue;
            }
            walk(&child, &path, device, stamps, submodules, depth + 1)?;
            if stamp_at(dir, &name)? != before {
                return Err(Error::new(
                    "E_CHANGED",
                    "directory changed during cleanup scan",
                ));
            }
        }
    }
    Ok(())
}
fn paths(bytes: &[u8]) -> Result<BTreeSet<PathBuf>> {
    bytes
        .split(|v| *v == 0)
        .filter(|v| !v.is_empty())
        .map(|v| {
            let s = std::str::from_utf8(v).map_err(|_| {
                Error::new("E_UNSUPPORTED", "non-UTF-8 cleanup paths are unsupported")
            })?;
            Ok(PathBuf::from(s))
        })
        .collect()
}
fn scan(repo: &Repository, root: &File, policy: &config::CleanupPolicy) -> Result<Scan> {
    let index = repo.index()?;
    let tracked: BTreeSet<PathBuf> = index
        .iter()
        .chain(repo.tree("HEAD")?.iter())
        .map(|e| PathBuf::from(&e.path))
        .collect();
    let tracked_folded: BTreeSet<_> = tracked.iter().map(|p| folded(p)).collect();
    let mut tracked_inodes = BTreeSet::new();
    for path in &tracked {
        match fs::symlink_metadata(repo.root.join(path)) {
            Ok(m) => {
                tracked_inodes.insert((m.dev(), m.ino()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    let mut excluded_inodes = BTreeSet::new();
    for path in &policy.exclude {
        let absolute = repo.root.join(path);
        for ancestor in absolute
            .ancestors()
            .take_while(|p| p.starts_with(&repo.root))
        {
            match fs::symlink_metadata(ancestor) {
                Ok(m) => {
                    excluded_inodes.insert((m.dev(), m.ino()));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    let submodules = index
        .iter()
        .filter(|e| e.mode == "160000")
        .map(|e| PathBuf::from(&e.path))
        .collect();
    let mut stamps = BTreeMap::new();
    let root_stamp = Stamp::new(&root.metadata()?);
    walk(
        root,
        Path::new(""),
        root_stamp.dev,
        &mut stamps,
        &submodules,
        0,
    )?;
    let mut candidates: Vec<Candidate> = Vec::new();
    for (path, stamp) in &stamps {
        if !stamp.directory()
            || !policy.folders.iter().any(|name| {
                path.file_name()
                    .is_some_and(|v| v.eq_ignore_ascii_case(OsStr::new(name)))
            })
            || candidates.iter().any(|c| path.starts_with(&c.path))
        {
            continue;
        }
        let mut candidate = Candidate {
            path: path.clone(),
            logical_bytes: 0,
            reason: None,
        };
        if policy.exclude.iter().any(|p| {
            folded(path).starts_with(folded(Path::new(p)))
                || folded(Path::new(p)).starts_with(folded(path))
        }) {
            candidate.reason = Some("excluded".into());
        }
        for (child, value) in stamps.range(path.clone()..) {
            if !child.starts_with(path) {
                break;
            }
            if excluded_inodes.contains(&(value.dev, value.ino)) {
                candidate.reason = Some("excluded".into());
            }
            if value.regular() {
                candidate.logical_bytes = candidate.logical_bytes.saturating_add(value.len);
            }
            if value.dev != root_stamp.dev
                || value.uid != unsafe { libc::geteuid() }
                || value.mode & 0o022 != 0
                || (!value.directory() && !value.regular())
                || child.components().any(|c| boundary_name(c.as_os_str()))
            {
                candidate.reason = Some("unsafe_entry_or_dependency_boundary".into());
            }
        }
        if path
            .ancestors()
            .filter_map(|p| stamps.get(p))
            .any(|s| excluded_inodes.contains(&(s.dev, s.ino)))
        {
            candidate.reason = Some("excluded".into());
        }
        let path_folded = folded(path);
        if tracked_folded
            .range(path_folded.clone()..)
            .next()
            .is_some_and(|p| p.starts_with(&path_folded))
            || stamps
                .range(path.clone()..)
                .take_while(|(p, _)| p.starts_with(path))
                .any(|(_, s)| tracked_inodes.contains(&(s.dev, s.ino)))
        {
            candidate.reason = Some("contains_tracked_files".into());
        }
        candidates.push(candidate);
    }
    let mut input = Vec::new();
    let mut wanted = BTreeSet::new();
    for candidate in &candidates {
        if candidate.reason.is_some() {
            continue;
        }
        for path in stamps
            .range(candidate.path.clone()..)
            .map(|(p, _)| p)
            .take_while(|p| p.starts_with(&candidate.path))
        {
            let text = path
                .to_str()
                .ok_or_else(|| Error::new("E_UNSUPPORTED", "non-UTF-8 cleanup path"))?;
            input.extend_from_slice(text.as_bytes());
            input.push(0);
            wanted.insert(path.clone());
        }
    }
    let ignored = if input.is_empty() {
        BTreeSet::new()
    } else {
        paths(
            &git::run(
                &repo.root,
                ["check-ignore", "--no-index", "-z", "--stdin"],
                Some(&input),
            )
            .or_else(|e| {
                if e.data["git_exit_code"] == 1 {
                    Ok(Vec::new())
                } else {
                    Err(e)
                }
            })?,
        )?
    };
    for candidate in &mut candidates {
        if candidate.reason.is_none()
            && wanted
                .range(candidate.path.clone()..)
                .take_while(|p| p.starts_with(&candidate.path))
                .any(|p| !ignored.contains(p))
        {
            candidate.reason = Some("contains_non_ignored_entries".into());
        }
    }
    let mut hash = blake3::Hasher::new();
    hash.update(repo.head()?.as_bytes());
    hash.update(&fs::read(repo.admin.join("index"))?);
    hash.update(&serde_json::to_vec(&stamps)?);
    hash.update(&serde_json::to_vec(&root_stamp)?);
    for path in [
        config::user_path()?,
        repo.common.join("sprig.toml"),
        repo.common.join("tool.toml"),
        repo.admin.join(LEASE),
    ] {
        match fs::symlink_metadata(path) {
            Ok(m) => {
                hash.update(&serde_json::to_vec(&Stamp::new(&m))?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Scan {
        stamps,
        fingerprint: hash.finalize().to_hex().to_string(),
        candidates,
    })
}
fn read_observation(repo: &Repository) -> Result<Option<Observation>> {
    let path = repo.admin.join(STATE);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0 => {
            let value: Observation =
                serde_json::from_reader(crate::platform::open_regular(&path)?)?;
            if value.schema_version != 1 {
                return Err(Error::new("E_STATE", "unsupported cleanup state"));
            }
            Ok(Some(value))
        }
        Ok(_) => Err(Error::new("E_STATE", "unsafe cleanup state")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn managed(repo: &Repository) -> Result<bool> {
    Ok(repo.admin != repo.common && state::receipt(repo)?.is_some())
}
fn lease(repo: &Repository, exclusive: bool, create: bool) -> Result<Option<File>> {
    let file = match OpenOptions::new()
        .create(create)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(repo.admin.join(LEASE))
    {
        Ok(file) => file,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
        || m.nlink() != 1
    {
        return Err(Error::new("E_STATE", "unsafe cleanup lease"));
    }
    if unsafe {
        libc::flock(
            file.as_raw_fd(),
            (if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            }) | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(Error::new("E_BUSY", "worktree has an active cleanup lease"));
    }
    Ok(Some(file))
}
fn pinned(repo: &Repository) -> Result<bool> {
    match fs::symlink_metadata(repo.admin.join(PIN)) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
/// Open files, cwd and mapped executables protect non-cooperating builds and apps.
type Usage = std::result::Result<Vec<PathBuf>, String>;
fn open_paths() -> Result<Vec<PathBuf>> {
    let executable = if cfg!(target_os = "macos") {
        "/usr/sbin/lsof"
    } else {
        "lsof"
    };
    let child = Command::new(executable)
        .args([
            "-nP",
            "-F0pn",
            "-u",
            &unsafe { libc::geteuid() }.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let lsof_pid = child.id();
    let output = child.wait_with_output()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(Error::new(
            "E_BUSY",
            "cannot verify open files with lsof; cleanup skipped",
        ));
    }
    parse_open_paths(&output.stdout, &[std::process::id(), lsof_pid])
}
fn decoded_name(value: &[u8]) -> Vec<u8> {
    // lsof escapes control bytes even in NUL-delimited output. Keep both the
    // literal and decoded spelling, since a real filename can contain a backslash.
    let mut result = Vec::new();
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'\\' && i + 1 < value.len() {
            let escaped = match value[i + 1] {
                b'b' => Some(8),
                b'f' => Some(12),
                b'r' => Some(13),
                b'n' => Some(10),
                b't' => Some(9),
                _ => None,
            };
            if let Some(byte) = escaped {
                result.push(byte);
                i += 2;
                continue;
            }
            if value[i + 1] == b'x' && i + 3 < value.len() {
                if let (Some(a), Some(b)) = (
                    (value[i + 2] as char).to_digit(16),
                    (value[i + 3] as char).to_digit(16),
                ) {
                    result.push((a * 16 + b) as u8);
                    i += 4;
                    continue;
                }
            }
        }
        if value[i] == b'^' && i + 1 < value.len() && (b'@'..=b'_').contains(&value[i + 1]) {
            result.push(value[i + 1] - b'@');
            i += 2;
            continue;
        }
        result.push(value[i]);
        i += 1;
    }
    result
}
fn parse_open_paths(bytes: &[u8], excluded: &[u32]) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    let mut pid = None;
    let mut saw_name = false;
    for field in bytes.split(|b| *b == 0) {
        let line = field.strip_prefix(b"\n").unwrap_or(field);
        if let Some(value) = line.strip_prefix(b"p") {
            pid = Some(
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .ok_or_else(|| {
                        Error::new("E_BUSY", "invalid lsof process record; cleanup skipped")
                    })?,
            );
        }
        if let Some(value) = line.strip_prefix(b"n") {
            let process = pid.ok_or_else(|| {
                Error::new("E_BUSY", "lsof filename without process; cleanup skipped")
            })?;
            saw_name = true;
            if excluded.contains(&process) {
                continue;
            }
            for name in [value.to_vec(), decoded_name(value)] {
                let path = PathBuf::from(OsString::from_vec(name));
                if path.is_absolute() {
                    paths.insert(path);
                }
            }
        }
    }
    if pid.is_none() || !saw_name {
        return Err(Error::new(
            "E_BUSY",
            "incomplete lsof output; cleanup skipped",
        ));
    }
    Ok(paths.into_iter().collect())
}
fn in_use(repo: &Repository, paths: &Usage) -> Result<bool> {
    let paths = paths
        .as_ref()
        .map_err(|e| Error::new("E_BUSY", e.clone()))?;
    let root_folded = folded(&repo.root);
    if paths.iter().any(|p| folded(p).starts_with(&root_folded)) {
        return Ok(true);
    }
    let root = fs::symlink_metadata(&repo.root)?;
    let depth = repo.root.components().count();
    let ancestors: BTreeSet<_> = paths
        .iter()
        .filter_map(|p| p.ancestors().find(|a| a.components().count() == depth))
        .collect();
    // APFS also aliases Unicode normalization and non-ASCII case variants.
    for path in ancestors {
        match fs::symlink_metadata(path) {
            Ok(m) if m.dev() == root.dev() && m.ino() == root.ino() => return Ok(true),
            Ok(_) => (),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}
fn delete_candidate(root: &File, scan: &Scan, candidate: &Candidate) -> Result<u64> {
    let mut removed = 0;
    // Delete only observed entries through pinned, no-follow directory descriptors.
    // Unexpected files survive; rmdir then fails rather than recursively removing them.
    let mut entries: Vec<_> = scan
        .stamps
        .iter()
        .filter(|(p, _)| p.starts_with(&candidate.path))
        .collect();
    entries.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    for (path, stamp) in entries {
        let parent = relative_dir(
            root,
            path.parent()
                .ok_or_else(|| Error::new("E_STATE", "missing output parent"))?,
        )?;
        let name = path
            .file_name()
            .ok_or_else(|| Error::new("E_STATE", "missing output name"))?;
        let current = stamp_at(&parent, name)?;
        // Removing children changes a directory's timestamps and size, but not its identity.
        if if stamp.directory() {
            current.dev != stamp.dev
                || current.ino != stamp.ino
                || current.mode != stamp.mode
                || current.uid != stamp.uid
        } else {
            current != *stamp
        } {
            return Err(Error::new(
                "E_CHANGED",
                "output changed during cleanup; remaining entries retained",
            ));
        }
        let name = cstring(name)?;
        if unsafe {
            libc::unlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                if stamp.directory() {
                    libc::AT_REMOVEDIR
                } else {
                    0
                },
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if stamp.regular() {
            removed += stamp.len;
        }
    }
    Ok(removed)
}
fn worktree(repo: &Repository, dry_run: bool, usage: &mut Option<Usage>) -> Result<Value> {
    let policy = config::effective(Some(repo), None)?.cleanup;
    if !managed(repo)? {
        return Ok(json!({"worktree":repo.root,"reason":"not_sprig_managed"}));
    }
    if !dry_run && !policy.enabled {
        return Ok(
            json!({"worktree":repo.root,"policy":policy,"reason":"disabled","dry_run":false}),
        );
    }
    let _lock = if dry_run {
        None
    } else {
        Some(state::Lock::acquire(repo)?)
    };
    let (_lease, leased) = match lease(repo, true, !dry_run) {
        Ok(file) => (if dry_run { None } else { file }, false),
        Err(e) if e.code == "E_BUSY" => (None, true),
        Err(e) => return Err(e),
    };
    repo.idle()?;
    state::require_recovered(repo)?;
    let root = open_root(repo)?;
    let stamp = Stamp::new(&root.metadata()?);
    let scan = scan(repo, &root, &policy)?;
    let before = read_observation(repo)?;
    let now = now()?;
    let policy_hash = blake3::hash(&serde_json::to_vec(&policy)?)
        .to_hex()
        .to_string();
    let unchanged = before.as_ref().is_some_and(|s| {
        s.root == repo.root
            && s.device == stamp.dev
            && s.inode == stamp.ino
            && s.fingerprint == scan.fingerprint
            && s.policy == policy_hash
            && s.last_scan <= now
            && s.stable_since <= now
    });
    let pin = pinned(repo)?;
    let busy_result = in_use(
        repo,
        usage.get_or_insert_with(|| open_paths().map_err(|e| e.message)),
    );
    let activity_error = busy_result.as_ref().err().map(|e| e.message.clone());
    let busy = busy_result.unwrap_or(true);
    let since = if unchanged && !pin && !busy && !leased {
        before.as_ref().map_or(now, |s| s.stable_since)
    } else {
        now
    };
    let reason = if !policy.enabled {
        "disabled"
    } else if pin {
        "pinned"
    } else if leased {
        "leased"
    } else if activity_error.is_some() {
        "activity_check_failed"
    } else if busy {
        "worktree_in_use"
    } else if !unchanged {
        "observing"
    } else if now.saturating_sub(since) < policy.idle_hours * 3600 {
        "waiting_for_idle"
    } else {
        "eligible"
    };
    let mut observation = Observation {
        schema_version: 1,
        root: repo.root.clone(),
        device: stamp.dev,
        inode: stamp.ino,
        fingerprint: scan.fingerprint.clone(),
        policy: policy_hash,
        stable_since: since,
        last_scan: now,
        history: before.map_or_else(Vec::new, |s| s.history),
    };
    let mut result = json!({"worktree":repo.root,"policy":policy,"reason":reason,"dry_run":dry_run,"stable_seconds":now.saturating_sub(since),"candidates":scan.candidates,"removed_logical_bytes":0,"history":observation.history});
    if let Some(error) = activity_error {
        result["activity_error"] = json!(error);
    }
    if !dry_run && policy.enabled {
        state::atomic_json(&repo.admin.join(STATE), &observation)?;
        if reason == "eligible" {
            let _index_lock = gix_lock::File::acquire_to_update_resource(
                repo.admin.join("index"),
                gix_lock::acquire::Fail::Immediately,
                None,
            )
            .map_err(|e| Error::new("E_BUSY", format!("cannot protect Git index: {e}")))?;
            state::boundary("cleanup_before_delete")?;
            repo.idle_except_cleanup_index()?;
            let recheck = scan_check(repo, &root, &policy, &scan)?;
            if !recheck
                || pinned(repo)?
                || in_use(repo, &open_paths().map_err(|e| e.message))?
                || config::effective(Some(repo), None)?.cleanup.enabled != policy.enabled
            {
                return Err(Error::new(
                    "E_CHANGED",
                    "worktree or policy changed before cleanup",
                ));
            }
            let mut removed = 0;
            let mut completed = Vec::new();
            for candidate in &scan.candidates {
                if candidate.reason.is_none() {
                    if let Err(e) = delete_candidate(&root, &scan, candidate) {
                        observation.history.push(json!({"at":now,"error":e.message,"completed":completed,"removed_logical_bytes":removed}));
                        if observation.history.len() > 20 {
                            observation.history.remove(0);
                        }
                        observation.stable_since = now;
                        state::atomic_json(&repo.admin.join(STATE), &observation)?;
                        return Err(e.with_data(json!({"worktree":repo.root,"completed":completed,"removed_logical_bytes":removed})));
                    }
                    removed += candidate.logical_bytes;
                    completed.push(candidate.path.clone());
                }
            }
            observation
                .history
                .push(json!({"at":now,"paths":completed,"removed_logical_bytes":removed}));
            if observation.history.len() > 20 {
                observation.history.remove(0);
            }
            observation.stable_since = now;
            // The next scan establishes a fresh fingerprint after our own deletions.
            observation.fingerprint.clear();
            state::atomic_json(&repo.admin.join(STATE), &observation)?;
            result["removed_logical_bytes"] = json!(removed);
            result["reason"] = json!("cleaned");
            result["history"] = json!(observation.history);
        }
    }
    Ok(result)
}
fn scan_check(
    repo: &Repository,
    root: &File,
    policy: &config::CleanupPolicy,
    before: &Scan,
) -> Result<bool> {
    if fs::symlink_metadata(&repo.root)?.ino() != root.metadata()?.ino() {
        return Ok(false);
    }
    let after = scan(repo, root, policy)?;
    Ok(after.fingerprint == before.fingerprint
        && after.candidates == before.candidates
        && serde_json::to_vec(&config::effective(Some(repo), None)?.cleanup)?
            == serde_json::to_vec(policy)?)
}
pub fn run(repositories: &[PathBuf], dry_run: bool) -> Result<Output> {
    let defaults = vec![PathBuf::from(".")];
    let repositories = if repositories.is_empty() {
        &defaults
    } else {
        repositories
    };
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut usage = None;
    for path in repositories {
        let repo = match Repository::open(path) {
            Ok(r) => r,
            Err(e) => {
                errors.push(json!({"repository":path,"error":e.message}));
                continue;
            }
        };
        if !seen.insert(repo.common.clone()) {
            continue;
        }
        let worktrees = match repo.worktrees() {
            Ok(paths) => paths,
            Err(e) => {
                errors.push(json!({"repository":path,"error":e.message}));
                continue;
            }
        };
        for path in worktrees {
            let result = Repository::open(&path).and_then(|r| worktree(&r, dry_run, &mut usage));
            match result {
                Ok(v) => {
                    if let Some(error) = v["activity_error"].as_str() {
                        errors.push(json!({"worktree":path,"error":error}));
                    }
                    rows.push(v);
                }
                Err(e) => errors
                    .push(json!({"worktree":path,"code":e.code,"error":e.message,"data":e.data})),
            }
        }
    }
    let mut output = Output::success(
        json!({"dry_run":dry_run,"worktrees":rows,"errors":errors,"bytes_note":"Logical output bytes; APFS sharing and snapshots affect actual reclaimed space. Times in state and JSON are Unix seconds in UTC."}),
    );
    if !errors.is_empty() {
        output.warn_code(
            "W_CLEANUP_SKIPPED",
            "some worktrees could not be inspected or cleaned; see errors",
        );
    }
    Ok(output)
}
pub fn pin(path: &Path, remove: bool) -> Result<Output> {
    let repo = Repository::open(path)?;
    if path.canonicalize()? != repo.root || !managed(&repo)? {
        return Err(Error::new(
            "E_ARGUMENT",
            "pin requires an exact Sprig linked-worktree root",
        ));
    }
    let _lock = state::Lock::acquire(&repo)?;
    if remove {
        match fs::remove_file(repo.admin.join(PIN)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    } else {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(repo.admin.join(PIN))
        {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let m = fs::symlink_metadata(repo.admin.join(PIN))?;
                if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
                    return Err(Error::new("E_STATE", "unsafe cleanup pin"));
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    // Pin transitions always require a new full observation period.
    if let Some(mut s) = read_observation(&repo)? {
        s.fingerprint.clear();
        state::atomic_json(&repo.admin.join(STATE), &s)?;
    }
    Ok(Output::success(
        json!({"worktree":repo.root,"pinned":!remove}),
    ))
}
pub fn hold(path: &Path, command: &[OsString]) -> Result<Output> {
    let repo = Repository::open(path)?;
    if !managed(&repo)? {
        return Err(Error::new(
            "E_ARGUMENT",
            "hold requires a Sprig linked worktree",
        ));
    }
    let started = std::time::Instant::now();
    let lease = loop {
        match lease(&repo, false, true) {
            Ok(Some(file)) => break file,
            Ok(None) => return Err(Error::new("E_STATE", "cleanup lease unavailable")),
            Err(e)
                if e.code == "E_BUSY" && started.elapsed() < std::time::Duration::from_secs(60) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            Err(e) => return Err(e),
        }
    };
    (&lease).write_all(b"active\n")?;
    let (program, args) = command
        .split_first()
        .ok_or_else(|| Error::new("E_ARGUMENT", "hold requires a command"))?;
    let status = Command::new(program)
        .args(args)
        .current_dir(&repo.root)
        .stdout(std::process::Stdio::from(
            std::io::stderr().as_fd().try_clone_to_owned()?,
        ))
        .status();
    (&lease).write_all(b"finished\n")?;
    let status = status?;
    if !status.success() {
        return Err(Error::new(
            "E_COMMAND",
            format!("held command exited with {status}"),
        ));
    }
    Ok(Output::success(
        json!({"worktree":repo.root,"command_completed":true}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lsof_protocol_preserves_paths_and_fails_closed() -> Result<()> {
        let paths = parse_open_paths(
            b"p101\0\nfcwd\0n/tmp/work tree\0\np102\0\nf3\0n/tmp/line\\nname\0",
            &[101],
        )?;
        assert!(paths.contains(&PathBuf::from("/tmp/line\nname")));
        assert!(paths.contains(&PathBuf::from("/tmp/line\\nname")));
        assert!(!paths.contains(&PathBuf::from("/tmp/work tree")));
        assert!(parse_open_paths(b"p101\nn/tmp/work tree\n", &[]).is_err());
        assert!(parse_open_paths(b"p101\0", &[]).is_err());
        assert!(parse_open_paths(b"n/tmp/orphan\0", &[]).is_err());
        Ok(())
    }
}
