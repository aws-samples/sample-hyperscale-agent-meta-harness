#!/usr/bin/env bash
# Build the terminal client and the operator tool beside it for every platform of the table in scripts/setup/names.sh and publish them to the user's own
# setup bucket. The runner of the setup runs this after the backend deploy; the user's setup.sh later downloads the
# files with `aws s3 cp` and checks them against the SHA-256 in the result manifest.
#
# usage: scripts/setup/build-client.sh <out-dir> <bucket> <build-id>
#   <out-dir>   an absolute path; <out-dir>/<platform>/<client> and <out-dir>/<platform>/<admin> are the binaries, <out-dir>/SHA256SUMS lists them as
#               "<sha256>  <platform>/<client>" and "<sha256>  <platform>/<admin>" (check with `cd <out-dir> && sha256sum -c SHA256SUMS`),
#               <out-dir>/<platform>.build.log is the log of each cargo run and <out-dir>/client.json repeats the JSON of stdout
#   <bucket>    the setup bucket (hamh-setup-<account>-<region>); an S3 bucket name
#   <build-id>  the CodeBuild build id of the runner (it may hold a colon) or any id of letters, digits, colon, dot, underscore and dash
#
# For each platform, in table order: `cargo zigbuild --release --locked --target <target> -p <client> --bin <client> -p <admin> --bin <admin>` through
# cargo-slot.sh (<client> is $HAMH_CLIENT_BIN and <admin> is $HAMH_ADMIN_BIN from names.sh; one cargo run builds both, so the shared crates are compiled
# once; CARGO_TARGET_DIR defaults to <repo>/target; the environment, HAMH_ACCOUNTS_FILE among it, reaches cargo unchanged), then client_check.py on each
# file: a signed thin arm64 Mach-O for darwin-arm64 (Apple Silicon refuses an unsigned one), a static ELF of the right machine for the Linux rows. Only when
# every binary of every platform built and passed does anything leave the machine: `aws s3 cp` puts each binary at
# s3://<bucket>/client/<build-id>/<platform>/<client> (and .../<admin>) and SHA256SUMS at s3://<bucket>/client/<build-id>/SHA256SUMS, last. The AWS CLI is
# used with its environment (profile, Region, credentials) and nothing else; the script reads no credential.
#
# stdout: ONE line of JSON, {"<platform>": {"key": "client/<build-id>/<platform>/<client>", "sha256": "<hex>", "bytes": <n>, "admin": {"key":
# "client/<build-id>/<platform>/<admin>", "sha256": "<hex>", "bytes": <n>}}, ...}, in table order; nothing else, and nothing at all on a failure. stderr: progress lines `build-client: ...`, then the last line `BUILD-CLIENT PASS platforms=<n> bucket=<bucket>
# build=<build-id>` (exit 0) or `BUILD-CLIENT FAIL <platform> <reason>` / `BUILD-CLIENT FAIL <reason>` (exit 1). Reasons: `missing-<tool>` (cargo,
# cargo-zigbuild, zig, aws, python, sha256sum; checked before any build), `build exit=<n> (see <log>)`, `missing-binary <path>`, `<code>: <reason>` of the
# check, `upload <key> (aws s3 cp exit=<n>)`; a reason about the operator tool starts with its name (`<platform> <admin> missing-binary <path>`). Exit 2: a usage error (the line starts `usage:`), before any build.
# A platform that cannot be built is a row removed from the table in names.sh with its reason; this script never skips one.
# Cargo slots: CARGO_SLOT_DIR, else $HAMH_TOOLS/locks, else <out-dir>/.cargo-slots. Test hooks: BUILD_CLIENT_CARGO_SLOT (default scripts/cargo-slot.sh), HAMH_CLIENT_PLATFORM_TABLE and HAMH_CLIENT_BIN (names.sh), HAMH_PYTHON (lib/python.sh).
# Bash 3.2 syntax.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
. "$here/names.sh" || { echo "usage: names.sh refused its settings (see above)" >&2; exit 2; }
. "$repo/scripts/lib/python.sh" 2>/dev/null
export AWS_PAGER=""

fail() { echo "BUILD-CLIENT FAIL $*" >&2; exit 1; }
usage() {
  echo "usage: build-client.sh <out-dir> <bucket> <build-id>${1:+ ($1)}" >&2
  exit 2
}

[ $# -eq 3 ] || usage "three arguments"
out="$1"
bucket="$2"
build_id="$3"
case $out in
  /*) ;;
  *) usage "<out-dir> must be an absolute path, got $out" ;;
esac
bucket_ok() { # an S3 bucket name: 3 to 63 of a-z 0-9 . -, starting and ending with a letter or digit, no ".."
  [ "${#1}" -ge 3 ] && [ "${#1}" -le 63 ] || return 1
  case $1 in
    *[!a-z0-9.-]*|[.-]*|*[.-]|*..*) return 1 ;;
  esac
  return 0
}
build_id_ok() { # 1 to 128 of A-Za-z0-9 : . _ -, starting with a letter or digit, no ".."
  [ -n "$1" ] && [ "${#1}" -le 128 ] || return 1
  case $1 in
    *[!A-Za-z0-9:._-]*|[:._-]*|*..*) return 1 ;;
  esac
  return 0
}
bucket_ok "$bucket" || usage "<bucket> must be an S3 bucket name, got $bucket"
build_id_ok "$build_id" || usage "<build-id> must be letters, digits, colon, dot, underscore and dash, got $build_id"

platform_list=$(ham_client_platforms) || usage "the platform table of names.sh is malformed"
platforms=()
while IFS= read -r line; do
  if [ -n "$line" ]; then platforms+=("$line"); fi
done <<< "$platform_list"
[ "${#platforms[@]}" -gt 0 ] || usage "the platform table of names.sh has no row"

for tool in cargo cargo-zigbuild zig aws; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing-$tool"
done
[ -n "${py:-}" ] || fail "missing-python"
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "missing-sha256sum"
fi
slot="${BUILD_CLIENT_CARGO_SLOT:-$repo/scripts/cargo-slot.sh}"
mkdir -p "$out" || fail "out-dir $out"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
# cargo-slot.sh keeps its lock folder in CARGO_SLOT_DIR, else below HAMH_TOOLS. With neither set, the slots go beside the output.
if [ -z "${CARGO_SLOT_DIR:-}" ] && [ -z "${HAMH_TOOLS:-}" ]; then
  export CARGO_SLOT_DIR="$out/.cargo-slots"
fi
echo "build-client: rust=$(rustc --version 2>/dev/null | cut -d' ' -f2) zig=$(zig version 2>/dev/null) client=$HAMH_CLIENT_BIN admin=$HAMH_ADMIN_BIN platforms=${platforms[*]}" >&2

: > "$out/SHA256SUMS"
json=""
sep=""
for platform in "${platforms[@]}"; do
  target=$(ham_client_target "$platform") || fail "$platform table"
  kind=$(ham_client_kind "$platform") || fail "$platform table"
  log="$out/$platform.build.log"
  start=$(date +%s)
  (cd "$repo" && "$slot" cargo zigbuild --release --locked --target "$target" -p "$HAMH_CLIENT_BIN" --bin "$HAMH_CLIENT_BIN" -p "$HAMH_ADMIN_BIN" --bin "$HAMH_ADMIN_BIN") >"$log" 2>&1
  rc=$?
  now=$(date +%s)
  secs=$((now - start))
  echo "build-client: platform=$platform target=$target cargo zigbuild exit=$rc seconds=$secs log=$log" >&2
  if [ "$rc" -ne 0 ]; then
    grep -E '^error' "$log" | head -5 | sed 's/^/build-client:   /' >&2
    fail "$platform build exit=$rc (see $log)"
  fi
  mkdir -p "$out/$platform" || fail "$platform out-dir $out/$platform"
  entry=""
  for name in "$HAMH_CLIENT_BIN" "$HAMH_ADMIN_BIN"; do
    # the reasons about the operator tool start with its name, so a log line says which of the two files failed
    who=""
    [ "$name" = "$HAMH_CLIENT_BIN" ] || who="$name "
    built="$CARGO_TARGET_DIR/$target/release/$name"
    [ -f "$built" ] || fail "$platform ${who}missing-binary $built"
    verdict=$("$py" -I "$here/client_check.py" "$kind" "$built")
    vrc=$?
    if [ "$vrc" -ne 0 ]; then
      [ -n "$verdict" ] || verdict="FAIL check-error: client_check.py exited $vrc and printed nothing"
      echo "build-client:   ${verdict#FAIL }" >&2
      fail "$platform ${who}${verdict#FAIL }"
    fi
    echo "build-client: platform=$platform $name ${verdict}" >&2
    cp "$built" "$out/$platform/$name" || fail "$platform ${who}copy $built"
    chmod 0755 "$out/$platform/$name"
    sum=$(sha256_of "$out/$platform/$name")
    bytes=$(wc -c < "$out/$platform/$name" | tr -d ' ')
    printf '%s  %s/%s\n' "$sum" "$platform" "$name" >> "$out/SHA256SUMS"
    member="\"key\":\"client/$build_id/$platform/$name\",\"sha256\":\"$sum\",\"bytes\":$bytes"
    if [ "$name" = "$HAMH_CLIENT_BIN" ]; then entry="$member"; else entry="$entry,\"admin\":{$member}"; fi
  done
  json="$json$sep\"$platform\":{$entry}"
  sep=","
done
json="{$json}"
printf '%s\n' "$json" > "$out/client.json"

# Every binary of every platform was built and passed its check: now, and only now, publish. SHA256SUMS goes last, so a reader that finds it finds every binary.
prefix="s3://$bucket/client/$build_id"
upload() { # upload <file> <key relative to client/<build-id>>
  local msg urc
  msg=$(aws s3 cp "$1" "$prefix/$2" --only-show-errors 2>&1)
  urc=$?
  if [ "$urc" -ne 0 ]; then
    printf '%s\n' "$msg" | sed 's/^/build-client:   /' >&2
    fail "upload client/$build_id/$2 (aws s3 cp exit=$urc)"
  fi
  echo "build-client: uploaded client/$build_id/$2" >&2
}
for platform in "${platforms[@]}"; do
  upload "$out/$platform/$HAMH_CLIENT_BIN" "$platform/$HAMH_CLIENT_BIN"
  upload "$out/$platform/$HAMH_ADMIN_BIN" "$platform/$HAMH_ADMIN_BIN"
done
upload "$out/SHA256SUMS" "SHA256SUMS"

printf '%s\n' "$json"
echo "BUILD-CLIENT PASS platforms=${#platforms[@]} bucket=$bucket build=$build_id" >&2
