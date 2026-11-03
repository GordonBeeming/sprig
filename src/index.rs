use crate::{
    git::{Entry, Repository},
    parallel,
    platform::{self, Fingerprint},
    Error, Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufWriter, Write},
    os::unix::fs::MetadataExt,
};

pub(crate) struct VerifiedClone {
    pub path: String,
    pub fingerprint: Fingerprint,
}

pub(crate) fn refresh(
    repo: &Repository,
    entries: &[Entry],
    clones: Vec<VerifiedClone>,
    commit: &str,
) -> Result<()> {
    if clones.is_empty() {
        return Ok(());
    }
    let path = repo.admin.join("index");
    let mut lock = gix_lock::File::acquire_to_update_resource(
        &path,
        gix_lock::acquire::Fail::AfterDurationWithBackoff(std::time::Duration::from_secs(2)),
        None,
    )
    .map_err(|e| match e {
        gix_lock::acquire::Error::Io(e) => Error::from(e),
        e => Error::new("E_BUSY", e.to_string()),
    })?;
    let metadata = fs::metadata(&path)?;
    let kind = match commit.len() {
        40 => gix_hash::Kind::Sha1,
        64 => gix_hash::Kind::Sha256,
        _ => return Err(Error::new("E_UNSUPPORTED", "unsupported Git object hash")),
    };
    if metadata.len() < if commit.len() == 40 { 32 } else { 44 } {
        return Err(Error::new(
            "E_CHANGED",
            "destination index was truncated during creation",
        ));
    }
    let mut index = gix_index::File::at(&path, kind, false, Default::default())
        .map_err(|e| Error::new("E_GIT", format!("cannot read destination index: {e}")))?;
    let expected: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| {
            let id = gix_hash::ObjectId::from_hex(entry.oid.as_bytes())
                .map_err(|e| Error::new("E_GIT", e.to_string()))?;
            let mode = u32::from_str_radix(&entry.mode, 8)
                .map_err(|e| Error::new("E_GIT", e.to_string()))?;
            Ok((entry.path.as_bytes().to_vec(), (id, mode)))
        })
        .collect::<Result<_>>()?;
    if index.entries().len() != expected.len() {
        return Err(Error::new(
            "E_CHANGED",
            "destination index entries changed during creation",
        ));
    }
    let mut seen = BTreeSet::new();
    for entry in index.entries() {
        let name: &[u8] = entry.path(&index).as_ref();
        let flags = gix_index::entry::Flags::ASSUME_VALID
            | gix_index::entry::Flags::SKIP_WORKTREE
            | gix_index::entry::Flags::INTENT_TO_ADD;
        if !seen.insert(name.to_vec())
            || entry.stage() != gix_index::entry::Stage::Unconflicted
            || entry.flags.intersects(flags)
            || expected.get(name) != Some(&(entry.id, entry.mode.bits()))
        {
            return Err(Error::new(
                "E_CHANGED",
                "destination index no longer matches the requested tree",
            ));
        }
    }
    let stats: BTreeMap<_, _> = parallel::map(clones, |clone| {
        let path = repo.root.join(&clone.path);
        let (observed, file) = platform::fingerprint_with_file(&path)?;
        if !observed.unchanged(&clone.fingerprint) {
            return Err(Error::new(
                "E_CHANGED",
                format!("clone changed before indexing: {}", path.display()),
            ));
        }
        let stat = gix_index::fs::Metadata::from_file(&file)?;
        let after = file.metadata()?;
        if (after.dev(), after.ino()) != observed.identity()
            || after.len() != observed.len
            || (after.ctime(), after.ctime_nsec()) != observed.ctime
            || (after.mtime(), after.mtime_nsec()) != observed.mtime
        {
            return Err(Error::new(
                "E_CHANGED",
                "clone changed while capturing index stat data",
            ));
        }
        let stat = gix_index::entry::Stat::from_fs(&stat)
            .map_err(|e| Error::new("E_UNSUPPORTED", e.to_string()))?;
        Ok((clone.path.into_bytes(), stat))
    })?
    .into_iter()
    .collect();
    // Only destination inodes whose bytes were reverified get stat cache entries.
    // Object IDs, modes, paths and stages stay exactly as Git populated them.
    for (entry, name) in index.entries_mut_with_paths() {
        let name: &[u8] = name.as_ref();
        if let Some(stat) = stats.get(name) {
            entry.stat = *stat;
        }
    }
    {
        let mut out = BufWriter::with_capacity(64 * 1024, &mut lock);
        index
            .write_to(&mut out, Default::default())
            .map_err(|e| Error::new("E_IO", e.to_string()))?;
        out.flush()?;
    }
    lock.with_mut(|file| file.set_permissions(metadata.permissions()))?;
    platform::preserve_group(lock.lock_path(), metadata.gid())?;
    lock.with_mut(|file| file.sync_all())?;
    lock.commit()
        .map_err(|e| Error::new("E_IO", e.to_string()))?;
    platform::sync_dir(&repo.admin)
}
