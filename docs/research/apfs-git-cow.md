# git-worktree-apfs and git-cow investigation

Source inspection on 29 October 2026 AEST. “Code” means the implementation was read at the pinned commit. “Test” means an assertion exists in that source; these upstream suites were not executed. README measurements remain author claims. Risks described below are inferences from the code, unless a historical report is linked.

| Project | Inspected commit | Release | Licence |
| --- | --- | --- | --- |
| git-worktree-apfs | [`15abaf49c1ddc558bfc40e97fa54b0ac099b9469`](https://github.com/dannolan/git-worktree-apfs/commit/15abaf49c1ddc558bfc40e97fa54b0ac099b9469) | [v0.1.3](https://github.com/dannolan/git-worktree-apfs/releases/tag/v0.1.3) | [MIT](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/LICENSE) |
| git-cow | [`b54c1d59632d354752cb76bf8bef2119f84fa7e2`](https://github.com/gabrielkoerich/git-cow/commit/b54c1d59632d354752cb76bf8bef2119f84fa7e2) | [v0.4.0](https://github.com/gabrielkoerich/git-cow/releases/tag/v0.4.0) | [MIT](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/LICENSE) |

No source was reused. Reuse of substantial portions would require retaining the MIT notices. Repository instructions in git-worktree-apfs were read before its implementation. git-cow has no AGENTS.md at this commit; its SKILL.md is usage material, not instructions for this project.

## git-worktree-apfs

### 1. Real worktree and shared blocks

The tool asks the installed Git executable for a temporary linked worktree using `worktree add --no-checkout`. It then calls native `clonefile(source, destination, 0)` on the entire source directory, removes the cloned `.git`, installs the temporary worktree's pointer, runs `worktree repair`, and removes the temporary directory. Git owns the linked registration and independent HEAD; `reset --mixed --no-refresh` creates the destination index. See [creation](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/worktree/add.go#L29-L84), [temporary registration](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/worktree/add.go#L128-L164), and [clone implementation](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/clone/apfs_darwin.go#L36-L54).

Both paths are checked for an APFS filesystem name through `df` and `diskutil`. This is not a cross-path clone probe or same-volume check; an incompatible pair fails at the actual clone, after temporary Git registration. The strategy has no full-copy fallback. See [support check](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/clone/apfs_darwin.go#L23-L33).

### 2. Different commit and index reconciliation

The default rejects a target whose resolved HEAD differs from the source. This check happens after Git creates the temporary worktree and potentially creates the requested branch. `--allow-mismatched-ref` bypasses it; mixed reset then puts the target commit in the index while retaining all cloned source bytes. Those differences become local changes, rather than a clean checkout of the requested commit. See [HEAD comparison](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/worktree/add.go#L167-L179) and [reset](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/worktree/add.go#L189-L194).

### 3. Dirty state, ignored files and metadata

Cleanliness is `git status --porcelain=v1 --untracked-files=all`: staged changes, unstaged changes and ordinary untracked files reject creation. Ignored files do not make that check dirty. The directory clone includes ignored caches, ignored credentials and machine configuration without a selection policy. With `--allow-dirty`, source materialised bytes are cloned, but its index is not preserved: source staged-versus-unstaged distinctions disappear when the target index is rebuilt. See [clean check](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/worktree/add.go#L99-L107) and the clone/reset paths above.

Symlinks, executable bits, xattrs, ACLs and backup exclusions receive no explicit treatment. They depend on native directory-clone semantics, which this repository does not test. Passing flags `0` also leaves top-level symlink following to the API; source root resolution uses Git's textual root rather than an explicit inode/path ownership protocol. No file-by-file metadata inventory or verification exists.

### 4. Existing worktree conversion

There is no conversion operation, donor matching, byte comparison or atomic per-file replacement protocol. The [CLI dispatch](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/cli/cli.go#L19-L38) exposes creation and version/help only.

### 5. Failure and concurrency

Normal clone, pointer-write and reset errors attempt recursive destination deletion. Those cleanup results are ignored. Deferred temporary-directory deletion also ignores errors, does not deregister the temporary worktree and does not delete an owned new branch. A mismatched-ref failure therefore leaves registration/branch cleanup unresolved. There is no journal, operation lock, source-change check, disk-full test or process-interruption handling. The initial destination existence check and subsequent clone/delete are separate operations, so another actor can create the path between them; cleanup lacks a durable ownership proof. See [error paths](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/worktree/add.go#L46-L78).

### 6. Git features

It invokes installed Git and validates mutually exclusive branch modes in the CLI. It does not explicitly reject submodules, sparse checkouts, split indexes, LFS, filters or overlapping source/destination paths. Cloning submodule `.git` files can preserve pointers into source metadata; rebuilding a full index beside a sparse source can expose missing files as deletions. These are untested risks, not reproduced failures. Linked sources appear structurally compatible because only the cloned top-level `.git` is replaced, but there is no integration test. Path arguments are passed as separate process arguments, while Git output is trimmed and filesystem detection parses human-readable output; unusual root names need dedicated tests.

### 7. Verification and history

The entire test file has three CLI/help/argument-normalisation tests, with [no creation, independence, metadata or allocation assertions](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/internal/cli/cli_test.go). The [CI matrix](https://github.com/dannolan/git-worktree-apfs/blob/15abaf49c1ddc558bfc40e97fa54b0ac099b9469/.github/workflows/test.yml) runs those tests and builds on macOS and Linux. This provides no physical-saving evidence.

All five commits and four releases were inspected. v0.1.2 switched to native path cloning; v0.1.3 switched to a directory clone. The GitHub all-state issues endpoint returned no entries. These changes show an optimisation sequence, without preservation or failure-recovery evidence.

## git-cow

### 1. Real worktree and shared blocks

git-cow maintains a private template checkout. It creates a linked target with `git worktree add --no-checkout`, clones each template top-level entry except `.git`, copies the template index into the target gitdir, refreshes its stat cache and runs `git reset --hard`. macOS uses `clonefile` with `CLONE_NOFOLLOW`; Linux uses `cp -a --reflink=always`. See [native backend](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L92-L129) and [populate](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L356-L391).

It probes a real small-file clone for each source/destination directory pair. The CLI falls back to ordinary worktree creation on errors, unsupported options or `GIT_COW=0`, with a stderr explanation. The library `add_with` requires cloning; `add_or_plain` exposes the fallback as an `Added::Plain(reason)` result. This fallback conflicts with this project's strict CoW creation contract. See [probe](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L143-L162), [CLI fallback](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/main.rs#L124-L209) and [library outcome](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L468-L507).

### 2. Different commit and index reconciliation

The template follows `origin/HEAD`, falling back to HEAD. Copying the template index lets Git recognise unchanged cloned files; refreshing stat data then resetting to the target HEAD changes differing entries. A [test covers changed, added and deleted paths on another commit](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L99-L133). The implementation ignores refresh errors, and there are no split-index, sparse-index, worktree-specific attributes or index-extension tests. Copying an index requires more validation than this test establishes.

### 3. Dirty state, ignored files and metadata

Template bootstrap clones tracked top-level source paths, copies the source index, then resets and cleans only the template. Dirty source bytes and untracked files therefore need not become target changes. A [test checks unstaged edits, untracked files and nested ignored secrets while retaining source state](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L454-L514). Staged/unstaged distinction is not tested or promised for the target. See [bootstrap](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L203-L241).

Ignored directories are warmed separately. Loose ignored files are excluded; every hidden component must be on the share list. Non-hidden ignored directories are copied automatically. This reduces `.env` leakage, but a non-hidden ignored credentials directory still qualifies. [Tests cover hidden-directory policy and independent `.git` pointers](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L135-L229). Warm-up errors can be silently ignored, so a successful target may lack some expected caches. See [selection and replacement](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L243-L295).

Symlink handling and executable/ACL/xattr preservation rely on the backend. No test explicitly verifies these, and no code removes donor backup exclusions or records provenance.

### 4. Existing worktree conversion

There is no in-place conversion or identical-file donor search. `warm` replaces template cache directories; it does not convert an existing user worktree. Its delete-then-rename cache operation has no preservation contract suitable for target tracked files.

### 5. Failure and concurrency

A stable adjacent file is OS-locked during template mutation and cloning. Process termination releases that lock, avoiding a PID-based stale ownership check, but it can leave partial worktrees and staging files because there is no durable operation journal. Source editors and builds are not locked. Cleanup and warming errors are often discarded. See [lock](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L164-L175), [add cleanup](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L415-L465) and [template removal's concurrency warning](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L314-L337).

An important ownership risk: any `populate` error triggers `worktree remove --force` and recursive removal of the requested target, including when `worktree add` failed because that path already existed. There is no guard showing that this call created the directory. Branch cleanup is more careful: it deletes only a branch absent before the operation. Tests cover [pre-existing branch preservation](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L261-L278), but do not cover pre-existing destination preservation.

A [required-smudge-filter failure test](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L673-L717) verifies normal cleanup after registration. It does not cover failed cleanup, ENOSPC, kill boundaries or recovery.

### 6. Git features

The template is rejected if `.gitmodules` exists. This checks the template commit, not every target gitlink or every possible `.gitmodules`-free submodule shape. Unknown CLI flags use plain Git, so “accepted” flags need not use CoW. Git subprocesses preserve ordinary configuration and run filters when Git materialises changed files; a required failing filter is tested. Hook behaviour is not tested, and `--no-checkout` plus hard reset does not establish ordinary checkout-hook equivalence. Sparse checkouts, split indexes, LFS and worktree-specific configuration have no explicit support checks or tests.

Git output is decoded lossily and parsed by lines without `-z`; path arguments also use `to_string_lossy`. Newlines, non-UTF-8 paths and Git-quoted names are not handled safely by those parsers. See [Git wrapper](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L131-L141), [top-level path enumeration](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/lib.rs#L225-L240) and [worktree listing](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/src/main.rs#L212-L232).

### 7. Verification and history

The [CI matrix](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/.github/workflows/ci.yml#L8-L40) requires successful probes on APFS and a loop-mounted XFS reflink volume, and also tests ext4 fallback. Tests verify content, Git status, cleanup and probe concurrency. They do not measure physical extents or allocation, and have no explicit two-way post-clone write-independence test. The [README benchmarks](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/README.md#benchmarks) report one run per workload and bytes written after creation; no measurement script establishes repeatability or isolated filesystem allocation.

All 18 commits, six releases and all ten issue/PR entries were inspected. [Issue 5](https://github.com/gabrielkoerich/git-cow/issues/5) records real integration problems. [PR 6](https://github.com/gabrielkoerich/git-cow/pull/6) fixes concurrent PID-only probes that removed each other's files, and removes global `worktree prune`, which could delete unrelated worktree administration while another add ran or a drive was absent. Tests now [exercise concurrent probes](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L596-L619) and [preserve unrelated registrations](https://github.com/gabrielkoerich/git-cow/blob/b54c1d59632d354752cb76bf8bef2119f84fa7e2/tests/cow.rs#L621-L653). [PR 7](https://github.com/gabrielkoerich/git-cow/pull/7) adds an actual “never warm” mode because host build output can be incompatible with Linux container builds. [PR 8](https://github.com/gabrielkoerich/git-cow/pull/8) adds explicit library fallback results. The newer open [PR 9](https://github.com/gabrielkoerich/git-cow/pull/9) proposes local macOS CI; it is outside the inspected main SHA and is not evidence that those changes shipped.

## Decisions these projects inform

Adopt real Git registration through the installed executable, a strict native-clone primitive, actual source/destination capability probes, NUL-delimited Git plumbing and stable OS lock files. Keep explicit clone-required outcomes. Preserve the historical fix that avoids blanket pruning of unrelated registrations.

Avoid whole-checkout cloning followed by deletion of copied repository metadata, implicit cache selection, unconditional cleanup of a caller-supplied path, copying arbitrary source indexes without feature checks, and treating successful `du` output or a clone probe as quantified physical savings.

Neither project answers conversion preservation, interruption recovery, metadata fidelity or Time Machine provenance. These need this project's own protocol and real APFS tests. The remaining creation question is how to reconcile changed commits while retaining sharing and normal hook/filter behaviour without inheriting source index extensions or unsafe cache contents.
