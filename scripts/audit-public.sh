#!/usr/bin/env bash
#
# Fails if the tracked tree holds something that must not be published:
# credentials, key files or personal home folders. It prints every offending
# path and line.
#
#   scripts/audit-public.sh [--history [--single-commit]]
#
# Only tracked files are checked, so `git add` new files first.
#
#   --history         also checks every commit reachable from HEAD: author and
#                     committer emails, messages and tree paths
#   --single-commit   with --history, also fails if more than one commit is
#                     reachable from HEAD
#
# Extra regular expressions, one per line and matched case-insensitively, are
# read from the git-ignored file named below when it exists.
#
# Exit status: 0 clean, 1 something was found, 2 the audit itself failed.

set -euo pipefail

cd "$(dirname "$0")/.."

# Patterns may hold \b, which `git grep -E` silently never matches on some
# platforms, so everything goes through grep. The C locale keeps grep from
# failing on a file that isn't valid UTF-8.
export LC_ALL=C

SELF="scripts/audit-public.sh"

die() {
  echo "error: $*" >&2
  exit 2
}

usage() {
  echo "usage: scripts/audit-public.sh [--history [--single-commit]]"
}

HISTORY=0
SINGLE_COMMIT=0
for arg in "$@"; do
  case "$arg" in
    --history) HISTORY=1 ;;
    --single-commit) SINGLE_COMMIT=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "unknown argument: $arg"
      ;;
  esac
done
if [[ "$SINGLE_COMMIT" == 1 && "$HISTORY" == 0 ]]; then
  usage >&2
  die "--single-commit needs --history"
fi

# Extended regular expressions. The lines between the markers are exempt from
# the audit.
# audit-patterns:begin
PRIVATE_DIR=".superpowers"
PRIVATE_FILE="$PRIVATE_DIR/audit-private.txt"
GITIGNORE_EXEMPT_1="$PRIVATE_DIR/"
GITIGNORE_EXEMPT_2="docs/${PRIVATE_DIR#.}/"
FILE_PATTERNS=(
  '\.(p12|p8|cer|mobileprovision)$'
  '(^|/)[^/]*\.env(\.[^/]*)?$'
)
TEXT_PATTERNS=(
  'BEGIN .*PRIVATE KEY'
  'ghp_'
  'gho_'
  'github_pat_'
  'AKIA[0-9A-Z]{16}'
)
HOME_PATTERN='/Users/[a-z]'
# Home folders the tests use as sample data.
PLACEHOLDER_HOMES='me|other'
# audit-patterns:end

PRIVATE_ARGS=()
if [[ -e "$PRIVATE_FILE" ]]; then
  [[ -r "$PRIVATE_FILE" ]] || die "cannot read $PRIVATE_FILE"
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    # A blank line would be an empty pattern, which matches everything.
    if [[ -n "$line" ]]; then
      PRIVATE_ARGS+=(-e "$line")
    fi
  done <"$PRIVATE_FILE"
fi

FILE_ARGS=()
for pattern in "${FILE_PATTERNS[@]}"; do
  FILE_ARGS+=(-e "$pattern")
done
TEXT_ARGS=()
for pattern in "${TEXT_PATTERNS[@]}"; do
  TEXT_ARGS+=(-e "$pattern")
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

status=0
FOUND=""

report() {
  echo "$1:"
  sort -u <<<"$2" | sed -e '/^$/d' -e 's/^/  /'
  status=1
}

# Runs grep and appends its output to FOUND. grep exits 1 when nothing matches
# and higher on a real failure. The checks below run in this shell, not in a
# command substitution, so that die stops the script.
collect() {
  local rc=0 out=""
  out="$(grep "$@")" || rc=$?
  [[ "$rc" -le 1 ]] || die "grep failed"
  if [[ -n "$out" ]]; then
    FOUND+="$out"$'\n'
  fi
}

# Same for home folders, leaving out lines that only name a placeholder.
collect_homes() {
  local rc=0 out=""
  out="$(grep -E -e "$HOME_PATTERN" "$@")" || rc=$?
  [[ "$rc" -le 1 ]] || die "grep failed"
  if [[ -z "$out" ]]; then
    return 0
  fi
  out="$(awk -v home="$HOME_PATTERN" \
    -v placeholder="/Users/($PLACEHOLDER_HOMES)([^[:alnum:]_.-]|\$)" '
      { line = $0; gsub(placeholder, "", line); if (line ~ home) print }
    ' <<<"$out")" || die "awk failed"
  if [[ -n "$out" ]]; then
    FOUND+="$out"$'\n'
  fi
}

# Arguments are passed on to grep: its options and the files to read. Each
# pass opens the files itself, since a shared stdin would be empty for all but
# the first.
scan_text() {
  collect -E "${TEXT_ARGS[@]}" "$@"
  if [[ "${#PRIVATE_ARGS[@]}" -gt 0 ]]; then
    collect -E -i "${PRIVATE_ARGS[@]}" "$@"
  fi
  collect_homes "$@"
}

# A file name can carry the same strings as a line, so paths get both checks.
scan_paths() {
  collect -E "${FILE_ARGS[@]}" "$1"
  scan_text "$1"
}

# Paths.
git -c core.quotePath=off ls-files >"$WORK/tracked" || die "git ls-files failed"
FOUND=""
scan_paths "$WORK/tracked"
[[ -z "$FOUND" ]] || report "Tracked files that must not be published" "$FOUND"

# Content. A tracked file deleted from the working tree has nothing to read.
files=()
while IFS= read -r file || [[ -n "$file" ]]; do
  if [[ -f "$file" ]]; then
    files+=("$file")
  fi
done <"$WORK/tracked"
[[ "${#files[@]}" -gt 0 ]] || die "no tracked files to check"

FOUND=""
scan_text -n -H -- "${files[@]}"

begin="$(grep -n '^# audit-patterns:begin$' "$SELF" | cut -d: -f1)" || die "cannot read $SELF"
end="$(grep -n '^# audit-patterns:end$' "$SELF" | cut -d: -f1)" || die "cannot read $SELF"
[[ -n "$begin" && -n "$end" ]] || die "pattern markers not found in $SELF"

hits="$(awk -F: -v self="$SELF" -v begin="$begin" -v end="$end" \
  -v one="$GITIGNORE_EXEMPT_1" -v two="$GITIGNORE_EXEMPT_2" '
    $1 == self && $2 + 0 >= begin && $2 + 0 <= end { next }
    $1 == ".gitignore" && NF == 3 && ($3 == one || $3 == two) { next }
    { print }
  ' <<<"$FOUND")" || die "awk failed"
[[ -z "$hits" ]] || report "Lines that must not be published" "$hits"

# History.
if [[ "$HISTORY" == 1 ]]; then
  git log --format='%ae%n%ce%n%B' HEAD >"$WORK/log" || die "git log failed"
  FOUND=""
  scan_text "$WORK/log"
  [[ -z "$FOUND" ]] || report "Commit emails or messages that must not be published" "$FOUND"

  git -c core.quotePath=off rev-list --objects HEAD >"$WORK/objects" || die "git rev-list failed"
  # Each object is "<sha> <path>"; commits and the root tree have no path.
  sed -n 's/^[0-9a-f]* //p' "$WORK/objects" >"$WORK/paths" || die "sed failed"
  FOUND=""
  scan_paths "$WORK/paths"
  [[ -z "$FOUND" ]] || report "Paths in history that must not be published" "$FOUND"

  if [[ "$SINGLE_COMMIT" == 1 ]]; then
    count="$(git rev-list --count HEAD)" || die "git rev-list failed"
    if [[ "$count" -gt 1 ]]; then
      report "More than one commit is reachable from HEAD" "$count commits"
    fi
  fi
fi

if [[ "$status" == 0 ]]; then
  echo "audit-public: clean"
fi
exit "$status"
