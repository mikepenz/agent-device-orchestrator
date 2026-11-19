#!/usr/bin/env bash
# Phase 3 acceptance 3.1-3.4 (+ live data) against the real emulator-5554.
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build --release -q || exit 1
DEVQ="$PWD/target/release/devq"
PORT=$(python3 -c "import socket;s=socket.socket();s.bind(('127.0.0.1',0));print(s.getsockname()[1])"); U=http://127.0.0.1:$PORT
FAILS=0
check() { if [ "$2" = 1 ]; then echo "$1 PASS $3"; else echo "$1 FAIL $3"; FAILS=$((FAILS+1)); fi; }
cleanup() { jobs -p | xargs kill 2>/dev/null; wait 2>/dev/null; true; }
trap cleanup EXIT
export DEVQ_HOME=$(mktemp -d)
"$DEVQ" live --port "$PORT" > "$DEVQ_HOME/live.log" 2>&1 &
LIVE=$!
for _ in $(seq 50); do curl -sf "$U/api/state" >/dev/null && break; sleep 0.1; done
kill -0 $LIVE 2>/dev/null || { echo "setup FAIL live server died: $(cat "$DEVQ_HOME/live.log")"; exit 1; }
SERIAL=$("$DEVQ" devices --json | python3 -c "import sys,json;print(json.load(sys.stdin)[0]['serial'])")
[ -n "$SERIAL" ] || { echo "setup FAIL no device"; exit 1; }
# jq-free json helper: py <expr on d>
py() { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

# 3.1
s=$(curl -s "$U/api/state"); s60=$(curl -s "$U/api/state?window=60")
ok=$(echo "$s" | py "int(set(['now','since','devices','queue','history','stats','per_device','per_agent'])<=set(d) and set(['wait_p50_ms','wait_p95_ms','hold_mean_ms','expect_ratio','timeouts','orphaned','abandoned','queued','grants'])<=set(d['stats']) and d['now']-d['since']==3600000)")
a=$(echo "$s" | py "d['since']"); b=$(echo "$s60" | py "d['since']")
bad=$(curl -s "$U/api/state?window=abc" | py "d['now']-d['since']")
h=$(curl -s "$U/api/state"); # history key check happens in the live-data section
check 3.1 $([ "$ok" = 1 ] && [ "$a" != "$b" ] && [ "$bad" = 3600000 ] && echo 1 || echo 0) "keys ok=$ok; since(3600)=$a since(60)=$b; bad window span=${bad}ms"

# 3.1b headers, odd windows, repeated key
ah2=$(curl -s -D- -o/dev/null "$U/api/state" | tr -d '\r')
hok=$(echo "$ah2" | grep -qi '^content-type: application/json' && echo "$ah2" | grep -qi '^cache-control: no-store' && echo 1 || echo 0)
wev=""; wok=1
for w in -5 0 99999999999 "" "60&window=120"; do
  sp=$(curl -s "$U/api/state?window=$w" | py "d['now']-d['since']") || sp=ERR
  case $sp in ''|*[!0-9]*) wok=0;; *) [ "$sp" -ge 1000 ] && [ "$sp" -le 604800000 ] || wok=0;; esac
  wev="$wev [$w]=$sp"
done
kill -0 $LIVE 2>/dev/null || wok=0
check 3.1b $([ "$hok" = 1 ] && [ "$wok" = 1 ] && echo 1 || echo 0) "json+no-store headers=$hok; spans:$wev"

# 3.2
page=$(curl -s "$U/"); ext=$(echo "$page" | grep -cE "https?://")
ct=$(curl -s -D- -o/dev/null "$U/" | tr -d '\r' | grep -i '^content-type'); nf=$(curl -s -o /dev/null -w '%{http_code}' "$U/nope")
check 3.2 $(echo "$page" | grep -q '<title>' && [ "$ext" = 0 ] && echo "$ct" | grep -qi 'text/html; charset=utf-8' && [ "$nf" = 404 ] && echo 1 || echo 0) "title present, external urls=$ext, $ct, /nope=$nf"

# 3.6 PWA routes
pw=1; pev=""
for pair in "manifest.webmanifest:application/manifest+json" "icon.svg:image/svg+xml" "icon-192.png:image/png" "icon-512.png:image/png" "icon-maskable-512.png:image/png" "sw.js:text/javascript"; do
  r=${pair%%:*}; want=${pair#*:}
  hd=$(curl -s -D- -o/dev/null "$U/$r" | tr -d '\r'); code=$(echo "$hd" | head -1 | awk '{print $2}')
  echo "$hd" | grep -qi "^content-type: $want" && [ "$code" = 200 ] && echo "$hd" | grep -qi '^x-content-type-options: nosniff' || pw=0
  pev="$pev $r=$code"
done
curl -sI "$U/sw.js" | grep -qi '^service-worker-allowed: /' || pw=0
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: evil.com:$PORT" "$U/manifest.webmanifest")" = 403 ] || pw=0
curl -s "$U/manifest.webmanifest" | py "int(d['display']=='standalone' and bool(d['name']) and bool(d['start_url']) and {'192x192','512x512'}<={i['sizes'] for i in d['icons']})" | grep -q 1 || pw=0
curl -s "$U/" | grep -q 'rel="manifest"' || pw=0
check 3.6 $pw "pwa routes:$pev"

# 3.5 Host check
sp=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: evil.com:$PORT" "$U/api/state")
sp2=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: evil.com:$PORT" "$U/")
lh=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: localhost:$PORT" "$U/api/state")
check 3.5 $([ "$sp" = 403 ] && [ "$sp2" = 403 ] && [ "$lh" = 200 ] && echo 1 || echo 0) "evil host: api=$sp /=$sp2; localhost host=$lh"

# 3.4
ls=$(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN | awk 'NR>1{print $9}' | sort -u | tr '\n' ' ')
check 3.4 $([ "$ls" = "127.0.0.1:$PORT " ] && echo 1 || echo 0) "listening on: $ls"

# live data
"$DEVQ" run --agent demo --purpose "e2e check" --description "phase 3" -- sh -c 'adb shell getprop ro.product.model; sleep 8' > "$DEVQ_HOME/run.out" 2>&1 &
sleep 1.5
"$DEVQ" request --agent demo2 --purpose wait --wait 20s > "$DEVQ_HOME/req.out" 2>&1 &
busy=0; queued=0; released=0; ev=""
for _ in $(seq 60); do
  s=$(curl -s "$U/api/state")
  r=$(echo "$s" | py "' '.join(map(str,[int(any(x['serial']=='$SERIAL' and x['slot'] and x['slot']['agent']=='demo' and x['slot']['command'] and x['slot']['purpose']=='e2e check' and x['slot']['description']=='phase 3' for x in d['devices'])), int(any(q['agent']=='demo2' for q in d['queue']))]))")
  set -- $r; [ "${1:-0}" = 1 ] && busy=1; [ "${2:-0}" = 1 ] && queued=1
  # after demo is released, demo2 gets it; history must hold both
  hist=$(echo "$s" | py "sorted((h['agent'],h['end_reason'],h['exit_code'],h['command'] is not None,'want_device' in h,'max_secs' in h) for h in d['history'])")
  if echo "$hist" | grep -q "'demo', 'released', 0, True, True, True" && echo "$hist" | grep -q "'demo2'"; then released=1; ev=$hist; break; fi
  sleep 0.5
done
check live.busy $busy "device busy with holder demo, command, purpose, description"
check live.queue $queued "queue entry for demo2 seen"
check live.release $released "history: $ev"
echo "model output: $(head -1 "$DEVQ_HOME/run.out")"

# live process still alive after all requests; clean stop on SIGINT
kill -0 $LIVE 2>/dev/null; alive=$?
kill -INT $LIVE; wait $LIVE 2>/dev/null; rc=$?
check 3.x $([ $alive = 0 ] && [ $rc = 0 ] && echo 1 || echo 0) "server survived requests; SIGINT exit rc=$rc"
[ $FAILS = 0 ] && echo "ALL PASS" || { echo "$FAILS FAILED"; exit 1; }
