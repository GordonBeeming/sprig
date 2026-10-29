#!/usr/bin/env bash
set -euo pipefail

: "${HOMEBREW_TAP_DEPLOY_KEY:?missing tap deploy key}"
: "${COMMIT_SIGNING_KEY:?missing signing key}"
: "${RUNNER_TEMP:?missing runner temporary directory}"
: "${VERSION:?missing version}"
formula="${1:?usage: publish-tap.sh FORMULA}"
[[ -f "$formula" ]] || { echo "formula missing" >&2; exit 1; }
key_dir="$(mktemp -d "$RUNNER_TEMP/sprig-tap-keys.XXXXXX")"
tap_dir="$(mktemp -d "$RUNNER_TEMP/sprig-tap.XXXXXX")"
trap 'rm -rf "$key_dir" "$tap_dir"' EXIT
chmod 700 "$key_dir"
printf '%s\n' "$HOMEBREW_TAP_DEPLOY_KEY" > "$key_dir/tap"
printf '%s\n' "$COMMIT_SIGNING_KEY" > "$key_dir/signing"
chmod 600 "$key_dir/tap" "$key_dir/signing"
printf '%s\n' 'github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl' > "$key_dir/known_hosts"
export GIT_TERMINAL_PROMPT=0
export GIT_SSH_COMMAND="ssh -i $key_dir/tap -o BatchMode=yes -o IdentitiesOnly=yes -o UserKnownHostsFile=$key_dir/known_hosts -o StrictHostKeyChecking=yes"
git clone git@github.com:GordonBeeming/homebrew-tap.git "$tap_dir"
if [[ -f "$tap_dir/Formula/sprig.rb" ]]; then
  python3 - "$tap_dir/Formula/sprig.rb" "$VERSION" <<'PY'
import re, sys
from pathlib import Path
match = re.search(r'^  version "([0-9]+\.[0-9]+\.[0-9]+)"$', Path(sys.argv[1]).read_text(), re.M)
if not match:
    raise SystemExit("Cannot establish current tap version; refusing to replace it")
if tuple(map(int, match[1].split('.'))) > tuple(map(int, sys.argv[2].split('.'))):
    raise SystemExit("Refusing to downgrade the Homebrew formula")
PY
fi
if [[ -f "$tap_dir/Formula/sprig.rb" ]] && cmp -s "$formula" "$tap_dir/Formula/sprig.rb"; then
  echo "Tap formula already matches this release."
  exit 0
fi
cp "$formula" "$tap_dir/Formula/sprig.rb"
cd "$tap_dir"
git switch -c "gb/chore/sprig-$VERSION"
git config user.name 'sprig-release[bot]'
git config user.email 'sprig-release[bot]@users.noreply.github.com'
git config gpg.format ssh
git config user.signingkey "$key_dir/signing"
git config commit.gpgsign true
git add Formula/sprig.rb
git commit -S -m "Update sprig to $VERSION"
# A concurrent tap update fails safely; it never rewrites another publisher's work.
git push origin HEAD:main
