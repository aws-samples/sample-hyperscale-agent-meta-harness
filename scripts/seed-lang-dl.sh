#!/usr/bin/env bash
# seed-lang-dl.sh <lang.lock> <download cache dir>: put the pinned Maven archive into the language tools download cache from Maven Central.
# Why: a build host installs the language tools on a fresh machine (scripts/gates-lang-tools.sh), and the lock names archive.apache.org for Maven.
# That host can time out from a build network (curl exit status 28). Maven Central
# serves the same bytes (SHA-256 80ffca22... for 3.9.16, also dlcdn.apache.org and downloads.apache.org). images/tools/lang/install.sh uses a cache
# file named like the lock URL's last segment when its SHA-256 matches the lock, so a verified file here makes its own download unnecessary.
# The pin is never relaxed: the file takes its name only after its SHA-256 equals the lock's. Any failure leaves the cache as it was, prints a SEED
# line that says why and exits 0, so the installer's own download still runs. A failed transfer leaves its staging file (<name>.part.XXXXXX) in the cache.
# usage: seed-lang-dl.sh <path of lang.lock> <cache dir>      prints one SEED lang-dl line
set -uo pipefail
lock=${1:?usage: seed-lang-dl.sh <lang.lock> <cache dir>} cache=${2:?usage: seed-lang-dl.sh <lang.lock> <cache dir>}
read -r version sha url < <(awk '$1 == "maven" { print $2, $4, $5; exit }' "$lock" 2>/dev/null) || :
if [ -z "${version:-}" ] || [ -z "${sha:-}" ] || [ -z "${url:-}" ]; then
  echo "SEED lang-dl maven skipped: no maven row in $lock"
  exit 0
fi
mirror="https://repo.maven.apache.org/maven2/org/apache/maven/apache-maven/$version/apache-maven-$version-bin.tar.gz"
dest="$cache/$(basename "$url")"
mkdir -p "$cache" || { echo "SEED lang-dl maven $version skipped: cannot create $cache"; exit 0; }
if [ -f "$dest" ] && echo "$sha  $dest" | sha256sum -c --status -; then
  echo "SEED lang-dl maven $version already cached"
  exit 0
fi
part=$(mktemp "$dest.part.XXXXXX") || { echo "SEED lang-dl maven $version skipped: cannot create a staging file beside $dest"; exit 0; }
rc=0
curl -fsSL --retry 3 --retry-all-errors --connect-timeout 20 --max-time 180 -o "$part" "$mirror" || rc=$?
if [ "$rc" -ne 0 ]; then
  echo "SEED lang-dl maven $version skipped: transfer failed (curl exit status $rc)"
  exit 0
fi
if echo "$sha  $part" | sha256sum -c --status -; then
  chmod 0644 "$part" && mv -f "$part" "$dest" && echo "SEED lang-dl maven $version from repo.maven.apache.org"
else
  echo "SEED lang-dl maven $version skipped: SHA-256 of the mirror copy differs from the lock"
fi
exit 0
