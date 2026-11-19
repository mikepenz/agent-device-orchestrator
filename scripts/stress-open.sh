#!/usr/bin/env bash
# usage: stress-open.sh DEVQ  -> prints failure count over 20 iterations of 8 concurrent fresh-home starts
DEVQ=$1; F=0
for i in $(seq 20); do
  H=$(mktemp -d); pids=()
  for j in 1 2 3 4; do
    DEVQ_HOME=$H "$DEVQ" devices >/dev/null 2>"$H/e$j" & pids+=($!)
    DEVQ_HOME=$H "$DEVQ" status --json >/dev/null 2>"$H/f$j" & pids+=($!)
  done
  for p in "${pids[@]}"; do wait $p || F=$((F+1)); done
  F=$((F+$(cat "$H"/e* "$H"/f* 2>/dev/null | grep -ciE 'locked|busy')))
  rm -rf "$H"
done
echo $F
