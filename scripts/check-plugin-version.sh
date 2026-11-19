#!/usr/bin/env bash
# The plugin installs the CLI release with its own version number, so the two must match.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo_v=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
plugin_v=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' .claude-plugin/plugin.json | head -1)
if [ "$cargo_v" != "$plugin_v" ]; then
  echo "::error::Cargo.toml version $cargo_v != .claude-plugin/plugin.json version $plugin_v" >&2
  exit 1
fi
