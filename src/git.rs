use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
    pub common: PathBuf,
    pub admin: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
fn stamp(path: &Path) -> Result<Option<Stamp>> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some(Stamp {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug)]
pub struct Snapshot {
    bytes: Vec<u8>,
    watched: FileStamps,
}
type FileStamps = Vec<(PathBuf, Option<Stamp>)>;
impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}
impl Eq for Snapshot {}
fn stamps_unchanged(watched: &[(PathBuf, Option<Stamp>)]) -> Result<bool> {
    for (path, before) in watched {
        if stamp(path)? != *before {
            return Ok(false);
        }
    }
    Ok(true)
}
impl Snapshot {
    pub fn unchanged(&self, repo: &Repository) -> Result<bool> {
        if !self.watched.is_empty() && stamps_unchanged(&self.watched)? {
            return Ok(true);
        }
        Ok(self == &repo.snapshot()?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub mode: String,
    pub oid: String,
    pub path: String,
}

pub fn run<I, S>(root: &Path, args: I, input: Option<&[u8]>) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .args(["-c", "diff.autoRefreshIndex=false"])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A caller's Git context must never redirect the operation to a different repository/index.
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    let mut child = command.spawn()?;
    let writer = child.stdin.take().map(|mut stdin| {
        let bytes = input.unwrap_or_default().to_vec();
        std::thread::spawn(move || stdin.write_all(&bytes))
    });
    let output = child.wait_with_output()?;
    if let Some(writer) = writer {
        let result = writer
            .join()
            .map_err(|_| Error::new("E_GIT", "Git input writer terminated"))?;
        if output.status.success() {
            result?;
        }
    }
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::new(
            "E_GIT",
            if detail.is_empty() {
                format!("Git exited with status {}", output.status)
            } else {
                detail
            },
        )
        .with_data(serde_json::json!({"git_exit_code": output.status.code()})));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout)
}

fn raw_hashes(root: &Path, entries: &[&Entry]) -> Result<BTreeMap<String, String>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch", "-Z"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    let mut child = command.spawn()?;
    let mut writer = None;
    let result = (|| -> Result<_> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("E_GIT", "missing cat-file stdin"))?;
        let mut input = Vec::new();
        for entry in entries {
            if matches!(entry.mode.as_str(), "100644" | "100755") {
                input.extend_from_slice(entry.oid.as_bytes());
                input.push(0);
            }
        }
        writer = Some(std::thread::spawn(move || stdin.write_all(&input)));
        let mut stdout = BufReader::with_capacity(
            256 * 1024,
            child
                .stdout
                .take()
                .ok_or_else(|| Error::new("E_GIT", "missing cat-file stdout"))?,
        );
        let mut hashes = BTreeMap::new();
        for entry in entries {
            if !matches!(entry.mode.as_str(), "100644" | "100755") {
                continue;
            }
            let mut header = Vec::new();
            stdout.read_until(0, &mut header)?;
            let header = std::str::from_utf8(&header)
                .map_err(|_| Error::new("E_GIT", "invalid cat-file header"))?;
            let fields: Vec<_> = header.trim_end_matches('\0').split(' ').collect();
            if fields.len() != 3 || fields[0] != entry.oid || fields[1] != "blob" {
                return Err(Error::new("E_GIT", "unexpected cat-file response"));
            }
            let size: u64 = fields[2]
                .parse()
                .map_err(|_| Error::new("E_GIT", "invalid cat-file size"))?;
            let mut hash = blake3::Hasher::new();
            let copied = std::io::copy(&mut stdout.by_ref().take(size), &mut hash)?;
            let mut terminator = [1];
            stdout.read_exact(&mut terminator)?;
            if copied != size || terminator != [0] {
                return Err(Error::new("E_GIT", "truncated cat-file response"));
            }
            hashes.insert(entry.oid.clone(), hash.finalize().to_hex().to_string());
        }
        Ok(hashes)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let written = writer
        .map(|writer| {
            writer
                .join()
                .map_err(|_| Error::new("E_GIT", "cat-file input writer terminated"))
        })
        .transpose()?;
    let status = status?;
    if result.is_ok() {
        if let Some(written) = written {
            written?;
        }
    }
    if !status.success() && result.is_ok() {
        return Err(Error::new("E_GIT", "cat-file failed"));
    }
    result
}

pub fn rendered_hashes(root: &Path, entries: &[Entry]) -> Result<BTreeMap<String, String>> {
    let unique: BTreeMap<_, _> = entries
        .iter()
        .filter(|e| matches!(e.mode.as_str(), "100644" | "100755"))
        .map(|e| (e.oid.as_str(), e))
        .collect();
    let count = if unique.len() >= 128 { 4 } else { 1 };
    let mut chunks = vec![vec![]; count];
    for (index, entry) in unique.into_values().enumerate() {
        chunks[index % count].push(entry);
    }
    let mut objects = BTreeMap::new();
    for hashes in crate::parallel::map_bounded(chunks, count, |chunk| raw_hashes(root, &chunk))? {
        objects.extend(hashes);
    }
    let mut hashes: BTreeMap<_, _> = entries
        .iter()
        .filter(|e| matches!(e.mode.as_str(), "100644" | "100755"))
        .filter_map(|e| {
            objects
                .get(&e.oid)
                .map(|digest| (e.path.clone(), digest.clone()))
        })
        .collect();
    let mut input = Vec::new();
    for entry in entries {
        input.extend_from_slice(entry.path.as_bytes());
        input.push(0);
    }
    let attrs = text(run(
        root,
        [
            "check-attr",
            "-z",
            "--stdin",
            "text",
            "eol",
            "ident",
            "crlf",
        ],
        Some(&input),
    )?)?;
    let mut ident = BTreeSet::new();
    let mut crlf = BTreeSet::new();
    let mut lf = BTreeSet::new();
    let mut text_attributes = BTreeMap::new();
    let mut legacy_crlf = BTreeMap::new();
    for fields in attrs.split('\0').collect::<Vec<_>>().chunks_exact(3) {
        match (fields[1], fields[2]) {
            ("ident", value) if !["unspecified", "unset"].contains(&value) => {
                ident.insert(fields[0].to_string());
            }
            ("eol", "lf") => {
                lf.insert(fields[0].to_string());
            }
            ("eol", value) if !["unspecified", "unset"].contains(&value) => {
                crlf.insert(fields[0].to_string());
            }
            ("text", value) => {
                text_attributes.insert(fields[0], value);
            }
            ("crlf", value) => {
                legacy_crlf.insert(fields[0], value);
            }
            _ => {}
        }
    }
    let (autocrlf, input_only) = match line(root, &["config", "--get", "core.autocrlf"]) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" | "" => (true, false),
            "false" | "no" | "off" | "0" => (false, false),
            "input" => (false, true),
            _ => return Err(Error::new("E_CONFIG", "unsupported core.autocrlf value")),
        },
        Err(e) if e.data["git_exit_code"] == 1 => (false, false),
        Err(e) => return Err(e),
    };
    let core_crlf = !input_only
        && match line(root, &["config", "--get", "core.eol"]) {
            Ok(value) => !["lf", "native"].contains(&value.to_ascii_lowercase().as_str()),
            Err(e) if e.data["git_exit_code"] == 1 => false,
            Err(e) => return Err(e),
        };
    for entry in entries {
        let text = match text_attributes.get(entry.path.as_str()).copied() {
            Some(value @ ("set" | "unset" | "auto" | "input")) => value,
            _ => legacy_crlf
                .get(entry.path.as_str())
                .copied()
                .unwrap_or("unspecified"),
        };
        // Checkout's text conversion only expands LF to CRLF. LF-only checkouts need no
        // per-file filter process; ident and every possible CRLF case still go through Git.
        let filtered = ident.contains(&entry.path)
            || crlf.contains(&entry.path)
            || ((autocrlf || (core_crlf && matches!(text, "set" | "auto")))
                && !lf.contains(&entry.path));
        if hashes.contains_key(&entry.path) && filtered {
            // Batch headers describe object size, which can differ from the filtered byte count.
            let bytes = run(
                root,
                [
                    "cat-file",
                    "--filters",
                    &format!("--path={}", entry.path),
                    &entry.oid,
                ],
                None,
            )?;
            hashes.insert(
                entry.path.clone(),
                blake3::hash(&bytes).to_hex().to_string(),
            );
        }
    }
    Ok(hashes)
}

pub fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| Error::new("E_UNSUPPORTED", "non-UTF-8 Git output/path"))
}
pub fn line(root: &Path, args: &[&str]) -> Result<String> {
    let text = text(run(root, args, None)?)?;
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_string())
}
pub fn version() -> Result<String> {
    let value = line(Path::new("."), &["--version"])?;
    let parts: Vec<u32> = value
        .split_whitespace()
        .nth(2)
        .unwrap_or_default()
        .split('.')
        .take(2)
        .map(|s| s.parse().unwrap_or(0))
        .collect();
    if parts.as_slice() < [2, 43].as_slice() {
        return Err(Error::new(
            "E_UNSUPPORTED",
            format!("Git >= 2.43 required; found {value}"),
        ));
    }
    Ok(value)
}
pub fn safe_path(path: &str) -> Result<()> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path.split('/').any(|c| c.eq_ignore_ascii_case(".git"))
    {
        return Err(Error::new(
            "E_UNSUPPORTED",
            format!("unsafe tracked path: {path:?}"),
        ));
    }
    Ok(())
}
pub fn no_symlink_components(root: &Path, relative: &str) -> Result<PathBuf> {
    safe_path(relative)?;
    let mut path = root.to_path_buf();
    let components: Vec<_> = Path::new(relative).components().collect();
    for (i, part) in components.iter().enumerate() {
        path.push(part);
        if i + 1 < components.len() && std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(Error::new("E_CHANGED", "symlink parent encountered"));
        }
    }
    Ok(path)
}

impl Repository {
    pub fn open(path: &Path) -> Result<Self> {
        version()?;
        let root = PathBuf::from(line(
            path,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )?)
        .canonicalize()?;
        let common = PathBuf::from(line(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?)
        .canonicalize()?;
        let admin =
            PathBuf::from(line(&root, &["rev-parse", "--absolute-git-dir"])?).canonicalize()?;
        Ok(Self {
            root,
            common,
            admin,
        })
    }
    pub fn head(&self) -> Result<String> {
        line(&self.root, &["rev-parse", "--verify", "HEAD^{commit}"])
    }
    pub fn resolve(&self, reference: &str) -> Result<String> {
        if reference.starts_with('-') || reference.is_empty() {
            return Err(Error::new("E_ARGUMENT", "invalid commit reference"));
        }
        line(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
        )
    }
    pub fn worktrees(&self) -> Result<Vec<PathBuf>> {
        let bytes = text(run(
            &self.root,
            ["worktree", "list", "--porcelain", "-z"],
            None,
        )?)?;
        bytes
            .split("\0\0")
            .filter_map(|record| {
                let fields: Vec<_> = record.split('\0').collect();
                if fields.contains(&"bare") {
                    return None;
                }
                fields.iter().find_map(|s| s.strip_prefix("worktree "))
            })
            .map(|s| {
                let path = PathBuf::from(s);
                match path.canonicalize() {
                    Ok(path) => Ok(path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
                    Err(e) => Err(e.into()),
                }
            })
            .collect()
    }
    pub fn index(&self) -> Result<Vec<Entry>> {
        parse_entries(run(&self.root, ["ls-files", "--stage", "-z"], None)?, false)
    }
    pub fn tree(&self, reference: &str) -> Result<Vec<Entry>> {
        parse_entries(
            run(&self.root, ["ls-tree", "-r", "-z", reference], None)?,
            true,
        )
    }
    pub fn dirty(&self) -> Result<Vec<u8>> {
        run(
            &self.root,
            [
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            None,
        )
    }
    pub fn modified(&self) -> Result<BTreeSet<String>> {
        let mut paths = BTreeSet::new();
        // Name-only diff can report stat-only changes without checking blob content.
        // Status refreshes that comparison in memory; optional locks keep the index untouched.
        let status = text(run(
            &self.root,
            [
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=no",
                "--ignore-submodules=none",
            ],
            None,
        )?)?;
        let mut records = status.split('\0').filter(|s| !s.is_empty());
        while let Some(record) = records.next() {
            let header = record.as_bytes();
            if header.len() < 4 || header[2] != b' ' {
                return Err(Error::new("E_GIT", "malformed status record"));
            }
            paths.insert(record[3..].to_string());
            if header[..2].iter().any(|c| matches!(c, b'R' | b'C')) {
                let original = records
                    .next()
                    .ok_or_else(|| Error::new("E_GIT", "missing rename source"))?;
                paths.insert(original.to_string());
            }
        }
        Ok(paths)
    }
    pub fn preflight(&self, target: &str) -> Result<()> {
        let sparse = run(
            &self.root,
            ["config", "--bool", "core.sparseCheckout"],
            None,
        )
        .ok();
        if sparse.as_deref() == Some(b"true\n") {
            return Err(Error::new(
                "E_UNSUPPORTED",
                "sparse checkouts are unsupported",
            ));
        }
        let entries = self.tree(target)?;
        if entries.iter().any(|e| e.mode == "160000") {
            return Err(Error::new("E_UNSUPPORTED", "submodules are unsupported"));
        }
        let tags = text(run(&self.root, ["ls-files", "-v", "-z"], None)?)?;
        if tags
            .split('\0')
            .any(|s| s.starts_with('S') || s.chars().next().is_some_and(|c| c.is_ascii_lowercase()))
        {
            return Err(Error::new(
                "E_UNSUPPORTED",
                "skip-worktree/assume-unchanged index entries are unsupported",
            ));
        }
        let mut input = Vec::new();
        for e in &entries {
            safe_path(&e.path)?;
            input.extend_from_slice(e.path.as_bytes());
            input.push(0);
        }
        // Tree attribute lookup scales poorly with large directory trees. The
        // index gives identical attributes only when its entries match this commit.
        let attribute_source = match run(
            &self.root,
            [
                "diff-index",
                "--cached",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                target,
                "--",
            ],
            None,
        ) {
            Ok(_) => "--cached".to_string(),
            Err(e) if e.data["git_exit_code"] == 1 => format!("--source={target}"),
            Err(e) => return Err(e),
        };
        let attrs = text(run(
            &self.root,
            [
                "check-attr",
                &attribute_source,
                "-z",
                "--stdin",
                "filter",
                "working-tree-encoding",
            ],
            Some(&input),
        )?)?;
        let fields: Vec<_> = attrs.split('\0').collect();
        for fields in fields.chunks_exact(3) {
            if !["unspecified", "unset"].contains(&fields[2]) {
                return Err(Error::new(
                    "E_UNSUPPORTED",
                    format!("{} attribute on {:?} is unsupported", fields[1], fields[0]),
                ));
            }
        }
        self.idle()?;
        Ok(())
    }
    pub fn idle(&self) -> Result<()> {
        let common = (self.common != self.admin).then_some(&self.common);
        for dir in std::iter::once(&self.admin).chain(common) {
            for name in [
                "index.lock",
                "HEAD.lock",
                "packed-refs.lock",
                "shallow.lock",
                "MERGE_HEAD",
                "rebase-merge",
                "rebase-apply",
                "sequencer",
                "CHERRY_PICK_HEAD",
                "REVERT_HEAD",
            ] {
                if dir.join(name).exists() {
                    return Err(Error::new(
                        "E_BUSY",
                        format!("Git operation marker exists: {}", dir.join(name).display()),
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let watched = self.snapshot_watches()?.unwrap_or_default();
        // Git remains authoritative for ref resolution. File stamps only avoid
        // resolving the same HEAD and rereading an unchanged index per candidate.
        let mut bytes = self.head()?.into_bytes();
        bytes.extend(run(&self.root, ["symbolic-ref", "-q", "HEAD"], None).unwrap_or_default());
        if self.admin.join("index").exists() {
            bytes.extend(std::fs::read(self.admin.join("index"))?);
        }
        if !stamps_unchanged(&watched)? {
            return Err(Error::new(
                "E_CHANGED",
                "Git metadata changed while taking snapshot",
            ));
        }
        Ok(Snapshot {
            bytes: blake3::hash(&bytes).as_bytes().to_vec(),
            watched,
        })
    }

    fn snapshot_watches(&self) -> Result<Option<FileStamps>> {
        let mut watched = vec![];
        for path in [self.common.join("reftable"), self.admin.join("reftable")] {
            let state = stamp(&path)?;
            if state.is_some() {
                return Ok(None);
            }
            watched.push((path, state));
        }
        for path in [self.admin.join("index"), self.common.join("packed-refs")] {
            let state = stamp(&path)?;
            watched.push((path, state));
        }
        let mut path = self.admin.join("HEAD");
        let mut visited = BTreeSet::new();
        for _ in 0..16 {
            if !visited.insert(path.clone()) {
                return Ok(None);
            }
            let state = stamp(&path)?;
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR | libc::ENOTDIR)) => {
                    return Ok(None)
                }
                Err(e) => return Err(e.into()),
            };
            watched.push((path, state));
            let reference = match bytes.as_deref() {
                Some(value) if value.starts_with(b"ref:") => {
                    let Some(value) = value.strip_prefix(b"ref: ") else {
                        return Ok(None);
                    };
                    let Ok(value) = std::str::from_utf8(value) else {
                        return Ok(None);
                    };
                    let value = value.trim_end_matches('\n');
                    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
                        return Ok(None);
                    }
                    value
                }
                _ => return Ok(Some(watched)),
            };
            // Unusual symbolic references remain Git's responsibility.
            if !reference.starts_with("refs/") || safe_path(reference).is_err() {
                return Ok(None);
            }
            let per_worktree = ["refs/bisect/", "refs/worktree/", "refs/rewritten/"]
                .iter()
                .any(|prefix| reference.starts_with(prefix));
            path = if per_worktree {
                &self.admin
            } else {
                &self.common
            }
            .join(reference);
        }
        Ok(None)
    }
}

fn parse_entries(bytes: Vec<u8>, tree: bool) -> Result<Vec<Entry>> {
    let value = text(bytes)?;
    let mut result = vec![];
    for record in value.split('\0').filter(|s| !s.is_empty()) {
        let (header, path) = record
            .split_once('\t')
            .ok_or_else(|| Error::new("E_GIT", "malformed Git entry"))?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3 || (!tree && fields[2] != "0") {
            return Err(Error::new("E_UNSUPPORTED", "unmerged index is unsupported"));
        }
        safe_path(path)?;
        result.push(Entry {
            mode: fields[0].to_string(),
            oid: fields[if tree { 2 } else { 1 }].to_string(),
            path: path.to_string(),
        });
    }
    Ok(result)
}
