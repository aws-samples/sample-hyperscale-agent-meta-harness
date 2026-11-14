#!/usr/bin/env bash
# Deploy the committed HEAD to one stage, stack by stack; the flow is in
# scripts/lib/deploy.py. Run on a Linux machine with the tools pinned in `tools.lock`.
# usage: scripts/deploy.sh --stage direct [--azs 1] [--bump-worker-version] [--check-guard] [--print-settings] [--fork-source auto|build|copy]
#        scripts/deploy.sh --stage direct --relabel
# Last line: DEPLOY-PASS stage=<s> digest=<12> sourceTree=<12> (exit 0),
#            DEPLOY-FAIL <step> <detail> (exit 1; `DEPLOY-STAGE unchanged` comes on the line before it when no stack had changed),
#            DEPLOY-REFUSED reason=<id> wait=<yes|no> stage=unchanged:: <action> (exit 3: a check refused before the first stack change);
#            bad flags, a stage setting its registry pattern does not take, or a dirty tree exit 2.
# --check-guard: DEPLOY-GUARD PASS stage=<s> running=0 (exit 0) or DEPLOY-FAIL guard... (exit 1); it deploys nothing.
# --print-settings: the DEPLOY-SETTING line of every stage setting (exit 0) or a refusal line (exit 3); it writes nothing.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
set +e
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
source_rc=$?
set -e
if [ "$source_rc" -ne 0 ]
then
  exit 2
fi
if ! py="$(ham_pick_python 11)"
then
  exit 2
fi
exec "$py" "$here/lib/deploy.py" "$@"
