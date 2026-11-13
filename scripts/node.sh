#!/usr/bin/env bash
# COMPAT-CAH: this wrapper forwards and folds the previous variable names (CAH_*) for the window of the previous names.
# Run Node, npm, npx, pnpm or the CDK CLI in the digest-pinned container from tools.lock. Node 18+
# needs glibc 2.28 and an older Linux machine has 2.26, so this is the way Node runs there.
# usage: scripts/node.sh <node|npm|npx|pnpm|cdk> [args...]    exit code: the wrapped command's
#
# - Runs as the host uid:gid, unprivileged, with the git top-level mounted at its host path.
# - Caches and HOME live under <workspace tmp>/node-sh; pnpm and aws-cdk install once, at the
#   tools.lock versions and integrity, into HAMH_TOOLS/node.
# - AWS access: a mounted credential_process shim reads credentials.json from a 0700 per-invocation
#   directory mounted read-only at /hamh/creds. The script replaces the file by atomic rename every
#   NODE_SH_REFRESH_SECS (900) from NODE_SH_CREDS_CMD (default: AWS CLI v2 `configure
#   export-credentials --profile <profile> --format process`) and deletes the directory on exit. The profile,
#   Region and account are the accounts.json entry HAMH_AWS_PROFILE selects, else the active one;
#   an inherited AWS_PROFILE, AWS_REGION or AWS_DEFAULT_REGION never names them.
#   NODE_SH_REFRESH_SECS is a whole number of seconds, 1 or more (anything else exits 2). The refresher ends with the call: the cleanup
#   stops it with TERM, and it also looks every second whether node.sh and the credentials directory are still there, so it ends within
#   about a second even when TERM does not reach it (a parent that ignores TERM; node.sh killed with -9).
# - Environment: a fixed list plus every exported HAMH_* variable by name (the CDK app's inputs,
#   HAMH_LAMBDA_DIR and HAMH_WORKER_IMAGE) and, for the window of the previous names, every exported CAH_* variable (COMPAT-CAH); nothing else from the host environment.
# - The account-file override: HAMH_ACCOUNTS_FILE (or, for the window of the previous names, CAH_ACCOUNTS_FILE, which is folded into it), an absolute path, is read by the CDK app inside the container, so
#   the file is mounted read-only at its host path unless the git checkout (already mounted) holds it. Both paths are resolved first
#   (lib/path-canon.sh: `..` and links are followed), so the file is judged by where it really is, and the container gets the resolved path in
#   HAMH_ACCOUNTS_FILE. A path with a colon, as written or once resolved, is refused (exit 2).
# - Image source: the pin in tools.lock [containers].node, pulled by `docker run` as needed. The pull retries a throttle, a 5xx or a
#   network error (NODE_SH_PULL_ATTEMPTS, default 4, pauses of NODE_SH_PULL_SLEEP x attempt squared seconds, default 5), never a missing
#   manifest; a mirror that stays down falls back to the pinned reference. Unset or empty, node.sh pulls nothing itself.
# Test hooks: NODE_SH_DOCKER_BIN, NODE_SH_CREDS_CMD, NODE_SH_TMP, NODE_SH_TOOLS, NODE_SH_REFRESH_SECS, NODE_SH_PULL_ATTEMPTS, NODE_SH_PULL_SLEEP.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
# shellcheck source=lib/aws-target.sh
. "$here/lib/aws-target.sh"
# shellcheck source=lib/path-canon.sh
. "$here/lib/path-canon.sh"

# COMPAT-CAH: the previous spelling of the account-file override is folded into the new one, and then dropped, so the container never sees two spellings of one selector
# with different text (the path as written and the path as resolved below). aws-target.sh has already refused two different values (exit 2) and printed the notice.
if [ -z "${HAMH_ACCOUNTS_FILE:-}" ] && [ -n "${CAH_ACCOUNTS_FILE:-}" ]; then
  HAMH_ACCOUNTS_FILE=$CAH_ACCOUNTS_FILE
  export HAMH_ACCOUNTS_FILE
fi
unset CAH_ACCOUNTS_FILE
# COMPAT-CAH: the host-level previous name of the tools folder
if [ -z "${HAMH_TOOLS:-}" ] && [ -n "${CAH_TOOLS:-}" ]; then HAMH_TOOLS=$CAH_TOOLS; fi

case "${HAMH_ACCOUNTS_FILE:-}" in
  *:*) echo "node.sh: HAMH_ACCOUNTS_FILE must not contain a colon (it is mounted by its path)" >&2; exit 2 ;;
esac

case "${1:-}" in
  node|npm|npx|pnpm|cdk) tool=$1; shift ;;
  *) echo "usage: node.sh <node|npm|npx|pnpm|cdk> [args...]" >&2; exit 2 ;;
esac

lockget() { "$py" "$here/lib/tools_lock.py" get --lock "$repo/tools.lock" "$1"; }
image=$(lockget containers.node)
pnpm_v=$(lockget node.pnpm) pnpm_i=$(lockget node.pnpm_integrity)
cdk_v=$(lockget node.aws_cdk) cdk_i=$(lockget node.aws_cdk_integrity)

docker_bin="${NODE_SH_DOCKER_BIN:-docker}"
work="${NODE_SH_TMP:-${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/ham}/node-sh"
global="${NODE_SH_TOOLS:-${HAMH_TOOLS:-${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/ham}}/node"
refresh="${NODE_SH_REFRESH_SECS:-900}"
case "$refresh" in
  ''|0*|*[!0-9]*) echo "node.sh: NODE_SH_REFRESH_SECS must be a whole number of seconds, 1 or more (got '$refresh')" >&2; exit 2 ;;
esac
mkdir -p "$work/home" "$work/cache/npm" "$work/cache/pnpm-store" "$work/aws" "$work/creds" "$global"
chmod 700 "$work/creds"

# <mirror>/<name>@<digest> for a pin of the form <name>[:<tag>]@sha256:<digest> (an official image: no namespace); 1 when it cannot be mirrored.
mirror_ref() { # <pin> <mirror prefix>
  local re='^([a-z0-9][a-z0-9._-]*)(:[A-Za-z0-9._-]+)?@(sha256:[0-9a-f]{64})$'
  [[ $1 =~ $re ]] || return 1
  printf '%s/%s@%s\n' "${2%/}" "${BASH_REMATCH[1]}" "${BASH_REMATCH[3]}"
}

# 0 once the daemon holds <ref>: nothing to do when it does, else `docker pull` with retries. A manifest that does not exist is permanent;
# anything else (429 toomanyrequests, 5xx, a reset connection, a timeout) is tried again after unit x attempt squared seconds.
pull_image() { # <ref>
  local ref=$1 attempts=${NODE_SH_PULL_ATTEMPTS:-4} unit=${NODE_SH_PULL_SLEEP:-5} i=1 out last pause
  if "$docker_bin" image inspect "$ref" >/dev/null 2>&1; then return 0; fi
  while :; do
    if out=$("$docker_bin" pull -q "$ref" 2>&1); then echo "node.sh: pulled $ref" >&2; return 0; fi
    last=$(printf '%s\n' "$out" | tail -n 1 | cut -c1-200)
    if printf '%s\n' "$out" | grep -Eiq 'manifest unknown|name unknown|not found|invalid reference|no matching manifest'; then
      echo "node.sh: docker pull $ref failed and is not retried: $last" >&2
      return 1
    fi
    if [ "$i" -ge "$attempts" ]; then
      echo "node.sh: docker pull $ref failed after $i attempts: $last" >&2
      return 1
    fi
    pause=$(awk -v u="$unit" -v i="$i" 'BEGIN { printf "%g", u * i * i }')
    echo "node.sh: docker pull $ref attempt $i failed ($last); retrying in ${pause}s" >&2
    sleep "$pause"
    i=$((i + 1))
  done
}

if [ -n "${NODE_SH_IMAGE_MIRROR:-}" ]; then
  [[ ${NODE_SH_PULL_ATTEMPTS:-4} =~ ^[1-9][0-9]*$ && ${NODE_SH_PULL_SLEEP:-5} =~ ^[0-9]+(\.[0-9]+)?$ ]] ||
    { echo "node.sh: NODE_SH_PULL_ATTEMPTS must be a positive integer and NODE_SH_PULL_SLEEP a number of seconds" >&2; exit 2; }
  [[ $NODE_SH_IMAGE_MIRROR =~ ^[a-z0-9][a-z0-9.-]*(:[0-9]+)?(/[a-z0-9][a-z0-9._-]*)*/?$ ]] ||
    { echo "node.sh: NODE_SH_IMAGE_MIRROR is not a registry prefix like public.ecr.aws/docker/library" >&2; exit 2; }
  mirror_image=$(mirror_ref "$image" "$NODE_SH_IMAGE_MIRROR") ||
    { echo "node.sh: no mirror reference can be derived from the tools.lock pin $image (it must be <name>[:tag]@sha256:<digest>, no namespace)" >&2; exit 2; }
  if pull_image "$mirror_image"; then
    image=$mirror_image
  else
    echo "node.sh: falling back to the pinned reference $image" >&2
    pull_image "$image" || { echo "node.sh: neither $mirror_image nor $image could be pulled" >&2; exit 1; }
  fi
fi

fetch_creds() {
  if [ -n "${NODE_SH_CREDS_CMD:-}" ]; then "$NODE_SH_CREDS_CMD"
  else aws configure export-credentials --profile "$AWS_PROFILE" --format process; fi
}

creds_dir=$(mktemp -d "$work/creds/node-sh.XXXXXX")
chmod 700 "$creds_dir"
# Replace by rename inside the mounted directory: a reader sees the old or the new file, never a
# partial one (a single-file bind mount would pin the first inode instead).
write_creds() {
  local fresh tmp
  fresh=$(fetch_creds) || return 1
  tmp=$(mktemp "$creds_dir/.credentials.XXXXXX")
  chmod 600 "$tmp"
  printf '%s\n' "$fresh" >"$tmp" && mv -f "$tmp" "$creds_dir/credentials.json"
}
write_creds || { echo "node.sh: could not fetch AWS credentials" >&2; rm -rf "$creds_dir"; exit 1; }

cat >"$work/aws/credential-shim.js" <<'JS'
// credential_process shim: print the host-refreshed credentials (replaced by rename; retries cover a slow host).
const fs = require("fs");
const f = "/hamh/creds/credentials.json";
for (let i = 0; i < 40; i++) {
  try {
    const s = fs.readFileSync(f, "utf8");
    if (JSON.parse(s).Version === 1) { process.stdout.write(s); process.exit(0); }
  } catch (e) { /* partial write; retry */ }
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
}
console.error("credential shim: no valid credentials at " + f);
process.exit(1);
JS
region="$HAMH_REGION"
printf '[default]\ncredential_process = node /hamh/aws/credential-shim.js\nregion = %s\n\n[profile %s]\ncredential_process = node /hamh/aws/credential-shim.js\nregion = %s\n' \
  "$region" "$AWS_PROFILE" "$region" >"$work/aws/config"

# The refresher ends with the call, and the cleanup below stops it with TERM. TERM cannot be counted on, though: a parent that ignores it
# (a Python runner inside its own cleanup, a launcher with `trap '' TERM`) hands that setting down, bash cannot trap a signal that was
# ignored when it started, and a refresher that could not be told to stop looped for ever with the init process as its parent.
# So it also looks every second whether node.sh is still alive and its credentials directory is
# still there, and ends when either is gone, which covers a node.sh killed with -9 as well. It sleeps one second at a time in the background
# and waits for the sleep: a TERM that does arrive ends it at once and takes the sleep along, because an orphaned sleep would hold the caller's pipes open.
node_pid=$$
(
  nap=
  trap 'kill "$nap" 2>/dev/null; exit 0' TERM
  waited=0
  while :; do
    sleep 1 & nap=$!
    wait "$nap"
    [ -d "$creds_dir" ] && kill -0 "$node_pid" 2>/dev/null || exit 0
    waited=$((waited + 1))
    if [ "$waited" -ge "$refresh" ]; then
      waited=0
      write_creds || echo "node.sh: credential refresh failed" >&2
    fi
  done
) &
refresher=$!
cleanup() { kill "$refresher" 2>/dev/null || true; rm -rf "$creds_dir"; }
trap cleanup EXIT

# The checkout is mounted at its physical path, so every path that is compared with it or mounted next to it is resolved first (lib/path-canon.sh). A string test on
# the path as written took `<checkout>/../hamh-setup-work/account/accounts.json` for a file of the checkout (the setup runner names its work folder that way):
# no mount was added and the CDK app found no file. The working directory is the physical one for the same reason.
root=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || pwd -P)
root=$(ham_canon_path "$root") || { echo "node.sh: cannot resolve the path of the checkout ($root)" >&2; exit 2; }
workdir=$(pwd -P)
accounts_mount=()
if [ -n "${HAMH_ACCOUNTS_FILE:-}" ]; then
  ham_file_mount_args "$root" "$HAMH_ACCOUNTS_FILE" || { echo "node.sh: cannot resolve HAMH_ACCOUNTS_FILE ($HAMH_ACCOUNTS_FILE)" >&2; exit 2; }
  case "$ham_file_canon" in
    *:*) echo "node.sh: HAMH_ACCOUNTS_FILE must not contain a colon ($ham_file_canon once resolved; it is mounted by its path)" >&2; exit 2 ;;
  esac
  HAMH_ACCOUNTS_FILE=$ham_file_canon
  export HAMH_ACCOUNTS_FILE
  if [ "${#ham_mount_args[@]}" -gt 0 ]; then accounts_mount=("${ham_mount_args[@]}"); fi
fi
tty=()
if [ -t 0 ] && [ -t 1 ]; then tty=(-t); fi

case "$tool" in
  node|npm|npx) cmd=("$tool" "$@") ;;
  pnpm|cdk)
    # Install the pinned CLIs once; the registry integrity must equal the tools.lock value.
    install='set -e; P=/hamh/node-global; want="pnpm@'"$pnpm_v"' aws-cdk@'"$cdk_v"'"
if [ "$(cat "$P/.versions" 2>/dev/null)" != "$want" ]; then
  [ "$(npm view pnpm@'"$pnpm_v"' dist.integrity)" = "'"$pnpm_i"'" ] || { echo "pnpm integrity mismatch" >&2; exit 3; }
  [ "$(npm view aws-cdk@'"$cdk_v"' dist.integrity)" = "'"$cdk_i"'" ] || { echo "aws-cdk integrity mismatch" >&2; exit 3; }
  npm install -g --prefix "$P" --no-fund --no-audit --loglevel=error pnpm@'"$pnpm_v"' aws-cdk@'"$cdk_v"' >&2
  echo "$want" > "$P/.versions"
fi
t=$1; shift; exec "$P/bin/$t" "$@"'
    cmd=(sh -c "$install" node.sh "$tool" "$@") ;;
esac

# The CDK app's inputs pass through by name; any other host variable stays outside. The app reads both spellings of an input for the window of the previous names
# (COMPAT-CAH, infra/lib/env.ts), so the container is given every exported variable of either prefix.
app_env=()
while IFS= read -r name; do app_env+=(-e "$name"); done < <(compgen -e | grep -E '^(CAH|HAMH)_' || true)

set +e
"$docker_bin" run --rm -i ${tty[@]+"${tty[@]}"} --init \
  --user "$(id -u):$(id -g)" \
  -e HOME=/hamh/home \
  -e npm_config_cache=/hamh/cache/npm \
  -e npm_config_store_dir=/hamh/cache/pnpm-store \
  -e pnpm_config_store_dir=/hamh/cache/pnpm-store \
  -e npm_config_update_notifier=false \
  -e AWS_CONFIG_FILE=/hamh/aws/config \
  -e AWS_SHARED_CREDENTIALS_FILE=/hamh/aws/no-credentials \
  -e AWS_PROFILE="$AWS_PROFILE" \
  -e AWS_REGION="$region" \
  -e AWS_EC2_METADATA_DISABLED=true \
  -e CDK_DISABLE_CLI_TELEMETRY=true \
  ${app_env[@]+"${app_env[@]}"} \
  -v "$work/home:/hamh/home" \
  -v "$work/cache:/hamh/cache" \
  -v "$work/aws:/hamh/aws:ro" \
  -v "$creds_dir:/hamh/creds:ro" \
  -v "$global:/hamh/node-global" \
  -v "$root:$root" \
  ${accounts_mount[@]+"${accounts_mount[@]}"} \
  -w "$workdir" \
  "$image" "${cmd[@]}"
rc=$?
set -e
# nosemgrep: unquoted-variable-expansion-in-command -- rc holds the numeric exit status saved earlier from the last command and exit takes one number, so word splitting cannot change it
exit $rc
