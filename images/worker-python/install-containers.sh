#!/usr/bin/env bash
# install-containers.sh <staged> <dest>: installs the in-session container runtime in the direct worker image, at image build time,
# as root, from the Dockerfile (LANG_FAMILY=direct only):
#   <staged>   what containers-assets.sh staged (bind-mounted, so no layer keeps a second copy)
#   <dest>     /opt/harness/containers: bin/ and lib/ (binaries), etc/ (containers.conf, storage.conf, registries.conf), images/ (the offline
#              images and images.json), manifest.json (SHA-256 of every installed file)
# Every binary is checked against containers.lock again before it is installed. Also installs /etc/containers/policy.json (the image
# tools read it from there) and /opt/harness/bin/docker (Podman's client on the session socket). Prints CONTAINERS OK lines, then CONTAINERS PASS.
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: install-containers.sh <staged> <dest>" >&2
  exit 2
fi
staged=$1 dest=$2
here="$(cd "$(dirname "$0")" && pwd)"
lock="$here/containers.lock"
arch=arm64

install -d -m 0755 "$dest" "$dest/bin" "$dest/lib" "$dest/etc" "$dest/images" /etc/containers /opt/harness/bin
while read -r _ rel _ _ sha; do
  echo "$sha  $staged/$rel" | sha256sum -c --status - || { echo "install-containers.sh: SHA-256 mismatch for $rel" >&2; exit 1; }
  install -m 0755 "$staged/$rel" "$dest/$rel"
  echo "CONTAINERS OK $rel"
done < <(awk -v a="$arch" '$1 == "member" && $3 == a' "$lock")
install -m 0755 "$here/containers/lib/pasta" "$dest/lib/pasta"
for f in containers.conf storage.conf registries.conf; do install -m 0644 "$here/containers/etc/$f" "$dest/etc/$f"; done
install -m 0644 "$here/containers/etc/policy.json" /etc/containers/policy.json
install -m 0644 "$here/containers/images.json" "$dest/images/images.json"
while read -r ref; do
  base=$(printf '%s' "${ref##*/}" | tr ':' '-')
  for f in "$base.rootfs.tar.xz" "$base.config.json"; do
    [ -s "$staged/images/$f" ] || { echo "install-containers.sh: $staged/images/$f is missing" >&2; exit 1; }
    install -m 0644 "$staged/images/$f" "$dest/images/$f"
  done
  echo "CONTAINERS OK image $ref"
done < <(awk -v a="$arch" '$1 == "image" && $3 == a { print $2 }' "$lock")
install -m 0755 "$here/docker" /opt/harness/bin/docker
(cd "$dest" && find . -type f ! -name manifest.json -print0 | sort -z | xargs -0 sha256sum | awk '{ printf "%s%s", sep, "\"" $2 "\": \"" $1 "\""; sep = ", " } BEGIN { printf "{" } END { print "}" }' >manifest.json)
chmod -R go-w "$dest"
echo "CONTAINERS PASS"
