#!/usr/bin/env bash
# Sourced by scripts/build-cline-fork.sh: the uid:gid its containers run as.
# Upstream Cline suites assume a non-root user (plugin-uninstall tests expect permission errors),
# so a root host (CodeBuild) runs containers as FORK_BUILD_UID (default 1000); any other host
# keeps its own uid:gid so the run dir stays its own.

# container_user <host uid> <host gid>: prints uid:gid; fails if the build uid would be root.
container_user() {
  local uid=$1 gid=$2 build_uid=${FORK_BUILD_UID:-1000}
  if [ "$uid" != 0 ]; then
    printf '%s:%s\n' "$uid" "$gid"
    return 0
  fi
  if [ "$build_uid" = 0 ]; then
    echo "fork_user: FORK_BUILD_UID must be a non-root uid" >&2
    return 1
  fi
  printf '%s:%s\n' "$build_uid" "$build_uid"
}

# needs_handover <host uid>: true when the run dir must be handed to the build uid (root host).
needs_handover() {
  [ "$1" = 0 ]
}
