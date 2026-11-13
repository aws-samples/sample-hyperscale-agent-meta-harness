#!/usr/bin/env bash
# containers-check.sh <image>: the image check of the in-session container runtime, run by images/codebuild/worker-python.yml on the built
# direct image: every binary in /opt/harness/containers has the SHA-256 that containers.lock pins, the static binaries run, the offline image
# files are there and the helper scripts are in place. Prints "CONTAINERS-CHECK OK <what>" lines, then CONTAINERS-CHECK PASS.
set -euo pipefail
if [ $# -ne 1 ]; then
  echo "usage: containers-check.sh <image>" >&2
  exit 2
fi
img=$1
here="$(cd "$(dirname "$0")" && pwd)"
lock="$here/containers.lock"
run() { docker run --rm "$@"; }
while read -r _ rel _ _ sha; do
  got=$(run --entrypoint sha256sum "$img" "/opt/harness/containers/$rel" | cut -d' ' -f1)
  [ "$got" = "$sha" ] || { echo "containers-check.sh: /opt/harness/containers/$rel has SHA-256 $got, the pin is $sha" >&2; exit 1; }
  echo "CONTAINERS-CHECK OK /opt/harness/containers/$rel $sha"
done < <(awk '$1 == "member" && $3 == "arm64"' "$lock")
# The output goes to a variable first: under pipefail a reader that exits early (grep -q) can make the writer exit 141.
out=$(run --entrypoint /opt/harness/containers/bin/podman "$img" --version); grep -q 'podman version 6\.1\.4' <<<"$out"
out=$(run --entrypoint /opt/harness/containers/bin/crun "$img" --version); grep -q 'crun version' <<<"$out"
out=$(run --entrypoint /opt/harness/containers/lib/netavark "$img" --version); grep -q netavark <<<"$out"
run --entrypoint sh "$img" -c 'test -x /opt/harness/bin/ham-containers && test -x /opt/harness/bin/docker && test -x /opt/harness/containers/lib/pasta && test -s /opt/harness/containers/images/postgres-16.rootfs.tar.xz && test -s /opt/harness/containers/images/postgres-16.config.json && test -s /etc/containers/policy.json'
run --entrypoint /usr/bin/python3.12 "$img" -c 'import json; json.load(open("/opt/harness/containers/manifest.json"))'
# The direct image carries both engines. OpenCode's binary is present, root-owned, mode 755 (not writable by others) and has the pinned hash: OPENCODE_SHA256 (the build
# spec's variable) or, when that is unset, binarySha256 of [opencode] in engine/lock.toml. The files the container runtime needs are root-owned and not writable by others too.
opencode_pin=${OPENCODE_SHA256:-}
if [ -z "$opencode_pin" ]; then
  opencode_pin=$(awk '/^\[/ {section = $1} section == "[opencode]" && $1 == "binarySha256" {gsub(/"/, "", $3); print $3}' "$here/../../engine/lock.toml")
fi
[[ $opencode_pin =~ ^[0-9a-f]{64}$ ]] || { echo "containers-check.sh: no OpenCode pin (set OPENCODE_SHA256, or fix binarySha256 of [opencode] in engine/lock.toml)" >&2; exit 1; }
run --entrypoint sh "$img" -c 'test -x /opt/harness/bin/opencode && test "$(stat -c %U:%a /opt/harness/bin/opencode)" = root:755' \
  || { echo "containers-check.sh: /opt/harness/bin/opencode is missing from the image, or is not root-owned with mode 755" >&2; exit 1; }
got=$(run --entrypoint sha256sum "$img" /opt/harness/bin/opencode | cut -d' ' -f1)
[ "$got" = "$opencode_pin" ] || { echo "containers-check.sh: /opt/harness/bin/opencode has SHA-256 $got, the pin is $opencode_pin" >&2; exit 1; }
echo "CONTAINERS-CHECK OK /opt/harness/bin/opencode $got"
run --entrypoint sh "$img" -c 'test "$(stat -c %U:%a /opt/harness/containers/bin/podman)" = root:755'
out=$(run --entrypoint /opt/harness/bin/ham-containers "$img" env); grep -q '^export DOCKER_HOST=unix:///var/run/docker.sock$' <<<"$out"
echo "CONTAINERS-CHECK PASS"
