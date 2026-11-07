#!/usr/bin/env bash
# rust-check.sh <image> <rust version>   |   rust-check.sh --run <rust version>: the compile-and-run check of the Rust toolchain in the worker image.
# rustc links through `cc` and cargo needs the standard library, so a toolchain that prints its version can still fail to build anything.
# images/codebuild/worker-python.yml runs `rust-check.sh <image> <version>` on the built image BEFORE the push: a missing linker, C library file or standard
# library fails the build, not a run. <image> starts a throwaway container as the engine user (agent) that runs this file with --run. --run takes the
# engine's PATH from AGENTD_ENGINE_PATH (the image's own ENV), checks the versions, the sysroot and the linker, then builds, runs, tests, lints and format-checks a
# crate with no dependency in a fresh mktemp directory, offline (about 10 seconds). Prints RUST-CHECK OK <step> lines, then RUST-CHECK PASS (exit 0), or
# RUST-CHECK FAIL <step> (exit 1); a bad call exits 2. Nothing is deleted: the scratch directory goes with the throwaway container.
set -uo pipefail
usage() {
  echo "usage: rust-check.sh <image> <rust version>  |  rust-check.sh --run <rust version>" >&2
  exit 2
}
if [ "${1:-}" = --run ]; then
  [ $# -eq 2 ] || usage
  version=$2
else
  [ $# -eq 2 ] || usage
  here="$(cd "$(dirname "$0")" && pwd)"
  exec docker run --rm --user agent -v "$here/rust-check.sh:/rust-check.sh:ro" --entrypoint bash "$1" /rust-check.sh --run "$2"
fi

fail() {
  echo "RUST-CHECK FAIL $1"
  exit 1
}
ok() { echo "RUST-CHECK OK $1"; }
[ -n "${AGENTD_ENGINE_PATH:-}" ] || fail engine-path
export PATH="$AGENTD_ENGINE_PATH"

out=$(rustc --version 2>&1) || fail rustc
case "$out" in "rustc $version "*) ok rustc ;; *) fail rustc ;; esac
# The commands in the engine's bin directory are links to the real files in the Rust prefix, and rustc finds its standard library from its own resolved
# file: through the link, `rustc --print sysroot` must name the prefix of the real file (a copy of the binary, or a broken link, would name another place).
real=$(readlink -f "$(command -v rustc)") || fail sysroot
[ "$(rustc --print sysroot 2>/dev/null)" = "${real%/bin/rustc}" ] || fail sysroot
ok sysroot
out=$(cargo --version 2>&1) || fail cargo
case "$out" in "cargo $version "*) ok cargo ;; *) fail cargo ;; esac
cargo nextest --version >/dev/null 2>&1 || fail nextest
ok nextest
cargo clippy --version >/dev/null 2>&1 || fail clippy
ok clippy
cargo fmt --version >/dev/null 2>&1 || fail rustfmt
ok rustfmt
# rustc links through cc (gcc), which calls ld (binutils) and needs the C library's start files (glibc-devel).
command -v cc >/dev/null 2>&1 && command -v ld >/dev/null 2>&1 || fail linker
ok linker

work=$(mktemp -d "${TMPDIR:-/tmp}/rust-check.XXXXXX") || fail workdir
cd "$work" || fail workdir
mkdir src
cat >Cargo.toml <<'TOML'
[package]
name = "rustcheck"
version = "0.1.0"
edition = "2021"
TOML
cat >src/main.rs <<'RUST'
fn greeting() -> &'static str {
    "rust-check-ok"
}

fn main() {
    println!("{}", greeting());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets() {
        assert_eq!(greeting(), "rust-check-ok");
    }
}
RUST
log="$work/step.log"
step() { # <name> <command...>: runs the command with its output in the log; a failure shows the log's tail
  local name=$1
  shift
  "$@" >"$log" 2>&1 || {
    tail -n 30 "$log" >&2
    fail "$name"
  }
  ok "$name"
}
step build cargo build --offline
[ "$(./target/debug/rustcheck 2>&1)" = rust-check-ok ] || fail run
ok run
step test cargo test --offline
step nextest-run cargo nextest run --offline
step clippy-run cargo clippy --offline -- -D warnings
step fmt-check cargo fmt --check
echo "RUST-CHECK PASS"
