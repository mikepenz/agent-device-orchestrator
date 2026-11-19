#!/usr/bin/env bash
# Dependency minimum-age (cooldown) gate.
#
# CRITICAL SUPPLY-CHAIN RULE: we never adopt a crate version that was published
# less than MIN_AGE_DAYS (default 7) days ago. Most malicious crate releases are
# detected and yanked within days; a cooldown window means we simply never run
# the freshly-poisoned version. cargo-deny / cargo-audit cannot express this, so
# this script queries the crates.io publish date for each version in Cargo.lock.
#
# Usage:
#   scripts/check-dependency-age.sh            # diff mode: only crates added/changed vs HEAD
#   scripts/check-dependency-age.sh --all      # scan every crates.io dep in Cargo.lock
#   MIN_AGE_DAYS=14 scripts/check-dependency-age.sh --all
#
# Exit 0 = all checked versions are old enough. Exit 1 = at least one is too new
# (or a crates.io lookup failed). Diff mode is fast (only new deps); --all is for
# the scheduled CI run and is rate-limited (~1 req/s, polite to crates.io).
set -euo pipefail

MIN_AGE_DAYS="${MIN_AGE_DAYS:-7}"
LOCKFILE="${LOCKFILE:-Cargo.lock}"
UA="devq-dep-age-check (+https://github.com/mikepenz/agent-device-orchestrator)"
MODE="diff"
[ "${1:-}" = "--all" ] && MODE="all"

command -v jq >/dev/null || { echo "ERROR: jq is required"; exit 1; }
command -v curl >/dev/null || { echo "ERROR: curl is required"; exit 1; }
[ -f "$LOCKFILE" ] || { echo "ERROR: $LOCKFILE not found"; exit 1; }

# ── Temporary exceptions ──────────────────────────────────────────────────
# Crate versions explicitly accepted as "too new" until a stated expiry date.
# Format: "name version EXPIRY-YYYY-MM-DD  # rationale". An entry is honored
# ONLY while today < EXPIRY; on or after that date this script FAILS until the
# entry is deleted (see the expiry sweep below). The version it covered is past
# the cooldown by then, so removal is the whole fix. Keep this list short.
TEMP_ALLOW=(
)
TODAY=$(date -u +%Y-%m-%d)
is_temp_allowed() { # args: name version
  local n="$1" v="$2" an av ae
  [ "${#TEMP_ALLOW[@]}" -eq 0 ] && return 1   # empty list (set -u safe on bash 3.2)
  local IFS=$' \t\n'   # ensure word-splitting regardless of caller's IFS
  for entry in "${TEMP_ALLOW[@]}"; do
    read -r an av ae _ <<< "$entry"
    if [ "$an" = "$n" ] && [ "$av" = "$v" ]; then
      [[ "$TODAY" < "$ae" ]] && return 0   # still within exception window
    fi
  done
  return 1
}

# ── An expired exception is a failure, not a no-op ──
# Runs before the "nothing to check" exit: the list rots on its own schedule.
expired=0
if [ "${#TEMP_ALLOW[@]}" -gt 0 ]; then
  for entry in "${TEMP_ALLOW[@]}"; do
    IFS=$' \t\n' read -r en ev ee _ <<< "$entry"
    if [ -z "$ee" ] || ! [[ "$ee" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
      echo "  ✗ TEMP_ALLOW entry '${entry}' has no valid YYYY-MM-DD expiry."
      expired=$((expired+1))
    elif ! [[ "$TODAY" < "$ee" ]]; then
      echo "  ✗ TEMP_ALLOW exception for ${en} ${ev} expired ${ee} — delete its entry in $0."
      expired=$((expired+1))
    fi
  done
fi
if [ "$expired" -gt 0 ]; then
  echo ""
  echo "BLOCKED: ${expired} expired/malformed TEMP_ALLOW entr(ies). Remove them; the"
  echo "versions they covered are past the ${MIN_AGE_DAYS}-day cooldown on their own."
  exit 1
fi

# Compute the cutoff timestamp (now - MIN_AGE_DAYS) as a UTC ISO string.
# ISO-8601 UTC strings are lexicographically ordered, so we compare as strings
# and avoid non-portable epoch math between GNU and BSD `date`.
if date -u -d '1 day ago' >/dev/null 2>&1; then
  CUTOFF=$(date -u -d "${MIN_AGE_DAYS} days ago" +%Y-%m-%dT%H:%M:%S)   # GNU/Linux (CI)
else
  CUTOFF=$(date -u -v-"${MIN_AGE_DAYS}"d +%Y-%m-%dT%H:%M:%S)            # BSD/macOS (local)
fi

# Extract "name version" for crates.io-sourced packages only (skip path/git deps).
extract_crates() {
  awk '
    /^\[\[package\]\]/      { name=""; ver=""; src=""; inpkg=1; next }
    inpkg && /^name = /     { name=$3; gsub(/"/,"",name) }
    inpkg && /^version = /  { ver=$3;  gsub(/"/,"",ver) }
    inpkg && /^source = /   { src=$0 }
    /^$/ { if (inpkg && name && ver && src ~ /crates\.io-index/) print name" "ver; inpkg=0 }
    END  { if (inpkg && name && ver && src ~ /crates\.io-index/) print name" "ver }
  '
}

if [ "$MODE" = "all" ]; then
  TARGETS=$(extract_crates < "$LOCKFILE" | sort -u)
else
  # Diff mode: crates present now but not in the baseline lockfile (added or
  # version-bumped). Baseline = BASE_REF (set to the PR base in CI), else HEAD
  # (local: working-tree Cargo.lock vs the committed one).
  BASE_REF="${BASE_REF:-HEAD}"
  if ! git rev-parse --verify "$BASE_REF" >/dev/null 2>&1; then
    echo "No git ref '$BASE_REF' — falling back to --all scan."; TARGETS=$(extract_crates < "$LOCKFILE" | sort -u)
  else
    CUR=$(extract_crates < "$LOCKFILE" | sort -u)
    OLD=$(git show "$BASE_REF":"$LOCKFILE" 2>/dev/null | extract_crates | sort -u || true)
    TARGETS=$(comm -23 <(printf '%s\n' "$CUR") <(printf '%s\n' "$OLD"))
  fi
fi

if [ -z "${TARGETS//[[:space:]]/}" ]; then
  echo "✓ No new crates.io dependencies to age-check (cutoff: published before ${CUTOFF}Z)."
  exit 0
fi

echo "Checking dependency age (must be published before ${CUTOFF}Z, i.e. ≥ ${MIN_AGE_DAYS} days old):"
violations=0
errors=0
while read -r name ver; do
  [ -z "$name" ] && continue
  created=$(curl -fsSL -A "$UA" "https://crates.io/api/v1/crates/${name}/${ver}" 2>/dev/null \
            | jq -r '.version.created_at // empty' 2>/dev/null || true)
  if [ -z "$created" ]; then
    echo "  ⚠ ${name} ${ver}: could not fetch publish date from crates.io"
    errors=$((errors+1))
    sleep 1; continue
  fi
  # Compare first 19 chars (YYYY-MM-DDTHH:MM:SS) — both UTC, lexically ordered.
  if [[ "${created:0:19}" > "$CUTOFF" ]]; then
    if is_temp_allowed "$name" "$ver"; then
      echo "  ⏳ ${name} ${ver}: published ${created} — too new, but TEMP-ALLOWED (see TEMP_ALLOW)."
    else
      echo "  ✗ ${name} ${ver}: published ${created} — TOO NEW (< ${MIN_AGE_DAYS} days). Wait before adopting."
      violations=$((violations+1))
    fi
  else
    echo "  ✓ ${name} ${ver}: published ${created}"
  fi
  sleep 1   # be polite to crates.io
done <<< "$TARGETS"

if [ "$violations" -gt 0 ]; then
  echo ""
  echo "BLOCKED: ${violations} dependency version(s) younger than ${MIN_AGE_DAYS} days."
  echo "Pin to an older published version or wait out the cooldown window."
  exit 1
fi
if [ "$errors" -gt 0 ]; then
  echo ""
  echo "FAILED: ${errors} crates.io lookup(s) failed — cannot prove age. Treat as blocking."
  exit 1
fi
echo "✓ All checked dependencies satisfy the ${MIN_AGE_DAYS}-day minimum-age rule."
