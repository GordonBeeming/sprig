use sprig::{
    git::{self, Repository},
    Result,
};
use std::{ffi::OsStr, fs, path::Path};

fn commit(root: &Path, message: &str) -> Result<String> {
    let tree = git::text(git::run(root, ["mktree"], Some(b""))?)?;
    let body = format!("tree {}\nauthor Fixture <fixture@example.invalid> 1700000000 +0000\ncommitter Fixture <fixture@example.invalid> 1700000000 +0000\n\n{message}\n", tree.trim());
    Ok(git::text(git::run(
        root,
        ["hash-object", "-t", "commit", "-w", "--stdin"],
        Some(body.as_bytes()),
    )?)?
    .trim()
    .into())
}

#[test]
fn snapshot_tracks_real_linked_worktree_head_index_and_ref_storage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    fs::create_dir(&source)?;
    git::run(&source, ["init", "--initial-branch=main"], None)?;
    let first = commit(&source, "first")?;
    let second = commit(&source, "second")?;
    git::run(&source, ["update-ref", "refs/heads/main", &first], None)?;
    let target = dir.path().join("target");
    git::run(
        &source,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            target.as_os_str(),
            OsStr::new(&first),
        ],
        None,
    )?;
    let repo = Repository::open(&target)?;
    let head = repo.admin.join("HEAD");
    let branch = repo.common.join("refs/heads/main");
    let index = repo.admin.join("index");
    fs::write(&head, b"ref: refs/heads/main\n")?;
    let initial = repo.snapshot()?;
    assert!(initial.unchanged(&repo)?);
    git::run(&source, ["update-ref", "refs/heads/main", &second], None)?;
    assert!(
        !initial.unchanged(&repo)?,
        "branch moved without changing HEAD"
    );
    git::run(&source, ["update-ref", "refs/heads/main", &first], None)?;
    assert!(initial.unchanged(&repo)?);
    let original_index = fs::read(&index)?;
    fs::write(&index, b"changed index")?;
    assert!(!initial.unchanged(&repo)?, "index content changed");
    fs::write(&index, original_index)?;
    git::run(&source, ["pack-refs", "--all"], None)?;
    assert!(!branch.exists());
    assert!(
        initial.unchanged(&repo)?,
        "packing an unchanged HEAD must not invalidate its state"
    );
    let packed = repo.snapshot()?;
    git::run(&source, ["update-ref", "refs/heads/main", &second], None)?;
    git::run(&source, ["pack-refs", "--all"], None)?;
    assert!(!packed.unchanged(&repo)?, "packed branch moved");
    fs::write(&head, format!("{first}\n"))?;
    let detached = repo.snapshot()?;
    fs::write(&head, format!("{second}\n"))?;
    assert!(!detached.unchanged(&repo)?, "detached HEAD moved");
    fs::create_dir_all(repo.admin.join("refs/worktree"))?;
    fs::create_dir_all(repo.common.join("refs/worktree"))?;
    fs::write(&head, b"ref: refs/heads/alias\n")?;
    fs::write(
        repo.common.join("refs/heads/alias"),
        b"ref: refs/worktree/local\n",
    )?;
    let local = repo.admin.join("refs/worktree/local");
    fs::write(&local, format!("{first}\n"))?;
    let symbolic = repo.snapshot()?;
    fs::write(
        repo.common.join("refs/worktree/local"),
        format!("{second}\n"),
    )?;
    assert!(
        symbolic.unchanged(&repo)?,
        "per-worktree refs must use the linked admin directory"
    );
    fs::write(&local, format!("{second}\n"))?;
    assert!(!symbolic.unchanged(&repo)?, "symbolic ref chain moved");
    for (prefix, suffix) in [("ref:\t", "\n"), ("ref: ", "\r\n"), ("ref: ", " \n")] {
        fs::write(&branch, format!("{first}\n"))?;
        fs::write(&head, format!("{prefix}refs/heads/main{suffix}"))?;
        let unusual = repo.snapshot()?;
        fs::write(&branch, format!("{second}\n"))?;
        assert!(
            !unusual.unchanged(&repo)?,
            "unusual refs must fall back to Git"
        );
    }
    Ok(())
}

#[test]
fn reftable_snapshots_use_git_to_detect_ref_changes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    if let Err(e) = git::run(
        dir.path(),
        ["init", "--initial-branch=main", "--ref-format=reftable"],
        None,
    ) {
        if e.message.contains("unknown option") {
            // Reftable was added after the minimum supported Git version.
            return Ok(());
        }
        return Err(e);
    }
    let first = commit(dir.path(), "first")?;
    let second = commit(dir.path(), "second")?;
    git::run(dir.path(), ["update-ref", "refs/heads/main", &first], None)?;
    let repo = Repository::open(dir.path())?;
    let snapshot = repo.snapshot()?;
    assert!(snapshot.unchanged(&repo)?);
    git::run(dir.path(), ["update-ref", "refs/heads/main", &second], None)?;
    assert!(!snapshot.unchanged(&repo)?);
    Ok(())
}
