# Sprig

Fast, space-efficient Git worktrees using copy-on-write. Create independent development workspaces that share storage until files change.

## Install

```sh
brew install gordonbeeming/tap/sprig
```

Already installed? Run `brew upgrade gordonbeeming/tap/sprig`.

The Homebrew package supports Apple Silicon macOS 26+ on APFS. Git is installed as a dependency; Rust and Xcode are not needed to use Sprig.

## Using Sprig

Run these examples from the original, clean repository checkout. Create an independent workspace:

```sh
sprig create ../workspace --from .
```

Create a new branch or use an available existing branch:

```sh
sprig create ../feature --from . --new-branch feature/example
sprig create ../existing --from . --branch available-branch
```

Preview conversion before sharing matching files in an existing worktree:

```sh
sprig convert ../existing --dry-run --json
sprig convert ../existing --source .
sprig status ../existing
```

**Sprig excludes created and converted workspaces from Time Machine by default, including local uncommitted work.** Add `--no-time-machine-exclude` to prevent new exclusions.

```sh
sprig create ../backed-up-workspace --from . --no-time-machine-exclude
sprig --help
```

`sprig create` creates a detached workspace at the source's HEAD. Choose one of `--detach`, `--branch EXISTING` or `--new-branch NAME`; `--branch` cannot accompany `--ref`. New branches start at a resolved commit without changing upstream configuration. Existing destinations and paths overlapping other registered worktrees or Git metadata are rejected.

New workspace roots are private (`0700`). Creation requires parent directories and their ancestors to be owned by the current user or root. Group/world-writable ancestors require sticky-directory protection; allow ACL entries are conservatively rejected. APFS volumes with ownership disabled are unsupported because they cannot enforce these permission safeguards. Reservation identity is checked before Git registration and materialisation; Sprig does not follow a substituted destination symlink.

External APFS drives can mount with ownership disabled. Enable volume ownership through macOS before using them; Sprig never changes mount ownership settings itself. Creation also checks the filesystems of ancestor directories, including across mount boundaries.

Creation requires a clean source: no staged, unstaged, conflicted or non-ignored untracked files. Ignored files are omitted. The initial dependency/cache policy is `copy.ignored = "none"`; no ignored directories, environment files or credentials are copied. If a target has no reusable files, creation reports `W_NO_SHARED_FILES` and partial success so the absence of sharing is visible.

Creation also screens donor metadata: files with ACLs or xattrs other than the OS-generated `com.apple.provenance` are materialised through Git, so donor-only private metadata is not copied into the new workspace. Conversion instead preserves the existing target's metadata.

Stop editors, builds and Git operations in participating worktrees before running either command. The repository lock coordinates Sprig instances; it cannot stop another program or a process holding an old file descriptor. Hash, identity and metadata checks detect many external changes, with a residual race between a check and a filesystem mutation.

## Adopt Sprig across your worktrees

To discover and preview conversion across repository locations, use the optional bulk-adoption helper. For future worktrees, configure the creating application to call Sprig or use a separate Git shortcut. Installing Sprig does not redirect built-in Git commands. The [adoption guide](docs/adopting-sprig.md) covers scan scope, explicit application, aliases, hooks and application integration.

## Conversion and recovery

Conversion discovers compatible registered donors in the same repository; `--source` restricts selection. Commits may differ. Git object IDs narrow the search, then cryptographic BLAKE3 digests of materialised bytes verify matches. Different line endings do not count as identical. Staged/unstaged paths and unsupported file types are skipped, with a reason for each tracked path. Untracked and ignored files remain untouched.

Each replacement uses an owned sibling file, target metadata, a durable journal and native atomic swap. The original stays at the sibling until validation and cleanup finish. Conversion preserves index bytes; the new inode makes Git's stat cache stale, so the next ordinary Git operation may rehash files and refresh that cache.

```sh
sprig doctor ../workspace --json
sprig recover ../workspace --json
```

Recovery verifies file identities and contents before removing a known sibling. It retains changed or unknown files for manual resolution. A crash immediately after native cloning but before its ownership checkpoint can leave an uncheckpointed sibling: inspect it, remove it only when satisfied it is disposable, then retry recovery. Neither recovery nor normal cleanup prunes unrelated Git registrations or deletes branches.

If a journal proves no swap occurred and its sibling is already absent, recovery clears only that journal. Later user edits or deletion of the target do not block this cleanup.

Interrupted creation retains its directory, any branch/registration and its journal. Inspect HEAD, index and local files first. A registered directory with no index needs `git read-tree HEAD` only after checking that no user index state needs saving; `git checkout-index --all` can restore missing paths without `--force`. `sprig recover <repository> --acknowledge-create` accepts the retained creation state and clears its journal without changing workspace files or refs. It does not claim the workspace has been repaired.

## Time Machine

Newly created or converted worktrees are excluded by default. **Local uncommitted work inside an excluded directory is excluded too.** The default sticky exclusion follows the directory when moved. It needs no backup disk or backup run, and Sprig never requests elevated privileges.

```toml
[backup]
time_machine_exclude = true

[copy]
ignored = "none"
```

Configuration precedence is command line, repository, user, built-in default. The user file is `~/Library/Application Support/sprig/config.toml`; the repository file is `<common-git-directory>/sprig.toml`. The repository file is local Git-administrator data, not a checked-in executable policy. TOML accepts only documented keys and values. Git still uses the user's trusted Git configuration and hooks.

```sh
sprig config get backup.time_machine_exclude --json
sprig config set backup.time_machine_exclude false
sprig config set backup.time_machine_exclude true --scope repository
sprig create ../workspace --from . --no-time-machine-exclude
sprig convert ../workspace --time-machine-exclude
sprig backup apply ../workspace
sprig backup remove ../workspace
```

Disabling policy prevents new exclusions and preserves existing ones. Explicit removal requires matching Sprig provenance. Pre-existing and inherited exclusions are preserved. Backup commands require the exact worktree root. The original checkout and common object database are never automatically excluded: conversion of a main checkout remains valid but returns partial success when its default exclusion would contain Git metadata. Use `--no-time-machine-exclude` there.

An exclusion failure retains the valid workspace, reports partial success and provides a retry command. Interrupted exclusion application records pending provenance; if its ownership is uncertain, inspect it before explicitly accepting ownership with `sprig backup apply PATH --resolve-pending`. Sprig will not automatically remove that uncertain exclusion. Other backup controllers must be idle too: external removal/re-addition of identical sticky metadata cannot be distinguished atomically through `tmutil`.

Exclusions affect future Time Machine backups. They do not delete historical backups or prevent APFS local snapshots. On other platforms the backup setting is reported as not applicable.

## Support and limits

| Case | Initial behaviour |
| --- | --- |
| macOS/APFS | Native clone-or-error, capability probe before mutation |
| Other filesystems / Linux | Unsupported; no full-copy or hard-link fallback |
| Ordinary and linked-worktree sources | Supported |
| Linked worktrees belonging to a bare repository | Supported; the bare repository itself is excluded from donors |
| SHA-1 and SHA-256 repositories | Supported through Git |
| UTF-8 spaces, tabs and newlines | NUL-delimited plumbing; supported |
| Non-UTF-8 paths | Explicit rejection |
| Symlinks | Git materialises them during creation; conversion leaves them intact |
| Executable bits, file timestamps, ACLs, xattrs | Native preservation checked before replacement; unroundtrippable metadata is skipped |
| Hard links, set-id/flagged files, special files | Skipped during conversion |
| Sparse checkout/index, assume-unchanged, submodules | Rejected |
| Unmerged index | Rejected for conversion; source is dirty for creation |
| LFS, custom filters, working-tree encoding | Rejected in both operations initially |
| Text, EOL and ident attributes | Target materialisation verified through Git; different bytes are not shared |
| Modified or staged target files | Preserved and skipped |
| Dry-run | Reads only; no probe, lock creation, receipts, Git changes or exclusions |

Clone and replacement verification ignores only the kernel-managed `com.apple.provenance` attribute, whose value macOS can reassign. Other extended attributes, including quarantine, remain protected. Same-file change detection still compares every recorded extended attribute.

Replacement changes inode and ctime; parent directory timestamps may change. File mtime, birth time, atime, ownership, mode, ACLs and xattrs are checked separately. Filesystem reads can affect access-time bookkeeping on other mounts; no no-atime guarantee is made beyond tested APFS behaviour. Process-interruption tests do not establish power-loss durability. CoW saves initial storage, but later writes still require free space and can fail ENOSPC.

Status receipts report historical operation counts, not current exclusive storage savings. Clone-group skips are advisory because files in a clone family may have diverged extents. Logical bytes and summed `st_blocks` are not proof of physical savings. The current verified implementation prioritises preservation and is slower than ordinary warm-cache Git checkout in the measured workload.

## JSON and packaging

Use `--json` on every command consumed by MyTerm. stdout contains one final JSON result; diagnostics, hook output and progress go to stderr. The [version 1 envelope](docs/json-schema-v1.json) has `schema_version`, `outcome`, `code`, `data` and `warnings`; errors also include `message`. Warnings have stable `code` and `message` fields.

| Exit | Outcome |
| --- | --- |
| 0 | Success |
| 2 | Invalid argument or configuration |
| 3 | Unsupported capability |
| 4 | Busy, dirty, existing destination or detected concurrent change |
| 5 | Operation/I/O failure; inspect the diagnostic and recovery state |
| 6 | Workspace valid; configuration, hook, receipt, cleanup or absence of shareable files needs attention |

Consumers should inspect `code`, never match message text. Common error codes are `E_ARGUMENT`, `E_CONFIG`, `E_UNSUPPORTED`, `E_CLONE`, `E_BUSY`, `E_EXISTS`, `E_DIRTY`, `E_CHANGED`, `E_IO`, `E_GIT`, `E_STATE`, `E_RECOVERY` and `E_BACKUP`. Partial-success warnings include `W_CONFIGURATION`, `W_HOOK`, `W_RECOVERY`, `W_INSPECTION_RECORD` and `W_NO_SHARED_FILES`, under envelope code `W_PARTIAL`.

Failures after mutation include structured recovery data: `retained_workspace`, a `journal` or `journal_directory`, and `recovery_command` as an argv array. Conversion also reports `completed_files` and `completed_logical_bytes`. These counts describe work completed before the error; recovery can still be required for the file at the interruption boundary.

Executable semver and JSON schema version are separate. Schema major 1 is additive; consumers ignore unknown fields and reject unknown majors. Until executable 1.0, flags and the Rust library API may change between minor versions. A MyTerm package can bundle this executable per architecture, call `capabilities --json`, and open the returned `data.worktree`. Library code is shared with the CLI; JSON is the supported integration boundary. Sprig runs independently of MyTerm.

## Building from source

Requires Rust 1.85+, Git 2.43+ and macOS/APFS.

```sh
cargo build --release --locked
./target/release/sprig --help
```

## Support development

Support Sprig through [GitHub Sponsors](https://github.com/sponsors/GordonBeeming), [Patreon](https://www.patreon.com/GordonBeeming), or [Buy Me a Coffee](https://buymeacoffee.com/gordonbeeming).

## Licence

Sprig uses the [Functional Source License 1.1 with an MIT future licence (FSL-1.1-MIT)](LICENSE).

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
python3 tests/volume_acceptance.py
SPRIG_BINARY="$PWD/target/release/sprig" python3 tests/benchmark.py --output results.json
```

The default test suite needs Python 3 and uses disposable repositories, synthetic object fixtures and real APFS syscalls. Backup tests create temporary directories under the test user's home because system temporary paths can inherit exclusions. The separate volume suite mounts bounded, disposable APFS/HFS+ disk images headlessly; it never fills a live volume. Debug-only fault injection requires `SPRIG_TEST_FAULTS=1`; release builds omit it.

Read the [design](docs/design.md), [research comparison](docs/research/comparison.md), and [verification record](docs/verification.md) for the evidence, remaining questions and measured limitations.
