#!/usr/bin/env bash
# containers-assets.sh <outdir>: stages the inputs of the in-session container runtime for the direct worker image. Run by
# images/codebuild/worker-python.yml before `docker build`, for LANG_FAMILY=direct only, from the repository root:
#   <outdir>/bin, <outdir>/lib   the podman-static binaries named in containers.lock, each checked against its SHA-256
#   <outdir>/images              one flattened root file system (xz) and the Docker image config (json) per `image` row, exported from the
#                                arm64 image pinned by digest (docker pull, docker export): the vfs storage driver copies every layer, and
#                                postgres:16 as 14 layers costs 3.8 GB of the VM's 8.8 GB disk, as one layer 0.48 GB
#   <outdir>/SHA256SUMS          every staged file
# Every download goes through fetch_pinned (images/tools/fetch.sh: 4 attempts, a hash that stays wrong exits 65). Prints
# "CONTAINERS-ASSETS OK <what>" lines, then CONTAINERS-ASSETS PASS.
set -euo pipefail
if [ $# -ne 1 ]; then
  echo "usage: containers-assets.sh <outdir>" >&2
  exit 2
fi
out=$1
here="$(cd "$(dirname "$0")" && pwd)"
lock="$here/containers.lock"
# shellcheck source=/dev/null
. "$here/../tools/fetch.sh"
arch=arm64
work=$(mktemp -d "${TMPDIR:-/tmp}/containers-assets.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$out/bin" "$out/lib" "$out/images"

read -r _ name _ version sha url < <(awk -v a="$arch" '$1 == "archive" && $3 == a { print; exit }' "$lock")
[ -n "${url:-}" ] || { echo "containers-assets.sh: no archive row for $arch in containers.lock" >&2; exit 1; }
rc=0
fetch_pinned "$url" "$sha" "$work/archive.tgz" || rc=$?
[ "$rc" -ne 65 ] || { echo "containers-assets.sh: SHA-256 mismatch for $url" >&2; exit 1; }
[ "$rc" -eq 0 ] || { echo "containers-assets.sh: download of $url failed (exit status $rc)" >&2; exit 1; }
echo "CONTAINERS-ASSETS OK archive $name $version"

# A file, not a pipe into `head`: under pipefail an early-closing reader makes tar exit 141 (SIGPIPE).
tar -tzf "$work/archive.tgz" >"$work/list"
top=$(head -n 1 "$work/list" | cut -d/ -f1)
while read -r _ dest _ path msha; do
  tar -xzf "$work/archive.tgz" -C "$work" "$top/$path"
  echo "$msha  $work/$top/$path" | sha256sum -c --status - || { echo "containers-assets.sh: SHA-256 mismatch for $path" >&2; exit 1; }
  install -m 0755 "$work/$top/$path" "$out/$dest"
  echo "CONTAINERS-ASSETS OK member $dest"
done < <(awk -v a="$arch" '$1 == "member" && $3 == a' "$lock")

while read -r _ ref _ source image_id; do
  base=$(printf '%s' "${ref##*/}" | tr ':' '-')
  docker pull --platform linux/arm64 "$source" >/dev/null
  got=$(docker image inspect "$source" --format '{{.Id}}')
  [ "$got" = "$image_id" ] || { echo "containers-assets.sh: $source has image id $got, the pin is $image_id" >&2; exit 1; }
  digests=$(docker image inspect "$source" --format '{{json .RepoDigests}}')
  grep -q "${source#*@}" <<<"$digests" || { echo "containers-assets.sh: $source was not pulled by the pinned digest" >&2; exit 1; }
  cid=$(docker create --platform linux/arm64 "$source")
  docker export "$cid" | xz -T0 -6 >"$out/images/$base.rootfs.tar.xz"
  docker rm "$cid" >/dev/null
  docker image inspect "$source" --format '{{json .Config}}' >"$out/images/$base.config.json"
  xz -t "$out/images/$base.rootfs.tar.xz"
  echo "CONTAINERS-ASSETS OK image $ref ($(du -m "$out/images/$base.rootfs.tar.xz" | cut -f1) MiB xz)"
done < <(awk -v a="$arch" '$1 == "image" && $3 == a' "$lock")

(cd "$out" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum >SHA256SUMS)
echo "CONTAINERS-ASSETS PASS"
