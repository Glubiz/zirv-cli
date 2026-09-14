#!/usr/bin/env bash
# Issue #615 (N23): measures clean debug build time, clean release build
# time, and stripped release binary size for two git refs, and fails when
# either regresses past the documented threshold. Windows counterpart:
# bench-build-cost.ps1 (same contract, same output shape). Thresholds and
# the actual measurements recorded from this machine live in
# docs/benchmarks/build-cost.md -- read that first if a number here looks
# surprising.
#
# Usage:
#   scripts/bench-build-cost.sh [base-ref] [head-ref]
# Defaults: base-ref=main, head-ref=HEAD. Both are resolved with
# `git worktree add --detach`, so neither has to be checked out already and
# the caller's own working tree is never touched.
#
# Env overrides:
#   ZIRV_BENCH_BUILD_THRESHOLD_PCT   max allowed clean-build regression, %  (default 25)
#   ZIRV_BENCH_SIZE_THRESHOLD_PCT    max allowed release-size regression, % (default 15)
#   ZIRV_BENCH_SKIP_RELEASE=1        skip the (slow, LTO) release build/size measurement
set -euo pipefail

BASE_REF="${1:-main}"
HEAD_REF="${2:-HEAD}"
BUILD_THRESHOLD_PCT="${ZIRV_BENCH_BUILD_THRESHOLD_PCT:-25}"
SIZE_THRESHOLD_PCT="${ZIRV_BENCH_SIZE_THRESHOLD_PCT:-15}"
SKIP_RELEASE="${ZIRV_BENCH_SKIP_RELEASE:-0}"

repo_root="$(git rev-parse --show-toplevel)"
work="$(mktemp -d)"
cleanup() {
  for label in base head; do
    git -C "$repo_root" worktree remove --force "$work/$label" >/dev/null 2>&1 || true
  done
  rm -rf "$work"
}
trap cleanup EXIT

# Whole seconds only -- `date +%N` is a GNU extension BSD/macOS `date`
# lacks, and sub-second precision buys nothing for a build measured in
# minutes.
now_secs() { date +%s; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", b-a}'; }

# Measures one ref. Prints one line: "<label> ref=<ref> debug_secs=<n>
# release_secs=<n|skipped> stripped_bytes=<n|skipped>" to stdout; all
# `cargo`/`git` noise goes to stderr so the caller can parse stdout safely.
measure() {
  local ref="$1" label="$2"
  local wt="$work/$label"
  git -C "$repo_root" worktree add --quiet --detach "$wt" "$ref" >&2
  local target="$work/target-$label"

  local d0 d1
  d0=$(now_secs)
  ( cd "$wt" && CARGO_TARGET_DIR="$target" cargo build --bin zirv --quiet ) >&2
  d1=$(now_secs)
  local debug_secs
  debug_secs=$(elapsed "$d0" "$d1")

  local release_secs="skipped" stripped_bytes="skipped"
  if [ "$SKIP_RELEASE" != "1" ]; then
    local r0 r1
    r0=$(now_secs)
    ( cd "$wt" && CARGO_TARGET_DIR="$target" cargo build --release --bin zirv --quiet ) >&2
    r1=$(now_secs)
    release_secs=$(elapsed "$r0" "$r1")

    local bin="$target/release/zirv"
    [ -x "$bin" ] || bin="$target/release/zirv.exe"
    local stripped="$work/$label-stripped"
    cp "$bin" "$stripped"
    if command -v strip >/dev/null 2>&1; then
      strip "$stripped" 2>/dev/null || true
    fi
    stripped_bytes=$(wc -c < "$stripped" | tr -d ' ')
  fi

  local resolved
  resolved=$(git -C "$repo_root" rev-parse --short "$ref")
  echo "$label ref=$ref sha=$resolved debug_secs=$debug_secs release_secs=$release_secs stripped_bytes=$stripped_bytes"
}

echo "Measuring base ($BASE_REF)..." >&2
base_line=$(measure "$BASE_REF" base)
echo "Measuring head ($HEAD_REF)..." >&2
head_line=$(measure "$HEAD_REF" head)

echo "$base_line"
echo "$head_line"

field() { echo "$1" | grep -o "$2=[^ ]*" | cut -d= -f2; }

base_debug=$(field "$base_line" debug_secs)
head_debug=$(field "$head_line" debug_secs)
debug_pct=$(awk -v b="$base_debug" -v h="$head_debug" 'BEGIN{ if (b==0) {print 0} else {printf "%.1f", (h-b)/b*100} }')
echo "clean debug build: ${base_debug}s -> ${head_debug}s (${debug_pct}%)"

status=0
if awk -v p="$debug_pct" -v t="$BUILD_THRESHOLD_PCT" 'BEGIN{exit !(p>t)}'; then
  echo "FAIL: clean debug build time regressed ${debug_pct}% (threshold ${BUILD_THRESHOLD_PCT}%)" >&2
  status=1
fi

base_bytes=$(field "$base_line" stripped_bytes)
head_bytes=$(field "$head_line" stripped_bytes)
if [ "$base_bytes" != "skipped" ] && [ "$head_bytes" != "skipped" ]; then
  size_pct=$(awk -v b="$base_bytes" -v h="$head_bytes" 'BEGIN{ if (b==0) {print 0} else {printf "%.1f", (h-b)/b*100} }')
  echo "stripped release size: ${base_bytes}B -> ${head_bytes}B (${size_pct}%)"
  if awk -v p="$size_pct" -v t="$SIZE_THRESHOLD_PCT" 'BEGIN{exit !(p>t)}'; then
    echo "FAIL: stripped release binary size regressed ${size_pct}% (threshold ${SIZE_THRESHOLD_PCT}%)" >&2
    status=1
  fi
fi

exit $status
