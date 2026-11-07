"""Cleanup acceptance tests: real Git/Sprig worktrees, all writes in disposable fixtures."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from acceptance import BINARY, Fixture, run


@unittest.skipUnless(os.uname().sysname == "Darwin", "Sprig worktree creation requires APFS")
class Cleanup(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sprig-cleanup-")
        self.addCleanup(self.temp.cleanup)
        self.f = Fixture(self.temp.name)
        self.f.write(".gitignore", b"bin/\nobj/\ndist/\nnode_modules/\n.env.local\n")
        self.f.commit()
        self.target = Path(self.temp.name) / "sprig target"
        self.f.sprig("create", self.target, "--from", self.f.repo, "--no-time-machine-exclude")
        self.admin = Path(self.f.git("rev-parse", "--absolute-git-dir", path=self.target).decode().strip())
        self.state = self.admin / "sprig-cleanup.json"
        self.setting("cleanup.enabled", "true")
        self.write("src/main.cs", b"source")
        self.write("src/bin/Debug/app.dll", b"rebuildable" * 100)
        self.write("src/obj/project.assets.json", b"rebuildable metadata")
        self.write("node_modules/dependency/dist/index.js", b"dependency")
        self.write(".env.local", b"local config")

    def setting(self, key, value, code=0, scope="repository"):
        return self.f.sprig("config", "set", key, value, "--scope", scope, cwd=self.f.repo, code=code)

    def write(self, path, content):
        path = self.target / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return path

    def cleanup(self, dry=False, env=None, code=0):
        args = ["cleanup", "run", "--repository", self.f.repo]
        if dry:
            args.append("--dry-run")
        report, result = self.f.sprig(*args, env=env, code=code)
        row = next((r for r in report["data"]["worktrees"] if r["worktree"] == str(self.target.resolve())), None)
        return row, report, result

    def age(self):
        state = json.loads(self.state.read_text())
        state["stable_since"] -= 6 * 3600 + 1
        self.state.write_text(json.dumps(state))

    def test_preview_observe_then_clean_only_outputs(self):
        original_index = (self.admin / "index").read_bytes()
        original_head = self.f.git("rev-parse", "HEAD", path=self.target)
        row, report, _ = self.cleanup(True)
        self.assertEqual(row["reason"], "observing")
        self.assertFalse(self.state.exists())
        self.assertFalse((self.admin / "sprig-cleanup-lease").exists())
        self.assertTrue(any(r["reason"] == "not_sprig_managed" for r in report["data"]["worktrees"]))
        row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "observing")
        row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "waiting_for_idle")
        self.age()
        before = self.state.read_bytes()
        row, _, _ = self.cleanup(True)
        self.assertEqual(row["reason"], "eligible")
        self.assertEqual(before, self.state.read_bytes())
        row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "cleaned")
        self.assertGreater(row["removed_logical_bytes"], 0)
        self.assertFalse((self.target / "src/bin").exists())
        self.assertFalse((self.target / "src/obj").exists())
        self.assertTrue((self.target / "src/main.cs").exists())
        self.assertTrue((self.target / "node_modules/dependency/dist/index.js").exists())
        self.assertTrue((self.target / ".env.local").exists())
        self.assertEqual(original_index, (self.admin / "index").read_bytes())
        self.assertEqual(original_head, self.f.git("rev-parse", "HEAD", path=self.target))
        self.assertEqual(len(json.loads(self.state.read_text())["history"]), 1)

    def test_source_and_nested_output_edits_reset_idle(self):
        for path in ["src/main.cs", "src/bin/Debug/app.dll"]:
            self.cleanup()
            self.age()
            self.write(path, b"changed")
            row, _, _ = self.cleanup()
            self.assertEqual(row["reason"], "observing")
            self.assertTrue((self.target / "src/bin").exists())

    def test_opt_in_precedence_and_validation(self):
        self.setting("cleanup.enabled", "true", scope="user")
        self.setting("cleanup.enabled", "false")
        row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "disabled")
        self.assertFalse(self.state.exists())
        self.setting("cleanup.idle_hours", "0", code=2)
        self.setting("cleanup.idle_hours", "-1", code=2)
        self.setting("cleanup.folders", '["../bin"]', code=2)
        self.setting("cleanup.folders", '["node_modules"]', code=2)
        self.setting("cleanup.exclude", '["../src"]', code=2)
        value, _ = self.f.sprig("config", "get", "cleanup.idle_hours", cwd=self.f.repo)
        self.assertEqual(value["data"]["value"], 6)
        self.assertEqual(value["data"]["origin"], "built-in")
        self.setting("cleanup.idle_hours", "2")
        value, _ = self.f.sprig("config", "get", "cleanup.idle_hours", cwd=self.f.repo)
        self.assertEqual(value["data"]["value"], 2)
        self.assertEqual(value["data"]["origin"], "repository")

    def test_tracked_nonignored_symlink_nested_checkout_and_exclusions(self):
        self.write("bin/script.sh", b"tracked script")
        self.f.git("add", "-f", "bin/script.sh", path=self.target)
        self.write("unignored/dist/file", b"precious")
        # Override the root ignore rule for this nested directory.
        self.write("unignored/.gitignore", b"!dist/\n!dist/**\n")
        output = self.write("obj/file", b"output")
        os.symlink(self.target / "src", output.parent / "source-link")
        self.write("nested/dist/.GIT/config", b"nested git marker")
        self.setting("cleanup.exclude", '["src/bin"]')
        row, _, _ = self.cleanup(True)
        reasons = {r["path"]: r["reason"] for r in row["candidates"]}
        self.assertEqual(reasons["bin"], "contains_tracked_files")
        self.assertEqual(reasons["unignored/dist"], "contains_non_ignored_entries")
        self.assertEqual(reasons["obj"], "unsafe_entry_or_dependency_boundary")
        self.assertEqual(reasons["nested/dist"], "unsafe_entry_or_dependency_boundary")
        self.assertEqual(reasons["src/bin"], "excluded")
        self.cleanup()
        self.age()
        self.cleanup()
        for path in ["bin/script.sh", "unignored/dist/file", "obj/source-link", "nested/dist/.GIT/config", "src/bin/Debug/app.dll"]:
            self.assertTrue((self.target / path).exists(), path)

    def test_case_differences_protect_tracked_outputs_and_exclusions(self):
        self.write("build/keep.cs", b"tracked source")
        self.f.git("add", "build/keep.cs", path=self.target)
        # Move just the index entry; APFS still resolves this spelling to build/keep.cs.
        entry=self.f.git("ls-files", "--stage", "build/keep.cs", path=self.target).decode().split()
        self.f.git("update-index", "--force-remove", "build/keep.cs",path=self.target)
        self.f.git("update-index", "--add", "--cacheinfo", entry[0],entry[1],"Build/keep.cs",path=self.target)
        self.write(".gitignore", b"bin/\nobj/\nbuild/\ndist/\nnode_modules/\n")
        self.write("Café/bin/output",b"generated")
        self.setting("cleanup.exclude", '["SRC/BIN","CAFE\\u0301/bin"]')
        self.setting("cleanup.exclude", '["src/obj"]',scope="user")
        row,_,_=self.cleanup(True)
        reasons={c["path"]:c["reason"] for c in row["candidates"]}
        self.assertEqual(reasons["build"],"contains_tracked_files")
        self.assertEqual(reasons["src/bin"],"excluded")
        self.assertEqual(reasons["src/obj"],"excluded")
        import unicodedata
        normalized={unicodedata.normalize("NFC",path).casefold():reason for path,reason in reasons.items()}
        self.assertEqual(normalized["café/bin"],"excluded")
        self.cleanup()
        self.age()
        self.cleanup()
        self.assertTrue((self.target/"build/keep.cs").exists())

    def test_completed_hold_restarts_idle_without_an_intermediate_scan(self):
        self.cleanup()
        self.age()
        self.f.sprig("cleanup","hold",self.target,"--","/bin/echo","read-only application")
        row,_,_=self.cleanup()
        self.assertEqual(row["reason"],"observing")
        self.assertTrue((self.target/"src/bin").exists())

    def test_pin_and_unpin_restart_observation(self):
        self.cleanup()
        self.age()
        self.f.sprig("cleanup", "pin", self.target)
        row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "pinned")
        self.f.sprig("cleanup", "unpin", self.target)
        row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "observing")

    def test_open_file_protects_idle_worktree(self):
        self.cleanup()
        self.age()
        with (self.target / "src/main.cs").open("rb"):
            row, _, _ = self.cleanup()
        self.assertEqual(row["reason"], "worktree_in_use")
        self.assertTrue((self.target / "src/bin").exists())

    def test_hold_preserves_json_stdout_and_reports_command_failure(self):
        report, result = self.f.sprig("cleanup", "hold", self.target, "--", "/bin/echo", "held output")
        self.assertTrue(report["data"]["command_completed"])
        self.assertIn(b"held output", result.stderr)
        self.f.sprig("cleanup", "hold", self.target, "--", "/usr/bin/false", code=5)

    def test_open_file_in_unicode_and_newline_worktree(self):
        self.target=Path(self.temp.name)/"Café\nworktree"
        self.f.sprig("create",self.target,"--from",self.f.repo,"--no-time-machine-exclude")
        self.admin=Path(self.f.git("rev-parse","--absolute-git-dir",path=self.target).decode().rstrip("\n"))
        self.state=self.admin/"sprig-cleanup.json"
        self.write("bin/output",b"generated")
        self.write("source.cs",b"source")
        self.cleanup()
        self.age()
        with (self.target/"source.cs").open("rb"):
            row,_,_=self.cleanup()
        self.assertEqual(row["reason"],"worktree_in_use")
        self.assertTrue((self.target/"bin/output").exists())

    def test_lease_and_git_lock_block_cleanup(self):
        self.cleanup()
        self.age()
        import fcntl
        with (self.admin / "sprig-cleanup-lease").open("rb") as lease:
            fcntl.flock(lease, fcntl.LOCK_SH)
            row, report, _ = self.cleanup()
            self.assertEqual(row["reason"], "leased")
            self.assertEqual(row["stable_seconds"],0)
            preview, _, _=self.cleanup(True)
            self.assertEqual(preview["reason"],"leased")
        lock = self.admin / "index.lock"
        lock.touch()
        try:
            row, report, _ = self.cleanup(code=6)
            self.assertIsNone(row)
        finally:
            lock.unlink()
        self.assertTrue((self.target / "src/bin").exists())

    def test_change_between_observation_and_delete_is_retained(self):
        self.cleanup()
        self.age()
        ready = Path(self.temp.name) / "ready"
        release = Path(self.temp.name) / "release"
        env = dict(self.f.env, SPRIG_TEST_FAULTS="1", SPRIG_TEST_PAUSE="cleanup_before_delete", SPRIG_TEST_READY=str(ready), SPRIG_TEST_RELEASE=str(release))
        process = subprocess.Popen([BINARY, "--json", "cleanup", "run", "--repository", str(self.f.repo)], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 20
        while not ready.exists() and time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.02)
        self.assertTrue(ready.exists())
        self.write("src/bin/Debug/new.dll", b"new build")
        release.touch()
        stdout, stderr = process.communicate(timeout=20)
        self.assertEqual(process.returncode, 6, (stdout, stderr))
        self.assertTrue((self.target / "src/bin/Debug/new.dll").exists())
        self.assertTrue((self.target / "src/obj").exists())

    def test_ignore_rule_changes_before_delete_are_rechecked(self):
        self.setting("cleanup.folders", '["out"]')
        self.write("out/file", b"generated")
        exclude=self.f.repo/".git/info/exclude"
        exclude.write_text("out/\n")
        self.cleanup()
        self.age()
        ready=Path(self.temp.name)/"ignore-ready"
        release=Path(self.temp.name)/"ignore-release"
        env=dict(self.f.env, SPRIG_TEST_FAULTS="1", SPRIG_TEST_PAUSE="cleanup_before_delete", SPRIG_TEST_READY=str(ready), SPRIG_TEST_RELEASE=str(release))
        process=subprocess.Popen([BINARY,"--json","cleanup","run","--repository",str(self.f.repo)],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline=time.monotonic()+20
        while not ready.exists() and time.monotonic()<deadline and process.poll() is None:
            time.sleep(0.02)
        self.assertTrue(ready.exists())
        exclude.write_text("")
        release.touch()
        stdout,stderr=process.communicate(timeout=20)
        self.assertEqual(process.returncode,6,(stdout,stderr))
        self.assertTrue((self.target/"out/file").exists())

    def test_staged_removal_does_not_expose_head_tracked_output(self):
        self.f.write("bin/keep.cs",b"tracked in HEAD")
        self.f.git("add","-f","bin/keep.cs")
        head=self.f.commit()
        self.f.git("reset","--soft",head,path=self.target)
        self.f.git("update-index","--force-remove","bin/keep.cs",path=self.target)
        self.write("bin/keep.cs",b"tracked in HEAD")
        row,_,_=self.cleanup(True)
        self.assertEqual(next(c for c in row["candidates"] if c["path"]=="bin")["reason"],"contains_tracked_files")
        self.cleanup()
        self.age()
        self.cleanup()
        self.assertTrue((self.target/"bin/keep.cs").exists())

    def test_schedule_install_run_and_remove_with_disabled_fixture(self):
        domain = f"gui/{os.getuid()}"
        if subprocess.run(["/bin/launchctl", "print", domain], capture_output=True).returncode:
            self.skipTest("no per-user GUI launchd domain in this test environment")
        if subprocess.run(["/bin/launchctl", "print", domain + "/com.sprig.cleanup"], capture_output=True).returncode == 0:
            self.skipTest("an existing user cleanup schedule must remain untouched")
        self.setting("cleanup.enabled", "false")
        self.f.sprig("cleanup", "schedule", "install", "--repository", self.target)
        try:
            status, _ = self.f.sprig("cleanup", "schedule", "status")
            self.assertTrue(status["data"]["loaded"])
            self.assertTrue(status["data"]["installed"])
            self.assertTrue(status["data"]["binary_exists"])
            log = Path(self.f.env["HOME"]) / "Library/Application Support/sprig/cleanup.log"
            deadline = time.monotonic() + 20
            report=None
            while time.monotonic()<deadline:
                try:
                    report=json.loads(log.read_text())
                    break
                except (FileNotFoundError,json.JSONDecodeError):
                    time.sleep(0.05)
            self.assertIsNotNone(report,"scheduled run did not produce a complete JSON result")
            self.assertTrue(any(w["reason"] == "disabled" for w in report["data"]["worktrees"]))
            self.assertTrue((self.target / "src/bin/Debug/app.dll").exists())
            import plistlib
            plist=Path(status["data"]["path"])
            value=plistlib.loads(plist.read_bytes())
            value["ProgramArguments"][0]=str(Path(self.temp.name)/"missing-sprig")
            plist.write_bytes(plistlib.dumps(value))
            missing,_=self.f.sprig("cleanup","schedule","status")
            self.assertFalse(missing["data"]["binary_exists"])
        finally:
            self.f.sprig("cleanup", "schedule", "remove")
        status, _ = self.f.sprig("cleanup", "schedule", "status")
        self.assertFalse(status["data"]["loaded"])
        self.assertFalse(status["data"]["installed"])

    def test_unmanaged_linked_worktree_never_cleaned(self):
        ordinary = self.f.ordinary("ordinary")
        output = ordinary / "bin/output"
        output.parent.mkdir()
        output.write_bytes(b"must remain")
        self.cleanup()
        self.age()
        _, report, _ = self.cleanup()
        row = next(r for r in report["data"]["worktrees"] if r["worktree"] == str(ordinary.resolve()))
        self.assertEqual(row["reason"], "not_sprig_managed")
        self.assertTrue(output.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
