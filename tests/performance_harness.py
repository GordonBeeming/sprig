"""Exercise benchmark isolation and validation with real disposable Git/APFS fixtures."""
import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest

from acceptance import BINARY, Fixture
from performance import run_bounded


class PerformanceHarness(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sprig-harness-test-")
        self.addCleanup(self.temp.cleanup)
        self.f = Fixture(self.temp.name)
        self.f.write("payload.bin", os.urandom(8 * 1024 * 1024))

    def benchmark(self, env, binary=BINARY):
        output = self.f.base / "metrics.json"
        result = subprocess.run([sys.executable, "tests/performance.py", "--source", str(self.f.repo),
                                 "--binary", binary, "--output", str(output), "--repeats", "1",
                                 "--budget", "300"], env=env, capture_output=True)
        self.assertTrue(output.exists(), (result.stdout, result.stderr))
        return result, json.loads(output.read_text())

    def test_location_environment_cannot_redirect_fixture_writes(self):
        f = self.f
        f.head = f.commit()
        before, trees = f.state(f.repo), f.git("worktree", "list", "--porcelain")
        result, data = self.benchmark(dict(f.env, GIT_DIR=str(f.repo / ".git"),
                                           GIT_COMMON_DIR=str(f.repo / ".git"), GIT_WORK_TREE=str(f.repo),
                                           GIT_INDEX_FILE=str(f.repo / ".git/index"), GIT_NAMESPACE="unrelated",
                                           GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="core.worktree",
                                           GIT_CONFIG_VALUE_0=str(f.repo)))
        self.assertEqual(result.returncode, 0, (result.stdout, result.stderr))
        self.assertTrue(data["passed"])
        self.assertEqual(before, f.state(f.repo))
        self.assertEqual(trees, f.git("worktree", "list", "--porcelain"))
        self.assertEqual(f.git("symbolic-ref", "HEAD").strip(), b"refs/heads/main")

    def test_private_write_uses_tracked_shared_payload(self):
        f = self.f
        f.write(".gitattributes", b"# comment\n" * (8 * 1024 * 1024 // 10 + 1))
        f.write(".gitignore", b"cache/\n.env.local\n000-hook-file\n")
        f.head = f.commit()
        hooks = f.base / "hooks"
        hooks.mkdir()
        hook = hooks / "post-checkout"
        hook.write_text("#!/bin/sh\ndd if=/dev/zero of=000-hook-file bs=1048576 count=8 2>/dev/null\n")
        hook.chmod(0o755)
        config = f.base / "global.gitconfig"
        config.write_text(f"[core]\n\thooksPath = {hooks}\n")
        result, data = self.benchmark(dict(f.env, GIT_CONFIG_GLOBAL=str(config)))
        self.assertEqual(result.returncode, 0, (result.stdout, result.stderr))
        write = data["trials"][0]["private_write"]
        self.assertEqual(write["tracked_path"], "payload.bin")
        self.assertEqual(write["sharing_verified_by"], "native convert plan: same_clone_group")
        self.assertTrue(write["donor_unchanged"])

    def test_failed_git_comparator_cannot_pass(self):
        f = self.f
        f.head = f.commit()
        hooks = f.base / "hooks"
        hooks.mkdir()
        hook = hooks / "post-checkout"
        hook.write_text("#!/bin/sh\ncase \"$PWD\" in */ordinary-*) exit 1;; esac\nexit 0\n")
        hook.chmod(0o755)
        config = f.base / "global.gitconfig"
        config.write_text(f"[core]\n\thooksPath = {hooks}\n")
        result, data = self.benchmark(dict(f.env, GIT_CONFIG_GLOBAL=str(config)))
        self.assertEqual(result.returncode, 1, (result.stdout, result.stderr))
        self.assertFalse(data["passed"])
        trial = data["trials"][0]
        self.assertNotEqual(trial["git_create"]["exit_code"], 0)
        for name in ["create", "status", "convert_plan", "convert"]:
            self.assertEqual(trial[name]["exit_code"], 0, trial[name])

    def test_sharing_probe_failure_still_writes_a_failed_report(self):
        f = self.f
        f.head = f.commit()
        wrapper = f.base / "sprig-fault"
        wrapper.write_text("#!/bin/sh\ncase \"$*\" in *'--dry-run'*'/cow-'*|*'/cow-'*'--dry-run'*) "
                           "echo 'injected sharing probe failure' >&2; exit 5;; esac\n"
                           f"exec {shlex.quote(BINARY)} \"$@\"\n")
        wrapper.chmod(0o755)
        result, data = self.benchmark(f.env, str(wrapper))
        self.assertEqual(result.returncode, 1, (result.stdout, result.stderr))
        self.assertFalse(data["passed"])
        trial = data["trials"][0]
        self.assertIsNone(trial["private_write"])
        self.assertEqual(trial["sharing_verification"]["exit_code"], 5)
        self.assertIn("injected sharing probe failure", trial["sharing_verification"]["stderr_tail"])

    def test_timeout_stops_the_whole_subprocess_group(self):
        pid_file = self.f.base / "child-pid"
        script = ("import subprocess,time; from pathlib import Path; "
                  "child=subprocess.Popen(['sleep','60']); "
                  f"Path({str(pid_file)!r}).write_text(str(child.pid)); time.sleep(60)")
        process, _, _, timed_out = run_bounded([sys.executable, "-c", script], self.f.env, 1)
        self.assertTrue(timed_out)
        self.assertNotEqual(process.returncode, 0)
        self.assertTrue(pid_file.exists())
        child = subprocess.run(["ps", "-p", pid_file.read_text(), "-o", "stat="], capture_output=True)
        self.assertTrue(not child.stdout.strip() or child.stdout.strip().startswith(b"Z"), child.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
