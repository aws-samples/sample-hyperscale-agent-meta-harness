#!/usr/bin/env bash
# Pinned downloads that survive a bad transfer. A bad transfer is not a bad pin, so the installers that build the worker and
#   verifier images ask again before they give up: retry_run <label> <command> [<arg>...]   runs the command; when it fails,
#   waits and runs it again. HAMH_RETRY_ATTEMPTS (default 4) attempts in all. HAMH_RETRY_PAUSES (default "5 15 45") are the
#   seconds to wait before attempt 2, 3, ...; the last value repeats. Returns 0 on the first success, else the last attempt's
#   exit status. Notes go to stderr, the command's own output is untouched, and `set -e` is not touched: after the last failure
#   the caller sees an ordinary failing command. fetch_pinned <url> <sha256> <dest>   downloads to a staging file beside <dest>
#   (<dest>.part.XXXXXX, its own for each attempt), checks the pinned SHA-256 before the file takes its name, and runs that
#   pair again when either the transfer or the check fails. The pin is never relaxed: a file that stays wrong is never put in
#   place. A mismatch says the HTTP status, size and content type it saw, so a proxy's error page is recognisable. Returns 0,
#   curl's exit status for a transfer that never worked, 65 for a SHA-256 that never matched, or 2 for a malformed call. Worst
#   case for one asset: 4 attempts of curl's own 5 retries (1 to 16 s apart) and the 65 s of pauses, so about 3 minutes before
#   it gives up on a dead URL, and a 404 is retried like any other error (curl's --retry-all-errors does that). Run as a
#   command, the exit status is fetch_pinned's. Used by images/tools/install.sh and lang/install.sh, images/codebuild/*.yml
#   (the release assets of the image builds) and, as a copy of the block below, images/services/install.sh (which is
#   bind-mounted alone); scripts/tests/test_fetch_retry.py keeps the copy equal.

# >>> retry-lib (images/services/install.sh carries a copy of this block; scripts/tests/test_fetch_retry.py keeps them equal)
retry_run() { # <label> <command> [<arg>...]
  local label=${1-} attempts="${HAMH_RETRY_ATTEMPTS:-4}" pauses="${HAMH_RETRY_PAUSES-5 15 45}" n=1 rc=0 i p pause
  local num='[0-9]+(\.[0-9]+)?' re IFS=$' \t\n'
  [ $# -ge 2 ] || { echo "usage: retry_run <label> <command> [<arg>...]" >&2; return 2; }
  shift
  [[ $attempts =~ ^[1-9][0-9]*$ ]] || { echo "retry: HAMH_RETRY_ATTEMPTS must be a positive integer, got '$attempts'" >&2; return 2; }
  re="^$num( $num)*\$"
  [[ $pauses =~ $re ]] || { echo "retry: HAMH_RETRY_PAUSES must be seconds separated by spaces, got '$pauses'" >&2; return 2; }
  while :; do
    rc=0
    "$@" || rc=$?
    [ "$rc" -ne 0 ] || return 0
    if [ "$n" -ge "$attempts" ]; then
      echo "retry: $label failed after $n attempts (last exit status $rc)" >&2
      return "$rc"
    fi
    i=0
    pause=
    for p in $pauses; do
      pause=$p
      i=$((i + 1))
      [ "$i" -lt "$n" ] || break
    done
    echo "retry: $label attempt $n of $attempts failed (exit status $rc); retrying in ${pause}s" >&2
    sleep "$pause" || :
    n=$((n + 1))
  done
}
_fetch_pinned_once() { # <url> <sha256> <dest>
  local url=$1 want=$2 dest=$3 part meta code size type got rc=0 fmt='%{http_code} %{size_download} %{content_type}'
  # A staging file of its own, so two installers sharing a download cache cannot write one file.
  part=$(mktemp "$dest.part.XXXXXX") || { echo "fetch: cannot create a staging file beside $dest" >&2; return 1; }
  meta=$(curl -fsSL --retry 5 --retry-all-errors --connect-timeout 30 --speed-limit 1024 --speed-time 60 -w "$fmt" -o "$part" "$url") || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "fetch: transfer of $url failed (curl exit status $rc)" >&2
    rm -f "$part"
    return "$rc"
  fi
  got=$(sha256sum "$part") || { echo "fetch: cannot checksum the download of $url" >&2; rm -f "$part"; return 1; }
  got=${got%% *}
  if [ "$got" != "$want" ]; then
    read -r code size type <<<"$meta" || :
    echo "fetch: SHA-256 of $url is $got, wanted $want (HTTP ${code:-?}, ${size:-?} bytes, ${type:-no content type})" >&2
    rm -f "$part"
    return 65
  fi
  chmod 0644 "$part" && mv -f "$part" "$dest"
}
fetch_pinned() { # <url> <sha256> <dest>
  local want
  [ $# -eq 3 ] || { echo "usage: fetch_pinned <url> <sha256> <dest>" >&2; return 2; }
  want=${2,,}
  [[ $want =~ ^[0-9a-f]{64}$ ]] || { echo "fetch: not a SHA-256: '$2'" >&2; return 2; }
  retry_run "download of ${1##*/}" _fetch_pinned_once "$1" "$want" "$3"
}
# <<< retry-lib

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  [ $# -eq 3 ] || { echo "usage: fetch.sh <url> <sha256> <dest>" >&2; exit 2; }
  fetch_pinned "$@"
fi
