use crate::{
    backup, config,
    git::{self, Repository},
    platform::{self, Fingerprint},
    state::{self, CreationOwnership, Journal, Lock, Record},
    Error, Output, Result,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct CreateOptions {
    pub branch: Option<String>,
    pub new_branch: Option<String>,
    pub detach: bool,
    pub reference: Option<String>,
    pub backup: Option<bool>,
    pub dry_run: bool,
}
#[derive(Debug, Serialize)]
pub struct Decision {
    pub path: String,
    pub reason: String,
    pub donor: Option<PathBuf>,
    pub logical_bytes: u64,
}
struct Candidate {
    target: PathBuf,
    donor: PathBuf,
    original: Fingerprint,
    donor_state: Fingerprint,
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::new("E_UNSUPPORTED", "non-UTF-8 path is unsupported"))
}
fn verify_reservation(path: &Path, reservation: &File) -> Result<()> {
    let current = fs::symlink_metadata(path)?;
    let pinned = reservation.metadata()?;
    if !current.is_dir() || (current.dev(), current.ino()) != (pinned.dev(), pinned.ino()) {
        return Err(Error::new(
            "E_CHANGED",
            "reserved destination was replaced; refusing to follow it",
        ));
    }
    Ok(())
}
fn probe_at(repo: &Repository, source: &Path, volume: &Path, fallback: &Path) -> Result<()> {
    let dev = fs::metadata(volume)?.dev();
    let cache = repo.common.join("tool");
    let storage = if fs::metadata(&cache)?.dev() == dev {
        cache
    } else if fs::metadata(fallback)?.dev() == dev {
        fallback.to_path_buf()
    } else {
        return Err(Error::new(
            "E_UNSUPPORTED",
            "no probe location outside the worktree on its volume",
        ));
    };
    platform::probe(source, &storage)
}
fn backup_result(repo: &Repository, root: &Path, enabled: bool, output: &mut Output) {
    let result = if enabled {
        backup::apply(repo, root)
    } else {
        backup::status(root)
    };
    match result {
        Ok(status) => output.data["backup"] = json!(status),
        Err(e) => {
            output.data["backup"] =
                json!({"state":"unknown_or_failed", "code":e.code, "message":e.message});
            let action = if e.code == "E_ARGUMENT" {
                "Exclusion is unsafe at this location; use a linked worktree outside the original checkout, or disable new exclusions.".to_string()
            } else {
                format!("Retry: tool backup apply {}", root.display())
            };
            output.warn(format!(
                "Workspace is usable; backup configuration failed: {e}. {action}"
            ));
        }
    }
}
fn changed(expected: &[u8], repo: &Repository) -> Result<()> {
    repo.idle()?;
    if repo.snapshot()? != expected {
        return Err(Error::new(
            "E_CHANGED",
            "HEAD or index changed during operation",
        ));
    }
    Ok(())
}
fn destination(repo: &Repository, path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::new("E_ARGUMENT", "destination must name a new directory"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let dest = parent.join(name);
    platform::validate_workspace_parent(&parent)?;
    path_string(&dest)?;
    if fs::symlink_metadata(&dest).is_ok() {
        return Err(Error::new("E_EXISTS", "destination already exists"));
    }
    for root in repo.worktrees()?.into_iter().chain([repo.common.clone()]) {
        if dest.starts_with(&root) || root.starts_with(&dest) {
            return Err(Error::new(
                "E_ARGUMENT",
                "destination overlaps a registered worktree or Git metadata",
            ));
        }
    }
    Ok(dest)
}
fn branch_name(repo: &Repository, name: &str) -> Result<()> {
    if name.starts_with('-') || name == "HEAD" {
        return Err(Error::new("E_ARGUMENT", "invalid branch name"));
    }
    git::run(
        &repo.root,
        ["check-ref-format", &format!("refs/heads/{name}")],
        None,
    )
    .map_err(|e| Error::new("E_ARGUMENT", e.message))?;
    Ok(())
}

pub fn create(source: &Path, dest: &Path, opts: &CreateOptions) -> Result<Output> {
    if usize::from(opts.branch.is_some())
        + usize::from(opts.new_branch.is_some())
        + usize::from(opts.detach)
        > 1
        || (opts.branch.is_some() && opts.reference.is_some())
    {
        return Err(Error::new(
            "E_ARGUMENT",
            "choose one branch mode; --branch cannot be combined with --ref",
        ));
    }
    let repo = Repository::open(source)?;
    let source_head = repo.head()?;
    let reference = opts
        .branch
        .as_ref()
        .map(|v| format!("refs/heads/{v}"))
        .unwrap_or_else(|| opts.reference.clone().unwrap_or(source_head.clone()));
    if let Some(name) = opts.branch.as_ref().or(opts.new_branch.as_ref()) {
        branch_name(&repo, name)?;
    }
    if let Some(name) = &opts.new_branch {
        if git::run(
            &repo.root,
            ["show-ref", "--verify", &format!("refs/heads/{name}")],
            None,
        )
        .is_ok()
        {
            return Err(Error::new("E_EXISTS", "new branch already exists"));
        }
    }
    if let Some(name) = &opts.branch {
        let list = git::text(git::run(
            &repo.root,
            ["worktree", "list", "--porcelain", "-z"],
            None,
        )?)?;
        if list
            .split('\0')
            .any(|v| v == format!("branch refs/heads/{name}"))
        {
            return Err(Error::new("E_BUSY", "branch is already checked out"));
        }
    }
    let target = repo.resolve(&reference)?;
    repo.preflight(&source_head)?;
    repo.preflight(&target)?;
    if !repo.dirty()?.is_empty() {
        return Err(Error::new("E_DIRTY", "source has staged, unstaged, conflicted or non-ignored untracked files; ignored files are omitted"));
    }
    let dest = destination(&repo, dest)?;
    platform::supported(&repo.root)?;
    let destination_parent = dest
        .parent()
        .ok_or_else(|| Error::new("E_ARGUMENT", "destination parent missing"))?;
    platform::supported(destination_parent)?;
    if fs::metadata(&repo.root)?.dev() != fs::metadata(destination_parent)?.dev() {
        return Err(Error::new(
            "E_CLONE",
            "source and destination are on different volumes",
        ));
    }
    let policy = config::effective(Some(&repo), opts.backup)?;
    let entries = repo.tree(&target)?;
    let preview = json!({"worktree":dest,"commit":target,"branch":opts.branch.as_ref().or(opts.new_branch.as_ref()),"detached":opts.branch.is_none() && opts.new_branch.is_none(),"policy":policy,"files":entries.len(),"dry_run":opts.dry_run,"clone_capability":"unprobed"});
    if opts.dry_run {
        return Ok(Output::success(preview));
    }
    platform::supported(&repo.root)?;
    let _lock = Lock::acquire(&repo)?;
    state::require_recovered(&repo)?;
    destination(&repo, &dest)?;
    if !repo.dirty()?.is_empty() {
        return Err(Error::new(
            "E_DIRTY",
            "source changed before creation acquired its lock",
        ));
    }
    let snapshot = repo.snapshot()?;
    let parent = dest
        .parent()
        .ok_or_else(|| Error::new("E_ARGUMENT", "destination parent missing"))?;
    probe_at(&repo, &repo.root, parent, parent)?;
    changed(&snapshot, &repo)?;
    let mut record = Record::new(
        &repo,
        Journal {
            schema_version: 1,
            kind: "create".into(),
            root: dest.clone(),
            target: dest.clone(),
            sibling: None,
            original: None,
            clone: None,
            commit: Some(target.clone()),
            stage: "planned".into(),
            creation: Some(CreationOwnership {
                destination_identity: None,
                requested_new_branch: opts.new_branch.clone(),
                confirmed_new_branch: false,
            }),
        },
    )?;
    let result = (|| -> Result<Output> {
        fs::DirBuilder::new().mode(0o700).create(&dest)?;
        let reservation = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&dest)?;
        let identity = reservation.metadata()?;
        if let Some(ownership) = &mut record.journal.creation {
            ownership.destination_identity = Some((identity.dev(), identity.ino()));
        }
        record.journal.stage = "reserved".into();
        record.save()?;
        state::boundary("create_reserved")?;
        verify_reservation(&dest, &reservation)?;
        let mut args = vec!["worktree".to_string(), "add".into(), "--no-checkout".into()];
        if let Some(name) = &opts.new_branch {
            args.extend(["-b".into(), name.clone()]);
        } else if opts.branch.is_none() {
            args.push("--detach".into());
        }
        args.push(path_string(&dest)?.into());
        args.push(if let Some(branch) = &opts.branch {
            branch.clone()
        } else {
            target.clone()
        });
        git::run(&repo.root, &args, None)?;
        verify_reservation(&dest, &reservation)?;
        if let Some(ownership) = &mut record.journal.creation {
            ownership.confirmed_new_branch = opts.new_branch.is_some();
        }
        record.journal.stage = "registered".into();
        record.save()?;
        state::boundary("create_registered")?;
        verify_reservation(&dest, &reservation)?;
        let workspace = Repository::open(&dest)?;
        git::run(&dest, ["read-tree", &target], None)?;
        let mut remaining = vec![];
        let mut cloned = 0u64;
        let mut cloned_bytes = 0u64;
        let source_entries: BTreeMap<_, _> = repo
            .index()?
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();
        let attrs: Vec<_> = entries
            .iter()
            .filter(|e| {
                Path::new(&e.path)
                    .file_name()
                    .is_some_and(|n| n == ".gitattributes")
            })
            .collect();
        let attr_names: std::collections::BTreeSet<_> =
            attrs.iter().map(|e| e.path.as_str()).collect();
        checkout(&dest, attrs.iter().map(|e| e.path.as_str()))?;
        let rendered = git::rendered_hashes(&dest, &entries)?;
        for e in &entries {
            verify_reservation(&dest, &reservation)?;
            if attr_names.contains(e.path.as_str()) {
                continue;
            }
            let target_path = dest.join(&e.path);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut seeded = false;
            if matches!(e.mode.as_str(), "100644" | "100755")
                && source_entries
                    .get(&e.path)
                    .is_some_and(|s| s.oid == e.oid && s.mode == e.mode)
            {
                let donor = git::no_symlink_components(&repo.root, &e.path)?;
                if let Ok(before) = platform::fingerprint(&donor) {
                    // Rendering in the destination honours its attributes and Git's eol configuration.
                    let compatible = rendered.get(&e.path) == Some(&before.digest)
                        && (before.mode & 0o100 != 0) == (e.mode == "100755");
                    if compatible
                        && before.acl.is_empty()
                        && before.xattrs.keys().all(|k| k == "com.apple.provenance")
                    {
                        platform::clone(&donor, &target_path)?;
                        let after = platform::fingerprint(&target_path)?;
                        if !before.preserves(&after)
                            || !platform::fingerprint(&donor)?.unchanged(&before)
                        {
                            return Err(Error::new(
                                "E_CHANGED",
                                "source or clone metadata/bytes changed during creation",
                            ));
                        }
                        seeded = true;
                        cloned += 1;
                        cloned_bytes += before.len;
                    }
                }
            }
            if !seeded {
                remaining.push(e.path.as_str());
            }
            state::boundary("create_seed")?;
        }
        verify_reservation(&dest, &reservation)?;
        checkout(&dest, remaining.iter().copied())?;
        git::run(&dest, ["update-index", "--refresh"], None)?;
        changed(&snapshot, &repo)?;
        if workspace.head()? != target || !workspace.dirty()?.is_empty() {
            return Err(Error::new(
                "E_CHANGED",
                "created workspace did not match requested state",
            ));
        }
        record.journal.stage = "materialised".into();
        record.save()?;
        state::boundary("create_materialised")?;
        let mut output = Output::success(
            json!({"worktree":dest,"commit":target,"branch":opts.branch.as_ref().or(opts.new_branch.as_ref()),"detached":opts.branch.is_none() && opts.new_branch.is_none(),"policy":policy,"cloned_files":cloned,"cloned_logical_bytes":cloned_bytes,"git_materialised_files":entries.len() as u64-cloned,"clone_capability":"native_verified","dry_run":false}),
        );
        if cloned == 0 && !entries.is_empty() {
            output.warn_code("W_NO_SHARED_FILES", "Workspace is valid but no files were eligible for sharing; Git materialised the target files. Inspect ref differences, attributes and source metadata.");
        }
        let zeros = "0".repeat(target.len());
        match git::run(
            &dest,
            [
                "hook",
                "run",
                "--ignore-missing",
                "post-checkout",
                "--",
                &zeros,
                &target,
                "1",
            ],
            None,
        ) {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&bytes));
                }
            }
            Err(e) => output.warn_code("W_HOOK", format!("post-checkout hook failed: {e}")),
        }
        backup_result(&repo, &dest, policy.time_machine_exclude, &mut output);
        if let Err(e) = state::write_receipt(&workspace, "create", cloned, cloned_bytes) {
            output.warn_code(
                "W_INSPECTION_RECORD",
                format!("workspace valid; recording inspection metadata failed: {e}"),
            );
        }
        Ok(output)
    })();
    match result {
        Ok(mut output) => {
            if let Err(e) = record.finish() {
                output.warn_code(
                    "W_RECOVERY",
                    format!("workspace valid; journal cleanup failed: {e}"),
                );
            }
            Ok(output)
        }
        Err(e) => Err(Error::new(
            e.code,
            format!(
                "{}; requested destination {}; recovery journal {}. Run tool doctor {}",
                e.message,
                dest.display(),
                record.path.display(),
                repo.root.display()
            ),
        ).with_data(json!({"retained_workspace":if record.journal.creation.as_ref().is_some_and(|o| o.destination_identity.is_some()) {json!(dest)} else {Value::Null},"requested_destination":dest,"creation_ownership":record.journal.creation,"journal":record.path,"recovery_requires_inspection":true,"recovery_command":["tool","recover",repo.root],"acknowledge_command":["tool","recover",repo.root,"--acknowledge-create"],"source_error":e.data}))),
    }
}
fn checkout<'a>(root: &Path, paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut input = vec![];
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    if !input.is_empty() {
        git::run(root, ["checkout-index", "--stdin", "-z"], Some(&input))?;
    }
    Ok(())
}

pub fn convert(
    path: &Path,
    source: Option<&Path>,
    dry_run: bool,
    backup_override: Option<bool>,
) -> Result<Output> {
    let repo = Repository::open(path)?;
    platform::supported(&repo.root)?;
    let head = repo.head()?;
    repo.preflight(&head)?;
    // Even when backup policy is disabled, a main checkout remains eligible for file conversion.
    let policy = config::effective(Some(&repo), backup_override)?;
    let _lock = if dry_run {
        None
    } else {
        Some(Lock::acquire(&repo)?)
    };
    if !dry_run {
        state::require_recovered(&repo)?;
    }
    let snapshot = repo.snapshot()?;
    let registered = repo.worktrees()?;
    let donor_roots = if let Some(source) = source {
        let donor = Repository::open(source)?;
        if donor.common != repo.common
            || !registered.contains(&donor.root)
            || donor.root == repo.root
        {
            return Err(Error::new(
                "E_ARGUMENT",
                "donor must be a different registered worktree of the same repository",
            ));
        }
        vec![donor.root]
    } else {
        registered.into_iter().filter(|p| *p != repo.root).collect()
    };
    let dev = fs::metadata(&repo.root)?.dev();
    if let Some(root) = source.and_then(|_| donor_roots.first()) {
        if fs::metadata(root)?.dev() != dev {
            return Err(Error::new(
                "E_CLONE",
                "explicit donor and target are on different volumes",
            ));
        }
    }
    if !donor_roots
        .iter()
        .any(|root| fs::metadata(root).is_ok_and(|m| m.is_dir() && m.dev() == dev))
    {
        return Err(Error::new(
            "E_UNSUPPORTED",
            "no other registered worktree on a compatible volume; provide an eligible donor",
        ));
    }
    let modified = repo.modified()?;
    let mut donors = BTreeMap::<String, Vec<(PathBuf, Fingerprint)>>::new();
    let mut donor_snapshots = vec![];
    for root in &donor_roots {
        if !root.is_dir() {
            continue;
        }
        if fs::metadata(root)?.dev() != dev {
            continue;
        }
        let donor = Repository::open(root)?;
        donor.preflight(&donor.head()?)?;
        donor_snapshots.push((donor.clone(), donor.snapshot()?));
        let modified = donor.modified()?;
        for e in donor.index()? {
            if modified.contains(&e.path) || !matches!(e.mode.as_str(), "100644" | "100755") {
                continue;
            }
            let path = git::no_symlink_components(root, &e.path)?;
            if let Ok(fp) = platform::fingerprint(&path) {
                donors.entry(e.oid).or_default().push((path, fp));
            }
        }
    }
    let mut decisions = vec![];
    let mut candidates = vec![];
    for entry in repo.index()? {
        let mut decision = Decision {
            path: entry.path.clone(),
            reason: "no_identical_donor".into(),
            donor: None,
            logical_bytes: 0,
        };
        if modified.contains(&entry.path) {
            decision.reason = "modified_or_staged".into();
        } else if !matches!(entry.mode.as_str(), "100644" | "100755") {
            decision.reason = "unsupported_file_type".into();
        } else {
            let path = git::no_symlink_components(&repo.root, &entry.path)?;
            match platform::fingerprint(&path) {
                Err(e) => decision.reason = format!("ineligible: {}", e.message),
                Ok(original) => {
                    if let Some(matches) = donors.get(&entry.oid) {
                        for (donor, donor_state) in matches {
                            if donor_state.len != original.len
                                || donor_state.digest != original.digest
                            {
                                continue;
                            }
                            decision.donor = Some(donor.clone());
                            decision.logical_bytes = original.len;
                            let id = platform::clone_id(&path)?;
                            if id.is_some() && id == platform::clone_id(donor)? {
                                decision.reason = "same_clone_group".into();
                            } else {
                                decision.reason = "candidate".into();
                                candidates.push(Candidate {
                                    target: path,
                                    donor: donor.clone(),
                                    original,
                                    donor_state: donor_state.clone(),
                                });
                            }
                            break;
                        }
                    }
                }
            }
        }
        decisions.push(decision);
    }
    changed(&snapshot, &repo)?;
    for (donor, snapshot) in &donor_snapshots {
        changed(snapshot, donor)?;
    }
    let estimate: u64 = candidates.iter().map(|c| c.original.len).sum();
    let mut output = Output::success(
        json!({"worktree":repo.root,"dry_run":dry_run,"policy":policy,"candidate_files":candidates.len(),"candidate_logical_bytes":estimate,"estimate_limit":"upper bound on shareable logical data; excludes existing partial sharing, snapshots and metadata; not reclaimable bytes","clone_capability":if dry_run {"unprobed"} else {"pending"},"files":decisions,"replaced_files":0,"replaced_logical_bytes":0}),
    );
    output.data["metadata_skipped_files"] = json!(0);
    if dry_run {
        return Ok(output);
    }
    platform::supported(&repo.root)?;
    for root in &donor_roots {
        if !root.is_dir() {
            continue;
        }
        if fs::metadata(root)?.dev() == dev {
            probe_at(
                &repo,
                root,
                &repo.root,
                repo.root.parent().ok_or_else(|| {
                    Error::new("E_UNSUPPORTED", "worktree has no external probe parent")
                })?,
            )?;
        }
    }
    output.data["clone_capability"] = json!("native_verified");
    let mut count = 0u64;
    let mut bytes = 0u64;
    let mutation_result = (|| -> Result<()> {
        for candidate in candidates {
            changed(&snapshot, &repo)?;
            for (donor, snapshot) in &donor_snapshots {
                changed(snapshot, donor)?;
            }
            if !platform::fingerprint(&candidate.target)?.unchanged(&candidate.original)
                || !platform::fingerprint(&candidate.donor)?.unchanged(&candidate.donor_state)
            {
                return Err(Error::new(
                    "E_CHANGED",
                    "target or donor changed after planning",
                ));
            }
            eprintln!("sharing {}", candidate.target.display());
            let relative = candidate
                .target
                .strip_prefix(&repo.root)
                .map_err(|_| Error::new("E_STATE", "target escaped repository"))?
                .to_string_lossy()
                .into_owned();
            match replace(&repo, candidate) {
            Ok(Some(size)) => { count += 1; bytes += size; }
            Ok(None) => {
                output.data["metadata_skipped_files"] = json!(output.data["metadata_skipped_files"].as_u64().unwrap_or(0)+1);
                if let Some(files) = output.data["files"].as_array_mut() { for file in files { if file["path"].as_str() == Some(&relative) { file["reason"] = json!("metadata_not_preserved"); } } }
            }
            Err(e) => return Err(Error::new(e.code, format!("{}; {} earlier files converted safely; inspect tool doctor {} and run tool recover", e.message, count, repo.root.display()))),
        }
        }
        changed(&snapshot, &repo)?;
        Ok(())
    })();
    if let Err(e) = mutation_result {
        return Err(Error::new(e.code, e.message).with_data(json!({"retained_workspace":repo.root,"journal_directory":repo.common.join("tool/operations"),"completed_files":count,"completed_logical_bytes":bytes,"recovery_command":["tool","recover",repo.root],"source_error":e.data})));
    }
    output.data["replaced_files"] = json!(count);
    output.data["replaced_logical_bytes"] = json!(bytes);
    backup_result(&repo, &repo.root, policy.time_machine_exclude, &mut output);
    if let Err(e) = state::write_receipt(&repo, "convert", count, bytes) {
        output.warn_code(
            "W_INSPECTION_RECORD",
            format!("conversion valid; recording inspection metadata failed: {e}"),
        );
    }
    Ok(output)
}

fn replace(repo: &Repository, candidate: Candidate) -> Result<Option<u64>> {
    let parent = candidate
        .target
        .parent()
        .ok_or_else(|| Error::new("E_STATE", "file parent missing"))?;
    let reservation = tempfile::Builder::new()
        .prefix(".tool-clone-")
        .tempfile_in(parent)?;
    let sibling = reservation.path().to_path_buf();
    // Persist the owned reservation before releasing its name for clonefile's exclusive create.
    let mut record = Record::new(
        repo,
        Journal {
            schema_version: 1,
            kind: "convert".into(),
            root: repo.root.clone(),
            target: candidate.target.clone(),
            sibling: Some(sibling.clone()),
            original: Some(candidate.original.clone()),
            clone: None,
            commit: None,
            stage: "reserved".into(),
            creation: None,
        },
    )?;
    drop(reservation);
    state::boundary("convert_reserved")?;
    platform::clone(&candidate.donor, &sibling)?;
    state::boundary("convert_cloned")?;
    let cloned = platform::fingerprint(&sibling)?;
    record.journal.clone = Some(cloned.clone());
    record.journal.stage = "cloned".into();
    record.save()?;
    let metadata_result = state::boundary("convert_metadata")
        .and_then(|()| platform::metadata(&candidate.target, &sibling));
    let new = platform::fingerprint(&sibling)?;
    if metadata_result.is_err() || !candidate.original.preserves(&new) {
        if platform::fingerprint(&candidate.target)?.unchanged(&candidate.original)
            && new.identity() == cloned.identity()
            && new.digest == candidate.original.digest
        {
            fs::remove_file(&sibling)?;
            platform::sync_dir(parent)?;
            record.finish()?;
            eprintln!(
                "skipped {}: target metadata cannot be preserved",
                candidate.target.display()
            );
            return Ok(None);
        }
        return Err(Error::new(
            "E_CHANGED",
            "metadata preservation failed and files changed; recovery versions retained",
        ));
    }
    if !platform::fingerprint(&candidate.target)?.unchanged(&candidate.original)
        || !platform::fingerprint(&candidate.donor)?.unchanged(&candidate.donor_state)
    {
        return Err(Error::new(
            "E_CHANGED",
            "target or donor changed before swap",
        ));
    }
    File::open(&sibling)?.sync_all()?;
    record.journal.clone = Some(new.clone());
    record.journal.stage = "prepared".into();
    record.save()?;
    state::boundary("convert_prepared")?;
    // Recheck after the final injectable boundary, immediately before publishing.
    if !platform::fingerprint(&candidate.target)?.unchanged(&candidate.original)
        || !platform::fingerprint(&sibling)?.unchanged(&new)
    {
        return Err(Error::new("E_CHANGED", "file changed before atomic swap"));
    }
    repo.idle()?;
    platform::swap(&candidate.target, &sibling)?;
    state::boundary("convert_swapped")?;
    platform::sync_dir(parent)?;
    record.journal.stage = "swapped".into();
    record.save()?;
    let installed = platform::fingerprint(&candidate.target)?;
    let original = platform::fingerprint(&sibling)?;
    if installed.identity() != new.identity()
        || original.identity() != candidate.original.identity()
        || !installed.equivalent(&candidate.original)
        || !original.equivalent(&candidate.original)
    {
        return Err(Error::new(
            "E_CHANGED",
            "post-swap change detected; both versions retained",
        ));
    }
    state::boundary("convert_cleanup")?;
    if !platform::fingerprint(&candidate.target)?.equivalent(&candidate.original)
        || !platform::fingerprint(&sibling)?.equivalent(&candidate.original)
    {
        return Err(Error::new(
            "E_CHANGED",
            "file changed before cleanup; both versions retained",
        ));
    }
    fs::remove_file(&sibling)?;
    platform::sync_dir(parent)?;
    state::boundary("convert_cleaned")?;
    record.finish()?;
    Ok(Some(candidate.original.len))
}

pub fn status(path: &Path, doctor: bool) -> Result<Output> {
    let repo = Repository::open(path)?;
    let journals: Vec<_> = state::records(&repo)?
        .into_iter()
        .map(|r| json!({"journal":r.path,"kind":r.journal.kind,"root":r.journal.root,"target":r.journal.target,"sibling":r.journal.sibling,"stage":r.journal.stage}))
        .collect();
    let backup = match backup::status(&repo.root) {
        Ok(value) => json!(value),
        Err(e) => json!({"state":"unknown", "error":e.message}),
    };
    let platform = match platform::supported(&repo.root) {
        Ok(()) => json!({"supported":true,"native_clone":"unprobed"}),
        Err(e) => json!({"supported":false,"code":e.code,"reason":e.message}),
    };
    let mut data = json!({"worktree":repo.root,"head":repo.head()?,"common_git_dir":repo.common,"backup":backup,"policy":config::effective(Some(&repo),None)?,"recovery":journals,"platform":platform});
    let receipt = state::receipt(&repo)?;
    data["managed"] = json!(receipt.is_some());
    data["last_operation"] = json!(receipt);
    if doctor {
        data["git_version"] = json!(git::version()?);
        data["features"] = match repo.preflight(&repo.head()?) {
            Ok(()) => json!({"supported":true}),
            Err(e) => json!({"supported":false,"code":e.code,"reason":e.message}),
        };
        data["idle_contract"] = json!("Stop editors, builds and Git processes in all participating worktrees. The tool lock does not stop external writers.");
    }
    Ok(Output::success(data))
}

pub fn recover(path: &Path, acknowledge_create: bool) -> Result<Output> {
    let repo = Repository::open(path)?;
    let _lock = Lock::acquire(&repo)?;
    let mut recovered = vec![];
    for record in state::records(&repo)? {
        let j = &record.journal;
        if j.kind == "create" {
            if acknowledge_create {
                recovered.push(record.path.clone());
                record.finish()?;
                continue;
            }
            return Err(Error::new("E_RECOVERY", format!("interrupted creation retained at {} (stage {}). Inspect Git registration, HEAD and index there. Restore missing tracked files with git checkout-index --all (never --force). A registered workspace with no index first needs git read-tree HEAD after checking for user changes. To accept the retained directory/refs and clear only the tool journal: tool recover {} --acknowledge-create", j.root.display(), j.stage, repo.root.display())));
        }
        if j.kind != "convert" || j.root != repo.root || !j.target.starts_with(&repo.root) {
            return Err(Error::new(
                "E_RECOVERY",
                "journal target does not belong to the requested worktree",
            ));
        }
        let sibling = j
            .sibling
            .as_ref()
            .ok_or_else(|| Error::new("E_STATE", "journal sibling missing"))?;
        if sibling.parent() != j.target.parent()
            || !sibling
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with(".tool-clone-"))
        {
            return Err(Error::new("E_STATE", "unsafe journal sibling"));
        }
        let sibling_exists = match fs::symlink_metadata(sibling) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if !sibling_exists
            && ((j.clone.is_none() && j.stage == "reserved")
                || (j.clone.is_some() && j.stage == "cloned"))
        {
            // These stages cannot have installed a clone; no retained original needs protection.
            recovered.push(record.path.clone());
            record.finish()?;
            continue;
        }
        let relative = j
            .target
            .strip_prefix(&repo.root)
            .map_err(|_| Error::new("E_STATE", "unsafe target"))?;
        git::no_symlink_components(&repo.root, path_string(relative)?)?;
        let original = j
            .original
            .as_ref()
            .ok_or_else(|| Error::new("E_STATE", "original fingerprint missing"))?;
        match &j.clone {
            None => {
                if sibling_exists {
                    return Err(Error::new("E_RECOVERY", format!("uncheckpointed clone retained at {}; ownership cannot be proven, inspect and remove it manually, then retry", sibling.display())));
                }
                return Err(Error::new(
                    "E_STATE",
                    "inconsistent conversion journal stage",
                ));
            }
            Some(clone) => {
                let current = platform::fingerprint(&j.target)?;
                if sibling_exists {
                    let other = platform::fingerprint(sibling)?;
                    if j.stage == "cloned"
                        && current.identity() == original.identity()
                        && current.equivalent(original)
                        && other.identity() == clone.identity()
                        && other.digest == clone.digest
                    {
                        fs::remove_file(sibling)?;
                        platform::sync_dir(
                            sibling
                                .parent()
                                .ok_or_else(|| Error::new("E_STATE", "invalid sibling"))?,
                        )?;
                        recovered.push(record.path.clone());
                        record.finish()?;
                        continue;
                    }
                    let known_pair = (current.identity() == original.identity()
                        && other.identity() == clone.identity())
                        || (current.identity() == clone.identity()
                            && other.identity() == original.identity());
                    if !known_pair || !current.equivalent(original) || !other.equivalent(original) {
                        return Err(Error::new("E_RECOVERY", "recovery files changed or have unknown ownership; both versions retained"));
                    }
                    fs::remove_file(sibling)?;
                    platform::sync_dir(
                        sibling
                            .parent()
                            .ok_or_else(|| Error::new("E_STATE", "invalid sibling"))?,
                    )?;
                } else if ![clone.identity(), original.identity()].contains(&current.identity())
                    || !current.equivalent(original)
                {
                    return Err(Error::new(
                        "E_RECOVERY",
                        "missing recovery sibling and unexpected target state",
                    ));
                }
            }
        }
        recovered.push(record.path.clone());
        record.finish()?;
    }
    Ok(Output::success(
        json!({"recovered_journals":recovered,"worktree":repo.root}),
    ))
}

pub fn capabilities() -> Value {
    json!({"executable_version":env!("CARGO_PKG_VERSION"),"json_schema_versions":[1],"minimum_git":"2.43","platforms":["macOS/APFS"],"commands":["create","convert","status","doctor","recover","backup","config","capabilities"],"ignored_copy_policies":["none"],"progress":"stderr","daemon_required":false})
}
