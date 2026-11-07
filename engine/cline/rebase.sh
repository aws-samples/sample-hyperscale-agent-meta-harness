#!/usr/bin/env bash
# rebase.sh: re-apply the fork series in lock.toml onto an upstream Cline CLI tag.
# It never edits the lock: a passing rebase is the input to the steps in AGENTS.md.
#
# Usage: engine/cline/rebase.sh <tag> [--lock FILE] [--work-dir DIR] [--record-dir DIR] [--merged-upstream NNNN[:COMMIT][,...]]
#
#   <tag>             upstream tag, for example cli-v3.0.66
#   --lock            lock file (default: lock.toml next to this script); patches load from its patches/
#   --work-dir        scratch root (default $CLINE_FORK_WORK_DIR; one of the two is required)
#   --record-dir      on a pass, also write the evidence record <date>-rebase-pass-<run id>.json here.
#                     R24 reads the newest *-rebase-pass*.json when the lock tag
#                     is not fork.upstreamTag
#   --merged-upstream the patches upstream merged into <tag> that the lock no longer lists (the operator's statement; R24 compares
#                     their count with the lock's patch count), each NNNN:<40 lowercase hex digits of the upstream commit>.
#                     Four digits each,
#                     none still in the lock. With --record-dir the commit is required (a record without it cannot pass R23); without
#                     a record the bare NNNN is accepted. The commit is not checked against the fork checkout: it is a depth-1 clone
#
# Resolves the tag with git ls-remote, clones it into <work>/rebase/<tag>-<run id>/src, and runs
# git am for each series patch in lock order, printing "ok patch <file>". Writes
# <work>/rebase/<tag>-<run id>/rebase.json. The last stdout line is
#   REBASE-PASS tag=<tag> commit=<sha> tree=<sha> patches=<n>
# (with --record-dir, "record <path>" comes just before it). A patch that does not apply prints
# "REBASE-FAIL <file>" with the git am output on stderr and exits 6; that patch needs a manual rebase and its own tests
# before the build gate; no record is written.
#
# The record (kind rebase-pass, the evidence header, stage none) holds measurements {tag, commit, tree, patchCount, patches,
# patchSha256 {NNNN: SHA-256 of the bytes git am applied}, mergedUpstream [NNNN], mergedUpstreamCommits {NNNN: 40 hex}, lastLine}.
# It carries no provenPatches: a rebase
# proves the series applies to the tag, not that the package tests of a patch pass (R23 reads provenPatches as test evidence).
# The flow after a pass: regenerate patches/ and update the lock (engine/cline/AGENTS.md), then run this script again on the
# updated lock with --record-dir, so the record covers the patch files the lock now names.
#
# Exit codes: 0 pass; 2 usage or unsafe work or record dir; 3 unreadable lock; 5 tag not found; 6 clone or
# git am failure.
set -euo pipefail

PROG=rebase
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$HERE/../.." && pwd -P)"
die() { local code=$1; shift; printf '%s: FAIL: %s\n' "$PROG" "$*" >&2; exit "$code"; }
usage() { printf 'usage: engine/cline/rebase.sh <tag> [--lock FILE] [--work-dir DIR] [--record-dir DIR] [--merged-upstream NNNN[:COMMIT][,...]]\n' >&2; exit 2; }

TAG="" LOCK="$HERE/lock.toml" WORK="${CLINE_FORK_WORK_DIR:-}" RECORD_DIR="" MERGED="" MERGED_SET=0
while [ $# -gt 0 ]; do
  case $1 in
    --lock) [ $# -ge 2 ] || usage; LOCK=$2; shift ;;
    --work-dir) [ $# -ge 2 ] || usage; WORK=$2; shift ;;
    --record-dir) [ $# -ge 2 ] || usage; RECORD_DIR=$2; shift ;;
    --merged-upstream) [ $# -ge 2 ] || usage; MERGED=$2; MERGED_SET=1; shift ;;
    -h | --help) sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//'; exit 0 ;;
    -*) usage ;;
    *) [ -z "$TAG" ] || usage; TAG=$1 ;;
  esac
  shift
done
[ -n "$TAG" ] || usage
[[ $TAG =~ ^[A-Za-z0-9._/-]+$ ]] || die 2 "bad tag name: $TAG"
[ -n "$WORK" ] || die 2 "set --work-dir or CLINE_FORK_WORK_DIR to a directory under the workspace tmp"

case $WORK in /*) ;; *) WORK="$(pwd -P)/$WORK" ;; esac
case "$WORK/" in */../* | */./*) die 2 "refusing work dir $WORK: . or .. components" ;; esac
home_p=""
[ -n "${HOME:-}" ] && [ -d "$HOME" ] && home_p=$(cd "$HOME" && pwd -P)
case "$WORK/" in
  / | /tmp/* | /private/tmp/* | /var/tmp/* | /private/var/* | /var/folders/*)
    die 2 "refusing work dir $WORK: /tmp and system temp dirs are not allowed; use the workspace tmp" ;;
  "$REPO_ROOT"/*) die 2 "refusing work dir $WORK: inside the repository" ;;
esac
if [ -n "$home_p" ]; then
  [ "$WORK" != "$home_p" ] || die 2 "refusing work dir $WORK: it is \$HOME"
  case "$WORK/" in "$home_p"/.*) die 2 "refusing work dir $WORK: dot directory under \$HOME" ;; esac
fi

[ -f "$LOCK" ] || die 3 "lock file not found: $LOCK"
URL=$(sed -n 's/^url = "\(.*\)"$/\1/p' "$LOCK" | head -n 1)
[ -n "$URL" ] || die 3 "lock has no upstream url"
PATCHES=()
while IFS= read -r f; do PATCHES+=("$f"); done < <(
  awk '/^\[\[patches\]\]/ {p = 1; next} /^\[/ {p = 0} p && /^file = "/ {gsub(/^file = "|"$/, ""); print}' "$LOCK")
[ ${#PATCHES[@]} -gt 0 ] || die 3 "lock lists no [[patches]]"
PATCH_DIR="$(dirname "$LOCK")/patches"

# --merged-upstream: entries NNNN or NNNN:<40 lowercase hex digits>, each patch number once, none of them a patch the lock still
# applies; a record (--record-dir) needs the upstream commit of every merged patch.
MERGED_LIST=() MERGED_ENTRIES=()
if [ "$MERGED_SET" = 1 ]; then
  rest=$MERGED
  while :; do
    MERGED_ENTRIES+=("${rest%%,*}")
    case $rest in *,*) rest=${rest#*,} ;; *) break ;; esac
  done
  for entry in "${MERGED_ENTRIES[@]}"; do
    [[ $entry =~ ^[0-9]{4}(:[0-9a-f]{40})?$ ]] ||
      die 2 "--merged-upstream: '$entry' is not a four-digit patch number, alone or followed by :<40 lowercase hex digits of the upstream commit>"
    n=${entry:0:4}
    MERGED_LIST+=("$n")
    [ -z "$RECORD_DIR" ] || [ "${#entry}" -gt 4 ] ||
      die 2 "--merged-upstream: with --record-dir patch $n needs its upstream commit ($n:<40 hex digits>): a record without it cannot pass R23"
    for f in "${PATCHES[@]}"; do
      [ "${f:0:4}" != "$n" ] || die 2 "--merged-upstream: patch $n is still in the lock ($f)"
    done
  done
  [ "$(printf '%s\n' "${MERGED_LIST[@]}" | sort -u | wc -l | tr -d ' ')" = "${#MERGED_LIST[@]}" ] ||
    die 2 "--merged-upstream names a patch number twice"
fi

# --record-dir: an absolute path outside this repository; the account and Region come from accounts.json as the fork build record's do.
if [ -n "$RECORD_DIR" ]; then
  case $RECORD_DIR in /*) ;; *) RECORD_DIR="$(pwd -P)/$RECORD_DIR" ;; esac
  case "$RECORD_DIR/" in */../* | */./*) die 2 "refusing record dir $RECORD_DIR: . or .. components" ;; esac
  case "$RECORD_DIR/" in "$REPO_ROOT"/*) die 2 "refusing record dir $RECORD_DIR: inside the repository" ;; esac
  REC_ACCOUNT=$(python3 "$REPO_ROOT/scripts/lib/aws_target.py" get account) || die 2 "accounts.json: no target account"
  REC_REGION=$(python3 "$REPO_ROOT/scripts/lib/aws_target.py" get region) || die 2 "accounts.json: no target Region"
fi

RUN="$WORK/rebase/${TAG//\//_}-$(date -u +%Y%m%dT%H%M%SZ)-$$"
SRC="$RUN/src"
mkdir -p "$RUN/home" "$RUN/tmp"
git_isolated() {
  env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE \
    HOME="$RUN/home" XDG_CONFIG_HOME="$RUN/home/.config" TMPDIR="$RUN/tmp" \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    git -c core.hooksPath=/dev/null -c advice.detachedHead=false "$@"
}

refs=$(git_isolated ls-remote --tags "$URL" "refs/tags/$TAG" "refs/tags/$TAG^{}" 2>"$RUN/ls-remote.err") ||
  die 6 "git ls-remote $URL failed: $(tail -n 3 "$RUN/ls-remote.err" | tr '\n' ' ')"
commit=$(printf '%s\n' "$refs" | awk -v t="refs/tags/$TAG^{}" '$2 == t {print $1}')
[ -n "$commit" ] || commit=$(printf '%s\n' "$refs" | awk -v t="refs/tags/$TAG" '$2 == t {print $1}')
[ -n "$commit" ] || die 5 "tag $TAG not found at $URL"

git_isolated clone --quiet --depth 1 --branch "$TAG" "$URL" "$SRC" >"$RUN/clone.log" 2>&1 ||
  die 6 "git clone of tag $TAG failed: $(tail -n 3 "$RUN/clone.log" | tr '\n' ' ')"
[ "$(git_isolated -C "$SRC" rev-parse HEAD)" = "$commit" ] || die 6 "clone of $TAG is not at $commit"

write_report() { # <result> [failed patch]
  local list="" f failed=null
  for f in "${PATCHES[@]}"; do list="$list${list:+, }\"$f\""; done
  [ -z "${2:-}" ] || failed="\"$2\""
  printf '{"tag": "%s", "commit": "%s", "tree": "%s", "result": "%s", "failedPatch": %s, "patches": [%s]}\n' \
    "$TAG" "$commit" "$tree" "$1" "$failed" "$list" >"$RUN/rebase.json"
}

# Each patch is copied first and applied from the copy, so the bytes the record hashes are the bytes git am applied.
mkdir -p "$RUN/applied"
cp -- "$LOCK" "$RUN/lock.snapshot"
tree=""
for f in "${PATCHES[@]}"; do
  if ! { cp -- "$PATCH_DIR/$f" "$RUN/applied/$f" >"$RUN/am.log" 2>&1 &&
    git_isolated -C "$SRC" -c user.name=cline-fork-rebase -c user.email=cline-fork-rebase@localhost \
      am --quiet --committer-date-is-author-date "$RUN/applied/$f" >>"$RUN/am.log" 2>&1; }; then
    cat "$RUN/am.log" >&2
    git_isolated -C "$SRC" am --abort >/dev/null 2>&1 || true
    write_report fail "$f"
    echo "REBASE-FAIL $f"
    exit 6
  fi
  echo "ok patch $f"
done
tree=$(git_isolated -C "$SRC" rev-parse 'HEAD^{tree}')
write_report pass
echo "report $RUN/rebase.json"
LAST_LINE="REBASE-PASS tag=$TAG commit=$commit tree=$tree patches=${#PATCHES[@]}"

# ---- the evidence record, only with --record-dir ---------------------------------------
if [ -n "$RECORD_DIR" ]; then
  mkdir -p "$RECORD_DIR" || die 2 "could not create --record-dir $RECORD_DIR"
  RUN_ID=$(basename "$RUN")
  RECORD_PATH="$RECORD_DIR/$(date -u +%Y-%m-%d)-rebase-pass-$RUN_ID.json"
  CODE_SHA=$(git_isolated -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)
  CRITERIA_FILE=${CLINE_FORK_CRITERIA:-}
  REC_TMP="$RECORD_PATH.tmp.$$"
  REC_TAG=$TAG REC_COMMIT=$commit REC_TREE=$tree REC_LAST=$LAST_LINE REC_RUN_ID=$RUN_ID REC_URL=$URL REC_LOCK=$LOCK \
    REC_APPLIED_DIR="$RUN/applied" REC_LOCK_SNAPSHOT="$RUN/lock.snapshot" REC_CODE_SHA=$CODE_SHA REC_CRITERIA=$CRITERIA_FILE \
    REC_ACCOUNT=$REC_ACCOUNT REC_REGION=$REC_REGION REC_MERGED="${MERGED_ENTRIES[*]:-}" REC_PATCHES="$(printf '%s\n' "${PATCHES[@]}")" \
    python3 - "$REC_TMP" <<'PY' || die 2 "could not write the record"
import datetime, hashlib, json, os, socket, sys

e = os.environ


def sha(path):
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()


patches = e["REC_PATCHES"].splitlines()
merged = e["REC_MERGED"].split()  # NNNN:<40 hex> each (the script refused anything else before it cloned anything)
record = {
    "schemaVersion": 1, "kind": "rebase-pass",
    "createdAt": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "host": socket.getfqdn(), "account": e["REC_ACCOUNT"], "region": e["REC_REGION"], "stage": "none",
    "codeSha": e["REC_CODE_SHA"] or None, "sourceTree": None, "deploymentDigest": None, "imageDigest": None,
    "criteriaSha256": sha(e["REC_CRITERIA"]) if os.path.isfile(e["REC_CRITERIA"]) else None,
    "result": "pass",
    "inputs": {"tag": e["REC_TAG"], "lock": e["REC_LOCK"], "lockSha256": sha(e["REC_LOCK_SNAPSHOT"]), "upstreamUrl": e["REC_URL"],
               "runId": e["REC_RUN_ID"]},
    "measurements": {
        "tag": e["REC_TAG"], "commit": e["REC_COMMIT"], "tree": e["REC_TREE"], "patchCount": len(patches), "patches": patches,
        "patchSha256": {f[:4]: sha(os.path.join(e["REC_APPLIED_DIR"], f)) for f in patches},
        "mergedUpstream": [m[:4] for m in merged], "mergedUpstreamCommits": {m[:4]: m[5:] for m in merged},
        "lastLine": e["REC_LAST"]},
    "runIds": [],
    "notes": "The series applies to the tag (git am of each patch, in lock order, from a copy whose SHA-256 is patchSha256). "
             "No package test ran, so there is no provenPatches: R23 reads that field as test evidence. mergedUpstream and "
             "mergedUpstreamCommits are the operator's --merged-upstream statement (patch number and upstream commit); R24 checks the "
             "count against the lock's patch count and R23 the commits.",
}
with open(sys.argv[1], "w") as fh:
    json.dump(record, fh, indent=1)
    fh.write("\n")
PY
  mv -- "$REC_TMP" "$RECORD_PATH"
  echo "record $RECORD_PATH"
fi
echo "$LAST_LINE"
