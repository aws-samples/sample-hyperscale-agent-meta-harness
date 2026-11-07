#!/usr/bin/env bash
# build-cline-fork.sh: verify and build the pinned Cline fork described by engine/cline/lock.toml.
#
# Usage: scripts/build-cline-fork.sh [--verify-only] [--no-gate] [--target linux-x64|linux-arm64]
#          [--lock FILE] [--work-dir DIR] [--out DIR] [--keep-src]
#          [--record-dir DIR] [--image-digest HEX]
#
#   --verify-only  stop after the tree checks; no toolchain download, no build
#   --no-gate      build without the suite gate (development only; prints BUILD PASS, never
#                  FORK-BUILD PASS)
#   --target       binary to build (default: the host architecture); a target that differs
#                  from the host is cross-compiled with build.ts --install-native-variants
#   --lock         lock file (default engine/cline/lock.toml); patches load from <lock dir>/patches
#   --work-dir     scratch root (default $CLINE_FORK_WORK_DIR; one of the two is required)
#   --out          output dir (default <work>/out/<mode>-<run id>); must be new or empty
#   --keep-src     keep the scratch clone at <work>/runs/<run id>/src
#   --record-dir   also copy the build record into DIR (gated builds only)
#   --image-digest the imageDigest (64 hex, SSM /hamh/live/image-digest) the record is current for;
#                  without it the record's imageDigest is null
#
# Steps: parse the lock; require every file in patches/ and patches/tdd/ to be listed and every
# listed patch to match its SHA-256; clone the tag; require HEAD to equal the locked commit;
# git am the series one patch at a time, applying each test-only patch ([[tdd]]) on a side
# branch just before its series patch and requiring its tree; require HEAD^{tree} to equal the
# locked tree. A build then fetches the pinned bun zip into <work>/tools, checks its SHA-256,
# and, unless --no-gate, runs the gate inside the pinned build image as the host uid with a
# passwd entry: on the unpatched tag and on the patched tree, bun install --frozen-lockfile,
# bun run build:sdk, the five package suites (@cline/llms test, @cline/core test:unit,
# @cline/shared test:unit, @cline/agents test, @cline/cli test:unit) and bun run types, every
# one green. The @cline/core suite runs with vitest file parallelism off (--no-file-parallelism) on both
# trees, because upstream's bash.test.ts races another test file's LocalRuntimeHost start-up over the
# shared TMPDIR (see GATE_STEPS below); the record names the schedule (measurements.gateSchedule). It
# then runs build.ts with CLINE_BUILD_TELEMETRY=off; the build log must show the telemetry-off leak guard.
#
# Outputs: <out>/build-metadata.json always; builds add <out>/cline, <out>/cline.sha256,
# <out>/build.log and, with the gate, <out>/gate/<tree>-<step>.log and the record
# <out>/<date>-cline-fork-build-<x64|arm64>-<run id>.json (kind cline-fork-build; measurements
# arch, baseline {<suite>: "ran"}, suites {baseline, patched: "green"}, passed counts, telemetry,
# leaks, binarySha256), copied to --record-dir and announced as "FORK-BUILD RECORD <path>".
# The last stdout line is VERIFY PASS, BUILD PASS (--no-gate), or
#   FORK-BUILD PASS series=<n> tree=<sha12> bin=<sha12> telemetry=off leaks=0
# Failures print one "build-cline-fork: FAIL: ..." line on stderr.
#
# Isolation: refuses scratch or output dirs under /tmp, /var/tmp, system temp, $HOME itself, a
# dot directory under $HOME, or this repository. git, bun and docker run with HOME, TMPDIR and
# an empty git config from the run dir, so the caller's home config is never read or written.
#
# Exit codes: 0 pass; 2 usage or unsafe environment; 3 invalid lock or patch set; 4 patch
# SHA-256 mismatch; 5 upstream commit mismatch; 6 clone or git am failure; 7 tree mismatch
# (series or test-only); 8 toolchain fetch or SHA-256 mismatch; 9 build failure; 10 gate
# failure (a suite, build:sdk or types step).

set -euo pipefail

PROG=build-cline-fork
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
HERE="$REPO_ROOT/scripts"
# shellcheck source=lib/fork_user.sh
. "$HERE/lib/fork_user.sh"

die() {
  local code=$1
  shift
  printf '%s: FAIL: %s\n' "$PROG" "$*" >&2
  exit "$code"
}

say() { printf '%s\n' "$*"; }

usage() { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//'; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

matches() { [[ $1 =~ $2 ]]; }

json_escape() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  printf '%s' "$s"
}

utc_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# ---- arguments -------------------------------------------------------------------------------

MODE=build
GATE=1
TARGET=""
LOCK_FILE="$REPO_ROOT/engine/cline/lock.toml"
WORK_DIR="${CLINE_FORK_WORK_DIR:-}"
OUT_DIR=""
KEEP_SRC=0
RECORD_DIR=""
IMAGE_DIGEST=""

while [ $# -gt 0 ]; do
  case $1 in
    --verify-only) MODE=verify-only ;;
    --no-gate) GATE=0 ;;
    --target) [ $# -ge 2 ] || die 2 "--target needs a value"; TARGET=$2; shift ;;
    --target=*) TARGET=${1#--target=} ;;
    --lock) [ $# -ge 2 ] || die 2 "--lock needs a value"; LOCK_FILE=$2; shift ;;
    --lock=*) LOCK_FILE=${1#--lock=} ;;
    --work-dir) [ $# -ge 2 ] || die 2 "--work-dir needs a value"; WORK_DIR=$2; shift ;;
    --work-dir=*) WORK_DIR=${1#--work-dir=} ;;
    --out) [ $# -ge 2 ] || die 2 "--out needs a value"; OUT_DIR=$2; shift ;;
    --out=*) OUT_DIR=${1#--out=} ;;
    --keep-src) KEEP_SRC=1 ;;
    --record-dir) [ $# -ge 2 ] || die 2 "--record-dir needs a value"; RECORD_DIR=$2; shift ;;
    --record-dir=*) RECORD_DIR=${1#--record-dir=} ;;
    --image-digest) [ $# -ge 2 ] || die 2 "--image-digest needs a value"; IMAGE_DIGEST=$2; shift ;;
    --image-digest=*) IMAGE_DIGEST=${1#--image-digest=} ;;
    -h | --help) usage; exit 0 ;;
    *) die 2 "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

if [ -n "$IMAGE_DIGEST" ] && ! matches "$IMAGE_DIGEST" '^[0-9a-f]{64}$'; then
  die 2 "--image-digest must be 64 lowercase hex (got '$IMAGE_DIGEST')"
fi
if [ -n "$RECORD_DIR" ] && { [ "$MODE" = verify-only ] || [ "$GATE" = 0 ]; }; then
  die 2 "--record-dir needs a gated build (drop --verify-only and --no-gate)"
fi

case $(uname -m) in
  x86_64 | amd64) HOST_ARCH=x64 ;;
  aarch64 | arm64) HOST_ARCH=arm64 ;;
  *) HOST_ARCH=unknown ;;
esac

case ${TARGET:-linux-$HOST_ARCH} in
  linux-x64) TARGET_ARCH=x64 ;;
  linux-arm64) TARGET_ARCH=arm64 ;;
  *) die 2 "--target must be linux-x64 or linux-arm64 (got '${TARGET:-linux-$HOST_ARCH}')" ;;
esac
TARGET=linux-$TARGET_ARCH

case ${CLINE_BUILD_TELEMETRY:-off} in
  off) ;;
  *) die 2 "this build is telemetry-off only; unset CLINE_BUILD_TELEMETRY (got '${CLINE_BUILD_TELEMETRY}')" ;;
esac

# ---- scratch-path guards ---------------------------------------------------------------------

abspath() {
  case $1 in
    /*) printf '%s' "$1" ;;
    *) printf '%s/%s' "$(pwd -P)" "$1" ;;
  esac
}

# Prints the physical absolute path of $1; the leaf directories need not exist yet.
resolve_path() {
  local p head tail=""
  p=$(abspath "$1")
  case "$p/" in
    */../* | */./*) die 2 "path must not contain . or .. components: $1" ;;
  esac
  head=${p%/}
  while [ -n "$head" ] && [ ! -d "$head" ]; do
    tail="/${head##*/}$tail"
    head=${head%/*}
  done
  [ -n "$head" ] || head=/
  printf '%s%s' "$(cd "$head" && pwd -P)" "$tail"
}

# check_scratch <label> <original> <resolved>
check_scratch() {
  local label=$1 original=$2 p=$3 home_p=""
  if [ -n "${HOME:-}" ] && [ -d "$HOME" ]; then
    home_p=$(cd "$HOME" && pwd -P)
  fi
  case "$p/" in
    / | /tmp/* | /private/tmp/* | /var/tmp/* | /private/var/* | /var/folders/*)
      die 2 "refusing $label $original (resolves to $p): /tmp and system temp dirs are not allowed; use the workspace tmp" ;;
    "$REPO_ROOT"/*)
      die 2 "refusing $label $original (resolves to $p): inside the repository" ;;
  esac
  if [ -n "$home_p" ]; then
    [ "$p" != "$home_p" ] || die 2 "refusing $label $original: it is \$HOME"
    case "$p/" in
      "$home_p"/.*) die 2 "refusing $label $original (resolves to $p): dot directory under \$HOME" ;;
    esac
  fi
}

[ -n "$WORK_DIR" ] || die 2 "set --work-dir or CLINE_FORK_WORK_DIR to a directory under the workspace tmp"
WORK_ORIG=$WORK_DIR
WORK_DIR=$(resolve_path "$WORK_DIR") || exit $?
check_scratch "work dir" "$WORK_ORIG" "$WORK_DIR"

LOCK_FILE=$(abspath "$LOCK_FILE")
PATCH_DIR="$(dirname "$LOCK_FILE")/patches"

# ---- lock parsing ----------------------------------------------------------------------------

LOCK_KEYS=" schema upstream.url upstream.tag upstream.commit series.tree toolchain.bun_version toolchain.bun_linux_x64_zip_sha256 toolchain.bun_linux_aarch64_zip_sha256 toolchain.build_image reference.linux_x64_sha256 reference.linux_arm64_sha256 "
for key in $LOCK_KEYS; do
  printf -v "L_${key//./_}" '%s' ""
done
PATCH_FILES=()
PATCH_SHAS=()
TDD_FILES=()
TDD_SHAS=()
TDD_COMMITS=()
TDD_PARENTS=()
TDD_TREES=()

parse_lock() {
  local file=$1 section="" line lineno=0 key value full n seen=" "
  local re_array='^\[\[([a-z_]+)\]\]$'
  local re_table='^\[([a-z_]+)\]$'
  local re_kv='^([a-z0-9_]+)[[:space:]]*=[[:space:]]*"([^"\\]*)"$'
  [ -f "$file" ] || die 3 "lock file not found: $file"
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    line=${line%$'\r'}
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    case $line in '' | '#'*) continue ;; esac
    if [[ $line =~ $re_array ]]; then
      section=${BASH_REMATCH[1]}
      case $section in
        patches)
          n=${#PATCH_FILES[@]}
          PATCH_FILES[n]=""
          PATCH_SHAS[n]="" ;;
        tdd)
          n=${#TDD_FILES[@]}
          TDD_FILES[n]=""; TDD_SHAS[n]=""; TDD_COMMITS[n]=""; TDD_PARENTS[n]=""; TDD_TREES[n]="" ;;
        *) die 3 "lock line $lineno: unknown array table [[$section]]" ;;
      esac
    elif [[ $line =~ $re_table ]]; then
      section=${BASH_REMATCH[1]}
      case $section in
        upstream | series | toolchain | reference) ;;
        *) die 3 "lock line $lineno: unknown table [$section]" ;;
      esac
      case $seen in *" table:$section "*) die 3 "lock line $lineno: duplicate table [$section]" ;; esac
      seen="${seen}table:$section "
    elif [[ $line =~ $re_kv ]]; then
      key=${BASH_REMATCH[1]}
      value=${BASH_REMATCH[2]}
      if [ "$section" = patches ]; then
        n=$((${#PATCH_FILES[@]} - 1))
        case $key in
          file)
            [ -z "${PATCH_FILES[n]}" ] || die 3 "lock line $lineno: duplicate key patches.file"
            PATCH_FILES[n]=$value ;;
          sha256)
            [ -z "${PATCH_SHAS[n]}" ] || die 3 "lock line $lineno: duplicate key patches.sha256"
            PATCH_SHAS[n]=$value ;;
          *) die 3 "lock line $lineno: unknown key patches.$key" ;;
        esac
      elif [ "$section" = tdd ]; then
        n=$((${#TDD_FILES[@]} - 1))
        case $key in
          file) arr=TDD_FILES ;; sha256) arr=TDD_SHAS ;; commit) arr=TDD_COMMITS ;;
          parent) arr=TDD_PARENTS ;; tree) arr=TDD_TREES ;;
          *) die 3 "lock line $lineno: unknown key tdd.$key" ;;
        esac
        eval "cur=\${$arr[n]}"
        [ -z "$cur" ] || die 3 "lock line $lineno: duplicate key tdd.$key"
        printf -v "$arr[$n]" '%s' "$value"
      else
        full=${section:+$section.}$key
        case $LOCK_KEYS in *" $full "*) ;; *) die 3 "lock line $lineno: unknown key $full" ;; esac
        case $seen in *" $full "*) die 3 "lock line $lineno: duplicate key $full" ;; esac
        seen="$seen$full "
        printf -v "L_${full//./_}" '%s' "$value"
      fi
    else
      die 3 "lock line $lineno: unsupported syntax (only [table], [[patches]] and key = \"string\"): $line"
    fi
  done <"$file"
}

require_match() { # <key> <value> <regex> <description>
  [ -n "$2" ] || die 3 "lock is missing $1"
  matches "$2" "$3" || die 3 "lock $1 must be $4 (got '$2')"
}

parse_lock "$LOCK_FILE"
HEX40='^[0-9a-f]{40}$'
HEX64='^[0-9a-f]{64}$'
[ "$L_schema" = 1 ] || die 3 "lock schema must be \"1\" (got '$L_schema')"
require_match upstream.url "$L_upstream_url" '^https://[A-Za-z0-9._/-]+$' "an https URL"
require_match upstream.tag "$L_upstream_tag" '^[A-Za-z0-9._/-]+$' "a tag name"
require_match upstream.commit "$L_upstream_commit" "$HEX40" "a 40-hex commit"
require_match series.tree "$L_series_tree" "$HEX40" "a 40-hex tree"
require_match toolchain.bun_version "$L_toolchain_bun_version" '^[0-9]+\.[0-9]+\.[0-9]+$' "x.y.z"
require_match toolchain.bun_linux_x64_zip_sha256 "$L_toolchain_bun_linux_x64_zip_sha256" "$HEX64" "a SHA-256"
require_match toolchain.bun_linux_aarch64_zip_sha256 "$L_toolchain_bun_linux_aarch64_zip_sha256" "$HEX64" "a SHA-256"
require_match toolchain.build_image "$L_toolchain_build_image" '^[a-z0-9./_:-]+@sha256:[0-9a-f]{64}$' "an image pinned by digest"
for key in linux_x64_sha256 linux_arm64_sha256; do
  var="L_reference_$key"
  [ -z "${!var}" ] || require_match "reference.$key" "${!var}" "$HEX64" "a SHA-256"
done

PATCH_COUNT=${#PATCH_FILES[@]}
[ "$PATCH_COUNT" -gt 0 ] || die 3 "lock lists no [[patches]]"
for ((i = 0; i < PATCH_COUNT; i++)); do
  require_match "patches[$i].file" "${PATCH_FILES[i]}" '^[0-9]{4}-[A-Za-z0-9._-]+\.patch$' "NNNN-name.patch"
  require_match "patches[$i].sha256" "${PATCH_SHAS[i]}" "$HEX64" "a SHA-256"
  expected_prefix=$(printf '%04d-' $((i + 1)))
  case ${PATCH_FILES[i]} in
    "$expected_prefix"*) ;;
    *) die 3 "lock patches out of sequence: entry $((i + 1)) is ${PATCH_FILES[i]} (expected prefix $expected_prefix)" ;;
  esac
done
TDD_COUNT=${#TDD_FILES[@]}
TDD_AT=()   # TDD_AT[i] = index into TDD_* of the test-only patch applied before series patch i
for ((i = 0; i < TDD_COUNT; i++)); do
  require_match "tdd[$i].file" "${TDD_FILES[i]}" '^tdd/[0-9]{4}-test-only\.patch$' "tdd/NNNN-test-only.patch"
  require_match "tdd[$i].sha256" "${TDD_SHAS[i]}" "$HEX64" "a SHA-256"
  require_match "tdd[$i].commit" "${TDD_COMMITS[i]}" "$HEX40" "a 40-hex commit"
  require_match "tdd[$i].parent" "${TDD_PARENTS[i]}" "$HEX40" "a 40-hex commit"
  require_match "tdd[$i].tree" "${TDD_TREES[i]}" "$HEX40" "a 40-hex tree"
  num=${TDD_FILES[i]#tdd/}
  num=$((10#${num%%-*}))
  if [ "$num" -lt 1 ] || [ "$num" -gt "$PATCH_COUNT" ]; then
    die 3 "lock tdd entry ${TDD_FILES[i]}: no series patch $(printf '%04d' "$num") to precede"
  fi
  [ -z "${TDD_AT[num - 1]:-}" ] || die 3 "lock lists two test-only patches for series patch $(printf '%04d' "$num")"
  TDD_AT[num - 1]=$i
done
say "ok lock $LOCK_FILE ($PATCH_COUNT patches, $TDD_COUNT test-only patches, tag $L_upstream_tag)"

# ---- patch set -------------------------------------------------------------------------------

[ -d "$PATCH_DIR" ] || die 3 "patch directory not found: $PATCH_DIR"
for path in "$PATCH_DIR"/*; do
  [ -e "$path" ] || continue
  name=${path##*/}
  [ "$name" != tdd ] || [ ! -d "$path" ] || continue
  listed=0
  for ((i = 0; i < PATCH_COUNT; i++)); do
    [ "${PATCH_FILES[i]}" != "$name" ] || listed=1
  done
  [ "$listed" = 1 ] || die 3 "unlisted patch $name in $PATCH_DIR: every file there must be listed in the lock"
done
PATCH_PATHS=()
for ((i = 0; i < PATCH_COUNT; i++)); do
  path="$PATCH_DIR/${PATCH_FILES[i]}"
  [ -f "$path" ] || die 3 "patch listed in the lock is missing: ${PATCH_FILES[i]}"
  actual=$(sha256_of "$path")
  [ "$actual" = "${PATCH_SHAS[i]}" ] ||
    die 4 "SHA-256 mismatch for ${PATCH_FILES[i]}: lock ${PATCH_SHAS[i]}, file $actual"
  PATCH_PATHS[i]=$path
  say "ok patch ${PATCH_FILES[i]} sha256 $actual"
done
if [ -d "$PATCH_DIR/tdd" ]; then
  for path in "$PATCH_DIR/tdd"/*; do
    [ -e "$path" ] || continue
    name="tdd/${path##*/}"
    listed=0
    for ((i = 0; i < TDD_COUNT; i++)); do
      [ "${TDD_FILES[i]}" != "$name" ] || listed=1
    done
    [ "$listed" = 1 ] || die 3 "unlisted test-only patch $name in $PATCH_DIR: every file there must be listed in the lock"
  done
fi
TDD_PATHS=()
for ((i = 0; i < TDD_COUNT; i++)); do
  path="$PATCH_DIR/${TDD_FILES[i]}"
  [ -f "$path" ] || die 3 "test-only patch listed in the lock is missing: ${TDD_FILES[i]}"
  actual=$(sha256_of "$path")
  [ "$actual" = "${TDD_SHAS[i]}" ] ||
    die 4 "SHA-256 mismatch for ${TDD_FILES[i]}: lock ${TDD_SHAS[i]}, file $actual"
  # The header names the test-only commit and its parent; the tree is checked by applying it.
  header="X-Harness-Test-Only: commit=${TDD_COMMITS[i]} parent=${TDD_PARENTS[i]} tree="
  grep -qE "^${header}[0-9a-f]{40}\$" "$path" ||
    die 3 "${TDD_FILES[i]} lacks the header line '${header}<tree>'"
  TDD_PATHS[i]=$path
  say "ok patch ${TDD_FILES[i]} sha256 $actual"
done

# ---- run directory ---------------------------------------------------------------------------

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$WORK_DIR/runs/$RUN_ID"
SRC="$RUN_DIR/src"
if [ -z "$OUT_DIR" ]; then
  if [ "$MODE" = build ]; then OUT_DIR="$WORK_DIR/out/$TARGET-$RUN_ID"; else OUT_DIR="$WORK_DIR/out/verify-$RUN_ID"; fi
fi
OUT_ORIG=$OUT_DIR
OUT_DIR=$(resolve_path "$OUT_DIR") || exit $?
check_scratch "output dir" "$OUT_ORIG" "$OUT_DIR"
if [ -d "$OUT_DIR" ] && [ -n "$(ls -A "$OUT_DIR")" ]; then
  die 2 "output dir is not empty: $OUT_DIR"
fi

mkdir -p "$RUN_DIR/home" "$RUN_DIR/tmp"
cleanup() {
  if [ "$KEEP_SRC" = 0 ]; then rm -rf "$SRC" "$RUN_DIR/ctmp"; fi
}
trap cleanup EXIT
STARTED_UTC=$(utc_now)
STARTED_S=$(date +%s)

git_isolated() {
  env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE \
    HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/home/.config" TMPDIR="$RUN_DIR/tmp" \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    git -c core.hooksPath=/dev/null -c advice.detachedHead=false "$@"
}

# ---- upstream commit, series, tree ------------------------------------------------------------

say "cloning $L_upstream_url tag $L_upstream_tag"
git_isolated clone --quiet --depth 1 --branch "$L_upstream_tag" "$L_upstream_url" "$SRC" \
  >"$RUN_DIR/clone.log" 2>&1 ||
  die 6 "git clone of tag $L_upstream_tag from $L_upstream_url failed: $(tail -n 3 "$RUN_DIR/clone.log" | tr '\n' ' ')"
actual_commit=$(git_isolated -C "$SRC" rev-parse HEAD)
[ "$actual_commit" = "$L_upstream_commit" ] ||
  die 5 "commit mismatch: lock upstream.commit $L_upstream_commit, tag $L_upstream_tag resolves to $actual_commit"
say "ok commit $actual_commit"

git_am() { # <patch>
  git_isolated -C "$SRC" -c user.name=cline-fork-build -c user.email=cline-fork-build@localhost \
    am --quiet --committer-date-is-author-date "$1" >>"$RUN_DIR/am.log" 2>&1
}
git_isolated -C "$SRC" checkout -q -b harness-series
for ((i = 0; i < PATCH_COUNT; i++)); do
  t=${TDD_AT[i]:-}
  if [ -n "$t" ]; then
    # The test-only patch applies onto the tag plus the series patches before its number.
    git_isolated -C "$SRC" checkout -q -b "tdd-check-$i"
    git_am "${TDD_PATHS[t]}" ||
      die 6 "git am of ${TDD_FILES[t]} failed: $(tail -n 5 "$RUN_DIR/am.log" | tr '\n' ' ')"
    tdd_tree=$(git_isolated -C "$SRC" rev-parse 'HEAD^{tree}')
    [ "$tdd_tree" = "${TDD_TREES[t]}" ] ||
      die 7 "test-only tree mismatch for ${TDD_FILES[t]}: lock ${TDD_TREES[t]}, applied gives $tdd_tree"
    git_isolated -C "$SRC" checkout -q harness-series
    git_isolated -C "$SRC" branch -q -D "tdd-check-$i"
    say "ok tdd ${TDD_FILES[t]} tree $tdd_tree"
  fi
  git_am "${PATCH_PATHS[i]}" ||
    die 6 "git am of ${PATCH_FILES[i]} failed: $(tail -n 5 "$RUN_DIR/am.log" | tr '\n' ' ')"
done
applied=$(git_isolated -C "$SRC" rev-list --count "$L_upstream_commit..HEAD")
[ "$applied" = "$PATCH_COUNT" ] || die 6 "git am applied $applied commits, expected $PATCH_COUNT"
actual_tree=$(git_isolated -C "$SRC" rev-parse 'HEAD^{tree}')
series_head=$(git_isolated -C "$SRC" rev-parse HEAD)
[ "$actual_tree" = "$L_series_tree" ] ||
  die 7 "tree mismatch: lock series.tree $L_series_tree, applied series gives $actual_tree"
say "ok tree $actual_tree ($PATCH_COUNT patches applied, head $series_head)"

# ---- metadata --------------------------------------------------------------------------------

write_metadata() { # <file> [extra JSON members, already formatted]
  local file=$1 extra=${2:-} patches="" i finished
  finished=$(utc_now)
  for ((i = 0; i < PATCH_COUNT; i++)); do
    patches="$patches${patches:+,
}    {\"file\": \"${PATCH_FILES[i]}\", \"sha256\": \"${PATCH_SHAS[i]}\"}"
  done
  local tdd=""
  for ((i = 0; i < TDD_COUNT; i++)); do
    tdd="$tdd${tdd:+,
}    {\"file\": \"${TDD_FILES[i]}\", \"sha256\": \"${TDD_SHAS[i]}\", \"commit\": \"${TDD_COMMITS[i]}\", \"tree\": \"${TDD_TREES[i]}\"}"
  done
  mkdir -p "$(dirname "$file")"
  cat >"$file" <<EOF
{
  "schema": "1",
  "mode": "$MODE",
  "run_id": "$RUN_ID",
  "started_utc": "$STARTED_UTC",
  "finished_utc": "$finished",
  "duration_s": $(($(date +%s) - STARTED_S)),
  "host": {"os": "$(json_escape "$(uname -s)")", "arch": "$(json_escape "$(uname -m)")", "git": "$(json_escape "$(git --version)")"},
  "lock": {"path": "$(json_escape "$LOCK_FILE")", "sha256": "$(sha256_of "$LOCK_FILE")"},
  "upstream": {"url": "$L_upstream_url", "tag": "$L_upstream_tag", "commit": "$actual_commit"},
  "series": {"tree": "$actual_tree", "head": "$series_head", "patch_count": $PATCH_COUNT},
  "patches": [
$patches
  ],
  "tdd": [
$tdd
  ],
  "telemetry": "off"$extra
}
EOF
}

if [ "$MODE" = verify-only ]; then
  write_metadata "$OUT_DIR/build-metadata.json"
  say "metadata $OUT_DIR/build-metadata.json"
  say "VERIFY PASS"
  exit 0
fi

# ---- toolchain -------------------------------------------------------------------------------

[ "$(uname -s)" = Linux ] || die 2 "builds run on a Linux machine with docker and the tools pinned in tools.lock, or in CodeBuild; use --verify-only elsewhere"
case $HOST_ARCH in
  x64) BUN_ZIP=bun-linux-x64.zip; BUN_SHA=$L_toolchain_bun_linux_x64_zip_sha256 ;;
  arm64) BUN_ZIP=bun-linux-aarch64.zip; BUN_SHA=$L_toolchain_bun_linux_aarch64_zip_sha256 ;;
  *) die 2 "unsupported host architecture $(uname -m)" ;;
esac
command -v docker >/dev/null 2>&1 || die 8 "docker not found; the build runs in $L_toolchain_build_image"
command -v unzip >/dev/null 2>&1 || die 8 "unzip not found"
command -v curl >/dev/null 2>&1 || die 8 "curl not found"

TOOLS_DIR="$WORK_DIR/tools/bun-v$L_toolchain_bun_version"
mkdir -p "$TOOLS_DIR" "$WORK_DIR/cache/bun-install"
if [ ! -f "$TOOLS_DIR/$BUN_ZIP" ]; then
  url="https://github.com/oven-sh/bun/releases/download/bun-v$L_toolchain_bun_version/$BUN_ZIP"
  say "fetching $url"
  env HOME="$RUN_DIR/home" curl -fsSL --retry 5 --retry-all-errors -o "$TOOLS_DIR/$BUN_ZIP.part" "$url" ||
    die 8 "download failed: $url"
  mv "$TOOLS_DIR/$BUN_ZIP.part" "$TOOLS_DIR/$BUN_ZIP"
fi
actual=$(sha256_of "$TOOLS_DIR/$BUN_ZIP")
if [ "$actual" != "$BUN_SHA" ]; then
  rm -f "$TOOLS_DIR/$BUN_ZIP"
  die 8 "SHA-256 mismatch for $BUN_ZIP: lock $BUN_SHA, file $actual (file removed)"
fi
unzip -q -o "$TOOLS_DIR/$BUN_ZIP" -d "$RUN_DIR/tools"
BUN_DIR="$RUN_DIR/tools/${BUN_ZIP%.zip}"
[ -x "$BUN_DIR/bun" ] || die 8 "bun binary missing after unzip: $BUN_DIR/bun"
say "ok bun $L_toolchain_bun_version $BUN_ZIP sha256 $actual"

IMAGE=$L_toolchain_build_image
DOCKER_ENV=(env HOME="$RUN_DIR/home" DOCKER_CONFIG="$RUN_DIR/home/.docker")
if ! "${DOCKER_ENV[@]}" docker image inspect "$IMAGE" >/dev/null 2>&1; then
  say "pulling $IMAGE"
  "${DOCKER_ENV[@]}" docker pull -q "$IMAGE" >"$RUN_DIR/pull.log" 2>&1 ||
    die 8 "docker pull $IMAGE failed: $(tail -n 3 "$RUN_DIR/pull.log" | tr '\n' ' ')"
fi
say "ok image $IMAGE"

# ---- gate --------------------------------------------------------------------------------------

# Suites run as a non-root uid with a passwd entry: src/remote/shell-path.test.ts needs the entry
# and the plugin-uninstall tests need permission errors, which root never gets. A root host
# (CodeBuild) hands the run dir and cache to the build uid (scripts/lib/fork_user.sh).
CUSER=$(container_user "$(id -u)" "$(id -g)") || die 2 "no non-root build uid for the containers"
handover() { # give everything the host created so far to the container uid (root hosts only)
  needs_handover "$(id -u)" || return 0
  mkdir -p "$WORK_DIR/cache"
  chown -R "$CUSER" "$RUN_DIR" "$WORK_DIR/cache" || die 2 "could not hand the run dir to uid $CUSER"
}
PASSWD="$RUN_DIR/passwd"
"${DOCKER_ENV[@]}" docker run --rm "$IMAGE" cat /etc/passwd >"$PASSWD" 2>/dev/null ||
  die 8 "could not read /etc/passwd from $IMAGE"
printf 'fork-build:x:%s:%s::%s:/bin/bash\n' "${CUSER%%:*}" "${CUSER##*:}" "$RUN_DIR/home" >>"$PASSWD"

in_image() { # <workdir> <log> <command>
  "${DOCKER_ENV[@]}" docker run --rm --user "$CUSER" \
    -v "$RUN_DIR:$RUN_DIR" -v "$WORK_DIR/cache:$WORK_DIR/cache" -v "$RUN_DIR/ctmp:/tmp" \
    -v "$PASSWD:/etc/passwd:ro" -w "$1" \
    -e HOME="$RUN_DIR/home" -e TMPDIR="$RUN_DIR/tmp" -e XDG_CACHE_HOME="$RUN_DIR/home/.cache" \
    -e BUN_INSTALL_CACHE_DIR="$WORK_DIR/cache/bun-install" -e CLINE_BUILD_TELEMETRY=off -e CI=true \
    -e PATH="$BUN_DIR:/usr/local/bin:/usr/bin:/bin" \
    "$IMAGE" bash -c "set -e; $3" >"$2" 2>&1
}

# The @cline/core step runs its test files one at a time (vitest --no-file-parallelism), on the unpatched tag and on the
# patched tree alike; no test, assertion or pass criterion changes. Why: LocalRuntimeHost start-up calls
# cleanupStaleDetachedCommandLogs() over the shared TMPDIR, and upstream's bash.test.ts "finalizes a detached log when the
# shell exits while a background child holds the pipes" fakes its process start token, so a concurrent worker's start-up
# retires the test's live log directory and the test reads the log before the exit record.
# Serial files remove the
# concurrent reaper. The other suites keep vitest's default scheduling.
GATE_STEPS="install:bun install --frozen-lockfile|build-sdk:bun run build:sdk|llms:bun -F @cline/llms test|core:bun -F @cline/core test:unit --no-file-parallelism|shared:bun -F @cline/shared test:unit|agents:bun -F @cline/agents test|cli:bun -F @cline/cli test:unit|types:bun run types"
GATE_JSON=""
SUITE_STEPS=" llms core shared agents cli types "
PASSED_baseline=""
PASSED_patched=""
run_gate() { # <label> <tree dir>
  local label=$1 dir=$2 step name cmd log tests IFS='|'
  for step in $GATE_STEPS; do
    name=${step%%:*}
    cmd=${step#*:}
    log="$OUT_DIR/gate/$label-$name.log"
    if ! in_image "$dir" "$log" "$cmd"; then
      die 10 "gate $label $name failed ($cmd); log $log: $(tail -n 5 "$log" | tr '\n' ' ')"
    fi
    tests=$(sed 's/\x1b\[[0-9;]*m//g' "$log" | sed -n 's/.*Tests[[:space:]]\{1,\}\([0-9]\{1,\}\) passed.*/\1/p' | tail -n 1)
    say "ok gate $label $name${tests:+ ($tests passed)}"
    GATE_JSON="$GATE_JSON${GATE_JSON:+, }\"$label-$name\": {\"exit\": 0, \"passed\": ${tests:-null}}"
    case $SUITE_STEPS in
      *" $name "*)
        local var="PASSED_$label"
        printf -v "$var" '%s' "${!var}${!var:+, }\"$name\": ${tests:-null}" ;;
    esac
  done
}

mkdir -p "$RUN_DIR/ctmp"
if [ "$GATE" = 1 ]; then
  mkdir -p "$OUT_DIR/gate"
  BASE="$RUN_DIR/base"
  git_isolated -C "$SRC" worktree add -q --detach "$BASE" "$L_upstream_commit" >>"$RUN_DIR/am.log" 2>&1 ||
    die 6 "could not check out the unpatched tag for the baseline gate"
  say "gate: baseline on the unpatched tag $L_upstream_commit"
  handover
  run_gate baseline "$BASE"
  say "gate: patched series tree $actual_tree"
  run_gate patched "$SRC"
  git_isolated -C "$SRC" worktree remove --force "$BASE" >/dev/null 2>&1 || true
fi

# ---- build -----------------------------------------------------------------------------------

if [ "$TARGET_ARCH" = "$HOST_ARCH" ]; then BUILD_ARGS="--single"; else BUILD_ARGS="--install-native-variants"; fi
BUILD_CMD="bun install --frozen-lockfile && cd apps/cli && bun script/build.ts $BUILD_ARGS"
say "building $TARGET in $IMAGE: $BUILD_CMD"
handover
set +e
"${DOCKER_ENV[@]}" docker run --rm --user "$CUSER" \
  -v "$RUN_DIR:$RUN_DIR" -v "$WORK_DIR/cache:$WORK_DIR/cache" -v "$RUN_DIR/ctmp:/tmp" -w "$SRC" \
  -e HOME="$RUN_DIR/home" -e TMPDIR="$RUN_DIR/tmp" -e XDG_CACHE_HOME="$RUN_DIR/home/.cache" \
  -e BUN_INSTALL_CACHE_DIR="$WORK_DIR/cache/bun-install" -e CLINE_BUILD_TELEMETRY=off \
  -e PATH="$BUN_DIR:/usr/local/bin:/usr/bin:/bin" \
  "$IMAGE" bash -c "set -e; bun --version; $BUILD_CMD" >"$RUN_DIR/build.log" 2>&1
build_rc=$?
set -e
[ "$build_rc" = 0 ] || die 9 "build exited $build_rc; log $RUN_DIR/build.log: $(tail -n 5 "$RUN_DIR/build.log" | tr '\n' ' ')"

BIN="$SRC/apps/cli/dist/cli-$TARGET/bin/cline"
[ -f "$BIN" ] || die 9 "build produced no binary at $BIN"
grep -q '(telemetry: off)' "$RUN_DIR/build.log" || die 9 "build log lacks '(telemetry: off)'"
grep -q "Telemetry off: no build-shell telemetry endpoint, header or key values in cli-$TARGET" "$RUN_DIR/build.log" ||
  die 9 "build log lacks the telemetry-off leak guard line for cli-$TARGET"

elf=$(od -An -tx1 -N4 "$BIN" | tr -d ' \n')
machine=$(od -An -tx1 -j18 -N2 "$BIN" | tr -d ' \n')
case $TARGET_ARCH in x64) want_machine=3e00; machine_name=x86-64 ;; arm64) want_machine=b700; machine_name=aarch64 ;; esac
[ "$elf" = 7f454c46 ] || die 9 "$BIN is not an ELF file"
[ "$machine" = "$want_machine" ] || die 9 "$BIN has ELF machine $machine, expected $want_machine ($machine_name)"

expected_version=$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "$SRC/apps/cli/package.json" | head -n 1)
version=""
if [ "$TARGET_ARCH" = "$HOST_ARCH" ]; then
  version=$(env -i HOME="$RUN_DIR/home" TMPDIR="$RUN_DIR/tmp" PATH=/usr/bin:/bin CLINE_NO_AUTO_UPDATE=1 \
    timeout 60 "$BIN" --version </dev/null 2>"$RUN_DIR/version.err" | tr -d '\r' | head -n 1) ||
    die 9 "$BIN --version failed on the host: $(head -n 3 "$RUN_DIR/version.err" | tr '\n' ' ')"
  [ "$version" = "$expected_version" ] || die 9 "$BIN --version printed '$version', expected '$expected_version'"
  say "ok --version $version on the host"
fi

mkdir -p "$OUT_DIR"
cp "$BIN" "$OUT_DIR/cline"
chmod 755 "$OUT_DIR/cline"
cp "$RUN_DIR/build.log" "$OUT_DIR/build.log"
BIN_SHA=$(sha256_of "$OUT_DIR/cline")
BIN_SIZE=$(wc -c <"$OUT_DIR/cline" | tr -d ' ')
printf '%s  cline\n' "$BIN_SHA" >"$OUT_DIR/cline.sha256"
ref_var="L_reference_${TARGET//-/_}_sha256"
REF_SHA=${!ref_var}
if [ -z "$REF_SHA" ]; then match=null; elif [ "$REF_SHA" = "$BIN_SHA" ]; then match=true; else match=false; fi
BUN_REPORTED=$(sed -n '1p' "$RUN_DIR/build.log" | tr -cd '0-9A-Za-z.+-')

write_metadata "$OUT_DIR/build-metadata.json" ",
  \"target\": \"$TARGET\",
  \"toolchain\": {\"bun_version\": \"$BUN_REPORTED\", \"bun_zip\": \"$BUN_ZIP\", \"bun_zip_sha256\": \"$BUN_SHA\", \"build_image\": \"$IMAGE\", \"build_command\": \"$(json_escape "$BUILD_CMD")\"},
  \"binary\": {\"file\": \"cline\", \"size_bytes\": $BIN_SIZE, \"sha256\": \"$BIN_SHA\", \"elf_machine\": \"$machine_name\", \"version\": \"$(json_escape "${version:-not run: cross-compiled}")\", \"expected_version\": \"$(json_escape "$expected_version")\", \"reference_sha256\": \"$REF_SHA\", \"matches_reference\": $match},
  \"gate\": {\"ran\": $([ "$GATE" = 1 ] && echo true || echo false)${GATE_JSON:+, \"steps\": {$GATE_JSON\}}}"

say "binary $OUT_DIR/cline $BIN_SIZE bytes sha256 $BIN_SHA"
case $match in
  true) say "reference sha256 $REF_SHA: match" ;;
  false) say "reference sha256 $REF_SHA: differs (builds are not bit-reproducible)" ;;
  *) say "reference sha256: none recorded for $TARGET" ;;
esac
say "metadata $OUT_DIR/build-metadata.json"
say "BUILD PASS"
[ "$GATE" = 1 ] || exit 0

# leaks=0: the build log carries 0002's leak-guard line, which the build prints only when no
# build-shell telemetry endpoint, header or key value appears in the binary (checked above).
LAST_LINE="FORK-BUILD PASS series=$PATCH_COUNT tree=${actual_tree:0:12} bin=${BIN_SHA:0:12} telemetry=off leaks=0"

# ---- record (R24) --------------------------------------------------------------------

json_or_null() { if [ -n "$1" ]; then printf '"%s"' "$(json_escape "$1")"; else printf null; fi; }
CODE_SHA=$(git_isolated -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)
CODE_SHA=${CODE_SHA:-${SOURCE_SHA:-}}
CRITERIA_FILE=${CLINE_FORK_CRITERIA:-}
CRITERIA_SHA=""
[ -f "$CRITERIA_FILE" ] && CRITERIA_SHA=$(sha256_of "$CRITERIA_FILE")
BASELINE_RAN=""
for s in $SUITE_STEPS; do BASELINE_RAN="$BASELINE_RAN${BASELINE_RAN:+, }\"$s\": \"ran\""; done
RECORD_NAME="$(date -u +%Y-%m-%d)-cline-fork-build-$TARGET_ARCH-$RUN_ID.json"
# The record's account and Region: a CodeBuild build's own (its build ARN), else the
# accounts.json entry HAMH_AWS_PROFILE selects, else the active one.
if [ -n "${CODEBUILD_BUILD_ARN:-}" ]; then
  IFS=: read -r _ _ _ REC_REGION REC_ACCOUNT _ <<<"$CODEBUILD_BUILD_ARN"
else
  REC_ACCOUNT=$("${HAMH_PYTHON:-python3}" "$REPO_ROOT/scripts/lib/aws_target.py" get account) || die 2 "accounts.json: no target account"
  REC_REGION=$("${HAMH_PYTHON:-python3}" "$REPO_ROOT/scripts/lib/aws_target.py" get region) || die 2 "accounts.json: no target Region"
fi
cat >"$OUT_DIR/$RECORD_NAME" <<EOF
{
 "schemaVersion": 1,
 "kind": "cline-fork-build",
 "createdAt": "$(utc_now)",
 "host": "$(json_escape "$(hostname -f 2>/dev/null || hostname)")",
 "account": "$REC_ACCOUNT",
 "region": "$REC_REGION",
 "stage": "none",
 "codeSha": $(json_or_null "$CODE_SHA"),
 "sourceTree": null,
 "deploymentDigest": null,
 "imageDigest": $(json_or_null "$IMAGE_DIGEST"),
 "criteriaSha256": $(json_or_null "$CRITERIA_SHA"),
 "result": "pass",
 "inputs": {"target": "$TARGET", "lock": "$(json_escape "$LOCK_FILE")", "lockSha256": "$(sha256_of "$LOCK_FILE")", "upstreamTag": "$L_upstream_tag", "upstreamCommit": "$actual_commit", "buildImage": "$IMAGE", "bunVersion": "$BUN_REPORTED", "runId": "$RUN_ID"},
 "measurements": {
  "arch": "$TARGET_ARCH",
  "baseline": {$BASELINE_RAN},
  "suites": {"baseline": "green", "patched": "green"},
  "passed": {"baseline": {$PASSED_baseline}, "patched": {$PASSED_patched}},
  "gateSchedule": {"core": "file-parallelism-off"},
  "telemetry": "off",
  "leaks": 0,
  "binarySha256": "$BIN_SHA",
  "binaryBytes": $BIN_SIZE,
  "elfMachine": "$machine_name",
  "version": "$(json_escape "${version:-not run: cross-compiled}")",
  "seriesTree": "$actual_tree",
  "patchCount": $PATCH_COUNT,
  "lastLine": "$LAST_LINE"
 },
 "runIds": [],
 "notes": "leaks=0: the build log carries patch 0002's leak-guard line, which the build prints only when no build-shell telemetry endpoint, header or key value appears in the binary."
}
EOF
RECORD_PATH="$OUT_DIR/$RECORD_NAME"
if [ -n "$RECORD_DIR" ]; then
  mkdir -p "$RECORD_DIR" || die 2 "could not create --record-dir $RECORD_DIR"
  cp "$OUT_DIR/$RECORD_NAME" "$RECORD_DIR/$RECORD_NAME"
  RECORD_PATH="$RECORD_DIR/$RECORD_NAME"
fi
say "FORK-BUILD RECORD $RECORD_PATH"
say "$LAST_LINE"
