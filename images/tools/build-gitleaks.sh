#!/usr/bin/env bash
# Builds gitleaks from its pinned module version with the Go toolchain pinned in gitleaks.lock.
# usage: build-gitleaks.sh <out file> [arm64|amd64]        (default arm64, the architecture of the images)
# Why: gitleaks 8.30.1 is the newest release and its release binaries carry the standard library of Go 1.24.11 (a critical finding of the image scan,
# CVE-2025-68121, and 24 more). The same source built with the Go toolchain of gitleaks.lock does not. The Go archive is checked against its SHA-256 in
# gitleaks.lock before it runs; the module and every module it needs are checked against go.sum and the Go checksum database. The build is
# reproducible (-trimpath, no cgo, no VCS stamp), so the caller compares the SHA-256 this script prints with the pin it holds
# (images/codebuild/*.yml GITLEAKS_SHA256): a different value stops the build.
# Needs: the machine can reach go.dev, the Go module proxy (or GOPROXY=direct) and sum.golang.org. GITLEAKS_BUILD_DIR (default $TMPDIR/build-gitleaks)
# keeps the Go toolchain and its caches between runs; nothing is removed.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=fetch.sh
. "$here/fetch.sh"
lock="$here/gitleaks.lock"
read -r version module < <(awk '$1 == "gitleaks" && $3 == "module" {print $2, $4}' "$lock")
[ -n "${module:-}" ] || { echo "build-gitleaks: no gitleaks row in gitleaks.lock" >&2; exit 2; }
out="${1:?usage: build-gitleaks.sh <out file> [arm64|amd64]}"
# The script changes into its module directory below, so a relative output path is made absolute against the caller's directory first (the buildspecs pass build/gitleaks).
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
goarch="${2:-arm64}"
case "$goarch" in arm64|amd64) ;; *) echo "build-gitleaks: architecture is arm64 or amd64" >&2; exit 2 ;; esac
case "$(uname -m)" in aarch64|arm64) host=aarch64 ;; x86_64) host=x86_64 ;; *) echo "build-gitleaks: unknown machine $(uname -m)" >&2; exit 2 ;; esac
read -r gover gosha gourl < <(awk -v a="$host" '$1 == "go" && $3 == a {print $2, $4, $5}' "$lock")
[ -n "${gosha:-}" ] || { echo "build-gitleaks: no go row for $host in gitleaks.lock" >&2; exit 2; }
work="${GITLEAKS_BUILD_DIR:-${TMPDIR:?set TMPDIR or GITLEAKS_BUILD_DIR}/build-gitleaks}"
mkdir -p "$work"
if [ ! -x "$work/go-$gover/bin/go" ]; then
  fetch_pinned "$gourl" "$gosha" "$work/go-$gover.tar.gz"
  mkdir -p "$work/go-$gover.part"
  tar -xzf "$work/go-$gover.tar.gz" -C "$work/go-$gover.part" --strip-components=1
  mv "$work/go-$gover.part" "$work/go-$gover"
fi
export GOROOT="$work/go-$gover" GOTOOLCHAIN=local GOFLAGS=-trimpath CGO_ENABLED=0 GOOS=linux GOARCH="$goarch" \
  GOPATH="$work/gopath" GOCACHE="$work/gocache"
export PATH="$GOROOT/bin:$PATH"
mod="$work/mod-$version"
mkdir -p "$mod"
cd "$mod"
[ -f go.mod ] || go mod init gitleaks-build >/dev/null 2>&1
go get "$module@v$version" >&2
go build -buildvcs=false -ldflags "-s -w -X $module/version.Version=$version" -o "$out" "$module" >&2
chmod 0755 "$out"
sha256sum "$out" | awk '{print $1}'
