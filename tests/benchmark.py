"""Disposable APFS measurements; volume allocation is noisy and is not a reclaimability promise."""
import argparse
import hashlib
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import statistics
import tempfile
import time
from acceptance import BINARY, Fixture, run

parser = argparse.ArgumentParser()
parser.add_argument("--output", required=True)
parser.add_argument("--repeats", type=int, default=3)
parser.add_argument("--files", type=int, default=16)
parser.add_argument("--mib", type=int, default=8)
parser.add_argument("--root", default=None)
args = parser.parse_args()


def free(path):
    stat = os.statvfs(path)
    return stat.f_bfree * stat.f_frsize


def measure(path, action):
    # The global sync is outside elapsed time and affects only accounting observations.
    run("/bin/sync")
    before = free(path)
    started = time.perf_counter()
    result = action()
    elapsed = time.perf_counter() - started
    run("/bin/sync")
    return {"elapsed_seconds": elapsed, "volume_used_delta_bytes": before - free(path)}, result


rows = []
with tempfile.TemporaryDirectory(prefix="tool-benchmark-", dir=args.root) as base:
    f = Fixture(base)
    for i in range(args.files):
        f.write(f"payload/{i:04}.bin", os.urandom(args.mib * 1024 * 1024))
    f.head = f.commit()
    logical = sum(p.stat().st_size for p in f.repo.rglob("*") if p.is_file() and not p.is_symlink() and ".git" not in p.parts)
    for i in range(args.repeats):
        noise, _ = measure(base, lambda: time.sleep(0.25))
        ordinary, target = measure(base, lambda: f.ordinary(f"ordinary-{i}"))
        cow, (cow_result, _) = measure(base, lambda: f.tool("create", f.base / f"cow-{i}", "--from", f.repo, "--no-time-machine-exclude"))
        conversion, (conversion_result, _) = measure(base, lambda: f.tool("convert", target, "--source", f.repo, "--no-time-machine-exclude"))
        allocated = sum(p.stat().st_blocks * 512 for p in target.rglob("*") if p.is_file())
        def private_write():
            path = f.base / f"cow-{i}/payload/0000.bin"
            with path.open("r+b") as file:
                file.write(os.urandom(args.mib * 1024 * 1024)); file.flush(); os.fsync(file.fileno())
        writes, _ = measure(base, private_write)
        rows.append({"trial": i+1,"background_control":noise,"ordinary_create":ordinary,"cow_create":cow,"convert":conversion,"private_write":writes,"cloned_files":cow_result["data"]["cloned_files"],"converted_files":conversion_result["data"]["replaced_files"],"target_st_blocks_bytes_including_shared":allocated})
    result = {"recorded_at_utc":datetime.now(timezone.utc).isoformat(),"git":f.git("--version").decode().strip(),"platform":run("sw_vers").stdout.decode(),"filesystem":"APFS (native tests and clonefile success)","binary":BINARY,"binary_sha256":hashlib.sha256(Path(BINARY).read_bytes()).hexdigest(),"payload_files":args.files,"payload_mib_per_file":args.mib,"logical_worktree_bytes":logical,"trials":rows,"median_seconds":{key:statistics.median(row[key]["elapsed_seconds"] for row in rows) for key in ["ordinary_create","cow_create","convert"]},"limitations":["volume-wide accounting includes concurrent applications and APFS metadata","snapshots can retain old allocation after conversion","clone group IDs and clonefile prove native sharing semantics, not exact per-directory exclusive extent ownership","st_blocks includes shared blocks and must not be summed into claimed physical savings","global sync is outside measured command duration","fresh random payload; warm object cache; no claim about cold or small-file workloads"]}
Path(args.output).write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result, indent=2))
