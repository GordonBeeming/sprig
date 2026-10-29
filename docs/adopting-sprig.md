# Adopt Sprig for existing and future worktrees

Sprig can convert existing worktrees and create new ones with shared APFS storage. It does not replace Git itself: commits, branches, status and removal still use ordinary Git.

## Convert existing worktrees

For one repository, inspect its registered worktrees and preview each linked checkout:

```sh
git -C /absolute/path/to/repository worktree list
sprig convert /absolute/path/to/worktree --dry-run --json
```

Stop editors, agents, builds and other Git operations in the target and its potential donor worktrees. Then apply conversion:

```sh
sprig convert /absolute/path/to/worktree --json
sprig status /absolute/path/to/worktree --json
```

Conversion preserves staged/unstaged changes and unrelated files; modified files are skipped. It does not reset or clean a worktree. Automatic donor discovery stays within the same repository and compatible volume. Unsupported cases remain usable and are reported. A single checkout with no eligible donors cannot gain sharing through conversion.

### Discover worktrees across your computer

Git has no computer-wide repository registry. Sprig includes an optional Python 3.9+ helper that scans selected locations for repositories and asks Git for their registered worktrees. Registered worktrees can be outside those scan roots. Add other repository locations explicitly; unmounted or unreadable locations cannot be covered.

Preview first:

```sh
python3 "$(brew --prefix sprig)/share/sprig/adopt-worktrees.py" \
  --root "$HOME/Developer" > sprig-preview.json
```

Repeat `--root` for other locations. The default scan root is `~/Developer`. The helper prunes Git object directories and common build/dependency caches. It does not follow directory symlinks.

Review the report, stop all participating processes, and explicitly confirm the idle requirement before applying:

```sh
python3 "$(brew --prefix sprig)/share/sprig/adopt-worktrees.py" \
  --root "$HOME/Developer" --apply --idle > sprig-conversion.json
```

The helper skips original checkouts, bare repositories, unavailable paths, mismatched registrations and registered worktree locks. It does not unlock or prune anything. Resolve skipped cases individually after checking why they were protected. It does not search every mounted disk unless you provide those roots, and it cannot prove that external writers are stopped. `--idle` is your confirmation, not process detection; donors must be idle too.

Preview invokes only `sprig convert --dry-run`; it does not change Git state, configuration, files or backup exclusions. Application is sequential and reports each result. A nonzero exit means discovery or conversion needs attention, including partial success. It does not undo earlier successful conversions or remove recovery records after a later failure. Use `sprig doctor` and `sprig recover` on a reported worktree when necessary.

By default, applying conversion adds Time Machine exclusions, including local uncommitted work. Add `--no-time-machine-exclude` to the helper to prevent new exclusions. Existing exclusions remain unchanged. When using a source checkout instead of Homebrew, run `python3 scripts/adopt-worktrees.py` from the Sprig repository; `--sprig /absolute/path/to/sprig` selects a particular executable.

## Use Sprig for new worktrees

The reliable creation path is:

```sh
sprig create /absolute/path/to/new-worktree --from /absolute/path/to/repository \
  --new-branch feature/example
```

Configure the application or agent that creates worktrees to call this command. If it only supports `git worktree add`, it continues to create ordinary Git checkouts. MyTerm and other worktree creators need their own integration; installing Sprig does not change them automatically.

The destination parent must already exist, and a new workspace must be outside every registered checkout and the common Git metadata. If a launcher currently puts worktrees inside the original checkout, choose a separate workspace directory for new Sprig workspaces, for example `~/Developer/worktrees/my-project/`. Existing nested linked worktrees can still be converted when they satisfy the conversion checks.

### An optional Git shortcut

You can add a separate Git command name without replacing any built-in command:

```sh
git config --global alias.swt '!sprig create --from .'
git swt /absolute/path/to/new-worktree --new-branch feature/example
```

Git shell aliases run from the repository's top-level directory, so use absolute destination paths. The arguments are Sprig arguments: `--new-branch`, not Git's `-b`. This shortcut applies when you invoke `git swt`; it does not intercept `git worktree add` in other applications. Remove it with `git config --global --unset alias.swt`. These commands change your own Git configuration only when you run them. [Git alias documentation](https://git-scm.com/docs/git-config#Documentation/git-config.txt-alias).

## Can Git hooks make every future worktree use Sprig?

Git has no supported hook that delegates worktree creation before checkout. Its `post-checkout` hook runs after `git worktree add` has materialised files, and is skipped by `--no-checkout`. A hook could request conversion afterward, but that is post-creation deduplication: the full initial checkout and its space requirement have already happened. It cannot supply Sprig's CoW creation path. [Git hook documentation](https://git-scm.com/docs/githooks#_post_checkout).

A global post-checkout hook also runs on normal checkouts and branch switches. Automatic replacement there would require explicit opt-in, idle detection/coordination, recursion protection, error reporting and preservation of existing hooks. Sprig replays post-checkout during its own creation while holding its operation lock, so an unconditional hook that calls conversion can conflict with that operation. The current release does not install or recommend such a hook.

Setting a global `core.hooksPath` selects a different hooks directory; it can stop repositories' existing hooks from being found. Do not overwrite that configuration or replace existing hook files just to enable Sprig. [Git hooks-path documentation](https://git-scm.com/docs/git-config#Documentation/git-config.txt-corehooksPath).

A Git alias cannot override the built-in `worktree` command. A PATH wrapper could intercept some callers, but applications using an absolute Git path bypass it, and Sprig itself invokes Git. A reliable wrapper needs a real-Git escape path and correct argument translation; a naive wrapper can recurse or change Git semantics. There is no universal automatic takeover in the current release. Use Sprig directly or integrate the specific worktree creator.
