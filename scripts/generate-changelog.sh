#!/usr/bin/env bash
# CHANGELOG generation from conventional commits (issue #433). A light
# script running inside the existing CD workflow, NOT release-please:
# release-please stages its own release PR and drops the per-PR version
# bump, which conflicts with this repo's decided model (linear branches,
# rebase-merge, CD triggers on push to main, every PR bumps Cargo.toml).
# This script only reads commit subjects and git tags; it never touches
# Cargo.toml or decides a version -- that stays the PR author's job, enforced
# by ci.yaml's existing `version-bump` job.
#
# Recognized conventional-commit subject shape: "type(scope)!: summary" or
# "type: summary" (scope and the breaking-change "!" are both optional).
# A trailing "BREAKING CHANGE:" footer in the commit body is NOT detected --
# only the "!" marker in the subject line -- because a footer needs the full
# commit body, which this light script deliberately does not parse. State
# breaking changes with "!" in the subject if you want them called out here.
#
# Usage:
#   generate-changelog.sh --print-header
#   generate-changelog.sh --section --to <rev> [--from <rev>] [--header <text>]
#   generate-changelog.sh --full [--to <rev>]
#   generate-changelog.sh --suggest-bump --to <rev> [--from <rev>]
#   generate-changelog.sh --self-test
#
# --print-header prints CHANGELOG.md's fixed top-of-file explanation, the
#   single source of truth for that text -- a full rebuild is
#   `{ generate-changelog.sh --print-header; generate-changelog.sh --full; }
#   > CHANGELOG.md`.
# --section prints ONE Markdown section for commits in (from, to], newest
#   first within each type group. --from defaults to the nearest tag
#   reachable from --to (excluding --to itself); with no tag at all, it
#   covers the full history up to --to. --header overrides the generated
#   "## <to>" line (CD passes the real tag and date).
# --full walks every tag in version order (oldest first is wrong for a
#   changelog -- newest first, matching Keep a Changelog convention) and
#   prints one --section per tag, for a one-time backfill or a full rebuild.
#   It does NOT print the header -- combine it with --print-header, above.
# --suggest-bump prints exactly one word ("major", "minor", "patch", or
#   "none") to stdout for the commits in (from, to], plus a one-line summary
#   to stderr. Always exits 0 -- this is an advisory hint, never a gate.
# --self-test builds a throwaway git repo with known commit types and
#   asserts both the grouping and the bump suggestion; exits 1 on mismatch.

set -euo pipefail

script_path=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")

mode=""
from_rev=""
to_rev="HEAD"
header=""

usage() {
  echo "usage: $0 --print-header" >&2
  echo "       $0 --section --to <rev> [--from <rev>] [--header <text>]" >&2
  echo "       $0 --full [--to <rev>]" >&2
  echo "       $0 --suggest-bump --to <rev> [--from <rev>]" >&2
  echo "       $0 --self-test" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --section) mode="section"; shift ;;
    --full) mode="full"; shift ;;
    --print-header) mode="print-header"; shift ;;
    --suggest-bump) mode="suggest-bump"; shift ;;
    --self-test) mode="self-test"; shift ;;
    --from)
      if [ $# -lt 2 ]; then usage; exit 2; fi
      from_rev="$2"; shift 2 ;;
    --to)
      if [ $# -lt 2 ]; then usage; exit 2; fi
      to_rev="$2"; shift 2 ;;
    --header)
      if [ $# -lt 2 ]; then usage; exit 2; fi
      header="$2"; shift 2 ;;
    *)
      usage; exit 2 ;;
  esac
done

if [ -z "$mode" ]; then
  usage
  exit 2
fi

# ---------------------------------------------------------------------
# nearest_tag <rev>: the closest tag strictly before <rev> (exclusive of
# <rev> itself, so `--section --to <new-tag>` does not resolve --from back
# to <new-tag> when the tag already exists at the time this runs). Empty
# output means no earlier tag exists -- the caller then covers full history.
nearest_tag() {
  local rev="$1"
  git describe --tags --abbrev=0 "${rev}^" 2>/dev/null || true
}

# ---------------------------------------------------------------------
# classify_subject <subject> -> prints "<group>\t<breaking>\t<rest>", where
# <group> is one of: feat fix perf docs chore other, <breaking> is "1" or
# "0", and <rest> is the subject with the leading "type(scope)!: " stripped.
# A subject with no recognized "type:" or "type(scope):" prefix at all
# classifies as "other" with the full original subject as <rest>.
classify_subject() {
  local subject="$1"
  local type rest breaking=0
  local pattern='^([A-Za-z]+)(\([^)]*\))?(!)?:[[:space:]]*(.*)$'

  if [[ "$subject" =~ $pattern ]]; then
    type=$(printf '%s' "${BASH_REMATCH[1]}" | tr '[:upper:]' '[:lower:]')
    [ -n "${BASH_REMATCH[3]}" ] && breaking=1
    rest="${BASH_REMATCH[4]}"
  else
    printf 'other\t0\t%s\n' "$subject"
    return
  fi

  case "$type" in
    feat) printf 'feat\t%s\t%s\n' "$breaking" "$rest" ;;
    fix) printf 'fix\t%s\t%s\n' "$breaking" "$rest" ;;
    perf) printf 'perf\t%s\t%s\n' "$breaking" "$rest" ;;
    docs) printf 'docs\t%s\t%s\n' "$breaking" "$rest" ;;
    chore | refactor | test | style | ci | build | revert)
      printf 'chore\t%s\t%s\n' "$breaking" "$rest" ;;
    *) printf 'other\t0\t%s\n' "$subject" ;;
  esac
}

# ---------------------------------------------------------------------
# emit_section <from> <to> <header>: prints one Markdown section. <from>
# may be empty (full history up to <to>).
emit_section() {
  local from="$1" to="$2" hdr="$3"
  local range
  if [ -n "$from" ]; then
    range="${from}..${to}"
  else
    range="$to"
  fi

  local tmp
  tmp=$(mktemp "${TMPDIR:-/tmp}/generate-changelog.XXXXXX")
  trap 'rm -f "$tmp"' RETURN
  git log --no-merges --pretty=format:%s "$range" > "$tmp" || true

  if [ ! -s "$tmp" ]; then
    return 1
  fi

  local -a feat=() fix=() perf=() docs=() chore=() other=() breaking=()
  while IFS=$'\t' read -r group brk rest; do
    if [ "$brk" = "1" ]; then
      breaking+=("$rest")
      continue
    fi
    case "$group" in
      feat) feat+=("$rest") ;;
      fix) fix+=("$rest") ;;
      perf) perf+=("$rest") ;;
      docs) docs+=("$rest") ;;
      chore) chore+=("$rest") ;;
      *) other+=("$rest") ;;
    esac
  done < <(while IFS= read -r subject || [ -n "$subject" ]; do classify_subject "$subject"; done < "$tmp")

  echo "$hdr"
  echo
  print_group() {
    local title="$1"; shift
    [ "$#" -eq 0 ] && return 0
    echo "### $title"
    echo
    local line
    for line in "$@"; do
      echo "- $line"
    done
    echo
  }
  print_group "Breaking Changes" "${breaking[@]}"
  print_group "Features" "${feat[@]}"
  print_group "Fixes" "${fix[@]}"
  print_group "Performance" "${perf[@]}"
  print_group "Documentation" "${docs[@]}"
  print_group "Chores" "${chore[@]}"
  print_group "Other" "${other[@]}"
  return 0
}

case "$mode" in
  print-header)
    cat <<'HEADER'
# Changelog

Generated from conventional-commit subjects (`feat`, `fix`, `perf`, `docs`,
plus `chore`/`refactor`/`test`/`style`/`ci`/`build`/`revert` folded into
Chores) by `scripts/generate-changelog.sh`. Never hand-edit this file --
regenerate a section instead:

    scripts/generate-changelog.sh --section --from <prev-tag> --to <new-tag>

`.github/workflows/cd.yaml` runs that command for every release and commits
the new top section here. A commit subject that does not match
`type(scope)!: summary` (most of the history before this file existed) is
listed under "Other" rather than dropped or guessed at. A `!` right after
the type/scope is the only breaking-change signal this script reads -- a
`BREAKING CHANGE:` footer in the commit body is not parsed. Full-rebuild
command: `scripts/generate-changelog.sh --full`.

HEADER
    ;;

  section)
    from="$from_rev"
    if [ -z "$from" ]; then
      from=$(nearest_tag "$to_rev")
    fi
    hdr="$header"
    if [ -z "$hdr" ]; then
      hdr="## ${to_rev}"
    fi
    emit_section "$from" "$to_rev" "$hdr" || echo "$hdr"$'\n\n(no user-facing commits in this range)\n'
    ;;

  full)
    tags=$(git tag --sort=v:refname)
    if [ -z "$tags" ]; then
      echo "no tags found" >&2
      exit 1
    fi
    # Newest first, each paired with the tag immediately before it.
    prev=""
    ordered=()
    while IFS= read -r t; do ordered+=("$t"); done <<< "$tags"
    for ((i = ${#ordered[@]} - 1; i >= 0; i--)); do
      t="${ordered[$i]}"
      if [ "$i" -gt 0 ]; then
        prev="${ordered[$((i - 1))]}"
      else
        prev=""
      fi
      date=$(git log -1 --format=%as "$t")
      emit_section "$prev" "$t" "## ${t} (${date})" || true
    done
    ;;

  suggest-bump)
    from="$from_rev"
    if [ -z "$from" ]; then
      from=$(nearest_tag "$to_rev")
    fi
    range="$to_rev"
    [ -n "$from" ] && range="${from}..${to_rev}"

    tmp=$(mktemp "${TMPDIR:-/tmp}/generate-changelog.XXXXXX")
    trap 'rm -f "$tmp"' EXIT
    git log --no-merges --pretty=format:%s "$range" > "$tmp" 2>/dev/null || true

    has_breaking=0 has_feat=0 has_fix_or_perf=0 total=0
    while IFS=$'\t' read -r group brk _rest; do
      total=$((total + 1))
      [ "$brk" = "1" ] && has_breaking=1
      [ "$group" = "feat" ] && has_feat=1
      { [ "$group" = "fix" ] || [ "$group" = "perf" ]; } && has_fix_or_perf=1
    done < <(while IFS= read -r subject || [ -n "$subject" ]; do classify_subject "$subject"; done < "$tmp")

    if [ "$has_breaking" = "1" ]; then
      suggestion="major"
    elif [ "$has_feat" = "1" ]; then
      suggestion="minor"
    elif [ "$has_fix_or_perf" = "1" ]; then
      suggestion="patch"
    elif [ "$total" -gt 0 ]; then
      suggestion="patch"
    else
      suggestion="none"
    fi
    echo "conventional-commit bump suggestion for ${range}: $suggestion ($total commit(s) examined)" >&2
    echo "$suggestion"
    ;;

  self-test)
    workdir=$(mktemp -d "${TMPDIR:-/tmp}/generate-changelog-selftest.XXXXXX")
    trap 'rm -rf "$workdir"' EXIT
    (
      cd "$workdir"
      git init -q -b main
      git config user.email "test@example.com"
      git config user.name "test"
      git commit -q --allow-empty -m "chore: initial"
      git tag v0.1.0
      git commit -q --allow-empty -m "feat(dash): add roster"
      git commit -q --allow-empty -m "fix(hook): correct pretool payload"
      git commit -q --allow-empty -m "docs: update README"
      git commit -q --allow-empty -m "feat(wrap)!: change passthrough contract"
      git tag v0.2.0
    )

    section_out=$(cd "$workdir" && "$script_path" --section --from v0.1.0 --to v0.2.0 --header "## v0.2.0")
    fail=0
    for expect in "Breaking Changes" "change passthrough contract" "Features" "add roster" "Fixes" \
      "correct pretool payload" "Documentation" "update README"; do
      if ! grep -qF "$expect" <<< "$section_out"; then
        echo "self-test FAIL: expected to find '$expect' in section output" >&2
        fail=1
      fi
    done

    bump=$(cd "$workdir" && "$script_path" --suggest-bump --from v0.1.0 --to v0.2.0 2>/dev/null)
    if [ "$bump" != "major" ]; then
      echo "self-test FAIL: expected bump suggestion 'major' (a '!' commit is present), got '$bump'" >&2
      fail=1
    fi

    bump_minor=$(cd "$workdir" && git tag v0.1.1 HEAD~2 && "$script_path" --suggest-bump --from v0.1.0 --to v0.1.1 2>/dev/null)
    if [ "$bump_minor" != "minor" ]; then
      echo "self-test FAIL: expected bump suggestion 'minor', got '$bump_minor'" >&2
      fail=1
    fi

    if [ "$fail" = "1" ]; then
      exit 1
    fi
    echo "self-test OK"
    ;;
esac
