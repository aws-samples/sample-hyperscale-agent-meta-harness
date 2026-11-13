#!/usr/bin/env bash
# Fill and audit the Go module store: the approved, create-only GOPROXY tree in the S3 bucket
# hamh-<stage>-gomod-<account> that the workers read for Go modules. Run on a Linux machine with the tools pinned in `tools.lock`.
# usage: scripts/gomod-store.sh fill  --stage live|drill|direct --modules <list file> [--approved FILE] [--out DIR] [--jobs N] [--dry-run]
#                                     [--go PATH] [--bucket NAME] [--no-assume-role] [--reuse-current]
#        scripts/gomod-store.sh check --stage live|drill|direct [--manifest FILE] [--bucket NAME] [--no-assume-role] [--quick]
#
# fill resolves each `module@version` of the list with real Go against the checksum database (GOPROXY=direct), proves the staged tree OFFLINE with the lock step's environment, then uploads create-only
# as the role hamh-<stage>-gomod-filler and writes a manifest. A module not on the approved list
# (engine/rules/deps/go-approved-modules.txt) is refused, exit 2, before any download. Last line:
#   GOMOD-FILL PASS stage=<s> roots=<n> modules=<n> objects=<n> put=<n> exists=<n> bucket=<b> manifest=<key>   (exit 0)
#   GOMOD-FILL CURRENT stage=<s> roots=<n> modules=<n> objects=<n> bucket=<b> manifest=<key>                    (exit 0; --reuse-current found the store current: nothing resolved or uploaded)
#   GOMOD-FILL DRY-RUN ... (exit 0; no AWS call)
#   GOMOD-FILL FAIL <reason>                                                                                    (exit 1)
# check: GOMOD-CHECK PASS|FAIL (manifest objects present with their SHA-256; every index names only complete versions).
# check --quick: GOMOD-CHECK PASS... quick=nonempty when the bucket holds an object, GOMOD-CHECK FAIL empty... with the fill command when it holds none.
# Usage errors and refusals exit 2 before any call. Scratch goes under $TMPDIR.
# Logic, flags and test hooks: lib/gomod_store.py.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
# The selected accounts.json entry: its profile, never the instance profile of the machine that runs this script.
# shellcheck source=lib/aws-target.sh
. "$here/lib/aws-target.sh"
exec "$py" "$here/lib/gomod_store.py" "$@"
