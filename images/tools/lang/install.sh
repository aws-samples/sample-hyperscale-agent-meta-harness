#!/usr/bin/env bash
# Install the language tool set at <dest>:
#   install.sh <dest> <python|go|jvm|direct|rust|all> [<download cache dir>]
# The family follows harness_core::stacks::Family: every family gets Node and the JS gate tools
# (the TypeScript profile); go adds Go, golangci-lint and gosec; jvm adds the JDK, Maven, Gradle,
# JaCoCo, SpotBugs with FindSecBugs, google-java-format and ktfmt; `direct` (the direct-run worker,
# which runs no gates) adds Go (go and gofmt), the JDK and Maven, and the slim Rust set, so one image
# builds and tests a Go, a Java, a Rust or a TypeScript project; rust adds the Rust toolchain
# (rustc, cargo, the standard library, clippy, rustfmt and llvm-tools from the standalone
# installers, no rustup) with cargo-nextest and cargo-llvm-cov (rustc links through `cc`: the image
# needs gcc and glibc-devel); the slim Rust set of `direct` is the same without llvm-tools and
# cargo-llvm-cov, because no run measures coverage and they cost image bytes; all installs
# everything (the test set). Every download is checked against its SHA-256 in
# lang.lock for this machine's architecture; the JS tools install with `npm ci --ignore-scripts`
# from js/, so each package is checked against its package-lock.json integrity.
# Layout: <dest>/{node,js,go,jdk,maven,gradle,rust}/, <dest>/bin/<tool>, <dest>/jars/*.jar,
# <dest>/spotbugs/, and <dest>/manifest.json (family, arch, versions, lock and package-lock SHA-256).
# Prints "LANG-TOOLS OK <tool> <version>" per tool, then LANG-TOOLS PASS (exit 0). A failed step
# exits 1; usage errors exit 2. Needs curl, tar, xz, unzip and sha256sum. A download that fails or does not match its SHA-256
# is fetched again (../fetch.sh: 4 attempts, pauses of 5, 15 and 45 seconds), and so is `npm ci`; the pin never changes, so a
# file that stays wrong fails with the same message.
set -euo pipefail
if [ $# -lt 2 ] || [ $# -gt 3 ]; then
  echo "usage: install.sh <dest> <python|go|jvm|direct|rust|all> [<download cache dir>]" >&2
  exit 2
fi
dest=$1 family=$2 cache=${3:-}
case "$family" in python | go | jvm | direct | rust | all) ;; *)
  echo "install.sh: unknown family $family" >&2
  exit 2
  ;;
esac
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../fetch.sh
. "$here/../fetch.sh"
lock="$here/lang.lock"
arch=$(uname -m)
case "$arch" in x86_64 | aarch64) ;; *)
  echo "install.sh: unsupported architecture $arch" >&2
  exit 2
  ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/lang-tools.XXXXXX")
trap 'rm -rf "$work"' EXIT
[ -n "$cache" ] || cache="$work/dl"
mkdir -p "$cache"

# row <tool>: "<version> <sha256> <url>" for this architecture (or any).
row() {
  awk -v t="$1" -v a="$arch" '$1 == t && ($3 == a || $3 == "any") { print $2, $4, $5; exit }' "$lock"
}
# fetch <tool>: the checked download's path.
fetch() {
  local r v sha url f rc=0
  r=$(row "$1")
  [ -n "$r" ] || { echo "install.sh: no $1 row for $arch in lang.lock" >&2; exit 1; }
  read -r v sha url <<<"$r"
  f="$cache/$(basename "$url")"
  if [ ! -f "$f" ] || ! echo "$sha  $f" | sha256sum -c --status -; then
    fetch_pinned "$url" "$sha" "$f" || rc=$?
    [ "$rc" -ne 65 ] || { echo "install.sh: SHA-256 mismatch for $url" >&2; exit 1; }
    [ "$rc" -eq 0 ] || { echo "install.sh: download of $url failed (exit status $rc)" >&2; exit 1; }
  fi
  echo "$sha  $f" | sha256sum -c --status - || { echo "install.sh: SHA-256 mismatch for $url" >&2; exit 1; }
  echo "$f"
}
version() { row "$1" | cut -d' ' -f1; }
untar() { # <archive> <dir>: the archive's single top directory becomes <dir>
  mkdir -p "$2"
  tar -xf "$1" -C "$2" --strip-components=1 --no-same-owner
}

rm -rf "$dest"
mkdir -p "$dest/bin" "$dest/jars"
tools=()

# Node and the JS gate tools (every family). The headers and docs are not needed to run tools.
untar "$(fetch node)" "$dest/node"
rm -rf "$dest/node/include" "$dest/node/share" "$dest/node/CHANGELOG.md" "$dest/node/README.md"
for b in node npm npx; do ln -s "../node/bin/$b" "$dest/bin/$b"; done
mkdir -p "$dest/js"
cp "$here/js/package.json" "$here/js/package-lock.json" "$dest/js/"
(cd "$dest/js" && retry_run "npm ci" env HOME="$work/home" npm_config_cache="$work/npm-cache" npm_config_update_notifier=false PATH="$dest/node/bin:$PATH" \
  "$dest/node/bin/npm" ci --ignore-scripts --no-audit --no-fund --loglevel=error)
ln -s ../js/node_modules/pnpm/bin/pnpm.mjs "$dest/bin/pnpm"
tools+=(node)

if [ "$family" = go ] || [ "$family" = direct ] || [ "$family" = all ]; then
  untar "$(fetch go)" "$dest/go"
  # Go's own tests, API lists and docs; src (the standard library) stays.
  rm -rf "$dest/go/test" "$dest/go/api" "$dest/go/doc" "$dest/go/misc"
  find "$dest/go/src" -type d -name testdata -prune -exec rm -rf {} +
  for b in go gofmt; do ln -s "../go/bin/$b" "$dest/bin/$b"; done
  tools+=(go)
fi
if [ "$family" = go ] || [ "$family" = all ]; then
  # The gate tools (golangci-lint, gosec) belong to the families that run gates; `direct` has none.
  mkdir -p "$work/gl" "$work/gs"
  tar -xf "$(fetch golangci-lint)" -C "$work/gl" --strip-components=1 --no-same-owner
  install -m 0755 "$work/gl/golangci-lint" "$dest/bin/golangci-lint"
  tar -xf "$(fetch gosec)" -C "$work/gs" --no-same-owner
  install -m 0755 "$work/gs/gosec" "$dest/bin/gosec"
  tools+=(golangci-lint gosec)
fi

if [ "$family" = jvm ] || [ "$family" = direct ] || [ "$family" = all ]; then
  untar "$(fetch jdk)" "$dest/jdk"
  # Sources, jmods (jlink input) and demos are not needed to compile or run.
  rm -rf "$dest/jdk/lib/src.zip" "$dest/jdk/jmods" "$dest/jdk/demo" "$dest/jdk/man"
  for b in java javac jar; do ln -s "../jdk/bin/$b" "$dest/bin/$b"; done
  untar "$(fetch maven)" "$dest/maven"
  ln -s ../maven/bin/mvn "$dest/bin/mvn"
  tools+=(jdk maven)
fi
if [ "$family" = jvm ] || [ "$family" = all ]; then
  # Gradle, JaCoCo, SpotBugs and the formatters belong to the families that run gates; `direct` has none.
  mkdir -p "$work/gradle"
  unzip -q "$(fetch gradle)" -d "$work/gradle"
  mv "$work/gradle/"gradle-* "$dest/gradle"
  ln -s ../gradle/bin/gradle "$dest/bin/gradle"
  mkdir -p "$work/jacoco"
  unzip -q "$(fetch jacoco)" -d "$work/jacoco"
  install -m 0644 "$work/jacoco/lib/jacocoagent.jar" "$dest/jars/jacocoagent.jar"
  install -m 0644 "$work/jacoco/lib/jacococli.jar" "$dest/jars/jacococli.jar"
  untar "$(fetch spotbugs)" "$dest/spotbugs"
  install -m 0644 "$(fetch findsecbugs)" "$dest/jars/findsecbugs.jar"
  install -m 0644 "$(fetch google-java-format)" "$dest/jars/google-java-format.jar"
  install -m 0644 "$(fetch ktfmt)" "$dest/jars/ktfmt.jar"
  tools+=(gradle jacoco spotbugs findsecbugs google-java-format ktfmt)
fi
if [ "$family" = rust ] || [ "$family" = direct ] || [ "$family" = all ]; then
  # Each standalone installer installs its components under one prefix; docs and man pages go. `direct` gets the slim set (no
  # llvm-tools, no cargo-llvm-cov): the coverage tools stay in the rust family, which carries the full set.
  parts=(rustc cargo rust-std clippy rustfmt)
  [ "$family" = direct ] || parts+=(llvm-tools)
  for t in "${parts[@]}"; do
    mkdir -p "$work/$t"
    tar -xf "$(fetch "$t")" -C "$work/$t" --strip-components=1 --no-same-owner
    bash "$work/$t/install.sh" --prefix="$dest/rust" --disable-ldconfig >/dev/null
  done
  rm -rf "$dest/rust/share/doc" "$dest/rust/share/man" "$dest/rust/share/zsh"
  # rustc finds its standard library relative to its own file, so the real files stay in <dest>/rust/bin; the commands a session runs are
  # linked into <dest>/bin, the directory the engine's PATH holds, like the other languages' commands.
  for b in rustc rustdoc cargo rustfmt cargo-fmt cargo-clippy clippy-driver; do ln -s "../rust/bin/$b" "$dest/bin/$b"; done
  mkdir -p "$work/nextest"
  tar -xf "$(fetch cargo-nextest)" -C "$work/nextest" --no-same-owner
  install -m 0755 "$work/nextest/cargo-nextest" "$dest/bin/cargo-nextest"
  tools+=(rustc cargo rust-std clippy rustfmt)
  [ "$family" = direct ] || tools+=(llvm-tools)
  tools+=(cargo-nextest)
  if [ "$family" != direct ]; then
    mkdir -p "$work/llvm-cov"
    tar -xf "$(fetch cargo-llvm-cov)" -C "$work/llvm-cov" --no-same-owner
    install -m 0755 "$work/llvm-cov/cargo-llvm-cov" "$dest/bin/cargo-llvm-cov"
    tools+=(cargo-llvm-cov)
  fi
fi
chmod -R go-w "$dest"

# Each tool runs once; its first output line is recorded.
export PATH="$dest/bin:$PATH" JAVA_HOME="$dest/jdk" HOME="$work/home" GOCACHE="$work/gocache" GOTOOLCHAIN=local
declare -A cmd=(
  [node]="node --version" [go]="go version" [golangci-lint]="golangci-lint version"
  [gosec]="gosec -version" [jdk]="java -version" [maven]="mvn -v" [gradle]="gradle --version"
  [jacoco]="java -jar $dest/jars/jacococli.jar version" [spotbugs]="java -jar $dest/spotbugs/lib/spotbugs.jar -textui -version"
  [findsecbugs]="unzip -p $dest/jars/findsecbugs.jar META-INF/MANIFEST.MF"
  [google-java-format]="java -jar $dest/jars/google-java-format.jar --version"
  [ktfmt]="java -jar $dest/jars/ktfmt.jar --help"
  [rustc]="$dest/rust/bin/rustc --version" [cargo]="$dest/rust/bin/cargo --version"
  [clippy]="$dest/rust/bin/cargo-clippy --version" [rustfmt]="$dest/rust/bin/rustfmt --version"
  [rust-std]="ls $dest/rust/lib/rustlib/*/lib/libstd-*.rlib"
  [llvm-tools]="find $dest/rust/lib/rustlib -name llvm-profdata -type f"
  [cargo-nextest]="cargo-nextest nextest --version" [cargo-llvm-cov]="cargo-llvm-cov llvm-cov --version"
)
versions="{}"
for t in "${tools[@]}"; do
  out=$(${cmd[$t]} 2>&1) || { echo "install.sh: $t does not run: $out" >&2; exit 1; }
  line=$(grep -m1 -E '[0-9]' <<<"$out" || true)
  echo "LANG-TOOLS OK $t $(version "$t") ($line)"
  versions=$(printf '%s' "$versions" | "$dest/node/bin/node" -e \
    'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const v=JSON.parse(s);v[process.argv[1]]=process.argv[2];process.stdout.write(JSON.stringify(v))})' \
    "$t" "$(version "$t")")
done
for p in eslint prettier typescript pnpm typescript-eslint eslint-plugin-security; do
  v=$("$dest/node/bin/node" -p "require('$dest/js/node_modules/$p/package.json').version")
  echo "LANG-TOOLS OK js:$p $v"
done
"$dest/node/bin/node" -e '
const fs = require("fs"), crypto = require("crypto");
const [dest, here, family, arch, versions] = process.argv.slice(1);
const sha = (f) => crypto.createHash("sha256").update(fs.readFileSync(f)).digest("hex");
const m = { schemaVersion: 1, family, arch, versions: JSON.parse(versions),
  langLockSha256: sha(here + "/lang.lock"), jsLockSha256: sha(here + "/js/package-lock.json") };
fs.writeFileSync(dest + "/manifest.json", JSON.stringify(m, null, 1) + "\n");
' "$dest" "$here" "$family" "$arch" "$versions"
echo "LANG-TOOLS PASS family=$family arch=$arch"
