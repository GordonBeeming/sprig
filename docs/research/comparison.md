# Reference comparison

All four public reference projects have MIT licences at the inspected revisions. Shunt has FSL-1.1-MIT restrictions at its inspected revision. No source was reused. The linked investigations contain the seven-question evidence, pinned code/tests, release and issue history, and distinctions between source verification, claims and inference. Upstream test suites were inspected, not executed.

| Project / revision | Creation approach | Conversion evidence | Carry forward / avoid |
| --- | --- | --- | --- |
| [simgit](simgit-cowtree.md), `bcb419bf0d4bfd71c82cf8913e2fa3c0848f572f` | Commit-keyed checkout baseline, native tree clone, custom index handling | No inspected in-place conversion | Immutable donor concept is useful. Avoid hand-written index/stat repair and assuming commit IDs alone determine filtered bytes. |
| [cowtree](simgit-cowtree.md), `82336e3bcb5e9c28018529e58a0c515aa3617fe0` | `--no-checkout`, verified file seeding, Git checkout for remainder, explicit hook | Hash matching and sibling rename; no durable replacement journal in inspected path | Adopt byte verification and normal Git plumbing. Strengthen locks, temporary-clone verification and recovery; keep status read-only. |
| [git-worktree-apfs](apfs-git-cow.md), `15abaf49c1ddc558bfc40e97fa54b0ac099b9469` | Directory clone, copied metadata removal, temporary registration and repair/reset | None inspected | Native cloning is useful. Avoid copying `.git`, temporary branch inference and repair as the normal creation path. |
| [git-cow](apfs-git-cow.md), `b54c1d59632d354752cb76bf8bef2119f84fa7e2` | Template/index seeding, stat refresh and changed-file checkout | None inspected | Learn from thread-safe probe names and narrow cleanup fixes. Avoid cleanup without destination ownership and any full-copy fallback. |
| [Shunt](shunt.md), `4c90f3251ba969fa3f1a011dc51b1d1a1be2720e` | Database volume sets with native clone fidelity repair and atomic publication | Whole-volume replacement, not Git-file conversion | Adopt metadata fixtures, inode ownership, committed/recovery outcomes and kernel locks. Database quiescence and complete environment restoration are separate constraints. |

The inspected projects do not establish a complete interruption-safe conversion protocol or exact physical savings for this product. README timings are not measured results for this implementation. Shunt's historical fixes make the strongest case for exact ownership, retained recovery paths, metadata tests and volume-level measurements.

Unresolved questions for later work are Linux reflink fidelity, safe support for LFS/custom filters and sparse indexes, a cache allowlist with a credential boundary, and faster verification without weakening concurrent-change detection. APFS clone groups identify lineage but do not establish exclusive extent ownership after writes. External writers and backup controllers cannot be made idle by a cooperating tool lock.
