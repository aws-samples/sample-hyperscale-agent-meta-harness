#!/usr/bin/env bash
# Install the Lane A native services into this Amazon
# Linux 2023 image or container: PostgreSQL 16, MariaDB 11.4 (for MySQL and MariaDB services),
# Valkey 9 (for Redis and Valkey) and mailpit, at the paths the removed harness-gates `--services native` started them from;
# ham-services (images/worker-python/ham-services.sh) starts PostgreSQL from here.
#   install.sh [<download cache dir>]
# The server packages of services.lock install without their dependency closure (`rpm --nodeps
# --noscripts`): systemd, perl, the packages' users and units are not needed to run the servers as
# processes; the shared libraries they link come from the lock's lib rows through dnf. Everything
# the native start does not run is then removed, every kept binary must resolve all its libraries,
# mailpit is checked against its SHA-256, and /opt/harness/services/manifest.json records the
# versions. Prints "SERVICES OK <name> <version>" per service and SERVICES PASS (exit 0); a failed
# step exits 1, usage errors exit 2. Run as root; needs dnf, rpm, curl, tar and sha256sum. The two dnf runs and the mailpit
# download are run again after a failure (4 attempts, pauses of 5, 15 and 45 seconds; the block below is a copy of the one in
# images/tools/fetch.sh, because this directory is bind-mounted alone), and the pin never changes.
set -euo pipefail
if [ $# -gt 1 ]; then echo "usage: install.sh [<download cache dir>]" >&2; exit 2; fi
# >>> retry-lib (images/services/install.sh carries a copy of this block; scripts/tests/test_fetch_retry.py keeps them equal)
retry_run() { # <label> <command> [<arg>...]
  local label=${1-} attempts="${HAMH_RETRY_ATTEMPTS:-4}" pauses="${HAMH_RETRY_PAUSES-5 15 45}" n=1 rc=0 i p pause
  local num='[0-9]+(\.[0-9]+)?' re IFS=$' \t\n'
  [ $# -ge 2 ] || { echo "usage: retry_run <label> <command> [<arg>...]" >&2; return 2; }
  shift
  [[ $attempts =~ ^[1-9][0-9]*$ ]] || { echo "retry: HAMH_RETRY_ATTEMPTS must be a positive integer, got '$attempts'" >&2; return 2; }
  re="^$num( $num)*\$"
  [[ $pauses =~ $re ]] || { echo "retry: HAMH_RETRY_PAUSES must be seconds separated by spaces, got '$pauses'" >&2; return 2; }
  while :; do
    rc=0
    "$@" || rc=$?
    [ "$rc" -ne 0 ] || return 0
    if [ "$n" -ge "$attempts" ]; then
      echo "retry: $label failed after $n attempts (last exit status $rc)" >&2
      return "$rc"
    fi
    i=0
    pause=
    for p in $pauses; do
      pause=$p
      i=$((i + 1))
      [ "$i" -lt "$n" ] || break
    done
    echo "retry: $label attempt $n of $attempts failed (exit status $rc); retrying in ${pause}s" >&2
    sleep "$pause" || :
    n=$((n + 1))
  done
}
_fetch_pinned_once() { # <url> <sha256> <dest>
  local url=$1 want=$2 dest=$3 part meta code size type got rc=0 fmt='%{http_code} %{size_download} %{content_type}'
  # A staging file of its own, so two installers sharing a download cache cannot write one file.
  part=$(mktemp "$dest.part.XXXXXX") || { echo "fetch: cannot create a staging file beside $dest" >&2; return 1; }
  meta=$(curl -fsSL --retry 5 --retry-all-errors --connect-timeout 30 --speed-limit 1024 --speed-time 60 -w "$fmt" -o "$part" "$url") || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "fetch: transfer of $url failed (curl exit status $rc)" >&2
    rm -f "$part"
    return "$rc"
  fi
  got=$(sha256sum "$part") || { echo "fetch: cannot checksum the download of $url" >&2; rm -f "$part"; return 1; }
  got=${got%% *}
  if [ "$got" != "$want" ]; then
    read -r code size type <<<"$meta" || :
    echo "fetch: SHA-256 of $url is $got, wanted $want (HTTP ${code:-?}, ${size:-?} bytes, ${type:-no content type})" >&2
    rm -f "$part"
    return 65
  fi
  chmod 0644 "$part" && mv -f "$part" "$dest"
}
fetch_pinned() { # <url> <sha256> <dest>
  local want
  [ $# -eq 3 ] || { echo "usage: fetch_pinned <url> <sha256> <dest>" >&2; return 2; }
  want=${2,,}
  [[ $want =~ ^[0-9a-f]{64}$ ]] || { echo "fetch: not a SHA-256: '$2'" >&2; return 2; }
  retry_run "download of ${1##*/}" _fetch_pinned_once "$1" "$want" "$3"
}
# <<< retry-lib
here="$(cd "$(dirname "$0")" && pwd)"
lock="$here/services.lock"
arch=$(uname -m)
case "$arch" in x86_64 | aarch64) ;; *) echo "install.sh: unsupported architecture $arch" >&2; exit 2 ;; esac
work=$(mktemp -d "${TMPDIR:-/tmp}/services.XXXXXX")
trap 'rm -rf "$work"' EXIT
cache=${1:-$work/dl}
mkdir -p "$cache" "$work/rpms"
dest=/opt/harness/services

rows() { awk -v k="$1" '$1 == k { print $2, $3 }' "$lock"; }
mapfile -t rpms < <(rows rpm | awk '{ print $1 "-" $2 }')
mapfile -t libs < <(rows lib | awk '{ print $1 }')
[ ${#rpms[@]} -gt 0 ] || { echo "install.sh: no rpm rows in $lock" >&2; exit 1; }

retry_run "dnf install libraries" dnf install -y -q --setopt=install_weak_deps=False --setopt=tsflags=nodocs "${libs[@]}" >/dev/null
retry_run "dnf download packages" dnf install -y -q --downloadonly --downloaddir "$work/rpms" --setopt=install_weak_deps=False "${rpms[@]}" >/dev/null
files=()
for r in "${rpms[@]}"; do
  f=$(ls "$work/rpms/$r".*.rpm 2>/dev/null | head -1)
  [ -n "$f" ] || { echo "install.sh: $r was not downloaded" >&2; exit 1; }
  files+=("$f")
done
# The packages' own users are not created (--noscripts); rpm says so for each file it gives root.
rpm -i --nodeps --noscripts --excludedocs "${files[@]}" 2>&1 | grep -v -E "^warning: (user|group) [a-z]+ does not exist - using root$" || true
for r in "${rpms[@]}"; do rpm -q --quiet "$r" || { echo "install.sh: $r did not install" >&2; exit 1; }; done
dnf clean all >/dev/null && rm -rf /var/cache/dnf

# Keep the programs the native start runs (images/worker-python/ham-services.sh starts PostgreSQL; the removed harness-gates started the rest) and the ones
# mariadb-install-db calls; the packages' other programs, units, plugins and non-English messages go.
keep=(
  /usr/bin/postgres /usr/bin/initdb /usr/bin/psql /usr/bin/pg_isready
  /usr/libexec/mariadbd /usr/sbin/mariadbd /usr/bin/mariadb-install-db /usr/bin/mysql_install_db
  /usr/bin/my_print_defaults /usr/bin/resolveip /usr/bin/valkey-server
)
kept() { local f; for f in "${keep[@]}"; do [ "$1" = "$f" ] && return 0; done; return 1; }
for r in "${rpms[@]}"; do
  rpm -ql "${r%-*-*}" | while read -r f; do
    case "$f" in
      /usr/bin/* | /usr/sbin/* | /usr/libexec/* | /usr/lib/systemd/* | /usr/lib/tmpfiles.d/* | /usr/lib/sysusers.d/* | /etc/logrotate.d/*)
        if [ -f "$f" ] && ! kept "$f"; then rm -f "$f"; fi ;;
    esac
  done
done
rm -rf /usr/lib64/mariadb/plugin
find /usr/share/mariadb -mindepth 1 -maxdepth 1 -type d ! -name english ! -name charsets -exec rm -rf {} +

# mailpit from its release asset.
row=$(awk -v a="$arch" '$1 == "mailpit" && $3 == a { print $2, $4, $5; exit }' "$lock")
read -r mp_version mp_sha mp_url <<<"$row"
[ -n "$mp_url" ] || { echo "install.sh: no mailpit row for $arch" >&2; exit 1; }
tgz="$cache/$(basename "$mp_url")"
if [ ! -f "$tgz" ] || ! echo "$mp_sha  $tgz" | sha256sum -c --status -; then
  rc=0
  fetch_pinned "$mp_url" "$mp_sha" "$tgz" || rc=$?
  [ "$rc" -ne 65 ] || { echo "install.sh: SHA-256 mismatch for $mp_url" >&2; exit 1; }
  [ "$rc" -eq 0 ] || { echo "install.sh: download of $mp_url failed (exit status $rc)" >&2; exit 1; }
fi
echo "$mp_sha  $tgz" | sha256sum -c --status - || { echo "install.sh: SHA-256 mismatch for $mp_url" >&2; exit 1; }
mkdir -p "$dest/bin"
tar -xzf "$tgz" -C "$work" mailpit
bin_sha=$(awk -v a="$arch" '$1 == "mailpit-bin" && $3 == a { print $4; exit }' "$lock")
echo "$bin_sha  $work/mailpit" | sha256sum -c --status - || { echo "install.sh: the mailpit binary is not the locked build" >&2; exit 1; }
install -m 0755 "$work/mailpit" "$dest/bin/mailpit"

for b in /usr/bin/postgres /usr/bin/initdb /usr/bin/psql /usr/libexec/mariadbd /usr/bin/my_print_defaults /usr/bin/valkey-server /usr/lib64/pgsql/plpgsql.so; do
  [ -e "$b" ] || { echo "install.sh: $b is missing" >&2; exit 1; }
  if ldd "$b" 2>&1 | grep -q 'not found'; then
    echo "install.sh: $b does not resolve: $(ldd "$b" | grep 'not found' | tr '\n' ' ')" >&2; exit 1
  fi
done
pg=$(/usr/bin/postgres --version | awk '{ print $3 }')
maria=$(/usr/libexec/mariadbd --version | grep -o '[0-9]*\.[0-9]*\.[0-9]*-MariaDB' | head -1)
valkey=$(/usr/bin/valkey-server --version | grep -o 'v=[0-9.]*' | cut -c3-)
# `mailpit version` asks GitHub for updates, so the pin is the binary's SHA-256 above.
"$dest/bin/mailpit" --help >/dev/null
printf '{"arch":"%s","postgres":"%s","mariadb":"%s","valkey":"%s","mailpit":"%s","lockSha256":"%s"}\n' \
  "$arch" "$pg" "$maria" "$valkey" "$mp_version" "$(sha256sum "$lock" | cut -d' ' -f1)" > "$dest/manifest.json"
echo "SERVICES OK postgres $pg"
echo "SERVICES OK mariadb $maria"
echo "SERVICES OK valkey $valkey"
echo "SERVICES OK mailpit $mp_version"
echo "SERVICES PASS"
