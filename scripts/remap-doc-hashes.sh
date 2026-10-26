#!/usr/bin/env bash
# Dev helper: after a branch was rebase-merged into main, every branch commit has a new hash, so the
# short commit hashes that the docs (CHANGELOG.md, docs/plans, ADRs) cite point to commits that are
# no longer in main's history. This script finds those hashes and maps each one to its rebased twin
# on the target ref (same subject and author date). Report only by default; --apply rewrites files.
set -euo pipefail
# A relative cd below must not follow an exported CDPATH (it could resolve another directory or print it).
unset CDPATH

usage() {
  cat <<'EOF'
Usage: scripts/remap-doc-hashes.sh [--target <ref>] [--apply] [--help] [<file>...]

Fix the commit hashes cited in the docs after a rebase-merge. A rebase-merge gives every branch
commit a new hash, so the hashes written in CHANGELOG.md, docs/plans/*.md and the ADRs while you were
on the branch now point to commits that are not in main's history (they disappear for good once the
old branch is deleted). This script finds them and maps each one to its rebased twin on the target.

By default it only REPORTS: no file is written. Add --apply to rewrite the files in place.

When to run it: right after the rebase-merge into main, BEFORE you delete the old branch (the old
commits must still exist locally, otherwise their hashes cannot be recognised).

How it works:
  1. Scans the files for tokens of 7 to 40 lowercase hex digits on word boundaries.
  2. Ignores every token that is not a commit in this repository (other repositories' hashes, random
     hex, plain numbers). A commit that is already an ancestor of the target is fine and untouched.
  3. A token that is a commit but NOT an ancestor of the target is stale. It is mapped to the one
     commit in the target's history with the same subject AND the same author date (a rebase keeps
     both). Zero or several matches: UNRESOLVED (reported, never changed).
  4. The replacement keeps the token's length (git may return a longer abbreviation if it must).

Options:
  --target <ref>   the branch or commit that holds the rebased commits (default: main)
  --apply          rewrite the files in place (default: report only, nothing is written)
  --help           show this text
  <file>...        the files to scan: a path relative to the current directory, else relative to the
                   repo root (default: every tracked Markdown file, git ls-files '*.md')

Output: one line per stale occurrence, then a summary.
  <file>:<line>: <old> -> <new|UNRESOLVED>  <subject, cut at 60 characters>

Prerequisites: git and perl. Works from any directory (the repo is the one this script lives in);
runs on macOS /bin/bash 3.2 as well as bash 5.

Examples:
  scripts/remap-doc-hashes.sh                     report the stale hashes in every tracked .md file
  scripts/remap-doc-hashes.sh --apply             map them and rewrite the files in place
  scripts/remap-doc-hashes.sh --target origin/main docs/plans/FEATURES.md
                                                  report one file against origin/main

Exit codes:
  0  no stale hash (report mode), or every stale hash was mapped and written (--apply)
  1  stale hashes found (report mode), or unresolved ones remain (--apply)
  2  usage error (unknown option, bad target ref, file not found) or a failure (no git or perl,
     not a git repository, a file that cannot be read or written)

Notes:
  - Read the report before --apply, then review the result with `git diff`.
  - Hashes of commits that are not in the target yet (a branch that is not merged) are reported as
    UNRESOLVED; they are not wrong, just not mapped.
EOF
}

die() {
  printf 'remap-doc-hashes: %s\n' "$*" >&2
  exit 2
}

usage_err() {
  printf 'remap-doc-hashes: %s\nTry: scripts/remap-doc-hashes.sh --help\n' "$*" >&2
  exit 2
}

TARGET="main"
APPLY=0
ARGS=() # the <file> arguments as given

while [ $# -gt 0 ]; do
  case $1 in
    -h | --help)
      usage
      exit 0
      ;;
    --apply) APPLY=1 ;;
    --target)
      case ${2:-} in
        "" | -*) usage_err "--target needs a ref" ;;
      esac
      TARGET=$2
      shift
      ;;
    -*) usage_err "unknown option: $1" ;;
    *) ARGS+=("$1") ;;
  esac
  shift
done

command -v git >/dev/null 2>&1 || die "git not found"
command -v perl >/dev/null 2>&1 || die "perl not found"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository: $SCRIPT_DIR"

g() { git -C "$ROOT" "$@"; }

TARGET_SHA="$(g rev-parse --verify -q "${TARGET}^{commit}" 2>/dev/null)" || usage_err "bad target ref (not a commit): $TARGET"

SCAN=() # the files to scan (paths usable as they are)
if [ "${#ARGS[@]}" -gt 0 ]; then
  for f in "${ARGS[@]}"; do
    if [ -f "$f" ]; then
      SCAN+=("$f")
    elif [ "${f#/}" = "$f" ] && [ -f "$ROOT/$f" ]; then
      SCAN+=("$ROOT/$f")
    else
      usage_err "file not found: $f"
    fi
  done
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/remap-doc-hashes.XXXXXX")" || die "cannot create a temporary directory"
trap 'rm -rf "$TMP"' EXIT
CACHE="$TMP/cache" # one file per token already looked at: "<status> TAB <new> TAB <subject>"
mkdir "$CACHE"
TLOG="$TMP/target.log" # the target's history: "<sha> TAB <author date> TAB <subject>", made on first use
MAP="$TMP/map.tsv"     # "<old token> TAB <new token>" for every mapped occurrence
OCC="$TMP/occurrences" # "<line> TAB <token>" for the file being scanned
: >"$MAP"

if [ "${#SCAN[@]}" -eq 0 ]; then
  g ls-files -z -- '*.md' >"$TMP/list" || die "git ls-files failed"
  while IFS= read -r -d '' f; do
    if [ -f "$ROOT/$f" ]; then
      SCAN+=("$ROOT/$f")
    fi
  done <"$TMP/list"
fi

# classify <token>: look the token up once and cache the answer in $CACHE/<token>.
#   none        not a commit of this repository (ignored)
#   ok          a commit that is an ancestor of the target (untouched)
#   mapped      a stale commit with exactly one twin on the target; <new> is the replacement
#   unresolved  a stale commit with no twin or several twins (reported, never changed)
classify() {
  local tok=$1 full rc subj at matches new
  if [ -e "$CACHE/$tok" ]; then
    return 0
  fi

  full="$(g rev-parse --verify -q "${tok}^{commit}" 2>/dev/null)" || full=""
  case $full in
    "$tok"*) ;; # resolved as an object id, not as a branch or tag that happens to look like hex
    *)
      printf 'none\t-\t-\n' >"$CACHE/$tok"
      return 0
      ;;
  esac

  rc=0
  g merge-base --is-ancestor "$full" "$TARGET_SHA" || rc=$?
  case $rc in
    0)
      printf 'ok\t-\t-\n' >"$CACHE/$tok"
      return 0
      ;;
    1) ;;
    *) die "git merge-base failed for $tok" ;;
  esac

  subj="$(g log -1 --format=%s "$full")"
  at="$(g log -1 --format=%at "$full")"
  if [ ! -e "$TLOG" ]; then
    g log --format='%H%x09%at%x09%s' "$TARGET_SHA" >"$TLOG" || die "git log failed for $TARGET"
  fi
  # ENVIRON, not awk -v: -v would interpret backslash sequences in the subject.
  matches="$(KEY="$at"$'\t'"$subj" awk '{ i = index($0, "\t"); if (substr($0, i + 1) == ENVIRON["KEY"]) print substr($0, 1, i - 1) }' "$TLOG")"

  # exactly one match: non-empty and no newline in it
  if [ -n "$matches" ] && [ "${matches#*$'\n'}" = "$matches" ]; then
    new="$(g rev-parse --short="${#tok}" "$matches")"
    printf 'mapped\t%s\t%s\n' "$new" "$subj" >"$CACHE/$tok"
  else
    printf 'unresolved\tUNRESOLVED\t%s\n' "$subj" >"$CACHE/$tok"
  fi
}

# trunc <text>: the text cut to about 60 characters
trunc() {
  local s=$1
  if [ "${#s}" -gt 60 ]; then
    s="${s:0:57}..."
  fi
  printf '%s' "$s"
}

# The same pattern is used to find the tokens and to replace them, so both agree on what a token is.
# shellcheck disable=SC2016 # perl code, not shell: the $ are perl's
PERL_SCAN='while (/\b([0-9a-f]{7,40})\b/g) { print "$.\t$1\n" }'
# shellcheck disable=SC2016
PERL_REPLACE='BEGIN { open(my $m, "<", $ENV{REMAP_MAP}) or die "map: $!\n"; while (<$m>) { chomp; my ($o, $n) = split /\t/; $map{$o} = $n } close($m) } s/\b([0-9a-f]{7,40})\b/exists $map{$1} ? $map{$1} : $1/ge'

scanned=0 checked=0 stale=0 mapped=0 unresolved=0 rewritten=0
stale_tokens=$'\n' # the distinct stale tokens, each between newlines
distinct=0

if [ "${#SCAN[@]}" -gt 0 ]; then
  for file in "${SCAN[@]}"; do
    disp=${file#"$ROOT"/}
    perl -ne "$PERL_SCAN" -- "$file" >"$OCC" || die "cannot read $file"
    scanned=$((scanned + 1))
    file_mapped=0

    while IFS=$'\t' read -r ln tok; do
      classify "$tok"
      IFS=$'\t' read -r status new subj <"$CACHE/$tok"
      if [ "$status" = none ]; then
        continue
      fi
      checked=$((checked + 1))
      if [ "$status" = ok ]; then
        continue
      fi

      stale=$((stale + 1))
      case $stale_tokens in
        *$'\n'"$tok"$'\n'*) ;;
        *)
          stale_tokens="$stale_tokens$tok"$'\n'
          distinct=$((distinct + 1))
          ;;
      esac
      if [ "$status" = mapped ]; then
        mapped=$((mapped + 1))
        file_mapped=$((file_mapped + 1))
        printf '%s\t%s\n' "$tok" "$new" >>"$MAP"
      else
        unresolved=$((unresolved + 1))
      fi
      printf '%s:%s: %s -> %s  %s\n' "$disp" "$ln" "$tok" "$new" "$(trunc "$subj")"
    done <"$OCC"

    if [ "$APPLY" -eq 1 ] && [ "$file_mapped" -gt 0 ]; then
      REMAP_MAP="$MAP" perl -i -pe "$PERL_REPLACE" -- "$file" || die "cannot rewrite $file"
      rewritten=$((rewritten + 1))
    fi
  done
fi

printf '\nSummary (target: %s = %s)\n' "$TARGET" "$(g rev-parse --short "$TARGET_SHA")"
printf '  files scanned:         %d\n' "$scanned"
printf '  commit-hash tokens:    %d  (occurrences that are commits of this repo)\n' "$checked"
printf '  stale:                 %d  (%d distinct hashes)\n' "$stale" "$distinct"
printf '  mapped:                %d\n' "$mapped"
printf '  unresolved:            %d\n' "$unresolved"
if [ "$APPLY" -eq 1 ]; then
  printf '  files rewritten:       %d\n' "$rewritten"
elif [ "$mapped" -gt 0 ]; then
  printf '\nReport only: no file was written. Run again with --apply to rewrite the files.\n'
fi

if [ "$APPLY" -eq 1 ]; then
  if [ "$unresolved" -gt 0 ]; then
    exit 1
  fi
  exit 0
fi
if [ "$stale" -gt 0 ]; then
  exit 1
fi
exit 0
