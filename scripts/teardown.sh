#!/usr/bin/env bash
# Tear one stage down and prove what is left; the flow is
# in scripts/lib/teardown.py. Run on a Linux machine with the tools pinned in `tools.lock`.
# usage: scripts/teardown.sh --stage direct [--e2e-only] [--dry-run] [--yes] [--network-wait-minutes N|full]
# Last line: TEARDOWN-INVENTORY 0 resources stage=<s> kms-pending=<n> (exit 0), or the remaining
#            resources then TEARDOWN-INVENTORY <k> resources (exit 1, or 9 when only the network resources that AgentCore holds are left); TEARDOWN-FAIL <why> (exit 1);
#            bad flags, or a change without --yes, exit 2 before any AWS call.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
exec "$py" "$here/lib/teardown.py" "$@"
