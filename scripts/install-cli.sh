#!/usr/bin/env bash
# Plugin SessionStart hook: make sure `devq` on PATH matches this plugin's version.
# Downloads the release archive for this OS/arch into ~/.devq/bin, verified against
# the release's CHECKSUMS-SHA256.txt. Never fails the session: problems print a hint.
set -u
ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
BIN="$HOME/.devq/bin"
REPO=mikepenz/agent-device-orchestrator

VERSION=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$ROOT/.claude-plugin/plugin.json" | head -1)
[ -n "$VERSION" ] || exit 0

# Make a previously installed copy visible to this session's Bash tool.
[ -n "${CLAUDE_ENV_FILE:-}" ] && echo "export PATH=\"$BIN:\$PATH\"" >> "$CLAUDE_ENV_FILE"

have=$(PATH="$BIN:$PATH" devq --version 2>/dev/null | awk '{print $2}')
[ "$have" = "$VERSION" ] && exit 0

hint() { echo "devq plugin: $1. Install manually: cargo install --git https://github.com/$REPO --tag v$VERSION --locked" >&2; exit 0; }

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)                  target=aarch64-apple-darwin ;;
  Darwin-x86_64)                 target=x86_64-apple-darwin ;;
  Linux-x86_64)                  target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64)     target=aarch64-unknown-linux-gnu ;;
  *) hint "no prebuilt devq for $(uname -s)-$(uname -m)" ;;
esac

name="devq-$VERSION-$target"
url="https://github.com/$REPO/releases/download/v$VERSION"
tmp=$(mktemp -d) || exit 0
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$url/$name.tar.gz" -o "$tmp/$name.tar.gz" \
  && curl -fsSL "$url/CHECKSUMS-SHA256.txt" -o "$tmp/sums" \
  || hint "download of $name failed (is v$VERSION released?)"

want=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f {print $1}' "$tmp/sums")
got=$( (sha256sum "$tmp/$name.tar.gz" 2>/dev/null || shasum -a 256 "$tmp/$name.tar.gz") | awk '{print $1}')
[ -n "$want" ] && [ "$want" = "$got" ] || hint "checksum mismatch for $name"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp" || hint "could not extract $name"
mkdir -p "$BIN" && install -m 755 "$tmp/$name/devq" "$BIN/devq" || hint "could not write $BIN/devq"
echo "devq plugin: installed devq $VERSION to $BIN/devq (add $BIN to your PATH for use outside Claude Code)" >&2
