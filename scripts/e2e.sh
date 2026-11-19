#!/usr/bin/env bash
# Phase 2 acceptance 2.1-2.9 against the real emulator-5554. Exits non-zero on any FAIL.
set -u
cd "$(dirname "$0")/.." || exit 1
cargo build --release -q || exit 1
DEVQ="$PWD/target/release/devq"
FAILS=0
pass() { echo "$1 PASS $2"; }
fail() { echo "$1 FAIL $2"; FAILS=$((FAILS+1)); }
check() { local n=$1 ok=$2 ev=$3; if [ "$ok" = 1 ]; then pass "$n" "$ev"; else fail "$n" "$ev"; fi; }
fresh() { export DEVQ_HOME=$(mktemp -d); DB="$DEVQ_HOME/devq.db"; }
q() { sqlite3 -cmd ".timeout 5000" "$DB" "$1"; }
ms() { python3 -c 'import time;print(int(time.time()*1000))'; }
cleanup() { jobs -p | xargs kill 2>/dev/null; true; }
trap cleanup EXIT
A="--agent a --purpose p"

# 2.1
fresh
out=$("$DEVQ" devices); echo "$out" | grep -q 'emulator-5554 *android *online'
check 2.1 $((! $?)) "$(echo "$out" | tail -1)"

# 2.2 / 2.3
fresh
a=$("$DEVQ" request $A); rc=$?
t0=$(ms); b=$("$DEVQ" request --agent b --purpose p --wait 2s); rcb=$?; t1=$(ms)
bid=${b#ticket }; dt=$((t1-t0))
ok=0; [ $rc = 0 ] && [ "$a" = "${a%% *} emulator-5554" ] && [ $rcb = 75 ] && [ $dt -ge 2000 ] && [ $dt -le 3000 ] && ok=1
check 2.2 $ok "A='$a' rc=$rc; B rc=$rcb out='$b' in ${dt}ms"
aid=${a%% *}
"$DEVQ" release "$aid"; rr=$?
t0=$(ms); c=$("$DEVQ" request --agent b --purpose p --ticket "$bid"); rcc=$?; dt=$(($(ms)-t0))
st3=$(q "select state from slots where id=$bid")
ok=0; [ $rr = 0 ] && [ $rcc = 0 ] && [ "${c%% *}" = "$bid" ] && [ "$st3" = active ] && [ $dt -le 1000 ] && ok=1
check 2.3 $ok "release rc=$rr; resume out='$c' (ticket $bid) in ${dt}ms"
"$DEVQ" release "$bid"

# 2.4
fresh
CMD="adb shell getprop ro.product.model; sleep 4"
t0=$(ms)
"$DEVQ" run --agent x --purpose t -- sh -c "$CMD" > "$DEVQ_HOME/o1" 2>&1 &
sleep 0.3
"$DEVQ" run --agent y --purpose t -- sh -c "$CMD" > "$DEVQ_HOME/o2" 2>&1 &
wait; dt=$(($(ms)-t0))
iv=$(q "select granted_at,released_at from slots order by granted_at" | tr '\n' ' ')
ov=$(q "select count(*) from slots a join slots b on a.id<b.id and a.granted_at<b.released_at and b.granted_at<a.released_at")
cnt=$(q "select count(*) from slots"); nul=$(q "select count(*) from slots where released_at is null")
ok=0; [ "$cnt" = 2 ] && [ "$nul" = 0 ] && grep -q sdk_gphone64_arm64 "$DEVQ_HOME/o1" && grep -q sdk_gphone64_arm64 "$DEVQ_HOME/o2" && [ "$ov" = 0 ] && [ $dt -ge 8000 ] && ok=1
check 2.4 $ok "model in both outputs; overlaps=$ov; wall=${dt}ms; intervals: $iv"

# 2.5
fresh
out=$("$DEVQ" run $A -- sh -c 'echo $ANDROID_SERIAL; exit 3'); rc=$?
ec=$(q 'select exit_code from slots')
ok=0; [ "$out" = emulator-5554 ] && [ $rc = 3 ] && [ "$ec" = 3 ] && ok=1
check 2.5 $ok "out='$out' exit=$rc exit_code col=$(q 'select exit_code from slots')"

# 2.6
fresh
a=$("$DEVQ" request --agent a --purpose p --expect 1s --max 3s); aid=${a%% *}
b=$("$DEVQ" request --agent b --purpose p)
er=$(q "select end_reason from slots where id=$aid")
dt=$(q "select b.granted_at-a.granted_at from slots a, slots b where a.id=$aid and b.id=${b%% *}")
ok=0; [ "$dt" -ge 2700 ] && [ "$dt" -le 5300 ] && [ "$er" = timeout ] && ok=1
check 2.6 $ok "second granted ('$b') ${dt}ms after A's grant (3000..5000 +-300); first end_reason=$er"
"$DEVQ" release "${b%% *}"

# 2.7
fresh
"$DEVQ" run $A -- sleep 60 & pid=$!
sleep 1.5
"$DEVQ" request --agent w --purpose p --wait 20s > "$DEVQ_HOME/w.out" & wpid=$!
sleep 1
cpid=$(q "select child_pgid from slots where agent='a'")
kill -9 $pid; t0=$(ms)
wait $wpid; rcw=$?; dt=$(($(ms)-t0))
er=$(q "select end_reason from slots where agent='a'")
case "$cpid" in ''|*[!0-9]*) cpid=0;; esac
alive=1; [ "$cpid" -gt 1 ] || alive=2
for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$cpid" 2>/dev/null || { alive=0; break; }; sleep 0.2; done
ok=0; [ $rcw = 0 ] && [ $dt -le 2000 ] && [ "$er" = orphaned ] && [ $alive = 0 ] && ok=1
check 2.7 $ok "waiter rc=$rcw granted ${dt}ms after kill -9; end_reason=$er; child alive=$alive"
"$DEVQ" release "$(cut -d' ' -f1 "$DEVQ_HOME/w.out")" 2>/dev/null

# 2.8
fresh
t0=$(ms); "$DEVQ" run $A --expect 1s --max 3s -- sleep 60; rc=$?; dt=$(($(ms)-t0))
er=$(q "select end_reason from slots"); pg=$(q "select child_pgid from slots")
kill -0 -- "-$pg" 2>/dev/null && dead=0 || dead=1
ok=0; [ $rc != 0 ] && [ $dt -ge 3000 ] && [ $dt -le 9000 ] && [ "$er" = timeout ] && [ $dead = 1 ] && ok=1
check 2.8 $ok "exit=$rc in ${dt}ms; end_reason=$er; child group $pg dead=$dead"

# 2.8b: TERM-ignoring child with a waiter queued: grace (5s) honoured, then SIGKILL
fresh
"$DEVQ" run $A --expect 1s --max 2s -- sh -c "trap '' TERM; sleep 60" & rpid=$!
sleep 1
"$DEVQ" request --agent w --purpose p --wait 30s > "$DEVQ_HOME/w.out" & wpid=$!
wait $rpid; rcr=$?
wait $wpid
pg=$(q "select child_pgid from slots where agent='a'")
kill -0 -- "-$pg" 2>/dev/null && dead=0 || dead=1
hold=$(q "select released_at-granted_at from slots where agent='a'"); er=$(q "select end_reason from slots where agent='a'")
wg=$(q "select granted_at from slots where agent='w'"); ar=$(q "select released_at from slots where agent='a'")
ok=0; [ $hold -ge 6000 ] && [ $hold -le 8500 ] && [ "$er" = timeout ] && [ $dead = 1 ] && [ $rcr != 0 ] && [ "$wg" -ge "$ar" ] && ok=1
check 2.8b $ok "hold=${hold}ms (6000..8500, no kill before deadline+4s); end_reason=$er; child dead=$dead; devq rc=$rcr; waiter granted after end=$([ "$wg" -ge "$ar" ] && echo yes || echo no)"
"$DEVQ" release "$(cut -d' ' -f1 "$DEVQ_HOME/w.out")" 2>/dev/null

# 2.9
fresh
printf '[pool]\nexclude = ["emulator-5554"]\nstatic = ["rig-a"]\n' > "$DEVQ_HOME/config.toml"
out=$("$DEVQ" devices); n=$(echo "$out" | tail -n +2 | wc -l | tr -d ' ')
r=$("$DEVQ" run $A -- sh -c 'echo $ANDROID_SERIAL'); rc=$?
ok=0; [ "$n" = 1 ] && echo "$out" | grep -q rig-a && ! echo "$out" | grep -q emulator && [ $rc = 0 ] && [ "$r" = rig-a ] && ok=1
check 2.9 $ok "devices rows=$n ($(echo "$out" | tail -1 | tr -s ' ')); run -> '$r' rc=$rc"

# 2.10: leader exits, background grandchild must be dead before the next waiter is granted
fresh
"$DEVQ" run $A -- sh -c 'sleep 30 & exit 0' & rpid=$!
sleep 1
"$DEVQ" request --agent w --purpose p --wait 20s > "$DEVQ_HOME/w.out" & wpid=$!
wait $rpid; rcr=$?
pg=$(q "select child_pgid from slots where agent='a'")
kill -0 -- "-$pg" 2>/dev/null && dead=0 || dead=1
wait $wpid; rcw=$?
wg=$(q "select granted_at from slots where agent='w'"); ar=$(q "select released_at from slots where agent='a'")
case "$pg" in ''|*[!0-9]*) pg=0;; esac
ok=0; [ "$pg" -gt 1 ] && [ $rcr = 0 ] && [ $dead = 1 ] && [ $rcw = 0 ] && [ "$wg" -ge "$ar" ] && ok=1
check 2.10 $ok "devq rc=$rcr; bg sleep group $pg dead right after exit=$dead; waiter rc=$rcw granted ${wg} >= released ${ar}"
"$DEVQ" release "$(cut -d' ' -f1 "$DEVQ_HOME/w.out")" 2>/dev/null

# 2.11: SIGINT to a waiting request cancels the slot, exit 130
fresh
a=$("$DEVQ" request $A)
"$DEVQ" request --agent b --purpose p & wp=$!
sleep 1.5
kill -INT $wp; wait $wp; rcw=$?
es=$(q "select end_reason from slots where agent='b'")
ok=0; [ $rcw = 130 ] && [ "$es" = cancelled ] && ok=1
check 2.11 $ok "waiter exit=$rcw; end_reason=$es"
"$DEVQ" release "${a%% *}"

# 2.12 concurrent first start on a fresh DEVQ_HOME: no "database is locked"
nf=$(scripts/stress-open.sh "$DEVQ")
check 2.12 $([ "$nf" = 0 ] && echo 1 || echo 0) "failures=$nf over 20 iterations x 8 concurrent fresh-home starts"

[ $FAILS = 0 ] && echo "ALL PASS" || echo "$FAILS FAILED"
exit $((FAILS>0))
