"""Opt-in headless disk-image tests. The image is bounded; live volumes are never filled."""
import errno
import json
import os
from pathlib import Path
import plistlib
import tempfile
from acceptance import Fixture, run

results = []
with tempfile.TemporaryDirectory(prefix="tool-volume-acceptance-") as base_name:
    base = Path(base_name)
    for filesystem, size in [("APFS", "192m"), ("HFS+", "64m")]:
        image = base / (filesystem.replace("+", "plus") + ".sparseimage")
        mount = base / ("mount-" + filesystem.replace("+", "plus")); mount.mkdir()
        run("hdiutil", "create", "-size", size, "-fs", filesystem, "-type", "SPARSE", "-volname", "ToolDisposable", str(image))
        attached = run("hdiutil", "attach", "-nobrowse", "-noautoopen", "-mountpoint", str(mount), "-plist", str(image))
        entities = plistlib.loads(attached.stdout)["system-entities"]
        device = entities[0]["dev-entry"]
        try:
            f = Fixture(mount)
            target = f.ordinary()
            before = f.state(target)
            if filesystem == "HFS+":
                out, _ = f.tool("convert", target, "--no-time-machine-exclude", code=3)
                assert out["code"] == "E_UNSUPPORTED"
                f.tool("convert", target, "--dry-run", code=3)
                assert before == f.state(target)
                results.append({"filesystem":filesystem,"unsupported_no_mutation":True})
                continue
            destination = base / "wrong-volume"
            source_before = f.state(f.repo)
            out, _ = f.tool("create", destination, "--from", f.repo, "--no-time-machine-exclude", code=3)
            assert out["code"] == "E_CLONE" and not destination.exists()
            assert source_before == f.state(f.repo)
            filler = mount / "bounded-fill"
            exhausted = False
            with filler.open("wb", buffering=0) as file:
                for chunk in [1024*1024, 65536, 4096, 512, 1]:
                    block = os.urandom(chunk)
                    try:
                        while True:
                            file.write(block); os.fsync(file.fileno())
                    except OSError as error:
                        if error.errno != errno.ENOSPC:
                            raise
                        exhausted = True
            assert exhausted
            try:
                out, process = f.tool("convert", target, "--no-time-machine-exclude", code=None)
                assert process.returncode in [0, 5]
                if process.returncode == 5:
                    assert out["code"] == "E_IO"
                assert before == f.state(target)
            finally:
                filler.unlink()
            results.append({"filesystem":filesystem,"wrong_volume_no_mutation":True,"native_enospc_observed":True,"low_space_convert_exit":process.returncode,"target_preserved":True})
        finally:
            run("hdiutil", "detach", device)
print(json.dumps(results, indent=2))
