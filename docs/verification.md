# Verification record

The implementation was built on Apple Silicon with macOS 27.2, Rust 1.89 and installed Git 2.53.0. Tests use disposable repositories. Reference application code remained unchanged during filesystem verification.

## Checks completed

| Check | Evidence and scope |
| --- | --- |
| Formatting and linting | `cargo fmt --all -- --check`; `cargo clippy --all-targets -- -D warnings` |
| Rust test command | `cargo test --locked`: CLI acceptance driver and native metadata/swap test pass |
| CLI scenarios | 37 CLI scenarios and two bulk-adoption scenarios pass; earlier compatibility runs also exercised Git 2.43 and 2.53 |
| Git minimum | Built the [official Git 2.43.0 release](https://www.kernel.org/pub/software/scm/git/git-2.43.0.tar.gz) in a temporary directory and reran all CLI scenarios. Local-command compatibility build disabled curl, OpenSSL, gettext and Tcl/Tk; network operations were not tested. System Git was unchanged. |
| Native metadata | Real APFS clone, target mode, ACL, xattr removal/copy, atime, birth time, swap and independent writes |
| Git interoperability | Requested refs/branches, linked sources, SHA-256 objects, clean status, diff and ordinary worktree removal. A separate disposable smoke test made and verified a normal signed commit through installed Git. |
| Preservation | Staged/unstaged changes, ignored credentials, untracked files, symlinks, executable modes and index bytes; different commits and line endings |
| Recovery and concurrency | Process exits around clone/swap/cleanup, missing siblings, metadata failure, journal publication, external target/donor edits and cooperating creation/conversion locks |
| Native volume failures | Bounded APFS disk image: wrong-volume rejection and actual ENOSPC with target preserved. HFS+ image: conversion and dry-run rejected without target mutation. |
| Time Machine | Actual `tmutil` sticky exclusions, defaults/overrides, inherited and pre-existing exclusions, moves, failure/retry, removal and pending provenance. Original checkout is protected even with a separate Git directory. |
| Release executable | `cargo build --release --locked`; version and capabilities output checked |

The installed `tmutil(8)`, `clonefile(2)` and `setattrlist(2)` manuals informed the native implementation. Tests found that metadata copying did not restore a newer target birth time; explicit `fsetattrlist` restoration now has a native assertion. Another regression test covers filtered Git output whose length differs from its original blob size.

All clone and swap tests above use real APFS syscalls, including the tests on mounted disk images. Disk images provide bounded virtual block devices, not mocked clone implementations. Debug-only injected errors simulate selected I/O failures; process-exit tests exercise actual reopening and recovery. These checks do not prove power-loss durability or protection against arbitrary external writers.

## Measurements

Raw repeated measurements are in [host results](benchmark-results.json) and [controlled APFS results](controlled-benchmark-results.json). Both record the measured release binary's SHA-256, environment, timings and volume allocation deltas. The workload has sixteen random 8 MiB files plus small tracked fixtures, warm Git objects, and three trials. `logical_worktree_bytes` counts regular files and excludes symlinks.

The controlled experiment uses a separate APFS filesystem in a disposable 2 GiB sparse disk image. Its idle controls isolate allocation from other applications writing to the host volume. Host results retain real background noise. Global sync runs outside elapsed command timing.

The current Sprig measurements report median times of 0.105 s for ordinary creation, 1.189 s for CoW creation and 4.716 s for conversion on the controlled volume. Host medians were 0.128 s, 1.015 s and 3.164 s respectively. Raw per-trial allocation deltas and the exact measured binary hash are in the linked JSON files. The [bounded-volume results](volume-results.json) record the actual ENOSPC and unsupported-filesystem checks.

Native sharing substantially reduces initial allocation in this workload, but creation remains slower than ordinary warm-cache Git checkout. Conversion performs repeated byte/metadata verification and durable per-file checkpoints. These results establish storage behaviour, not a speed advantage.

APFS snapshots can retain old blocks after conversion, and volume accounting includes metadata and delayed allocation. Summed `st_blocks` includes shared blocks; it is not exclusive usage. Clone-group IDs describe lineage and do not prove that all extents remain shared after writes. Later private writes allocate storage and can fail ENOSPC. A larger changed target still needs space for Git-materialised files plus metadata and recovery records.

## Independent review

Claude Sonnet reviewed the implementation through the core cross-agent runner, then checked the fixes. Its final reports state no blocking findings. Fixes addressed journal publication, pre-swap cleanup, missing-sibling recovery, access-time change detection, pending backup provenance, donor metadata screening, minimum-Git configuration parsing and structured failure recovery data. A further targeted read checked creation ownership, configuration locks and read-only preflight.

Creation journals deliberately retain uncertain state until inspection and explicit acknowledgement. Acknowledgement clears only Sprig's journal; it does not delete or repair files or refs. Probe files normally live in private Sprig-owned common-Git storage, with an external same-volume parent fallback when necessary.

Remaining limits are documented in the [README](../README.md): macOS/APFS only, unsupported Git features rejected, ignored-cache policy limited to `none`, residual external-writer races, manual inspection of uncheckpointed clones, and no power-loss guarantee. Rust 1.85 is the declared dependency/API floor; these runs used Rust 1.89.

The pre-release check added regressions for metadata-only donor timestamp changes and staged renames with newline paths. Git name-only diff can report stat-only changes while index auto-refresh is disabled; selection now uses porcelain status, which verifies contents in memory without writing the index. Independent review checked that parser and the PR CI workflow.

PR review added a recovery regression covering reserved/cloned journals with no sibling after the target is edited or deleted. Recovery clears only the pre-swap journal and preserves the current Git state. The bounded APFS volume test also registers a busy donor on another volume and verifies it is ignored before any donor Git preflight.

Further PR regressions cover automatic donor discovery for linked worktrees of a bare repository, and destination reservation replacement with a symlink. Creation retains an open private directory reservation and rejects unsafe ancestor permissions. The native image suite now enables ownership for normal cases and separately verifies that ownership-disabled APFS mounts are rejected. The symlink test also verifies explicit journal acknowledgement leaves the user-created symlink and victim directory untouched.

The adoption guide and packaged Python helper were reviewed independently. Tests cover preview, explicit idle confirmation, sequential application, locked/original skip policy, missing roots, and Git-state preservation. The documented Git alias was exercised with an absolute destination containing spaces. No global hooks or aliases were installed, and no live worktrees were bulk-converted.
