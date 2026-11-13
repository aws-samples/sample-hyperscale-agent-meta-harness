#!/usr/bin/env bash
# Build the language test container and install the language tool set for a
# Linux machine (TypeScript, Go and JVM profiles).
# usage: scripts/gates-lang-tools.sh    exit 0 with LANG-TOOLS PASS; 1 on a failed step
# - Image: images/lang-test/Dockerfile with images/services as the named build context
#   `services` (the native services), tagged hamh-gates-lang:<12 hex of the SHA-256 over the
#   Dockerfile, images/services/install.sh and services.lock> (built once per input).
# - Tools: images/tools/lang/install.sh <HAMH_TOOLS>/gate-tools-lang all, run inside the
#   image as the host uid (Node needs glibc 2.28); reinstalled when lang.lock or the JS package-lock
#   changes (the manifest records both SHA-256s). Downloads cache under <workspace tmp>/lang-dl.
# - Prints HARNESS_GATES_LANG_IMAGE=<tag> and HARNESS_GATES_LANG_TOOLS=<dir> for the tests that run the
#   language tool set (crates/agentd/tests/lock_turn.rs).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
tools_root="${HAMH_TOOLS:-${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/ham}"
tmp_root="${HAMH_TMP:-${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/ham/tmp}"
dest="${HARNESS_GATES_LANG_TOOLS:-$tools_root/gate-tools-lang}"
ctx="$repo/images/lang-test"
tools="$repo/images/tools"
lang="$tools/lang"
services="$repo/images/services"
tag="hamh-gates-lang:$(cat "$ctx/Dockerfile" "$services/install.sh" "$services/services.lock" | sha256sum | cut -c1-12)"
if ! docker image inspect "$tag" >/dev/null 2>&1; then
  DOCKER_BUILDKIT=1 docker build -q --build-context "services=$services" -t "$tag" "$ctx" >/dev/null
fi
want_lang=$(sha256sum "$lang/lang.lock" | cut -d' ' -f1)
want_js=$(sha256sum "$lang/js/package-lock.json" | cut -d' ' -f1)
have=""
[ -f "$dest/manifest.json" ] && have=$(tr -d ' \n' <"$dest/manifest.json")
if [[ "$have" != *"\"langLockSha256\":\"$want_lang\""* || "$have" != *"\"jsLockSha256\":\"$want_js\""* ]]; then
  mkdir -p "$(dirname "$dest")" "$tmp_root/lang-dl" "$tmp_root/tmpdir"
  # Maven comes from Maven Central when the archive.apache.org copy is out of reach (seed-lang-dl.sh; the SHA-256 pin stays); a failed seed leaves the installer to download.
  "$here/seed-lang-dl.sh" "$lang/lang.lock" "$tmp_root/lang-dl" || true
  docker run --rm --user "$(id -u):$(id -g)" \
    -v "$tools:$tools:ro" -v "$(dirname "$dest"):$(dirname "$dest")" \
    -v "$tmp_root:$tmp_root" -e TMPDIR="$tmp_root/tmpdir" \
    "$tag" bash "$lang/install.sh" "$dest" all "$tmp_root/lang-dl"
fi
echo "HARNESS_GATES_LANG_IMAGE=$tag"
echo "HARNESS_GATES_LANG_TOOLS=$dest"
echo "LANG-TOOLS PASS"
