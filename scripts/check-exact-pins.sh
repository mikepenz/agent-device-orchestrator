#!/usr/bin/env bash
# Fail if Cargo.toml declares a dependency with a version *range*.
#
# Cargo.lock pins the whole graph, but a range in Cargo.toml (`foo = "0.4"` ==
# `^0.4`) lets a lockfile refresh pull a newer in-range release than the one
# reviewed, which also slips past the 7-day cooldown gate. Every direct
# dependency is therefore an exact `=x.y.z`; transitive crates are pinned by
# Cargo.lock + `--locked`.
set -euo pipefail
cd "$(dirname "$0")/.."

violations=0
while IFS=$'\t' read -r dep kind req; do
  [ -z "${dep:-}" ] && continue
  if [[ "$req" != "="* ]]; then
    printf '  %s (%s) = "%s": not an exact pin\n' "$dep" "$kind" "$req"
    violations=$((violations + 1))
  fi
done < <(
  cargo metadata --no-deps --format-version 1 \
    | jq -r '.packages[].dependencies[] | select(.path == null) | [.name, (.kind // "normal"), .req] | @tsv'
)

if [ "$violations" -gt 0 ]; then
  echo "✗ $violations dependency declaration(s) use a range; pin each to the version in Cargo.lock (=x.y.z)." >&2
  exit 1
fi
echo "✓ every declared dependency is an exact '=' pin"
