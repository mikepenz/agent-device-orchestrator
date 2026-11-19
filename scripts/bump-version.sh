#!/usr/bin/env bash
# Usage: scripts/bump-version.sh <x.y.z>
# Sets the version in Cargo.toml, Cargo.lock and .claude-plugin/plugin.json (they must match;
# CI checks it). Does not commit or tag: review the diff, merge it, then tag vX.Y.Z on main.
set -euo pipefail
cd "$(dirname "$0")/.."
v="${1:-}"
[[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: $0 <x.y.z>" >&2; exit 2; }

# perl: first match only, same on macOS and Linux (BSD sed has no `0,/re/`)
perl -0pi -e 's/^version = ".*"$/version = "'"$v"'"/m' Cargo.toml
perl -0pi -e 's/("version": *")[^"]*"/${1}'"$v"'"/' .claude-plugin/plugin.json
cargo update --offline -p devq >/dev/null 2>&1 || cargo check --quiet # refresh the devq entry in Cargo.lock
bash scripts/check-plugin-version.sh
git diff --stat
echo "Next: commit, open a PR, merge, then: git tag -a v$v -m v$v <merge-sha> && git push origin v$v"
