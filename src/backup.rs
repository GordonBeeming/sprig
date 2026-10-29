use crate::{git::Repository, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const STICKY: &str = "com.apple.metadata:com_apple_backup_excludeItem";
const MARKER: &str = "com.tool.backup-provenance-v1";
#[derive(Debug, Serialize, Deserialize)]
struct Provenance {
    schema_version: u32,
    dev: u64,
    ino: u64,
    value: Option<Vec<u8>>,
}
#[derive(Debug, Serialize)]
pub struct Status {
    pub applicable: bool,
    pub excluded: bool,
    pub direct_sticky: bool,
    pub tool_managed: bool,
    pub origin: String,
}

fn tmutil(verb: &str, path: &Path) -> Result<Vec<u8>> {
    let mut cmd = Command::new("/usr/bin/tmutil");
    cmd.arg(verb);
    if verb == "isexcluded" {
        cmd.arg("-X");
    }
    let mut child = cmd
        .arg(path)
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > Duration::from_secs(20) {
            child.kill()?;
            let _ = child.wait();
            return Err(Error::new(
                "E_BACKUP",
                "tmutil timed out; inspect backup state and retry with tool backup apply",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::new(
            "E_BACKUP",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}
pub fn status(path: &Path) -> Result<Status> {
    if !cfg!(target_os = "macos") {
        return Ok(Status {
            applicable: false,
            excluded: false,
            direct_sticky: false,
            tool_managed: false,
            origin: "not_applicable".into(),
        });
    }
    let xml = String::from_utf8(tmutil("isexcluded", path)?)
        .map_err(|_| Error::new("E_BACKUP", "invalid tmutil output"))?;
    let field = xml
        .split_once("<key>IsExcluded</key>")
        .ok_or_else(|| Error::new("E_BACKUP", "tmutil exclusion state missing"))?
        .1
        .trim_start();
    let excluded = if field.starts_with("<integer>1</integer>") || field.starts_with("<true/>") {
        true
    } else if field.starts_with("<integer>0</integer>") || field.starts_with("<false/>") {
        false
    } else {
        return Err(Error::new("E_BACKUP", "unknown tmutil exclusion value"));
    };
    let sticky = xattr::get(path, STICKY)?;
    let m = fs::metadata(path)?;
    let provenance =
        xattr::get(path, MARKER)?.and_then(|v| serde_json::from_slice::<Provenance>(&v).ok());
    let pending = provenance.as_ref().is_some_and(|p| {
        p.schema_version == 1 && p.dev == m.dev() && p.ino == m.ino() && p.value.is_none()
    });
    let managed = provenance.is_some_and(|p| {
        p.schema_version == 1
            && p.dev == m.dev()
            && p.ino == m.ino()
            && sticky.is_some()
            && sticky == p.value
    });
    let origin = if pending {
        "pending_tool_operation"
    } else if managed {
        "tool"
    } else if sticky.is_some() {
        "pre_existing"
    } else if excluded {
        "inherited_or_system"
    } else {
        "included"
    };
    Ok(Status {
        applicable: true,
        excluded,
        direct_sticky: sticky.is_some(),
        tool_managed: managed,
        origin: origin.into(),
    })
}
pub fn safe_root(repo: &Repository, path: &Path) -> Result<()> {
    let path = path.canonicalize()?;
    let workspace = Repository::open(&path)?;
    if workspace.root != path || workspace.common != repo.common {
        return Err(Error::new(
            "E_ARGUMENT",
            "exclusion must target a worktree of this repository",
        ));
    }
    if workspace.admin == workspace.common {
        return Err(Error::new("E_ARGUMENT", "the original checkout is never automatically excluded, including with a separate Git directory"));
    }
    if repo.common.starts_with(&path) {
        return Err(Error::new(
            "E_ARGUMENT",
            "exclusion would contain the common Git object database",
        ));
    }
    for root in repo.worktrees()? {
        if root != path && root.starts_with(&path) {
            return Err(Error::new(
                "E_ARGUMENT",
                "exclusion would contain another registered checkout",
            ));
        }
    }
    Ok(())
}
pub fn apply(repo: &Repository, path: &Path) -> Result<Status> {
    apply_with_resolution(repo, path, false)
}
pub fn apply_with_resolution(
    repo: &Repository,
    path: &Path,
    resolve_pending: bool,
) -> Result<Status> {
    safe_root(repo, path)?;
    let before = status(path)?;
    if before.origin == "pending_tool_operation" && before.direct_sticky {
        if !resolve_pending {
            return Err(Error::new("E_BACKUP", "an interrupted backup operation left uncertain ownership; inspect the exclusion, then explicitly accept ownership with tool backup apply PATH --resolve-pending"));
        }
        let value = xattr::get(path, STICKY)?
            .ok_or_else(|| Error::new("E_BACKUP", "exclusion changed during resolution"))?;
        let m = fs::metadata(path)?;
        xattr::set(
            path,
            MARKER,
            &serde_json::to_vec(&Provenance {
                schema_version: 1,
                dev: m.dev(),
                ino: m.ino(),
                value: Some(value),
            })?,
        )?;
        return status(path);
    }
    if !before.applicable || before.excluded {
        return Ok(before);
    }
    crate::state::boundary("backup_apply")?;
    let m = fs::metadata(path)?;
    xattr::set(
        path,
        MARKER,
        &serde_json::to_vec(&Provenance {
            schema_version: 1,
            dev: m.dev(),
            ino: m.ino(),
            value: None,
        })?,
    )?;
    crate::platform::sync_dir(path)?;
    // Recheck after recording intent; another backup controller must also be idle.
    if xattr::get(path, STICKY)?.is_some() {
        return Err(Error::new(
            "E_BACKUP",
            "exclusion changed before application; pending provenance retained",
        ));
    }
    tmutil("addexclusion", path)?;
    crate::state::boundary("backup_added")?;
    let value = xattr::get(path, STICKY)?.ok_or_else(|| {
        Error::new(
            "E_BACKUP",
            "tmutil did not create a direct sticky exclusion",
        )
    })?;
    let m = fs::metadata(path)?;
    xattr::set(
        path,
        MARKER,
        &serde_json::to_vec(&Provenance {
            schema_version: 1,
            dev: m.dev(),
            ino: m.ino(),
            value: Some(value),
        })?,
    )?;
    let after = status(path)?;
    if !after.excluded || !after.tool_managed {
        return Err(Error::new(
            "E_BACKUP",
            "backup exclusion verification failed",
        ));
    }
    Ok(after)
}
pub fn remove(repo: &Repository, path: &Path) -> Result<Status> {
    safe_root(repo, path)?;
    let before = status(path)?;
    if !before.applicable {
        return Ok(before);
    }
    if !before.tool_managed {
        return Err(Error::new(
            "E_BACKUP",
            "refusing to remove a pre-existing, inherited or unowned exclusion",
        ));
    }
    tmutil("removeexclusion", path)?;
    if xattr::get(path, STICKY)?.is_some() {
        return Err(Error::new(
            "E_BACKUP",
            "direct sticky exclusion removal failed",
        ));
    }
    xattr::remove(path, MARKER)?;
    status(path)
}
