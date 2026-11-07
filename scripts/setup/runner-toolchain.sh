#!/usr/bin/env bash
# scripts/setup/runner-toolchain.sh: install the pinned build tools on the setup runner.
# usage: scripts/setup/runner-toolchain.sh <tools folder>
#
# The runner is a Linux x86_64 CodeBuild container with the AWS CLI, git, curl, tar and Python. This script adds what the deploy builds with, every download
# checked against a pinned SHA-256, into <tools folder> (nothing under /usr or /opt):
#   tools      the tools.lock entries rustup-init, cargo-lambda, cargo-zigbuild, zig and awscli, through scripts/lib/tools_lock.py install (it checks each SHA-256)
#   awscli     the AWS CLI version tools.lock pins, installed under <tools folder> and put first on PATH (the deploy uses create-only puts and
#              `aws configure export-credentials`, which an older CLI of the build image may not have)
#   rust       the toolchain, components and targets of tools.lock [rust], through rustup-init; `rustc --version` must name the pinned version
#   go         the Go toolchain row of images/tools/lang/lang.lock for this machine (scripts/gomod-store.sh fill needs Go); the archive is checked against its
#              SHA-256 before it is unpacked, and a folder that already holds that version is left alone
#   docker     a running Docker daemon (scripts/node.sh runs Node, pnpm and the CDK in the pinned container); one is started when `docker info` fails, and the
#              step waits up to RUNNER_TOOLCHAIN_DOCKER_WAIT seconds (default 60) for it
# pnpm and the CDK CLI are installed by scripts/node.sh itself, inside the container, against the integrity values of tools.lock.
# RUNNER_TOOLCHAIN_ONLY=template (the build that prints the CDK bootstrap template, SETUP_ACTION=bootstrap-template of runner-build.sh) installs the pinned AWS CLI and
# starts the Docker daemon, and nothing else: no Rust, no Go, no zig; env.sh then holds HAMH_TOOLS and PATH only and the last line is `TOOLCHAIN PASS awscli=<v>`.
# Writes <tools folder>/env.sh (HAMH_TOOLS, RUSTUP_HOME, CARGO_HOME and PATH) for the caller to source. Prints TOOL and step lines, then the last line
#   TOOLCHAIN PASS rust=<v> zig=<v> cargo-zigbuild=<v> cargo-lambda=<v> go=<v> awscli=<v>      (exit 0)
#   TOOLCHAIN FAIL <step> <detail>                                                    (exit 1)
# A usage error exits 2. Python 3.11 or later is needed (HAMH_PYTHON, or the interpreter scripts/lib/python.sh picks).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
if [ $# -ne 1 ] || [ -z "$1" ]; then
  echo "usage: runner-toolchain.sh <tools folder>" >&2
  exit 2
fi
mkdir -p "$1" || { echo "TOOLCHAIN FAIL tools cannot make $1"; exit 1; }
TOOLS="$(cd "$1" && pwd)"
only="${RUNNER_TOOLCHAIN_ONLY:-}"
case "$only" in
  '' | template) ;;
  *) echo "runner-toolchain.sh: RUNNER_TOOLCHAIN_ONLY must be empty or template, not '$only'" >&2; exit 2 ;;
esac
wait_s="${RUNNER_TOOLCHAIN_DOCKER_WAIT:-60}"
case "$wait_s" in
  '' | *[!0-9]*) echo "runner-toolchain.sh: RUNNER_TOOLCHAIN_DOCKER_WAIT must be a whole number of seconds, not '$wait_s'" >&2; exit 2 ;;
esac
# shellcheck source=../lib/python.sh
. "$ROOT/scripts/lib/python.sh"
LOCK="$ROOT/tools.lock"

tfail() { printf 'TOOLCHAIN FAIL %s %s\n' "$1" "$2"; exit 1; }
say() { printf 'toolchain: %s\n' "$*"; }
lock() { "$py" "$ROOT/scripts/lib/tools_lock.py" "$@"; }
toolver() { lock list --lock "$LOCK" | awk -v n="$1" '$1 == n { print $2 }'; }

# ---- tools ----
export TOOLS_LOCK_ROOTS="$TOOLS"
tools_wanted=rustup-init,cargo-lambda,cargo-zigbuild,zig,awscli
[ "$only" != template ] || tools_wanted=awscli
if ! lock install --lock "$LOCK" --dest "$TOOLS" --only "$tools_wanted" >"$TOOLS/tools.log" 2>&1; then
  cat "$TOOLS/tools.log"
  tfail tools "$(grep -m1 '^TOOL FAIL' "$TOOLS/tools.log" || tail -n 1 "$TOOLS/tools.log" | cut -c1-300)"
fi
cat "$TOOLS/tools.log"

# ---- awscli ----
aws_v=$(toolver awscli)
[ -n "$aws_v" ] || tfail awscli "tools.lock has no awscli entry"
if ! "$TOOLS/bin/aws" --version 2>&1 | grep -q "aws-cli/$aws_v "; then
  "$TOOLS/pkgs/awscli-$aws_v/aws/install" --install-dir "$TOOLS/aws-cli" --bin-dir "$TOOLS/bin" --update >"$TOOLS/awscli.log" 2>&1 ||
    tfail awscli "the installer of awscli $aws_v failed: $(tail -n 1 "$TOOLS/awscli.log" | cut -c1-300)"
fi
export PATH="$TOOLS/bin:$PATH"
"$TOOLS/bin/aws" --version 2>&1 | grep -q "aws-cli/$aws_v " || tfail awscli "$TOOLS/bin/aws does not report aws-cli/$aws_v"
say "aws-cli $aws_v"

# ---- rust and go (not for the template build) ----
if [ "$only" != template ]; then
  export RUSTUP_HOME="$TOOLS/rustup" CARGO_HOME="$TOOLS/cargo"
  export PATH="$TOOLS/bin:$CARGO_HOME/bin:$PATH"
  rust_v=$(lock get --lock "$LOCK" rust.toolchain) || tfail rust "tools.lock names no [rust] toolchain"
  toml_list() { "$py" -c 'import sys, tomllib; print(",".join(tomllib.load(open(sys.argv[1], "rb"))["rust"][sys.argv[2]]))' "$LOCK" "$1"; }
  components=$(toml_list components) || tfail rust "tools.lock [rust] components cannot be read"
  targets=$(toml_list targets) || tfail rust "tools.lock [rust] targets cannot be read"
  if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    say "rustup-init for $rust_v"
    "$TOOLS/bin/rustup-init" -y --no-modify-path --profile minimal --default-toolchain "$rust_v" -c "$components" -t "$targets" >"$TOOLS/rustup.log" 2>&1 ||
      tfail rust "rustup-init failed: $(tail -n 1 "$TOOLS/rustup.log" | cut -c1-300)"
  else
    # an interrupted earlier install may lack a component or a target: install the pinned set again (rustup leaves what is there alone)
    { rustup toolchain install "$rust_v" --profile minimal -c "$components" -t "$targets" && rustup default "$rust_v"; } >"$TOOLS/rustup.log" 2>&1 ||
      tfail rust "rustup toolchain install $rust_v failed: $(tail -n 1 "$TOOLS/rustup.log" | cut -c1-300)"
  fi
  got=$(rustc --version 2>/dev/null)
  case "$got" in
    "rustc $rust_v "*) say "$got" ;;
    *) tfail rust "rustc reports '${got:-nothing}', tools.lock pins $rust_v" ;;
  esac

  # ---- go ----
  arch=$(uname -m)
  row=$(awk -v a="$arch" '$1 == "go" && ($3 == a || $3 == "any") { print $2, $4, $5; exit }' "$ROOT/images/tools/lang/lang.lock")
  [ -n "$row" ] || tfail go "no go row for $arch in images/tools/lang/lang.lock"
  read -r go_v go_sha go_url <<<"$row"
  if [ -x "$TOOLS/go/bin/go" ] && [ "$(head -n 1 "$TOOLS/go/VERSION" 2>/dev/null)" = "go$go_v" ]; then
    say "go $go_v present"
  else
    mkdir -p "$TOOLS/dl" "$TOOLS/unpack.$$"
    archive="$TOOLS/dl/$(basename "$go_url")"
    curl -fsSL --retry 5 --retry-all-errors -o "$archive" "$go_url" || tfail go "download of $go_url failed"
    printf '%s  %s\n' "$go_sha" "$archive" | sha256sum -c --status - || tfail go "sha256 of $(basename "$go_url") is not the $go_sha that images/tools/lang/lang.lock pins"
    tar -xzf "$archive" -C "$TOOLS/unpack.$$" || tfail go "unpacking $(basename "$go_url") failed"
    [ -x "$TOOLS/unpack.$$/go/bin/go" ] || tfail go "the archive holds no go/bin/go"
    if [ -e "$TOOLS/go" ]; then mv "$TOOLS/go" "$TOOLS/go.previous.$$"; fi
    mv "$TOOLS/unpack.$$/go" "$TOOLS/go" || tfail go "cannot move the toolchain to $TOOLS/go"
    say "go $go_v installed"
  fi
  case "$("$TOOLS/go/bin/go" version 2>/dev/null)" in
    *"go$go_v "*) ;;
    *) tfail go "$TOOLS/go/bin/go does not report go$go_v" ;;
  esac
fi

# ---- docker ----
if docker info >/dev/null 2>&1; then
  say "docker daemon is up"
else
  say "starting the docker daemon"
  nohup dockerd --host=unix:///var/run/docker.sock --storage-driver=overlay2 >"$TOOLS/dockerd.log" 2>&1 &
  dpid=$!
  waited=0
  until docker info >/dev/null 2>&1; do
    if ! kill -0 "$dpid" 2>/dev/null; then tfail docker "dockerd stopped: $(tail -n 1 "$TOOLS/dockerd.log" | cut -c1-300)"; fi
    if [ "$waited" -ge "$wait_s" ]; then tfail docker "the Docker daemon did not answer within ${wait_s} seconds"; fi
    sleep 1
    waited=$((waited + 1))
  done
  say "docker daemon is up after ${waited}s"
fi

# ---- env.sh ----
if [ "$only" = template ]; then
  {
    printf 'export HAMH_TOOLS=%q\n' "$TOOLS"
    printf 'export PATH=%q:"$PATH"\n' "$TOOLS/bin"
  } >"$TOOLS/env.sh"
  printf 'TOOLCHAIN PASS awscli=%s\n' "$aws_v"
  exit 0
fi
{
  printf 'export HAMH_TOOLS=%q\n' "$TOOLS"
  printf 'export RUSTUP_HOME=%q\n' "$TOOLS/rustup"
  printf 'export CARGO_HOME=%q\n' "$TOOLS/cargo"
  printf 'export PATH=%q:"$PATH"\n' "$TOOLS/bin:$TOOLS/cargo/bin:$TOOLS/go/bin"
} >"$TOOLS/env.sh"
printf 'TOOLCHAIN PASS rust=%s zig=%s cargo-zigbuild=%s cargo-lambda=%s go=%s awscli=%s\n' "$rust_v" "$(toolver zig)" "$(toolver cargo-zigbuild)" "$(toolver cargo-lambda)" "$go_v" "$aws_v"
