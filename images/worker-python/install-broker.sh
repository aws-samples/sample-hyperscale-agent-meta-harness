#!/usr/bin/env bash
# Installs RabbitMQ for the direct worker image:
#   install-broker.sh <fetch.sh> <dest>
# Erlang/OTP comes from the pinned RabbitMQ-team RPM for Amazon Linux 2023 (dnf resolves its libraries), RabbitMQ from its generic-unix
# tarball unpacked at <dest> (sbin/, plugins/, ebin/). Every download is checked against its SHA-256 in broker.lock (fetch.sh fetch_pinned:
# 4 attempts, pauses of 5, 15 and 45 seconds; a hash that stays wrong exits 65 there and fails here). Root, at image build time.
# Prints "BROKER OK <tool> <version>" per tool, then BROKER PASS (exit 0).
set -euo pipefail
if [ $# -ne 2 ]; then
  echo "usage: install-broker.sh <fetch.sh> <dest>" >&2
  exit 2
fi
fetch_sh=$1 dest=$2
here="$(cd "$(dirname "$0")" && pwd)"
lock="$here/broker.lock"
# shellcheck source=/dev/null
. "$fetch_sh"
arch=$(uname -m)
work=$(mktemp -d "${TMPDIR:-/tmp}/broker.XXXXXX")
trap 'rm -rf "$work"' EXIT

# row <tool>: "<version> <sha256> <url>" for this architecture (or any).
row() { awk -v t="$1" -v a="$arch" '$1 == t && ($3 == a || $3 == "any") { print $2, $4, $5; exit }' "$lock"; }
# fetch <tool>: the checked download's path.
fetch() {
  local r v sha url f rc=0
  r=$(row "$1")
  [ -n "$r" ] || { echo "install-broker.sh: no $1 row for $arch in broker.lock" >&2; exit 1; }
  read -r v sha url <<<"$r"
  f="$work/$(basename "$url")"
  fetch_pinned "$url" "$sha" "$f" || rc=$?
  [ "$rc" -ne 65 ] || { echo "install-broker.sh: SHA-256 mismatch for $url" >&2; exit 1; }
  [ "$rc" -eq 0 ] || { echo "install-broker.sh: download of $url failed (exit status $rc)" >&2; exit 1; }
  echo "$sha  $f" | sha256sum -c --status - || { echo "install-broker.sh: SHA-256 mismatch for $url" >&2; exit 1; }
  echo "$f"
}
version() { row "$1" | cut -d' ' -f1; }

rpm_file=$(fetch erlang)
dnf install -y --setopt=install_weak_deps=False "$rpm_file" >/dev/null
dnf clean all >/dev/null
otp=$(erl -noshell -eval 'io:format("~s", [erlang:system_info(otp_release)]), halt().')
[ "$otp" = "$(version erlang | cut -d. -f1)" ] || { echo "install-broker.sh: OTP $otp is not the pinned $(version erlang)" >&2; exit 1; }
echo "BROKER OK erlang $(version erlang) (OTP $otp)"

mkdir -p "$dest"
tar -xf "$(fetch rabbitmq)" -C "$dest" --strip-components=1 --no-same-owner
chmod -R go-w "$dest"
want=$(version rabbitmq)
got=$(ls "$dest/plugins" | sed -n 's/^rabbit-\([0-9][0-9.]*\)\(\.ez\)\{0,1\}$/\1/p' | head -1)
[ "$got" = "$want" ] || { echo "install-broker.sh: the tarball holds rabbit $got, the pin is $want" >&2; exit 1; }
echo "BROKER OK rabbitmq $want"
echo "BROKER PASS"
