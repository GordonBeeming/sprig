"""Real CLI/APFS tests; every repository and mutation lives in a disposable directory."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

BINARY = str(Path(os.environ.get("TOOL_BINARY", "target/debug/tool")).resolve())


def run(*args, cwd=None, data=None, env=None, check=True):
    result = subprocess.run(args, cwd=cwd, input=data, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env)
    if check and result.returncode:
        raise AssertionError(f"{args}: {result.returncode}\n{result.stdout!r}\n{result.stderr!r}")
    return result


class Fixture:
    def __init__(self, base, object_format="sha1"):
        self.base = Path(base)
        self.repo = self.base / "source"
        self.repo.mkdir()
        run("git", "init", "--initial-branch=main", "--object-format=" + object_format, str(self.repo))
        self.write("a", b"original data\n" * 100)
        self.write("b", b"another file\n")
        self.write("executable", b"#!/bin/sh\nexit 0\n")
        (self.repo / "executable").chmod(0o755)
        self.write(".gitignore", b"cache/\n.env.local\n")
        os.symlink("a", self.repo / "link")
        self.head = self.commit()
        self.env = dict(os.environ, HOME=str(self.base / "home"))
        (self.base / "home").mkdir()

    def write(self, name, data):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def git(self, *args, path=None, data=None):
        return run("git", "-c", "diff.autoRefreshIndex=false", "-C", str(path or self.repo), *args, data=data,
                   env=dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0")).stdout

    def commit(self):
        self.git("add", "--all")
        tree = self.git("write-tree").strip()
        # Synthetic objects are fixture data; no commit-signing configuration is changed.
        old = self.git("rev-parse", "--verify", "HEAD") if (self.repo / ".git/refs/heads/main").exists() else b""
        body = b"tree " + tree + b"\n" + (b"parent " + old.strip() + b"\n" if old else b"")
        body += b"author Fixture <fixture@example.invalid> 1700000000 +0000\ncommitter Fixture <fixture@example.invalid> 1700000000 +0000\n\nfixture\n"
        oid = self.git("hash-object", "-t", "commit", "-w", "--stdin", data=body).strip().decode()
        self.git("update-ref", "refs/heads/main", oid)
        return oid

    def tool(self, *args, code=0, env=None, cwd=None):
        result = run(BINARY, "--json", *map(str, args), env=env or self.env, cwd=cwd, check=False)
        if code is not None:
            assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
        return json.loads(result.stdout), result

    def ordinary(self, name="target", commit=None):
        path = self.base / name
        self.git("worktree", "add", "--detach", str(path), commit or self.head)
        return path

    def state(self, root):
        return (self.git("rev-parse", "HEAD", path=root),
                self.git("status", "--porcelain=v1", "-z", "--untracked-files=all", path=root),
                self.git("diff", "--binary", path=root), self.git("diff", "--cached", "--binary", path=root),
                Path(self.git("rev-parse", "--absolute-git-dir", path=root).decode().strip(), "index").read_bytes())


@unittest.skipUnless(os.uname().sysname == "Darwin", "native acceptance requires macOS/APFS")
class Acceptance(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="tool-acceptance-")
        self.addCleanup(self.temp.cleanup)
        self.f = Fixture(self.temp.name)

    def test_create_default_and_write_independence(self):
        f = self.f
        f.write("cache/secret", b"ignored secret")
        source = f.state(f.repo)
        config = (f.repo / ".git/config").read_bytes()
        dest = f.base / "space and\nnewline"
        out, _ = f.tool("create", dest, "--from", f.repo, "--no-time-machine-exclude")
        self.assertEqual(out["data"]["commit"], f.head)
        self.assertGreater(out["data"]["cloned_files"], 0)
        self.assertEqual(f.git("status", "--porcelain", path=dest), b"")
        self.assertEqual(f.git("symbolic-ref", "-q", "--short", "HEAD", path=f.repo), b"main\n")
        self.assertEqual(source, f.state(f.repo))
        self.assertEqual(config, (f.repo / ".git/config").read_bytes())
        self.assertFalse((dest / "cache").exists())
        self.assertTrue((dest / "link").is_symlink())
        self.assertEqual(os.readlink(dest / "link"), "a")
        self.assertEqual((dest / "executable").stat().st_mode & 0o777, 0o755)
        (dest / "a").write_bytes(b"private target")
        self.assertTrue((f.repo / "a").read_bytes().startswith(b"original"))
        (f.repo / "b").write_bytes(b"private source")
        self.assertEqual((dest / "b").read_bytes(), b"another file\n")
        self.assertTrue(f.git("diff", path=dest))
        (dest / "a").write_bytes((f.repo / "a").read_bytes())
        f.git("worktree", "remove", str(dest))
        self.assertFalse(dest.exists())

    def test_creation_different_commit_and_linked_source(self):
        f = self.f
        old = f.head
        f.write("b", b"changed commit\n"); f.write("new", b"new file\n")
        new = f.commit()
        linked = f.ordinary("linked", old)
        dest = f.base / "created"
        out, _ = f.tool("create", dest, "--from", linked, "--new-branch", "feature/test", "--ref", new, "--no-time-machine-exclude")
        self.assertEqual(out["data"]["commit"], new)
        self.assertEqual((dest / "b").read_bytes(), b"changed commit\n")
        self.assertEqual(f.git("status", "--porcelain", path=dest), b"")
        self.assertEqual(f.git("symbolic-ref", "--short", "HEAD", path=dest), b"feature/test\n")

    def test_branch_modes_preflight(self):
        f = self.f; dest = f.base / "created"
        out, _ = f.tool("create", dest, "--from", f.repo, "--branch", "main", "--ref", f.head, code=2)
        self.assertEqual(out["code"], "E_ARGUMENT"); self.assertFalse(dest.exists())
        f.tool("create", dest, "--from", f.repo, "--branch", "main", code=4)
        self.assertFalse(dest.exists())
        f.git("branch", "available", f.head)
        f.tool("create", dest, "--from", f.repo, "--branch", "available", "--no-time-machine-exclude")
        self.assertEqual(f.git("symbolic-ref", "--short", "HEAD", path=dest), b"available\n")

    def test_sha256_and_non_utf8_rejection(self):
        parent = self.f.base / "sha256"; parent.mkdir()
        f = Fixture(parent, "sha256")
        target = f.base / "created"
        out, _ = f.tool("create", target, "--from", f.repo, "--no-time-machine-exclude")
        self.assertEqual(len(out["data"]["commit"]), 64)
        f.tool("convert", target, "--no-time-machine-exclude")
        # APFS rejects non-UTF-8 names, but a real Git tree can still contain one.
        blob = f.git("hash-object", "-w", "--stdin", data=b"unsupported filename").strip()
        tree = f.git("mktree", "-z", data=b"100644 blob " + blob + b"\tbad-\xff\0").strip()
        body = b"tree " + tree + b"\nauthor Fixture <fixture@example.invalid> 1700000000 +0000\ncommitter Fixture <fixture@example.invalid> 1700000000 +0000\n\nnon-UTF-8 fixture\n"
        commit = f.git("hash-object", "-w", "-t", "commit", "--stdin", data=body).strip().decode()
        f.tool("create", f.base / "unsupported", "--from", f.repo, "--ref", commit, code=3)
        self.assertFalse((f.base / "unsupported").exists())

    def test_dirty_existing_and_nesting_unchanged(self):
        f = self.f; f.write("untracked", b"keep")
        before = f.state(f.repo)
        f.tool("create", f.base / "created", "--from", f.repo, code=4)
        self.assertEqual(before, f.state(f.repo))
        (f.repo / "untracked").unlink()
        existing = f.base / "existing"; existing.mkdir(); (existing / "user").write_bytes(b"keep")
        f.tool("create", existing, "--from", f.repo, code=4)
        f.tool("create", f.repo / "nested", "--from", f.repo, code=2)
        self.assertEqual((existing / "user").read_bytes(), b"keep")

    def test_conversion_preserves_dirty_staged_ignored_symlink_index(self):
        f = self.f; target = f.ordinary()
        (target / "b").write_bytes(b"staged"); f.git("add", "b", path=target)
        (target / "b").write_bytes(b"unstaged after staged")
        (target / "untracked").write_bytes(b"keep")
        (target / ".env.local").write_bytes(b"credential")
        before = f.state(target)
        run("xattr", "-w", "test.tool.target", "preserve", str(target / "a"))
        out, _ = f.tool("convert", target, "--source", f.repo, "--no-time-machine-exclude")
        self.assertGreater(out["data"]["replaced_files"], 0)
        self.assertEqual(before, f.state(target))
        self.assertEqual((target / ".env.local").read_bytes(), b"credential")
        self.assertEqual((target / "untracked").read_bytes(), b"keep")
        self.assertEqual(run("xattr", "-p", "test.tool.target", str(target / "a")).stdout.strip(), b"preserve")
        self.assertTrue((target / "link").is_symlink())
        again, _ = f.tool("convert", target, "--no-time-machine-exclude")
        self.assertEqual(again["data"]["replaced_files"], 0)
        (target / "a").write_bytes(b"private")
        self.assertTrue((f.repo / "a").read_bytes().startswith(b"original"))

    def test_separate_git_directory_keeps_original_out_of_exclusions(self):
        f = self.f
        f.git("init", "--separate-git-dir", str(f.base / "git-admin"))
        target = f.ordinary()
        out, _ = f.tool("convert", f.repo, "--source", target, code=6)
        self.assertEqual(out["data"]["backup"]["code"], "E_ARGUMENT")
        self.assertTrue((f.repo / ".git").is_file())
        self.assertNotEqual(run("xattr", "-p", "com.tool.backup-provenance-v1", str(f.repo), check=False).returncode, 0)

    def test_metadata_only_donor_change_remains_eligible(self):
        f = self.f; target = f.ordinary(); donor = f.repo / "b"
        before = f.state(f.repo)
        metadata = donor.stat()
        os.utime(donor, ns=(metadata.st_atime_ns, metadata.st_mtime_ns + 2_000_000_000))
        self.assertEqual(f.git("status", "--porcelain=v1", "-z"), b"")
        out, _ = f.tool("convert", target, "--source", f.repo, "--no-time-machine-exclude")
        self.assertTrue(any(row["path"] == "b" and row["reason"] == "candidate" for row in out["data"]["files"]))
        self.assertEqual(before, f.state(f.repo))

    def test_staged_rename_with_unstaged_edits_is_preserved(self):
        f = self.f; target = f.ordinary()
        renamed = "renamed file\nwith newline"
        f.git("mv", "b", renamed, path=target)
        (target / renamed).write_bytes(b"unstaged edit on staged rename")
        before = f.state(target)
        out, _ = f.tool("convert", target, "--source", f.repo, "--no-time-machine-exclude")
        self.assertTrue(any(row["path"] == renamed and row["reason"] == "modified_or_staged" for row in out["data"]["files"]))
        self.assertEqual(before, f.state(target))
        self.assertEqual((target / renamed).read_bytes(), b"unstaged edit on staged rename")

    def test_conversion_different_commit(self):
        f = self.f; target = f.ordinary()
        f.write("b", b"new commit")
        f.commit()
        out, _ = f.tool("convert", target, "--source", f.repo, "--no-time-machine-exclude")
        self.assertGreater(out["data"]["replaced_files"], 0)
        self.assertEqual((target / "b").read_bytes(), b"another file\n")

    def test_dry_run_no_mutation_or_exclusion(self):
        f = self.f; target = f.ordinary()
        before = f.state(target)
        attrs = run("xattr", "-lx", str(target)).stdout
        journal = f.repo / ".git/tool"
        out, _ = f.tool("convert", target, "--dry-run")
        self.assertTrue(out["data"]["dry_run"])
        self.assertEqual(before, f.state(target))
        self.assertFalse(journal.exists())
        self.assertEqual(attrs, run("xattr", "-lx", str(target)).stdout)
        dest = f.base / "preview"
        f.tool("create", dest, "--from", f.repo, "--dry-run")
        self.assertFalse(dest.exists()); self.assertFalse(journal.exists())

    def test_process_crash_and_cleanup_recovery(self):
        f = self.f
        for point in ["convert_prepared", "convert_swapped", "convert_cleanup", "convert_cleaned"]:
            with self.subTest(point=point):
                target = f.ordinary(point)
                before = f.state(target)
                env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH=point)
                result = run(BINARY, "--json", "convert", str(target), "--source", str(f.repo), "--no-time-machine-exclude", env=env, check=False)
                self.assertEqual(result.returncode, 91, result.stderr)
                self.assertEqual((target / "a").read_bytes(), (f.repo / "a").read_bytes())
                f.tool("recover", target)
                self.assertEqual(before, f.state(target))
                self.assertFalse(list(target.glob(".tool-clone-*")))

    def test_uncheckpointed_clone_is_retained(self):
        f = self.f; target = f.ordinary()
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH="convert_cloned")
        result = run(BINARY, "convert", str(target), "--no-time-machine-exclude", env=env, check=False)
        self.assertEqual(result.returncode, 91)
        out, _ = f.tool("recover", target, code=5)
        self.assertEqual(out["code"], "E_RECOVERY")
        self.assertTrue(list(target.glob(".tool-clone-*")))
        self.assertEqual((target / "a").read_bytes(), (f.repo / "a").read_bytes())

    def test_disk_permission_and_cleanup_faults(self):
        f = self.f
        for point in ["convert_reserved", "convert_prepared", "convert_cleanup"]:
            with self.subTest(point=point):
                target = f.ordinary(point)
                env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_FAIL=point)
                out, _ = f.tool("convert", target, "--no-time-machine-exclude", env=env, code=5)
                self.assertEqual(out["code"], "E_IO")
                self.assertEqual((target / "a").read_bytes(), (f.repo / "a").read_bytes())
                f.tool("recover", target)

    def test_external_change_and_cooperating_lock(self):
        f = self.f; target = f.ordinary(); ready = f.base / "ready"
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_PAUSE="convert_prepared", TOOL_TEST_READY=str(ready))
        child = subprocess.Popen([BINARY, "--json", "convert", str(target), "--no-time-machine-exclude"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(ready.exists())
            f.tool("convert", target, "--no-time-machine-exclude", code=4)
            journals = list((f.repo / ".git/tool/operations").glob("*.json"))
            prepared_target = Path(json.loads(journals[0].read_text())["target"])
            prepared_target.write_bytes(b"external editor")
            child.kill(); child.communicate()
            out, _ = f.tool("recover", target, code=5)
            self.assertEqual(out["code"], "E_RECOVERY")
            self.assertEqual(prepared_target.read_bytes(), b"external editor")
        finally:
            if child.poll() is None:
                child.kill(); child.communicate()

    def test_active_external_change_detection(self):
        f = self.f
        for point in ["convert_prepared", "convert_cleanup"]:
            with self.subTest(point=point):
                target = f.ordinary(point)
                ready = f.base / (point + "-ready"); release = f.base / (point + "-release")
                env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_PAUSE=point, TOOL_TEST_READY=str(ready), TOOL_TEST_RELEASE=str(release))
                child = subprocess.Popen([BINARY, "--json", "convert", str(target), "--no-time-machine-exclude"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    deadline = time.monotonic() + 10
                    while not ready.exists() and time.monotonic() < deadline:
                        time.sleep(0.02)
                    self.assertTrue(ready.exists())
                    journal = json.loads(next((f.repo / ".git/tool/operations").glob("*.json")).read_text())
                    changed = Path(journal["target"]); changed.write_bytes(b"external writer")
                    release.write_text("continue")
                    stdout, stderr = child.communicate(timeout=10)
                    self.assertEqual(child.returncode, 4, (stdout, stderr))
                    self.assertEqual(json.loads(stdout)["code"], "E_CHANGED")
                    self.assertEqual(changed.read_bytes(), b"external writer")
                    # Accepting recovery requires resolving the external edit; all versions remain.
                    f.tool("recover", target, code=5)
                    for p in (f.repo / ".git/tool/operations").glob("*.json"):
                        j = json.loads(p.read_text())
                        # Disposable fixture cleanup only, after verifying the preservation failure path.
                        if j.get("sibling"):
                            Path(j["sibling"]).unlink(missing_ok=True)
                        p.unlink()
                finally:
                    if child.poll() is None:
                        child.kill(); child.communicate()

    def test_eol_attributes_are_materialised_correctly(self):
        f = self.f
        old = f.head
        f.write(".gitattributes", b"a text eol=crlf\n")
        new = f.commit()
        target = f.base / "eol"
        f.tool("create", target, "--from", f.repo, "--ref", new, "--no-time-machine-exclude")
        self.assertEqual((target / "a").read_bytes(), b"original data\r\n" * 100)
        self.assertEqual(f.git("status", "--porcelain", path=target), b"")
        source = f.ordinary("lf", old)
        out, _ = f.tool("convert", target, "--source", source, "--no-time-machine-exclude")
        self.assertTrue(any(row["path"] == "a" and row["reason"] == "no_identical_donor" for row in out["data"]["files"]))

    def test_unsupported_sparse_filter_and_submodule(self):
        f = self.f
        f.git("config", "core.sparseCheckout", "true")
        f.tool("create", f.base / "sparse", "--from", f.repo, code=3)
        f.git("config", "core.sparseCheckout", "false")
        f.write(".gitattributes", b"a filter=lfs\n"); f.commit()
        out, _ = f.tool("create", f.base / "filter", "--from", f.repo, code=3)
        self.assertEqual(out["outcome"], "unsupported")
        self.assertFalse((f.base / "filter").exists())
        (f.repo / ".gitattributes").unlink(); f.commit()
        f.git("update-index", "--add", "--cacheinfo", f"160000,{f.head},module")
        tree = f.git("write-tree").strip()
        raw = b"tree " + tree + b"\nauthor Fixture <fixture@example.invalid> 1700000000 +0000\ncommitter Fixture <fixture@example.invalid> 1700000000 +0000\n\nsubmodule\n"
        commit = f.git("hash-object", "-w", "-t", "commit", "--stdin", data=raw).strip().decode()
        f.tool("create", f.base / "submodule", "--from", f.repo, "--ref", commit, code=3)

    def test_hardlink_skipped_and_donor_metadata_does_not_leak(self):
        f = self.f; target = f.ordinary()
        os.link(target / "a", target / "alias")
        run("xattr", "-w", "test.donor-only", "secret", str(f.repo / "b"))
        out, _ = f.tool("convert", target, "--no-time-machine-exclude")
        self.assertTrue(any("hard links" in row["reason"] for row in out["data"]["files"]))
        self.assertEqual((target / "a").stat().st_ino, (target / "alias").stat().st_ino)
        self.assertTrue(any(row["path"] == "b" and row["reason"] == "candidate" for row in out["data"]["files"]))
        self.assertGreater(out["data"]["replaced_files"], 0)
        self.assertNotEqual(run("xattr", "-p", "test.donor-only", str(target / "b"), check=False).returncode, 0)

    def test_creation_hook_and_partial_success(self):
        f = self.f
        hook = f.repo / ".git/hooks/post-checkout"
        hook.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > hook-args\nexit 7\n')
        hook.chmod(0o755)
        out, _ = f.tool("create", f.base / "hook", "--from", f.repo, "--no-time-machine-exclude", code=6)
        self.assertEqual(out["outcome"], "partial_success")
        args = (f.base / "hook/hook-args").read_text().splitlines()
        self.assertEqual(args, ["0" * len(f.head), f.head, "1"])

    def test_create_does_not_copy_private_donor_metadata(self):
        f = self.f
        run("xattr", "-w", "test.donor.secret", "private metadata", str(f.repo / "a"))
        run("chmod", "+a", "everyone allow readsecurity", str(f.repo / "b"))
        target = f.base / "metadata"
        out, _ = f.tool("create", target, "--from", f.repo, "--no-time-machine-exclude")
        self.assertGreater(out["data"]["cloned_files"], 0)
        self.assertNotEqual(run("xattr", "-p", "test.donor.secret", str(target / "a"), check=False).returncode, 0)
        self.assertNotIn(b"0: ", run("ls", "-le", str(target / "b")).stdout)
        self.assertEqual((target / "a").read_bytes(), (f.repo / "a").read_bytes())

    def test_autocrlf_uses_filtered_bytes_and_reports_no_sharing(self):
        f = self.f
        f.git("config", "core.autocrlf", "YES")
        config = (f.repo / ".git/config").read_bytes()
        target = f.base / "crlf"
        out, _ = f.tool("create", target, "--from", f.repo, "--no-time-machine-exclude", code=6)
        self.assertEqual((target / "a").read_bytes(), b"original data\r\n" * 100)
        self.assertEqual(out["data"]["cloned_files"], 0)
        self.assertTrue(any(w["code"] == "W_NO_SHARED_FILES" for w in out["warnings"]))
        self.assertEqual(config, (f.repo / ".git/config").read_bytes())
        self.assertEqual((f.repo / "a").read_bytes(), b"original data\n" * 100)

    def test_pre_swap_missing_sibling_does_not_block_user_edits(self):
        f = self.f
        for stage in ["reserved", "cloned"]:
            for edit in ["changed", "deleted"]:
                with self.subTest(stage=stage, edit=edit):
                    target = f.ordinary(stage + edit)
                    point = "convert_reserved" if stage == "reserved" else "convert_metadata"
                    env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH=point)
                    result = run(BINARY, "convert", str(target), "--no-time-machine-exclude", env=env, check=False)
                    self.assertEqual(result.returncode, 91)
                    journal = json.loads(next((f.repo / ".git/tool/operations").glob("*.json")).read_text())
                    Path(journal["sibling"]).unlink(missing_ok=True)
                    path = Path(journal["target"])
                    if edit == "changed":
                        path.write_bytes(b"user edit after failure")
                    else:
                        path.unlink()
                    before = f.state(target)
                    f.tool("recover", target)
                    self.assertEqual(before, f.state(target))
                    if edit == "changed":
                        self.assertEqual(path.read_bytes(), b"user edit after failure")
                    else:
                        self.assertFalse(path.exists())

    def test_recovery_after_owned_sibling_already_removed(self):
        f = self.f; target = f.ordinary()
        before = f.state(target)
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH="convert_prepared")
        result = run(BINARY, "convert", str(target), "--no-time-machine-exclude", env=env, check=False)
        self.assertEqual(result.returncode, 91)
        journal = json.loads(next((f.repo / ".git/tool/operations").glob("*.json")).read_text())
        Path(journal["sibling"]).unlink()
        f.tool("recover", target)
        self.assertEqual(before, f.state(target))

    def test_donor_race_leaves_a_recoverable_owned_clone(self):
        f = self.f; target = f.ordinary(); ready = f.base / "ready"; release = f.base / "release"
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_PAUSE="convert_reserved", TOOL_TEST_READY=str(ready), TOOL_TEST_RELEASE=str(release))
        child = subprocess.Popen([BINARY, "--json", "convert", str(target), "--no-time-machine-exclude"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(ready.exists())
            journal = json.loads(next((f.repo / ".git/tool/operations").glob("*.json")).read_text())
            changed = Path(journal["target"])
            original = changed.read_bytes()
            donor = f.repo / changed.relative_to(target.resolve())
            donor.write_bytes(b"external donor write")
            release.write_text("continue")
            stdout, stderr = child.communicate(timeout=10)
            self.assertEqual(child.returncode, 4, (stdout, stderr))
            result = json.loads(stdout)
            self.assertEqual(result["data"]["completed_files"], 0)
            self.assertEqual(result["data"]["retained_workspace"], str(target.resolve()))
            self.assertTrue(result["data"]["recovery_command"])
            f.tool("recover", target)
            self.assertEqual(changed.read_bytes(), original)
            self.assertEqual(donor.read_bytes(), b"external donor write")
        finally:
            if child.poll() is None:
                child.kill(); child.communicate()

    def test_concurrent_creation_reserves_only_its_destination(self):
        f = self.f; target = f.base / "created"; ready = f.base / "ready"; release = f.base / "release"
        before = f.state(f.repo)
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_PAUSE="create_reserved", TOOL_TEST_READY=str(ready), TOOL_TEST_RELEASE=str(release))
        child = subprocess.Popen([BINARY, "--json", "create", str(target), "--from", str(f.repo), "--new-branch", "feature/race", "--no-time-machine-exclude"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 10
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(ready.exists())
            journal = json.loads(next((f.repo / ".git/tool/operations").glob("*.json")).read_text())
            self.assertEqual(journal["creation"]["destination_identity"], [target.stat().st_dev, target.stat().st_ino])
            self.assertEqual(journal["creation"]["requested_new_branch"], "feature/race")
            self.assertFalse(journal["creation"]["confirmed_new_branch"])
            f.tool("create", target, "--from", f.repo, code=4)
            f.tool("create", f.base / "other", "--from", f.repo, code=4)
            self.assertFalse((f.base / "other").exists())
            release.write_text("continue")
            stdout, stderr = child.communicate(timeout=10)
            self.assertEqual(child.returncode, 0, (stdout, stderr))
            self.assertEqual(f.git("symbolic-ref", "--short", "HEAD", path=target), b"feature/race\n")
            self.assertEqual(before, f.state(f.repo))
        finally:
            if child.poll() is None:
                child.kill(); child.communicate()

    def test_creation_interruption_retains_registration(self):
        f = self.f; dest = f.base / "interrupted"
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH="create_registered")
        result = run(BINARY, "create", str(dest), "--from", str(f.repo), env=env, check=False)
        self.assertEqual(result.returncode, 91)
        self.assertTrue((dest / ".git").is_file())
        (dest / "user-work").write_bytes(b"keep")
        out, _ = f.tool("doctor", dest)
        self.assertTrue(out["data"]["recovery"])
        f.tool("recover", dest, code=5)
        self.assertEqual((dest / "user-work").read_bytes(), b"keep")
        f.tool("recover", dest, "--acknowledge-create")
        self.assertEqual((dest / "user-work").read_bytes(), b"keep")

    def test_metadata_skip_and_atomic_journal_publication(self):
        f = self.f; target = f.ordinary()
        before = f.state(target)
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_FAIL="convert_metadata")
        out, _ = f.tool("convert", target, "--no-time-machine-exclude", env=env)
        self.assertGreater(out["data"]["metadata_skipped_files"], 0)
        self.assertTrue(any(row["reason"] == "metadata_not_preserved" for row in out["data"]["files"]))
        self.assertEqual(before, f.state(target))
        self.assertFalse(list((f.repo / ".git/tool/operations").glob("*.json")))
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH="journal_before_publish")
        result = run(BINARY, "create", str(f.base / "interrupted"), "--from", str(f.repo), env=env, check=False)
        self.assertEqual(result.returncode, 91)
        out, _ = f.tool("doctor", f.repo)
        self.assertFalse(out["data"]["recovery"])

    def test_newline_paths_and_different_donor_modes(self):
        f = self.f
        f.write("file with spaces\nand newline\n", b"unusual path")
        f.head = f.commit()
        target = f.ordinary("target\n")
        (f.repo / "a").chmod(0o755); f.commit()
        out, _ = f.tool("convert", target, "--source", f.repo, "--no-time-machine-exclude")
        self.assertGreater(out["data"]["replaced_files"], 0)
        self.assertEqual((target / "a").stat().st_mode & 0o777, 0o644)
        self.assertEqual((target / "file with spaces\nand newline\n").read_bytes(), b"unusual path")
        f.tool("create", f.base / "created\n", "--from", target, "--no-time-machine-exclude")

    def test_configuration_precedence_and_read_only_status(self):
        f = self.f
        f.tool("config", "set", "backup.time_machine_exclude", "false")
        f.tool("config", "set", "backup.time_machine_exclude", "true", "--scope", "repository", cwd=f.repo)
        out, _ = f.tool("create", f.base / "preview", "--from", f.repo, "--dry-run", "--no-time-machine-exclude")
        self.assertFalse(out["data"]["policy"]["time_machine_exclude"])
        self.assertEqual(out["data"]["policy"]["backup_origin"], "command-line")
        target = f.ordinary(); before = f.state(target)
        f.tool("status", target); f.tool("doctor", target)
        self.assertEqual(before, f.state(target))
        self.assertFalse((f.repo / ".git/tool").exists())
        f.tool("config", "set", "copy.ignored", "all", code=2)


@unittest.skipUnless(os.uname().sysname == "Darwin", "Time Machine integration requires macOS")
class BackupAcceptance(unittest.TestCase):
    def setUp(self):
        # tmutil treats many temporary-system paths as excluded even without a direct xattr.
        self.temp = tempfile.TemporaryDirectory(prefix="tool-backup-acceptance-", dir=Path.home())
        self.addCleanup(self.temp.cleanup)
        self.f = Fixture(self.temp.name)

    def test_default_move_and_explicit_remove(self):
        f = self.f; dest = f.base / "created"
        out, _ = f.tool("create", dest, "--from", f.repo)
        self.assertTrue(out["data"]["backup"]["excluded"])
        self.assertTrue(out["data"]["backup"]["tool_managed"])
        moved = f.base / "moved"
        f.git("worktree", "move", str(dest), str(moved))
        out, _ = f.tool("status", moved)
        self.assertTrue(out["data"]["backup"]["tool_managed"])
        # Disabling policy leaves the existing tool-managed exclusion in place.
        f.tool("convert", moved, "--no-time-machine-exclude")
        out, _ = f.tool("backup", "remove", moved)
        self.assertFalse(out["data"]["backup"]["direct_sticky"])
        self.assertFalse(out["data"]["backup"]["tool_managed"])

    def test_preexisting_and_inherited_preserved(self):
        f = self.f; target = f.ordinary()
        run("/usr/bin/tmutil", "addexclusion", str(target))
        prior = run("xattr", "-px", "com.apple.metadata:com_apple_backup_excludeItem", str(target)).stdout
        out, _ = f.tool("convert", target)
        self.assertEqual(out["data"]["backup"]["origin"], "pre_existing")
        f.tool("backup", "remove", target, code=5)
        self.assertEqual(prior, run("xattr", "-px", "com.apple.metadata:com_apple_backup_excludeItem", str(target)).stdout)
        run("/usr/bin/tmutil", "removeexclusion", str(target))
        run("/usr/bin/tmutil", "addexclusion", str(f.base))
        out, _ = f.tool("convert", target)
        self.assertEqual(out["data"]["backup"]["origin"], "inherited_or_system")
        self.assertFalse(out["data"]["backup"]["direct_sticky"])
        run("/usr/bin/tmutil", "removeexclusion", str(f.base))

    def test_overrides_failure_and_retry(self):
        f = self.f; dest = f.base / "created"
        f.tool("config", "set", "backup.time_machine_exclude", "false")
        out, _ = f.tool("create", dest, "--from", f.repo)
        self.assertFalse(out["data"]["backup"]["direct_sticky"])
        enabled = f.base / "enabled"
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_FAIL="backup_apply")
        out, _ = f.tool("create", enabled, "--from", f.repo, "--time-machine-exclude", env=env, code=6)
        self.assertEqual(out["outcome"], "partial_success")
        self.assertEqual(f.git("status", "--porcelain", path=enabled), b"")
        out, _ = f.tool("backup", "apply", enabled)
        self.assertTrue(out["data"]["backup"]["tool_managed"])

    def test_interrupted_exclusion_has_explicit_resolution(self):
        f = self.f; target = f.ordinary()
        env = dict(f.env, TOOL_TEST_FAULTS="1", TOOL_TEST_CRASH="backup_added")
        result = run(BINARY, "backup", "apply", str(target), env=env, check=False)
        self.assertEqual(result.returncode, 91)
        out, _ = f.tool("status", target)
        self.assertEqual(out["data"]["backup"]["origin"], "pending_tool_operation")
        f.tool("backup", "remove", target, code=5)
        f.tool("backup", "apply", target, code=5)
        f.tool("backup", "apply", target, "--resolve-pending")
        f.tool("backup", "remove", target)
        f.tool("backup", "apply", target / "subdirectory", code=5)


if __name__ == "__main__":
    unittest.main(verbosity=2)
