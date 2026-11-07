#!/usr/bin/env bash
# Deploy the committed HEAD to one stage, stack by stack; the flow is in
# scripts/lib/deploy.py. Run on a Linux machine with the tools pinned in `tools.lock`.
# usage: scripts/deploy.sh --stage direct [--azs 1] [--bump-worker-version] [--check-guard]
# Last line: DEPLOY-PASS stage=<s> digest=<12> sourceTree=<12> (exit 0),
#            DEPLOY-FAIL <step> <detail> (exit 1); bad flags or a dirty tree exit 2.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
exec "$py" "$here/lib/deploy.py" "$@"
