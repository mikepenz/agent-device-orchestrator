#!/usr/bin/env bash
# Device-less smoke test (no adb needed). Usage: scripts/smoke-linux.sh [path/to/devq]
set -u
command -v pgrep >/dev/null && command -v curl >/dev/null || { echo missing tools; exit 1; }
DEVQ=${1:-$(dirname "$0")/../target/release/devq}
DEVQ=$(cd "$(dirname "$DEVQ")" && pwd)/$(basename "$DEVQ")
export DEVQ_HOME=$(mktemp -d)
PORT=$((20000 + RANDOM % 20000))
fail=0
ok() { if [ "$1" = "$2" ]; then echo "PASS $3"; else echo "FAIL $3 (got '$1', want '$2')"; fail=1; fi; }
printf '[pool]\nstatic = ["rig-a"]\n' > "$DEVQ_HOME/config.toml"
which adb || echo "adb: absent (expected)"
echo "--- devices"; "$DEVQ" devices; ok $? 0 "devices rc"
echo "--- empty homes"; E=$(mktemp -d); pushd "$E" >/dev/null
HOME= DEVQ_HOME= "$DEVQ" devices >/dev/null 2>&1; ok $? 2 "empty HOME+DEVQ_HOME rejected"
[ -z "$(ls -A)" ] && echo "PASS no files in cwd" || { echo "FAIL stray files in cwd"; fail=1; }
popd >/dev/null
echo "--- run"; out=$("$DEVQ" run --agent t --purpose p -- sh -c 'echo $DEVQ_DEVICE; exit 3'); rc=$?
echo "$out"; ok $rc 3 "run exit passthrough"; ok "$out" "rig-a" "DEVQ_DEVICE set"
echo "--- request/release"; out=$("$DEVQ" request --agent t --purpose p); echo "$out"
"$DEVQ" release "${out%% *}"; ok $? 0 "release rc"
echo "--- live"; "$DEVQ" live --port $PORT & L=$!; sleep 1
curl -s -H "Host: 127.0.0.1:$PORT" localhost:$PORT/api/state | head -c 300; echo
ok "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: 127.0.0.1:$PORT" localhost:$PORT/api/state)" 200 "live good host"
ok "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: evil.com:$PORT" localhost:$PORT/api/state)" 403 "live evil host"
kill -INT $L; wait $L; ok $? 0 "live clean exit on SIGINT"
echo "--- orphan"; "$DEVQ" run --agent o --purpose p -- sleep 31 & R=$!
for _ in $(seq 50); do pgrep -f "sleep 31" >/dev/null && break; sleep 0.2; done
pgrep -f "sleep 31" >/dev/null || { echo "FAIL run never became active"; fail=1; }
kill -9 $R; sleep 0.3
"$DEVQ" request --agent w --purpose p --wait 20s; ok $? 0 "waiter granted after orphan"
"$DEVQ" status --json | grep -o '"end_reason":"[a-z_]*"' | sort | uniq -c
ok "$("$DEVQ" status --json | grep -o '"end_reason":"orphaned"' | wc -l | tr -d ' ')" 1 "one orphaned slot"
sleep 0.5
if pgrep -f "sleep 31" >/dev/null; then echo "FAIL sleep child STILL ALIVE"; fail=1; else echo "PASS sleep child reaped"; fi
[ $fail = 0 ] && echo "ALL PASS" || { echo "SOME FAILED"; exit 1; }
