#!/usr/bin/env bash
# Run the read-only deployment preflight for stage direct.
# Last line of standard output: DEPLOY-PREFLIGHT READY (exit 0), NOT-READY (exit 1), or USAGE (exit 2): `USAGE arguments`, `USAGE target-config`,
# or `USAGE python` when the machine has no Python 3.11 or later (the reason is on standard error).
# READY covers the caller, the Availability Zones and the VPC range; the deploy makes its other checks (the lock, the owner, the bootstrap, the stage settings) when it runs.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
set +e
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
source_rc=$?
set -e
if [ "$source_rc" -ne 0 ]
then
  echo "DEPLOY-PREFLIGHT USAGE python"
  exit 2
fi
if ! py="$(ham_pick_python 11)"
then
  echo "DEPLOY-PREFLIGHT USAGE python"
  exit 2
fi
exec "$py" "$here/lib/deploy_preflight.py" "$@"
