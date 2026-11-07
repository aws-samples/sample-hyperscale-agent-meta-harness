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
# The OpenCode binary is not in the direct image; the files the runtime needs are root-owned and not writable by others.
run --entrypoint sh "$img" -c '! test -e /opt/harness/bin/opencode && test "$(stat -c %U:%a /opt/harness/containers/bin/podman)" = root:755'
out=$(run --entrypoint /opt/harness/bin/ham-containers "$img" env); grep -q '^export DOCKER_HOST=unix:///var/run/docker.sock$' <<<"$out"
echo "CONTAINERS-CHECK PASS"
