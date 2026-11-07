#!/usr/bin/env bash
# Worker image key guard. usage: scripts/image-key-guard.sh [--root DIR] [--staged] [--update]
# Recomputes the worker image key exactly as scripts/lib/digests.py does (the crate closure of agentd,
# the Cargo files, accounts.json, engine/ outside catalog/ and policy/, the image sources) and compares
# it with engine/policy/image-key.pin. Logic lives in scripts/lib/image_key_guard.py; CI_CARGO_BIN is
# its cargo test hook.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
exec "$py" "$here/lib/image_key_guard.py" "$@"
