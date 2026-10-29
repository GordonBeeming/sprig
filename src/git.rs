use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::{BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
    pub common: PathBuf,
    pub admin: PathBuf,
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

pub fn rendered_hashes(root: &Path, entries: &[Entry]) -> Result<BTreeMap<String, String>> {
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
    let result = (|| -> Result<_> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("E_GIT", "missing cat-file stdin"))?;
        let mut stdout = BufReader::new(
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
            stdin.write_all(format!("{}\0", entry.oid).as_bytes())?;
            stdin.flush()?;
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
            hashes.insert(entry.path.clone(), hash.finalize().to_hex().to_string());
        }
        drop(stdin);
        Ok(hashes)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    if !status.success() && result.is_ok() {
        return Err(Error::new("E_GIT", "cat-file failed"));
    }
    let mut hashes = result?;
    let mut input = Vec::new();
    for entry in entries {
        input.extend_from_slice(entry.path.as_bytes());
        input.push(0);
    }
    let attrs = text(run(
        root,
        ["check-attr", "-z", "--stdin", "text", "eol", "ident"],
        Some(&input),
    )?)?;
    let mut transformed = BTreeSet::new();
    for fields in attrs.split('\0').collect::<Vec<_>>().chunks_exact(3) {
        if !["unspecified", "unset"].contains(&fields[2]) {
            transformed.insert(fields[0].to_string());
        }
    }
    let autocrlf = match line(root, &["config", "--get", "core.autocrlf"]) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" | "" => true,
            "false" | "no" | "off" | "0" | "input" => false,
            _ => return Err(Error::new("E_CONFIG", "unsupported core.autocrlf value")),
        },
        Err(e) if e.data["git_exit_code"] == 1 => false,
        Err(e) => return Err(e),
    };
    for entry in entries {
        if hashes.contains_key(&entry.path) && (transformed.contains(&entry.path) || autocrlf) {
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
            .split('\0')
            .filter_map(|s| s.strip_prefix("worktree "))
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
        let attrs = text(run(
            &self.root,
            [
                "check-attr",
                &format!("--source={target}"),
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
        for dir in [&self.admin, &self.common] {
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
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let mut bytes = self.head()?.into_bytes();
        bytes.extend(run(&self.root, ["symbolic-ref", "-q", "HEAD"], None).unwrap_or_default());
        if self.admin.join("index").exists() {
            bytes.extend(std::fs::read(self.admin.join("index"))?);
        }
        Ok(bytes)
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
