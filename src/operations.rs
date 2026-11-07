use crate::{
    backup, config,
    git::{self, Repository},
    parallel,
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

enum SeedOutcome {
    Cloned(Box<crate::index::VerifiedClone>),
    Checkout(String),
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
                format!("Retry: sprig backup apply {}", root.display())
            };
            output.warn(format!(
                "Workspace is usable; backup configuration failed: {e}. {action}"
            ));
        }
    }
}
fn changed(expected: &git::Snapshot, repo: &Repository) -> Result<()> {
    repo.idle()?;
    if !expected.unchanged(repo)? {
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
    if target != source_head {
        repo.preflight(&target)?;
    }
    if opts.dry_run && !repo.dirty()?.is_empty() {
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
            "source has staged, unstaged, conflicted or non-ignored untracked files; ignored files are omitted",
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
            clone_directory: None,
            pending_clone: None,
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
        let mut verified_clones = vec![];
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
        let seed_entries: Vec<_> = entries
            .iter()
            .filter(|e| !attr_names.contains(e.path.as_str()))
            .collect();
        let seeded = parallel::map(seed_entries, |e| {
            verify_reservation(&dest, &reservation)?;
            let target_path = dest.join(&e.path);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut cloned_file = None;
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
                        platform::preserve_group(&target_path, before.gid)?;
                        state::boundary("create_cloned")?;
                        let after = platform::fingerprint(&target_path)?;
                        if !before.preserves(&after) {
                            let differences = before.preservation_differences(&after);
                            let fields = differences.keys().cloned().collect::<Vec<_>>().join(", ");
                            return Err(Error::new(
                                "E_CHANGED",
                                format!("clone metadata/bytes did not match source ({fields}): {}", target_path.display()),
                            ).with_data(json!({"path":target_path,"source":donor,"kind":"clone_preservation","expected":before,"observed":after,"differences":differences})));
                        }
                        let donor_after = platform::fingerprint(&donor)?;
                        if !donor_after.unchanged(&before) {
                            return Err(Error::new(
                                "E_CHANGED",
                                format!("source metadata/bytes changed during creation: {}", donor.display()),
                            ).with_data(json!({"path":donor,"kind":"source_changed","expected":before,"observed":donor_after})));
                        }
                        cloned_file = Some(after);
                    }
                }
            }
            state::boundary("create_seed")?;
            Ok(match cloned_file {
                Some(fingerprint) => SeedOutcome::Cloned(Box::new(crate::index::VerifiedClone {
                    path: e.path.clone(),
                    fingerprint,
                })),
                None => SeedOutcome::Checkout(e.path.clone()),
            })
        })?;
        for outcome in seeded {
            match outcome {
                SeedOutcome::Cloned(file) => {
                    cloned += 1;
                    cloned_bytes += file.fingerprint.len;
                    verified_clones.push(*file);
                }
                SeedOutcome::Checkout(path) => remaining.push(path),
            }
        }
        verify_reservation(&dest, &reservation)?;
        checkout(&dest, remaining.iter().map(String::as_str))?;
        state::boundary("create_before_index")?;
        crate::index::refresh(&workspace, &entries, verified_clones, &target)?;
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
                "{}; requested destination {}; recovery journal {}. Run sprig doctor {}",
                e.message,
                dest.display(),
                record.path.display(),
                repo.root.display()
            ),
        ).with_data(json!({"retained_workspace":if record.journal.creation.as_ref().is_some_and(|o| o.destination_identity.is_some()) {json!(dest)} else {Value::Null},"requested_destination":dest,"creation_ownership":record.journal.creation,"journal":record.path,"recovery_requires_inspection":true,"recovery_command":["sprig","recover",repo.root],"acknowledge_command":["sprig","recover",repo.root,"--acknowledge-create"],"source_error":e.data}))),
    }
}
fn checkout<'a>(root: &Path, paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut input = vec![];
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    if !input.is_empty() {
        git::run(
            root,
            ["checkout-index", "--index", "--stdin", "-z"],
            Some(&input),
        )?;
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
    if !dry_run {
        platform::validate_workspace_parent(&repo.root)?;
    }
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
        let entries: Vec<_> = donor
            .index()?
            .into_iter()
            .filter(|e| {
                !modified.contains(&e.path) && matches!(e.mode.as_str(), "100644" | "100755")
            })
            .collect();
        for entry in parallel::map(entries, |e| {
            let path = git::no_symlink_components(root, &e.path)?;
            Ok(platform::fingerprint(&path)
                .ok()
                .map(|fp| (e.oid, path, fp)))
        })?
        .into_iter()
        .flatten()
        {
            donors.entry(entry.0).or_default().push((entry.1, entry.2));
        }
    }

    let mut decisions = vec![];
    let mut candidates = vec![];
    let planned = parallel::map(repo.index()?, |entry| {
        let mut candidate = None;
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
                                candidate = Some(Candidate {
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
        Ok((decision, candidate))
    })?;
    for (decision, candidate) in planned {
        decisions.push(decision);
        if let Some(candidate) = candidate {
            candidates.push(candidate);
        }
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
    platform::validate_workspace_parents(
        candidates
            .iter()
            .filter_map(|candidate| candidate.target.parent()),
    )?;
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
        let mut candidates = candidates.into_iter();
        loop {
            let batch: Vec<_> = candidates.by_ref().take(1024).collect();
            if batch.is_empty() {
                break;
            }
            let validate = || -> Result<()> {
                changed(&snapshot, &repo)?;
                for (donor, snapshot) in &donor_snapshots {
                    changed(snapshot, donor)?;
                }
                Ok(())
            };
            let skipped = replace_batch(&repo, batch, validate, &mut count, &mut bytes)
                .map_err(|e| Error::new(e.code, format!("{}; {} files completed cleanup; current-batch files may be installed with originals retained; inspect sprig doctor {} and run sprig recover", e.message, count, repo.root.display())).with_data(e.data))?;
            for path in skipped {
                let relative = path
                    .strip_prefix(&repo.root)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or_else(|| Error::new("E_STATE", "target escaped repository"))?;
                output.data["metadata_skipped_files"] =
                    json!(output.data["metadata_skipped_files"].as_u64().unwrap_or(0) + 1);
                if let Some(files) = output.data["files"].as_array_mut() {
                    if let Some(file) = files
                        .iter_mut()
                        .find(|file| file["path"].as_str() == Some(relative))
                    {
                        file["reason"] = json!("metadata_not_preserved");
                    }
                }
            }
        }
        changed(&snapshot, &repo)?;
        Ok(())
    })();
    if let Err(e) = mutation_result {
        return Err(Error::new(e.code, e.message).with_data(json!({"retained_workspace":repo.root,"journal_directory":repo.common.join("tool/operations"),"completed_files":count,"completed_logical_bytes":bytes,"recovery_command":["sprig","recover",repo.root],"source_error":e.data})));
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

struct Replacement {
    candidate: Candidate,
    sibling: PathBuf,
    record: Record,
}

enum PreparedReplacement {
    Ready(Box<Replacement>),
    Skipped(PathBuf, Box<Record>),
}

fn sync_replacements(repo: &Repository, replacements: &[Replacement]) -> Result<()> {
    let parents: std::collections::BTreeSet<_> = replacements
        .iter()
        .flat_map(|item| {
            [item.sibling.parent(), item.candidate.target.parent()]
                .into_iter()
                .flatten()
        })
        .collect();
    for parent in parents {
        platform::sync_pending(&File::open(parent)?)?;
    }
    platform::sync_dir(&repo.root)
}

fn clone_state(item: &Replacement) -> Result<&Fingerprint> {
    item.record
        .journal
        .clone
        .as_ref()
        .ok_or_else(|| Error::new("E_STATE", "missing clone checkpoint"))
}

fn replace_batch(
    repo: &Repository,
    candidates: Vec<Candidate>,
    validate: impl Fn() -> Result<()> + Sync,
    count: &mut u64,
    bytes: &mut u64,
) -> Result<Vec<PathBuf>> {
    platform::validate_workspace_parents(
        candidates
            .iter()
            .filter_map(|candidate| candidate.target.parent()),
    )?;
    let directory = state::CloneDirectory::new(&repo.root)?;
    match replace_in_directory(repo, candidates, validate, count, bytes, &directory) {
        Ok(skipped) => Ok(skipped),
        Err(e) => {
            let cleanup = directory.remove_if_empty(&repo.root);
            let message = match cleanup {
                Ok(_) => e.message,
                Err(cleanup) => format!("{}; clone directory retained: {}", e.message, cleanup),
            };
            Err(Error::new(e.code, message)
                .with_data(json!({"clone_directory":directory.path,"source_error":e.data})))
        }
    }
}

fn replace_in_directory(
    repo: &Repository,
    candidates: Vec<Candidate>,
    validate: impl Fn() -> Result<()> + Sync,
    count: &mut u64,
    bytes: &mut u64,
    directory: &state::CloneDirectory,
) -> Result<Vec<PathBuf>> {
    let validate_directory = || -> Result<()> {
        if !directory.validate(&repo.root)? {
            return Err(Error::new("E_CHANGED", "clone directory disappeared"));
        }
        Ok(())
    };
    let batch = Record::pending(
        repo,
        Journal {
            schema_version: 2,
            kind: "convert".into(),
            root: repo.root.clone(),
            target: directory.path.clone(),
            sibling: None,
            original: None,
            clone: None,
            commit: None,
            stage: "reserved".into(),
            creation: None,
            clone_directory: Some(directory.clone()),
            pending_clone: None,
        },
    )?;
    let candidates: Vec<_> = candidates.into_iter().enumerate().collect();
    let dev = fs::metadata(&repo.root)?.dev();
    let items = parallel::map(candidates, |(slot, candidate)| {
        validate()?;
        validate_directory()?;
        if candidate.original.dev != dev || candidate.donor_state.dev != dev {
            return Err(Error::new(
                "E_CLONE",
                "file crosses a nested mount; conversion requires files on the workspace volume",
            ));
        }
        let mut expected_clone = candidate.donor_state.clone();
        expected_clone.gid = directory.gid;
        let mut record = batch.batch_entry(
            slot,
            Journal {
                schema_version: 2,
                kind: "convert".into(),
                root: repo.root.clone(),
                target: candidate.target.clone(),
                sibling: None,
                original: Some(candidate.original.clone()),
                clone: None,
                commit: None,
                stage: "reserved".into(),
                creation: None,
                clone_directory: Some(directory.clone()),
                pending_clone: Some(expected_clone),
            },
        );
        // The journal's random identity supplies a sibling name without creating
        // an empty target-side reservation. clonefile still creates exclusively.
        let sibling = directory.path.join(record.sibling_name()?);
        record.journal.sibling = Some(sibling.clone());
        Ok(Replacement {
            candidate,
            sibling,
            record,
        })
    })?;
    state::save_batch(repo, items.iter().map(|item| &item.record), "reserved")?;
    state::boundary("convert_reserved")?;
    let items = parallel::map(items, |mut item| {
        validate_directory()?;
        eprintln!("sharing {}", item.candidate.target.display());
        platform::clone(&item.candidate.donor, &item.sibling)?;
        state::boundary("convert_cloned")?;
        item.record.journal.clone = Some(platform::fingerprint(&item.sibling)?);
        item.record.journal.stage = "cloned".into();
        item.record.journal.pending_clone = None;
        Ok(item)
    })?;
    state::save_batch(repo, items.iter().map(|item| &item.record), "cloned")?;
    let mut ready = vec![];
    let mut skipped = vec![];
    let mut skipped_records = vec![];
    let prepared = parallel::map(items, |mut item| {
        validate_directory()?;
        let result = state::boundary("convert_metadata")
            .and_then(|()| platform::metadata(&item.candidate.target, &item.sibling));
        let new = platform::fingerprint(&item.sibling)?;
        if result.is_err() || !item.candidate.original.preserves(&new) {
            if platform::fingerprint(&item.candidate.target)?.unchanged(&item.candidate.original)
                && new.identity() == clone_state(&item)?.identity()
                && new.digest == item.candidate.original.digest
            {
                fs::remove_file(&item.sibling)?;
                platform::sync_pending(&File::open(
                    item.sibling
                        .parent()
                        .ok_or_else(|| Error::new("E_STATE", "file parent missing"))?,
                )?)?;
                eprintln!(
                    "skipped {}: target metadata cannot be preserved",
                    item.candidate.target.display()
                );
                return Ok(PreparedReplacement::Skipped(
                    item.candidate.target,
                    Box::new(item.record),
                ));
            }
            return Err(Error::new(
                "E_CHANGED",
                "metadata preservation failed and files changed; recovery versions retained",
            ));
        }
        platform::sync_pending(&File::open(&item.sibling)?)?;
        item.record.journal.clone = Some(new);
        item.record.journal.stage = "prepared".into();
        Ok(PreparedReplacement::Ready(Box::new(item)))
    })?;
    for item in prepared {
        match item {
            PreparedReplacement::Ready(item) => ready.push(*item),
            PreparedReplacement::Skipped(path, record) => {
                skipped.push(path);
                skipped_records.push(*record);
            }
        }
    }
    // All clone data, inode metadata and directory entries reach the drive before
    // any prepared journal becomes durable, including on separate Git volumes.
    sync_replacements(repo, &ready)?;
    state::save_batch(
        repo,
        ready
            .iter()
            .map(|item| &item.record)
            .chain(skipped_records.iter()),
        "prepared",
    )?;
    let mut ready = parallel::map(ready, |item| {
        validate_directory()?;
        state::boundary("convert_prepared")?;
        validate()?;
        if !platform::fingerprint(&item.candidate.target)?.unchanged(&item.candidate.original)
            || !platform::fingerprint(&item.sibling)?.unchanged(clone_state(&item)?)
        {
            return Err(Error::new("E_CHANGED", "file changed before atomic swap"));
        }
        repo.idle()?;
        platform::swap(&item.candidate.target, &item.sibling)?;
        state::boundary("convert_swapped")?;
        Ok(item)
    })?;
    sync_replacements(repo, &ready)?;
    for item in &mut ready {
        item.record.journal.stage = "swapped".into();
    }
    state::save_batch(
        repo,
        ready
            .iter()
            .map(|item| &item.record)
            .chain(skipped_records.iter()),
        "swapped",
    )?;
    let completed = std::sync::atomic::AtomicU64::new(0);
    let completed_bytes = std::sync::atomic::AtomicU64::new(0);
    let cleaned = parallel::map(ready, |item| {
        validate_directory()?;
        state::boundary("convert_cleanup")?;
        // Validate both complete versions at the last point before discarding
        // the retained original, including changes through existing mmap/fds.
        let installed = platform::fingerprint(&item.candidate.target)?;
        let original = platform::fingerprint(&item.sibling)?;
        if installed.identity() != clone_state(&item)?.identity()
            || original.identity() != item.candidate.original.identity()
            || !installed.equivalent(&item.candidate.original)
            || !original.equivalent(&item.candidate.original)
        {
            return Err(Error::new(
                "E_CHANGED",
                "file changed before cleanup; both versions retained",
            ));
        }
        fs::remove_file(&item.sibling)?;
        state::boundary("convert_cleaned")?;
        completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        completed_bytes.fetch_add(
            item.candidate.original.len,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(item)
    });
    // The durable swapped journal covers every deletion until this shared
    // directory is flushed, including when a worker stops with an error.
    let cleanup_sync = state::boundary("convert_cleanup_flush")
        .and_then(|()| platform::sync_pending(&File::open(&directory.path)?));
    *count += completed.load(std::sync::atomic::Ordering::Relaxed);
    *bytes += completed_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let ready = cleaned?;
    cleanup_sync?;
    sync_replacements(repo, &ready)?;
    if !directory.remove_if_empty(&repo.root)? {
        return Err(Error::new(
            "E_RECOVERY",
            "clone directory contains unrecorded files; inspect before cleanup",
        ));
    }
    batch.finish_pending()?;
    state::checkpoint(repo)?;
    Ok(skipped)
}

pub fn status(path: &Path, doctor: bool) -> Result<Output> {
    let repo = Repository::open(path)?;
    let journals: Vec<_> = state::records(&repo)?
        .into_iter()
        .map(|r| json!({"journal":r.path,"journal_entry":r.entry,"kind":r.journal.kind,"root":r.journal.root,"target":r.journal.target,"sibling":r.journal.sibling,"stage":r.journal.stage,"journal_schema_version":r.document_schema_version(),"clone_directory":r.journal.clone_directory}))
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

fn finish_recovery(record: Record, pending: &mut Vec<Record>) -> Result<()> {
    if record.journal.clone_directory.is_some() {
        pending.push(record);
        Ok(())
    } else {
        record.finish()
    }
}

pub fn recover(path: &Path, acknowledge_create: bool) -> Result<Output> {
    let repo = Repository::open(path)?;
    let _lock = Lock::acquire(&repo)?;
    let mut recovered = vec![];
    let mut directories = BTreeMap::new();
    let mut pending_records = vec![];
    for record in state::records(&repo)? {
        let j = &record.journal;
        if j.kind == "create" {
            if acknowledge_create {
                recovered.push(record.path.clone());
                finish_recovery(record, &mut pending_records)?;
                continue;
            }
            return Err(Error::new("E_RECOVERY", format!("interrupted creation retained at {} (stage {}). Inspect Git registration, HEAD and index there. Locate its index.lock with git rev-parse --git-path index.lock; remove a stale lock only after confirming Git has stopped. Restore missing tracked files with git checkout-index --all (never --force). A registered workspace with no index first needs git read-tree HEAD after checking for user changes. To accept the retained directory/refs and clear only the Sprig journal: sprig recover {} --acknowledge-create", j.root.display(), j.stage, repo.root.display())));
        }
        if j.kind != "convert" || j.root != repo.root || !j.target.starts_with(&repo.root) {
            return Err(Error::new(
                "E_RECOVERY",
                "journal target does not belong to the requested worktree",
            ));
        }
        if !["reserved", "cloned", "prepared", "swapped"].contains(&j.stage.as_str()) {
            return Err(Error::new("E_STATE", "unknown conversion journal stage"));
        }
        let sibling = j
            .sibling
            .as_ref()
            .ok_or_else(|| Error::new("E_STATE", "journal sibling missing"))?;
        if let Some(directory) = &j.clone_directory {
            if j.schema_version != 2 || sibling.parent() != Some(directory.path.as_path()) {
                return Err(Error::new("E_STATE", "unsafe clone directory journal"));
            }
            directory.validate(&repo.root)?;
            if sibling.file_name() != Some(std::ffi::OsStr::new(&record.sibling_name()?)) {
                return Err(Error::new("E_STATE", "unsafe owned clone name"));
            }
            directories.insert(directory.path.clone(), directory.clone());
        } else if j.schema_version != 1 || sibling.parent() != j.target.parent() {
            return Err(Error::new("E_STATE", "unsafe journal sibling"));
        }
        if !sibling
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
            && (j.clone_directory.is_some()
                || (j.clone.is_none() && j.stage == "reserved")
                || (j.clone.is_some() && j.stage == "cloned"))
        {
            // No owned sibling remains to remove. The entry may already have
            // completed cleanup before a crash or another recovery error. Leave
            // subsequent edits or deletion of its target untouched on retries.
            recovered.push(record.path.clone());
            finish_recovery(record, &mut pending_records)?;
            continue;
        }
        if sibling_exists
            && j.stage == "reserved"
            && j.clone.is_none()
            && j.clone_directory.is_some()
        {
            let relative = j
                .target
                .strip_prefix(&repo.root)
                .map_err(|_| Error::new("E_STATE", "unsafe target"))?;
            git::no_symlink_components(&repo.root, path_string(relative)?)?;
            let original = j
                .original
                .as_ref()
                .ok_or_else(|| Error::new("E_STATE", "missing original fingerprint"))?;
            let expected = j
                .pending_clone
                .as_ref()
                .ok_or_else(|| Error::new("E_STATE", "missing pending clone fingerprint"))?;
            let target = platform::fingerprint(&j.target).map_err(|e| {
                Error::new(
                    "E_RECOVERY",
                    format!("cannot verify original; owned clone retained: {e}"),
                )
            })?;
            let clone = platform::fingerprint(sibling).map_err(|e| {
                Error::new(
                    "E_RECOVERY",
                    format!("cannot verify owned clone; file retained: {e}"),
                )
            })?;
            if !target.unchanged(original) || !clone.equivalent(expected) {
                return Err(Error::new(
                    "E_RECOVERY",
                    "original or uncheckpointed owned clone changed; both versions retained",
                ));
            }
            fs::remove_file(sibling)?;
            platform::sync_dir(
                sibling
                    .parent()
                    .ok_or_else(|| Error::new("E_STATE", "invalid clone parent"))?,
            )?;
            recovered.push(record.path.clone());
            finish_recovery(record, &mut pending_records)?;
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
                        finish_recovery(record, &mut pending_records)?;
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
        finish_recovery(record, &mut pending_records)?;
    }
    for directory in directories.values() {
        if !directory.remove_if_empty(&repo.root)? {
            return Err(Error::new(
                "E_RECOVERY",
                format!(
                    "unrecorded files retained in {}; inspect before cleanup",
                    directory.path.display()
                ),
            ));
        }
    }
    if !pending_records.is_empty() {
        let mut finished = std::collections::BTreeSet::new();
        for record in pending_records {
            if finished.insert(record.path.clone()) {
                record.finish_pending()?;
            }
        }
        state::checkpoint(&repo)?;
    }
    recovered.sort();
    recovered.dedup();
    Ok(Output::success(
        json!({"recovered_journals":recovered,"worktree":repo.root}),
    ))
}

pub fn capabilities() -> Value {
    json!({"executable_name":env!("CARGO_PKG_NAME"),"description":env!("CARGO_PKG_DESCRIPTION"),"executable_version":env!("CARGO_PKG_VERSION"),"json_schema_versions":[1],"journal_schema_versions":[1,2,3],"minimum_git":"2.43","platforms":["macOS/APFS"],"commands":["create","convert","status","doctor","recover","backup","config","cleanup","capabilities"],"ignored_copy_policies":["none"],"progress":"stderr","daemon_required":false})
}
