#!/usr/bin/env bash
# Build the macOS client: `ham` of this checkout for aarch64-apple-darwin, cross-built on a Linux build host with cargo
# zigbuild (webpki roots, no Apple framework).
# usage: scripts/build-macos-driver.sh <out-dir>        (an absolute path)
#
# Runs `scripts/cargo-slot.sh cargo zigbuild --release --locked --target aarch64-apple-darwin -p ham --bin ham` (CARGO_TARGET_DIR
# default <repo>/target; log <out-dir>/build.log), checks that the binary is a 64-bit arm64 Mach-O executable (header cffaedfe
# 0c000001), copies it to <out-dir>/, and writes <out-dir>/SHA256SUMS (check it on the Mac with `shasum -a 256 -c SHA256SUMS`)
# and <out-dir>/build.json (target, commit, dirty, rust, zig, sha256).
# stdout, last line: "MACOS-DRIVER PASS dir=<out-dir> ham=<sha12> commit=<sha7>" (exit 0), or "MACOS-DRIVER FAIL <reason>"
# (exit 1); usage errors exit 2.
# Copy the directory to the Mac and run `./setup.sh --from <dir>`; setup checks SHA256SUMS before it installs the binary.
# Test hook: BUILD_MACOS_CARGO_SLOT (default scripts/cargo-slot.sh).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
target=aarch64-apple-darwin
[ $# -eq 1 ] || { echo "usage: build-macos-driver.sh <out-dir>" >&2; exit 2; }
out=$1
case "$out" in
  /*) ;;
  *) echo "usage: <out-dir> must be an absolute path, got $out" >&2; exit 2 ;;
esac
fail() { echo "MACOS-DRIVER FAIL $*"; exit 1; }
for tool in cargo cargo-zigbuild zig; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing-$tool"
done
slot="${BUILD_MACOS_CARGO_SLOT:-$here/cargo-slot.sh}"
mkdir -p "$out" || fail "out-dir $out"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
bindir="$CARGO_TARGET_DIR/$target/release"
rm -f "$bindir/ham"
start=$(date +%s)
(cd "$repo" && "$slot" cargo zigbuild --release --locked --target "$target" -p ham --bin ham) \
  >"$out/build.log" 2>&1
rc=$?
# nosemgrep: unquoted-command-substitution-in-command -- the date command prints digits only and the substitution sits inside an arithmetic expansion, where word splitting does not apply
secs=$(( $(date +%s) - start ))
echo "build-macos-driver: cargo zigbuild exit=$rc seconds=$secs log=$out/build.log" >&2
if [ $rc -ne 0 ]; then
  grep -E '^error' "$out/build.log" | head -5 | sed 's/^/build-macos-driver:   /' >&2
  fail "build exit=$rc (see $out/build.log)"
fi

# "ok" when the file is a 64-bit arm64 Mach-O executable.
macho_arm64() {
  [ -f "$1" ] || { echo missing; return; }
  local head
  head=$(od -An -tx1 -N8 "$1" | tr -d ' \n')
  if [ "$head" = cffaedfe0c000001 ]; then echo ok; else echo "not-arm64-macho:$head"; fi
}
state=$(macho_arm64 "$bindir/ham")
[ "$state" = ok ] || fail "ham $state"
cp "$bindir/ham" "$out/ham" || fail "copy ham"
chmod 0755 "$out/ham"
sum=$(sha256sum "$out/ham" | cut -d' ' -f1)
printf '%s  ham\n' "$sum" >"$out/SHA256SUMS"
commit=$(git -C "$repo" rev-parse --short=7 HEAD 2>/dev/null || echo unknown)
dirty=false
[ -n "$(git -C "$repo" status --porcelain 2>/dev/null)" ] && dirty=true
rust_v=$(rustc --version 2>/dev/null | awk '{print $2}')
zig_v=$(zig version 2>/dev/null)
printf '{"target": "%s", "commit": "%s", "dirty": %s, "rust": "%s", "zig": "%s", "seconds": %s, "sha256": {"ham": "%s"}}\n' \
  "$target" "$commit" "$dirty" "$rust_v" "$zig_v" "$secs" "$sum" >"$out/build.json"
echo "MACOS-DRIVER PASS dir=$out ham=${sum:0:12} commit=$commit"
