#!/usr/bin/env bash
# Run a command while holding one of CARGO_SLOTS (default 2) flock slots, so at most that many
# cargo builds run at once on one machine across worktrees and builders.
# usage: scripts/cargo-slot.sh <command> [args...]    exit code: the command's; usage errors exit 2
# Slots are lock files <CARGO_SLOT_DIR>/cargo-<n>.lock (default HAMH_TOOLS/locks, else $TMPDIR/cargo-slots when that cannot be made). A free slot is
# taken at once; with none free the script polls every slot until one frees. The command runs
# without the lock fd, so a daemon it starts (the sccache server) cannot keep the slot held.
# CARGO_SLOT_PRIORITY=high (the deploy's builds) registers <dir>/priority/<pid> while waiting; a
# normal job takes no slot while a live registration exists, and a dead one's file is removed.
# flock comes with util-linux and a stock macOS has none, and its bash is older than the 4.1 that named lock descriptors need. Without either the command runs at
# once, after one line on standard error that says so: a machine of one person has no other build to wait for. A flock that fails in any other way than a busy slot is an error (exit 2) that prints what flock said; "busy" is the
# silent exit 1 of `flock -n` and `flock -w`, and nothing else is waited for.
set -uo pipefail
[ $# -ge 1 ] || { echo "usage: cargo-slot.sh <command> [args...]" >&2; exit 2; }
case "${CARGO_SLOT_PRIORITY:-normal}" in
  high) high=1 ;;
  normal) high="" ;;
  *) echo "cargo-slot.sh: CARGO_SLOT_PRIORITY must be high or normal" >&2; exit 2 ;;
esac
if ! command -v flock >/dev/null 2>&1; then
  echo "cargo-slot.sh: flock is unavailable on this machine. Your command runs immediately without a build slot." >&2
  exec "$@"
fi
# The lock file descriptors below (`exec {fd}>file`) need bash 4.1; the bash of a stock macOS is 3.2.
if [ "${BASH_VERSINFO[0]}" -lt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 1 ]; }; then
  echo "cargo-slot.sh: bash ($BASH_VERSION) is older than 4.1, which build locks require. Your command runs immediately without a build slot." >&2
  exec "$@"
fi
# A folder named with CARGO_SLOT_DIR is the caller's choice: if it cannot be made that is an error. The default folder is under the build host's
# tools folder; a ham session cannot make it, and its slots then live under $TMPDIR/cargo-slots (nothing else of the machine builds there).
if [ -n "${CARGO_SLOT_DIR:-}" ]; then
  dir="$CARGO_SLOT_DIR"
  mkdir -p "$dir/priority" 2>/dev/null || { echo "cargo-slot.sh: cannot create $dir" >&2; exit 2; }
else
  dir="${HAMH_TOOLS:-${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/ham}/locks"
  if ! mkdir -p "$dir/priority" 2>/dev/null; then
    dir="${TMPDIR:-/tmp}/cargo-slots"
    mkdir -p "$dir/priority" 2>/dev/null || { echo "cargo-slot.sh: cannot create $dir" >&2; exit 2; }
  fi
fi
slots="${CARGO_SLOTS:-2}"
prio="$dir/priority"
if [ -n "$high" ]; then
  echo $$ > "$prio/$$"
  trap 'rm -f "$prio/$$"' EXIT
fi

# True while another process's high-priority registration is live; removes dead ones.
priority_waiting() {
  local f pid
  for f in "$prio"/*; do
    [ -e "$f" ] || continue
    pid=${f##*/}
    [ "$pid" = $$ ] && continue
    if kill -0 "$pid" 2>/dev/null; then return 0; fi
    rm -f "$f"
  done
  return 1
}

# Try each slot once (-n, or waiting up to $1 seconds on each); sets got and fd on success. The lock belongs to the open file description, which the
# subshell of the command substitution shares with this shell, so it stays held when that subshell ends.
take() {
  local i err rc
  for ((i = 0; i < slots; i++)); do
    exec {fd}>"$dir/cargo-$i.lock"
    err=$(flock "$@" "$fd" 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ]; then got=$i; return 0; fi
    exec {fd}>&-
    if [ -n "$err" ] || [ "$rc" -ge 126 ]; then
      echo "cargo-slot.sh: flock failed (exit $rc): ${err:-no message}" >&2
      exit 2
    fi
  done
  return 1
}

got=""
mode=(-n)
while [ -z "$got" ]; do
  # A normal job yields to a waiting high-priority job; high-priority jobs compete among themselves.
  if [ -z "$high" ] && priority_waiting; then sleep 0.5; continue; fi
  # All slots busy: wait for whichever frees first by polling each slot.
  take "${mode[@]}" || mode=(-w 0.2)
done
[ -n "$high" ] && rm -f "$prio/$$"
"$@" {fd}>&-
rc=$?
exec {fd}>&-
# nosemgrep: unquoted-variable-expansion-in-command -- rc holds the numeric exit status saved earlier from the last command and exit takes one number, so word splitting cannot change it
exit $rc
