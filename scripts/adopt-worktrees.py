#!/usr/bin/env python3
"""Discover registered worktrees and preview conversion unless explicitly applied."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

PRUNE = {".git", "node_modules", "target", ".build", ".cache", ".Trash"}


def git(path, *args):
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0")
    for key in ("GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    result = subprocess.run(["git", "-C", str(path), *args], env=env, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout.decode("utf-8")


def discover(roots):
    repositories = {}
    errors = []

    def unreadable(error):
        errors.append({"path": error.filename, "message": str(error)})

    for root in roots:
        if not root.is_dir():
            errors.append({"path": str(root), "message": "scan root is not a directory"})
            continue
        for directory, dirs, files in os.walk(root, followlinks=False, onerror=unreadable):
            marker = ".git" in dirs or ".git" in files
            bare_shape = "HEAD" in files and "config" in files and "objects" in dirs
            dirs[:] = [d for d in dirs if d not in PRUNE and not (Path(directory) / d).is_symlink()]
            if not marker and not bare_shape:
                continue
            try:
                common = git(directory, "rev-parse", "--path-format=absolute", "--git-common-dir").removesuffix("\n")
                repositories.setdefault(str(Path(common).resolve()), directory)
                if bare_shape:
                    dirs[:] = []
            except (OSError, RuntimeError, UnicodeError) as error:
                errors.append({"path": directory, "message": str(error)})

    worktrees = {}
    for common, repository in sorted(repositories.items()):
        try:
            listing = git(repository, "worktree", "list", "--porcelain", "-z")
            for record in listing.split("\0\0"):
                fields = record.split("\0")
                name = next((f[9:] for f in fields if f.startswith("worktree ")), None)
                if name is None or "bare" in fields:
                    continue
                path = Path(name)
                reason = None
                if not path.is_dir():
                    reason = "unavailable"
                elif any(f == "locked" or f.startswith("locked ") for f in fields):
                    reason = "registered_worktree_locked"
                else:
                    actual_common = git(path, "rev-parse", "--path-format=absolute", "--git-common-dir").removesuffix("\n")
                    top = git(path, "rev-parse", "--show-toplevel").removesuffix("\n")
                    admin = git(path, "rev-parse", "--absolute-git-dir").removesuffix("\n")
                    if Path(actual_common).resolve() != Path(common) or Path(top).resolve() != path.resolve():
                        reason = "registration_mismatch"
                    elif Path(admin).resolve() == Path(common):
                        reason = "original_checkout"
                worktrees[str(path)] = {"path": str(path), "repository": common, "skip": reason}
        except (OSError, RuntimeError, UnicodeError) as error:
            errors.append({"path": repository, "message": str(error)})
    return list(worktrees.values()), errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", action="append", type=Path, help="repository scan root; repeat for other locations; default ~/Developer")
    parser.add_argument("--apply", action="store_true", help="convert instead of previewing")
    parser.add_argument("--idle", action="store_true", help="confirm participating worktrees, donors, editors, agents and builds are stopped")
    parser.add_argument("--sprig", default="sprig", help="Sprig executable name or absolute path")
    parser.add_argument("--no-time-machine-exclude", action="store_true", help="prevent new exclusions during application")
    args = parser.parse_args()
    if args.apply and not args.idle:
        parser.error("--apply requires --idle; stop editors, agents, builds and Git operations first")
    executable = shutil.which(args.sprig)
    if executable is None:
        parser.error("Sprig executable not found")
    roots = [p.expanduser().resolve() for p in (args.root or [Path.home() / "Developer"])]
    worktrees, errors = discover(roots)
    failures = bool(errors)
    for item in worktrees:
        if item["skip"]:
            continue
        command = [executable, "convert", item["path"], "--json"]
        if not args.apply:
            command.append("--dry-run")
        if args.no_time_machine_exclude:
            command.append("--no-time-machine-exclude")
        try:
            result = subprocess.run(command, capture_output=True, text=True)
            if result.stderr:
                print(result.stderr, file=sys.stderr, end="")
            item["exit_code"] = result.returncode
            item["result"] = json.loads(result.stdout)
            if result.returncode != 0:
                failures = True
        except (OSError, ValueError) as error:
            item["error"] = str(error)
            failures = True
    print(json.dumps({"schema_version": 1, "mode": "apply" if args.apply else "preview", "scan_roots": list(map(str, roots)), "worktrees": worktrees, "discovery_errors": errors}, indent=2))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
