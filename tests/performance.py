"""Time real content in disposable repositories without changing the source checkout.

Example: python3 tests/performance.py --source /path/to/SSW.Rules.Content \
    --output /tmp/sprig-performance.json --timeout 30
Exit 1 means a Sprig command failed, timed out, or exceeded the time budget.
Volume-wide allocation observations are diagnostic, not per-worktree savings.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import shutil
import statistics
import subprocess
import tempfile
import time


def positive(value):
    number = float(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=Path("target/release/sprig"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--budget", type=positive, default=5.0)
    parser.add_argument("--timeout", type=positive, default=1200.0)
    parser.add_argument("--trace", action="store_true", help="Count Git processes; timings include tracing overhead")
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    binary = args.binary.resolve(strict=True)
    source = args.source.resolve(strict=True)
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_OPTIONAL_LOCKS="0")

    def git(*arguments):
        return subprocess.check_output(
            ["git", "-c", "core.hooksPath=/dev/null", *map(str, arguments)],
            env=env, stderr=subprocess.PIPE).decode().strip()

    commit = git("-C", source, "rev-parse", "HEAD")
    report = {
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "source_commit": commit,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "platform": platform.platform(), "git": git("--version"),
        "budget_seconds": args.budget, "timeout_seconds": args.timeout,
        "traced": args.trace,
        "limitations": [
            "Warm local object cache; includes real committed content only.",
            "Independent repository metadata with local shared object storage.",
            "Volume-wide free-space changes include other applications and snapshots; not exclusive savings.",
            "Time Machine exclusion is disabled; its status query remains part of command time.",
            "Timing includes Git Trace2 overhead only when --trace is set.",
        ], "trials": [],
    }
    with tempfile.TemporaryDirectory(prefix="sprig-performance-", dir=args.root) as base:
        base = Path(base)
        def measure(name, command, trial):
            trace = base / f"{trial}-{name}.trace.json"
            before = os.statvfs(base)
            load_before = os.getloadavg()
            started = time.perf_counter()
            process = subprocess.Popen(list(map(str, command)), stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True,
                                       env=dict(env, GIT_TRACE2_EVENT=str(trace)) if args.trace else env)
            try:
                stdout, stderr = process.communicate(timeout=args.timeout)
                timed_out = False
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
                timed_out = True
            elapsed = time.perf_counter() - started
            after = os.statvfs(base)
            row = {"seconds": elapsed, "exit_code": process.returncode, "timed_out": timed_out,
                   "volume_used_delta_bytes": (before.f_bfree - after.f_bfree) * after.f_frsize,
                   "load_average_before": load_before, "load_average_after": os.getloadavg(),
                   "volume_available_bytes_before": before.f_bavail * before.f_frsize}
            if name == "convert":
                row["sharing_attempts"] = sum(line.startswith(b"sharing ") for line in stderr.splitlines())
                row["metadata_skip_messages"] = sum(line.startswith(b"skipped ") for line in stderr.splitlines())
            if trace.exists():
                events = [json.loads(line) for line in trace.read_text().splitlines()]
                starts = {event["sid"]: event["argv"] for event in events if event.get("event") == "start"}
                row["git_processes"] = len(starts)
                row["git_commands"] = [
                    {"argv": starts[event["sid"]], "seconds": event["t_abs"]}
                    for event in events if event.get("event") == "exit" and event["sid"] in starts]
            if process.returncode:
                row["stderr_tail"] = stderr.decode(errors="replace")[-2000:]
            elif name != "git_create":
                data = json.loads(stdout)["data"]
                row["result"] = {key: data[key] for key in ["cloned_files", "replaced_files",
                                 "candidate_files", "metadata_skipped_files"] if key in data}
            print(f"trial {trial} {name}: {elapsed:.3f}s (exit {process.returncode})", flush=True)
            return row

        for trial in range(1, args.repeats + 1):
            repo = base / f"source-{trial}"
            git("clone", "--shared", "--no-checkout", source, repo)
            git("-C", repo, "checkout", "--detach", commit)
            report["tracked_files"] = len(git("-C", repo, "ls-files", "-z").split("\0")) - 1
            entries = [record.split("\t", 1) for record in git("-C", repo, "ls-tree", "-r", "-z", commit).split("\0") if record]
            regular = [path for header, path in entries if header.split()[0] in ["100644", "100755"]]
            report["expected_cloned_files"] = sum(Path(path).name != ".gitattributes" for path in regular)
            report["expected_converted_files"] = len(regular)
            cow, ordinary = base / f"cow-{trial}", base / f"ordinary-{trial}"
            rows = {"trial": trial}
            rows["create"] = measure("create", [binary, "--json", "create", cow, "--from", repo,
                                               "--no-time-machine-exclude"], trial)
            rows["status"] = measure("status", [binary, "--json", "status", cow], trial)
            payload = next((path for path in sorted(cow.rglob("*")) if path.is_file()
                            and not path.is_symlink() and path.stat().st_size >= 8 * 1024 * 1024), None)
            rows["private_write"] = None
            if payload is not None:
                donor = repo / payload.relative_to(cow)
                donor_before = hashlib.sha256(donor.read_bytes()).hexdigest()
                data = os.urandom(8 * 1024 * 1024)
                before = os.statvfs(base)
                started = time.perf_counter()
                with payload.open("r+b") as file:
                    file.write(data)
                    file.flush()
                    os.fsync(file.fileno())
                elapsed = time.perf_counter() - started
                after = os.statvfs(base)
                rows["private_write"] = {
                    "seconds": elapsed, "written_bytes": len(data),
                    "volume_used_delta_bytes": (before.f_bfree - after.f_bfree) * after.f_frsize,
                    "donor_unchanged": hashlib.sha256(donor.read_bytes()).hexdigest() == donor_before,
                }
            rows["git_create"] = measure("git_create", ["git", "-C", repo, "worktree", "add",
                                                       "--detach", ordinary, commit], trial)
            rows["convert_plan"] = measure("convert_plan", [binary, "--json", "convert", ordinary,
                        "--source", repo, "--dry-run", "--no-time-machine-exclude"], trial)
            rows["convert"] = measure("convert", [binary, "--json", "convert", ordinary,
                        "--source", repo, "--no-time-machine-exclude"], trial)
            report["trials"].append(rows)
            # A timed-out conversion can retain a recovery journal. Each trial
            # owns its Git metadata; removing it cannot affect the source repo.
            for path in [cow, ordinary, repo]:
                if path.exists():
                    shutil.rmtree(path)

    names = ["create", "status", "git_create", "convert_plan", "convert"]
    report["median_seconds"] = {name: (None if any(row[name]["timed_out"] for row in report["trials"])
                                      else statistics.median(row[name]["seconds"] for row in report["trials"]))
                                for name in names}
    report["passed"] = len(report["trials"]) == args.repeats and all(
        row[name]["exit_code"] == 0 and row[name]["seconds"] <= args.budget
        for row in report["trials"] for name in names if name != "git_create")
    report["passed"] = report["passed"] and all(
        row["create"].get("result", {}).get("cloned_files") == report["expected_cloned_files"]
        and row["convert"].get("result", {}).get("replaced_files") == report["expected_converted_files"]
        and row["convert"].get("result", {}).get("metadata_skipped_files") == 0
        and row["private_write"] is not None and row["private_write"]["donor_unchanged"]
        for row in report["trials"])
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
