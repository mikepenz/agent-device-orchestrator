#!/usr/bin/env bash
# Regenerate art/dashboard-*.png from design/live.html in simulator mode (?mock=1).
# Needs python3 and Chrome (override with CHROME=/path/to/chrome).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
art="$root/art"
chrome=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
[ -x "$chrome" ] || chrome=$(command -v google-chrome chromium chromium-browser 2>/dev/null | head -1)
[ -x "${chrome:-}" ] || { echo "Chrome not found; set CHROME" >&2; exit 1; }

python3 "$root/design/src/build.py"
mkdir -p "$art"
port=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$root/design" >/dev/null 2>&1 &
srv=$!
prof=$(mktemp -d)
trap 'kill $srv 2>/dev/null || true; rm -rf "$prof"' EXIT
sleep 1

shot() { # name width height theme-flags...
  local out="$art/$1.png" w=$2 h=$3; shift 3
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --user-data-dir="$prof" \
    --force-device-scale-factor=2 --window-size="$w,$h" --virtual-time-budget=1500 "$@" \
    --screenshot="$out" "http://127.0.0.1:$port/live.html?mock=1" >/dev/null 2>&1
  [ -s "$out" ] || { echo "FAILED: $out is missing or empty" >&2; exit 1; }
  command -v pngquant >/dev/null && pngquant --force --skip-if-larger --output "$out" -- "$out" || true
  echo "$out $(wc -c <"$out") bytes"
}

# Chrome headless defaults to dark here; set the scheme explicitly (1 = light, 0 = dark).
# 1500 ms of virtual time: the simulator is pre-warmed, longer budgets drain the queue.
shot dashboard-light 1600 1300 --blink-settings=preferredColorScheme=1
shot dashboard-dark 1600 1300 --blink-settings=preferredColorScheme=0
