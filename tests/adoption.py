"""Exercise the documented bulk helper against disposable real Git worktrees."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from acceptance import BINARY, Fixture

SCRIPT = Path("scripts/adopt-worktrees.py").resolve()


@unittest.skipUnless(os.uname().sysname == "Darwin", "native conversion requires macOS/APFS")
class Adoption(unittest.TestCase):
    def test_preview_apply_and_skip_policy(self):
        with tempfile.TemporaryDirectory(prefix="sprig-adoption-") as directory:
            root = Path(directory); project = root / "project with spaces"; project.mkdir()
            fixture = Fixture(project); target = fixture.ordinary("target with spaces")
            locked = fixture.ordinary("locked")
            fixture.git("worktree", "lock", "--reason", "protected fixture", str(locked))
            before = fixture.state(target); original = fixture.state(fixture.repo)
            command = ["python3", str(SCRIPT), "--root", str(root), "--sprig", BINARY, "--no-time-machine-exclude"]
            result = subprocess.run(command, env=fixture.env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(report["mode"], "preview")
            rows = {Path(row["path"]).name: row for row in report["worktrees"]}
            self.assertEqual(rows["source"]["skip"], "original_checkout")
            self.assertEqual(rows["locked"]["skip"], "registered_worktree_locked")
            self.assertTrue(rows["target with spaces"]["result"]["data"]["dry_run"])
            self.assertEqual(before, fixture.state(target))
            self.assertFalse((fixture.repo / ".git/tool").exists())
            denied = subprocess.run(command + ["--apply"], env=fixture.env, capture_output=True, text=True)
            self.assertEqual(denied.returncode, 2)
            result = subprocess.run(command + ["--apply", "--idle"], env=fixture.env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            applied = next(r for r in report["worktrees"] if Path(r["path"]) == target.resolve())
            self.assertGreater(applied["result"]["data"]["replaced_files"], 0)
            self.assertEqual(before, fixture.state(target))
            self.assertEqual(original, fixture.state(fixture.repo))

    def test_missing_scan_root_is_reported(self):
        with tempfile.TemporaryDirectory(prefix="sprig-adoption-") as directory:
            result = subprocess.run(["python3", str(SCRIPT), "--root", str(Path(directory) / "missing"), "--sprig", BINARY], capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertTrue(json.loads(result.stdout)["discovery_errors"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
