#!/usr/bin/env python3
"""Render a formula from the checksum of an actual release archive."""
import argparse
import hashlib
from pathlib import Path
import re

parser = argparse.ArgumentParser()
parser.add_argument("version")
parser.add_argument("archive", type=Path)
parser.add_argument("output", type=Path)
args = parser.parse_args()
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
    parser.error("version must be a stable major.minor.patch release")
expected = f"sprig-{args.version}-aarch64-apple-darwin.tar.gz"
if args.archive.name != expected:
    parser.error(f"expected archive named {expected}")
sha = hashlib.sha256(args.archive.read_bytes()).hexdigest()
args.output.write_text(f'''# typed: false
# frozen_string_literal: true

class Sprig < Formula
  desc "Copy-on-write Git workspaces on APFS"
  homepage "https://github.com/GordonBeeming/sprig"
  url "https://github.com/GordonBeeming/sprig/releases/download/v{args.version}/{expected}"
  version "{args.version}"
  sha256 "{sha}"

  depends_on arch: :arm64
  depends_on "git"
  depends_on macos: :tahoe

  def install
    bin.install "tool"
  end

  def caveats
    <<~EOS
      The executable is currently named tool.
      Created and converted workspaces are excluded from Time Machine by default,
      including local uncommitted work. Use --no-time-machine-exclude to opt out.
    EOS
  end

  test do
    assert_match "tool {args.version}", shell_output("#{{bin}}/tool --version")
    assert_match '"schema_version":1', shell_output("#{{bin}}/tool capabilities --json")
  end
end
''')
