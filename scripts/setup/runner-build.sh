#!/usr/bin/env bash
# scripts/setup/runner-build.sh: the build the setup runner's CodeBuild project runs.
# It runs inside the user's own account, from the source archive `setup.sh` uploaded, and takes no argument. Inputs, all environment variables:
#   SETUP_SOURCE_KEY    source/<40 hex commit>.zip   the `git archive HEAD` of the clone, in the setup bucket hamh-setup-<account>-<Region>
#   SETUP_CONFIG_KEY    config/accounts-<12 hex>.json  the generated account file; the 12 hex are the start of its SHA-256 and are checked
#   SETUP_SOURCE_SHA256 the SHA-256 (64 hex) of the source archive setup.sh uploaded     SETUP_CONFIG_SHA256  the SHA-256 (64 hex) of the account file it uploaded
#                       Other principals of the account may be able to write the bucket, so the bytes this script reads are hashed and compared with what setup.sh sent in
#                       the build environment (which the bucket cannot change) before anything of them is used.
#   SETUP_STAGE        direct (the default; the only stage)      SETUP_AZS  1 (the default; the only count)
#   SETUP_ACTION        deploy (the default) or bootstrap-template. `setup.sh` starts it with bootstrap-template in an account that has no CDK bootstrap: the build prints
#                       the bootstrap template with the pinned CDK CLI and puts it in the bucket (phases inputs, fetch, toolchain, account-file, template). Any other value,
#                       teardown included, stops the build in inputs: `setup.sh --teardown` runs on the user's machine with the user's own credentials and starts no build.
#   SETUP_BINARY_NAME   the client binary to build (default: HAMH_CLIENT_BIN of scripts/setup/names.sh)
#   SETUP_EXPLICIT_SETTINGS  the stage settings the user chose: base64, without line breaks, of HAMH_<NAME>=value lines. Each name must be a row of
#                       scripts/lib/stage-settings.tsv of the archive and each value must match that row's PATTERN (or be `default`); phase inputs stops on anything else.
#                       The lines are exported into the environment of deploy.sh only. A revision without the registry takes no settings.
#   SETUP_DEPLOY_LOCK_TOKEN  the token of the deploy lock /hamh/<stage>/deploy-lock that setup.sh holds (32 hex); deploy.sh gets it as HAMH_DEPLOY_LOCK_TOKEN (C6)
#                       deploy.sh also gets HAMH_SOURCE_COMMIT, the commit of SETUP_SOURCE_KEY: the repository this script makes of the archive has a HEAD that no clone
#                       holds, and the stage's record must name the commit the user asked for, so that the next update can place it
#   SETUP_FORCE_DEPLOY  1: `setup.sh --update --force-deploy`. The phase deploy runs even when an earlier build finished it for this commit and account file
#   SETUP_CLIENT_ONLY   1: the stage already runs this commit (C7). The phase deploy is skipped, after a check that the account records this source tree and account file as
#                       deployed and that every stack is complete (a refusal, phase refused, when it does not), and after `scripts/deploy.sh --relabel` has written the label of this
#                       release; gomod, scanner-rules and the client run as always
#   CODEBUILD_BUILD_ID  the id of this build, the key of its results      AWS_REGION  the Region of the build
# The script lives in that archive, so it builds the tree it belongs to. CodeBuild has extracted the archive there already; this script makes a git repository of it
# (deploy.sh and codebuild-run.sh build a commit), and checks that the archive names the commit of its key.
#
# Nine phases run in this order; each ends as a line of results/<build id>/phases.jsonl in the bucket (state ok, skipped or failed):
#   inputs         the variables above, a Python of 3.11 or later, the account of the credentials, and a CDK bootstrap in the account (the runner role cannot make one)
#   fetch          the account file and the archive from the bucket: the digest of the file and the commit of the archive must be the ones of the keys; git init and commit
#   toolchain      scripts/setup/runner-toolchain.sh: the pinned tools, Rust, Go and a Docker daemon, into <work>/tools
#   account-file   the account file is validated with scripts/lib/aws_target.py, laid in <work>/account/accounts.json and exported as HAMH_ACCOUNTS_FILE; its account
#                  and Region must be those of the credentials and the build, and its invocationLogging must be read-only (the runner role cannot write that setting);
#                  a profile with the container credentials of CodeBuild is written for the profile name of the file; scripts/node.sh runs node once
#   deploy         scripts/deploy.sh --stage S --azs N --fork-source build (nothing is copied from another account)
#   gomod          scripts/gomod-store.sh fill --reuse-current (the phase ends there when the store already holds the modules), else its check
#   scanner-rules  scripts/scanner_kit_rules.py build, then publish --apply
#   client         scripts/setup/build-client.sh <out dir> <bucket> <build id>: the client and the operator tool beside it for every platform, SHA256SUMS in the
#                  bucket, and the client section as JSON on stdout (each platform lists the operator tool under admin); every file is read back from the bucket
#   outputs        the stack outputs, the endpoint (V1ApiUrl of hamh-direct-control) and the deployment digest; result.json
# A deploy that deploy.py refuses before its first stack change
# ends as the phase `refused`, with the text after `DEPLOY-REFUSED ` as its detail; setup.sh says the stage is unchanged and exits 7 (wait=yes) or 5. The deploy guard
# (runs are active and pin the worker runtime) is such a refusal, `reason=guard wait=yes`, as are the owner, caller, account, zone and VPC range checks. A line
# `DEPLOY-FAIL guard...` with exit 1, the form of a deploy.py before the refusals, still ends as the phase `guard`, which setup.sh reads as exit 7.
# result.json carries `stageUnchanged` (true after DEPLOY-REFUSED, after `DEPLOY-STAGE unchanged`, the line deploy.py prints after a step that failed before it wrote
# status=deploying, and for the guard) and `failedStep`.
# The phase `template` of a bootstrap-template build runs `scripts/node.sh cdk bootstrap --show-template` (the runner role cannot make a bootstrap and the user's desktop has no Node),
# refuses a template of more than 51,200 bytes (what the AWS CLI deploys without a staging bucket) or whose SHA-256 is not the one of
# engine/policy/cdk-bootstrap-template.pin, puts it at bootstrap/cdk-bootstrap-<12 hex of the SHA-256>.yaml and names it in result.json as "bootstrapTemplate" (key, sha256, bytes);
# the last line printed is `SETUP-PASS stage=<s> action=bootstrap-template template=<12 hex>`. `setup.sh` deploys the template with the user's credentials after its own check of the pin.
# Phases deploy and gomod are skipped when an earlier build finished them for the same commit and the same account file (read from the phases.jsonl files under
# results/) and the account says so as well: the deployment digest of the account is the one recorded, the source tree and the account file the stage was deployed
# from (SSM /hamh/<stage>/deployed-source-tree and deployment-config-digest) are those of this build, and all nine stacks are complete; the full check of the Go module
# store passes. A record in the bucket alone never skips a phase. Skipping lets `setup.sh` attach to an interrupted setup or retry after a failure without doing the
# finished work again. Phase scanner-rules always runs: the publish step leaves a version that is already there alone and says so. Phase client always runs: it is
# cheap next to the deploy, and what it builds is the one file that setup.sh installs and runs on the user's machine. When CodeBuild sets CODEBUILD_SOURCE_VERSION (setup.sh starts the build with --source-version, the S3 version id of its upload), the archive
# is read by that id, so the commit check is made on the bytes CodeBuild extracted; a value that is no version of the object stops the build.
#
# On a failure the script writes phases.jsonl and results/<build id>/result.json (schema "1": state, sourceCommit, configDigest, buildId, stage, region,
# account, endpoint, stackOutputs, client, phases, failedPhase, detail, deploymentDigest, action) when the bucket accepts the write; a build that CodeBuild kills (a timeout,
# a stop that comes as SIGKILL) leaves the phases written so far and no result.json. After a result that says SETUP-PASS it writes <work>/anchor.env, one line
# `export SETUP_RESULT_SHA256=<SHA-256 of the result.json it uploaded>`; the buildspec (images/codebuild/setup-runner.yml) sources it and lists the variable as an
# exported variable of the build, which CodeBuild keeps in the build's own record. `setup.sh` believes a result.json only when it hashes to that value. The last line it prints is
#   SETUP-PASS stage=<stage> digest=<12 hex of the deployment digest> endpoint=<url>      (exit 0)
#   SETUP-FAIL <phase> <detail>                                                           (exit 1)
# CodeBuild adds its own phase lines after it in the log; `setup.sh` reads result.json, which is the record.
# The generated account file reaches the build as HAMH_ACCOUNTS_FILE. infra/lib/config.ts and crates/harness-protocol/build.rs read the account file through that variable.
# For a revision without this variable, the script copies the file to accounts.json. The script commits the file on top of the archive.
# Test hooks: RUNNER_BUILD_WORK (the work folder; default beside the source folder; the script uses its physical path), RUNNER_BUILD_NO_SKIP=1 (skip nothing), RUNNER_BUILD_UPLOAD_SLEEP
# (seconds between the three tries of a write to the bucket; default 5).
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
STATE="$HERE/runner_state.py"
NODE_IMAGE_MIRROR="public.ecr.aws/docker/library"
SKIPPABLE=" deploy gomod "
STACKS="network security data packages control orchestration runtimes delivery observability"
STACK_PROBLEM=""
SETTING_ASSIGNS=()
CLIENT_ONLY="${SETUP_CLIENT_ONLY:-}"
STAGE_UNCHANGED=0
FAILED_STEP=""
PY=""
FINISHED=0
BUILD_ID="${CODEBUILD_BUILD_ID:-}"
REGION="${AWS_REGION:-}"
STAGE="${SETUP_STAGE:-direct}"
AZS="${SETUP_AZS:-1}"
ACTION="${SETUP_ACTION:-deploy}"
ACCOUNT=""
BUCKET=""
COMMIT=""
CONFIG_DIGEST=""
CUR_PHASE=""
DETAIL=""
DEPLOY_DIGEST12=""
ENDPOINT=""
LAST_LINE=""
PREV_LOADED=0
PREV_TABLE=""
CLIENT_FILE=""
TEMPLATE_KEY=""
TEMPLATE_SHA=""
TEMPLATE_BYTES=""
OUT_FILES=()
PREV_FILES=()

say() { printf 'setup-runner: %s\n' "$*"; }

# ---- the end of the script: one place prints the final line ----
die_early() { # <phase> <detail>: a failure before results can be written
  printf 'SETUP-FAIL %s %s\n' "$1" "$2"
  FINISHED=1
  exit 1
}

# shellcheck disable=SC2317
on_exit() {
  local rc=$?
  if [ "$FINISHED" != 1 ]; then
    FINISHED=1
    fail_run "${CUR_PHASE:-inputs}" "the script ended unexpectedly (exit $rc)"
    exit 1
  fi
}
trap on_exit EXIT
trap 'FINISHED=0; exit 143' TERM
trap 'FINISHED=0; exit 130' INT

# ---- helpers ----
pick_python() {
  local c
  if [ -n "${HAMH_PYTHON:-}" ]; then
    if "$HAMH_PYTHON" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' >/dev/null 2>&1; then PY="$HAMH_PYTHON"; return 0; fi
    return 1
  fi
  for c in python3.12 python3.11 python3; do
    if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' >/dev/null 2>&1; then
      PY="$(command -v "$c")"
      export HAMH_PYTHON="$PY"
      return 0
    fi
  done
  return 1
}

state() { "$PY" -I -B "$STATE" "$@"; }

last_nonblank() { # <file>: the last line that holds text, at most 300 characters
  grep -v '^[[:space:]]*$' "$1" 2>/dev/null | tail -n 1 | cut -c1-300
}

run_logged() { # <log file> <command...>: output to the console and the log; the command's exit code; LAST_LINE
  local log=$1 rc
  shift
  "$@" 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  LAST_LINE=$(last_nonblank "$log")
  return "$rc"
}

upload() { # <local file> <key>: three tries, a few seconds apart, so one S3 hiccup does not end a build that did its work
  local try=1
  while :; do
    if aws s3 cp "$1" "s3://$BUCKET/$2" --only-show-errors >"$WORK/upload.out" 2>&1; then return 0; fi
    UPLOAD_ERR=$(last_nonblank "$WORK/upload.out")
    [ "$try" -lt 3 ] || return 1
    try=$((try + 1))
    sleep "${RUNNER_BUILD_UPLOAD_SLEEP:-5}"
  done
}

upload_phases() {
  [ -n "$BUCKET" ] || return 0
  upload "$PHASES_FILE" "results/$BUILD_ID/phases.jsonl" ||
    die_early "${CUR_PHASE:-inputs}" "cannot write results/$BUILD_ID/phases.jsonl to s3://$BUCKET: $UPLOAD_ERR"
}

sts_account() {
  local out
  out=$(aws sts get-caller-identity --output json 2>"$WORK/sts.err") || { STS_ERR=$(last_nonblank "$WORK/sts.err"); return 1; }
  ACCOUNT=$(printf '%s' "$out" | state json-get Account) || return 1
}

record_phase() { # <name> <state> <seconds> <detail> [from]
  local args
  args=(phase-line --build-id "$BUILD_ID" --stage "$STAGE" --commit "$COMMIT" --digest "$CONFIG_DIGEST" --name "$1" --state "$2" --seconds "$3" --detail "$4")
  if [ -n "${5:-}" ]; then args+=(--from "$5"); fi
  if [ "$1" = deploy ] && [ -n "$DEPLOY_DIGEST12" ]; then args+=(--extra "deploymentDigest=$DEPLOY_DIGEST12"); fi
  if [ "$1" = client ] && [ "$2" != failed ]; then args+=(--extra "binary=$BINARY"); fi
  local line
  line=$(state "${args[@]}") || die_early "$1" "cannot make the phase record (runner_state.py failed)"
  printf '%s\n' "$line" >>"$PHASES_FILE" || die_early "$1" "cannot write $PHASES_FILE"
  upload_phases
}

write_result() { # <SETUP-PASS|SETUP-FAIL> <failed phase> <detail> <digest>: result.json to the bucket
  local args deployed=$4
  args=(result --state "$1" --build-id "$BUILD_ID" --stage "$STAGE" --commit "$COMMIT" --digest "$CONFIG_DIGEST" --region "$REGION" --account "$ACCOUNT"
        --endpoint "$ENDPOINT" --deployment-digest "$deployed" --phases "$PHASES_FILE" --failed-phase "$2" --detail "$3" --action "$ACTION" --failed-step "$FAILED_STEP")
  [ "$STAGE_UNCHANGED" != 1 ] || args+=(--stage-unchanged)
  local f
  for f in ${OUT_FILES[@]+"${OUT_FILES[@]}"}; do args+=(--outputs "$f"); done
  if [ -n "$CLIENT_FILE" ] && [ -f "$CLIENT_FILE" ]; then args+=(--client "$CLIENT_FILE"); fi
  if [ -n "$TEMPLATE_KEY" ]; then args+=(--bootstrap-template "$TEMPLATE_KEY" "$TEMPLATE_SHA" "$TEMPLATE_BYTES"); fi
  state "${args[@]}" >"$WORK/result.json" || return 1
  upload "$WORK/result.json" "results/$BUILD_ID/result.json" || return 1
  # The record that only this build can make: the SHA-256 of the bytes just uploaded, for the exported variable of the buildspec. It is written after the upload and
  # only for a result that passed, so a build that ended some other way leaves nothing for setup.sh to believe.
  if [ "$1" = SETUP-PASS ]; then
    printf 'export SETUP_RESULT_SHA256=%s\n' "$(sha256sum "$WORK/result.json" | cut -d' ' -f1)" >"$WORK/anchor.env" || return 1
  fi
  return 0
}

# The run failed in <phase>: record it, write result.json, print the last line, exit 1.
fail_run() { # <phase> <detail>
  local phase=$1 detail=$2 secs=0
  [ -n "${PHASE_START:-}" ] && secs=$((SECONDS - PHASE_START))
  if [ -z "$BUCKET" ] && [ -n "$REGION" ] && [ -n "$BUILD_ID" ] && [ -n "$PY" ]; then
    if sts_account; then BUCKET="hamh-setup-$ACCOUNT-$REGION"; fi
  fi
  if [ -n "$BUCKET" ] && [ -n "$PY" ]; then
    # a failure inside record_phase or write_result must not call fail_run again
    trap - EXIT
    record_phase "$phase" failed "$secs" "$detail" || true
    write_result SETUP-FAIL "$phase" "$detail" "" || say "result.json could not be written to s3://$BUCKET"
  fi
  printf 'SETUP-FAIL %s %s\n' "$phase" "$detail"
  FINISHED=1
  exit 1
}

# ---- the phases; each returns 0 or calls fail_run through `fail` ----
fail() { fail_run "$CUR_PHASE" "$1"; }

phase_inputs() {
  local re_src='^source/[0-9a-f]{40}\.zip$' re_cfg='^config/accounts-[0-9a-f]{12}\.json$' re_bin='^[a-z][a-z0-9-]{0,30}$' re_sha='^[0-9a-f]{64}$'
  [ "$ACTION" = deploy ] || [ "$ACTION" = bootstrap-template ] || fail "SETUP_ACTION must be deploy or bootstrap-template, not '$ACTION'"
  [ "$STAGE" = direct ] || fail "SETUP_STAGE must be direct, not $STAGE"
  [ "$AZS" = 1 ] || fail "SETUP_AZS must be 1, not $AZS"
  [[ ${SETUP_SOURCE_KEY:-} =~ $re_src ]] || fail "SETUP_SOURCE_KEY must be source/<40 hex commit>.zip, not '${SETUP_SOURCE_KEY:-}'"
  [[ ${SETUP_CONFIG_KEY:-} =~ $re_cfg ]] || fail "SETUP_CONFIG_KEY must be config/accounts-<12 hex>.json, not '${SETUP_CONFIG_KEY:-}'"
  [[ $BINARY =~ $re_bin ]] || fail "SETUP_BINARY_NAME must be a lowercase name, not '$BINARY'"
  [[ ${SETUP_SOURCE_SHA256:-} =~ $re_sha ]] || fail "SETUP_SOURCE_SHA256 must be the 64 hexadecimal digits of the SHA-256 of the source archive, not '${SETUP_SOURCE_SHA256:-}'"
  [[ ${SETUP_CONFIG_SHA256:-} =~ $re_sha ]] || fail "SETUP_CONFIG_SHA256 must be the 64 hexadecimal digits of the SHA-256 of the account file, not '${SETUP_CONFIG_SHA256:-}'"
  [[ $SETUP_CONFIG_SHA256 == "$CONFIG_DIGEST"* ]] || fail "SETUP_CONFIG_KEY names digest $CONFIG_DIGEST, which is not the start of SETUP_CONFIG_SHA256"
  [ -z "${SETUP_DEPLOY_LOCK_TOKEN:-}" ] || [[ $SETUP_DEPLOY_LOCK_TOKEN =~ ^[0-9a-f]{32}$ ]] || fail "SETUP_DEPLOY_LOCK_TOKEN must be 32 hexadecimal digits, not '$SETUP_DEPLOY_LOCK_TOKEN'"
  case $CLIENT_ONLY in ''|0|1) ;; *) fail "SETUP_CLIENT_ONLY must be 1 or empty, not '$CLIENT_ONLY'" ;; esac
  [ "$CLIENT_ONLY" != 1 ] || [ "$ACTION" = deploy ] || fail "SETUP_CLIENT_ONLY belongs to a deploy build"
  case ${SETUP_FORCE_DEPLOY:-} in ''|0|1) ;; *) fail "SETUP_FORCE_DEPLOY must be 1 or empty, not '$SETUP_FORCE_DEPLOY'" ;; esac
  [ "${SETUP_FORCE_DEPLOY:-0}" != 1 ] || [ "$CLIENT_ONLY" != 1 ] || fail "SETUP_FORCE_DEPLOY and SETUP_CLIENT_ONLY do not go together"
  explicit_settings
  sts_account || fail "the credentials of the build do not work: ${STS_ERR:-sts get-caller-identity failed}"
  BUCKET="hamh-setup-$ACCOUNT-$REGION"
  if [ "$ACTION" = bootstrap-template ]; then
    # this build exists because the account has no bootstrap
    DETAIL="account $ACCOUNT, CDK bootstrap template for Region $REGION"
    return 0
  fi
  local out
  out=$(aws ssm get-parameter --name /cdk-bootstrap/hnb659fds/version --output json 2>"$WORK/ssm.err") ||
    fail "no CDK bootstrap in account $ACCOUNT and Region $REGION (SSM /cdk-bootstrap/hnb659fds/version: $(last_nonblank "$WORK/ssm.err")); setup.sh creates it before the runner starts"
  DETAIL="account $ACCOUNT, CDK bootstrap version $(printf '%s' "$out" | state json-get Parameter.Value)"
  [ "${#SETTING_ASSIGNS[@]}" = 0 ] || DETAIL="$DETAIL, settings ${SETTING_ASSIGNS[*]}"
  [ "$CLIENT_ONLY" != 1 ] || DETAIL="$DETAIL, client only"
  [ "${SETUP_FORCE_DEPLOY:-0}" != 1 ] || DETAIL="$DETAIL, force deploy"
  return 0
}

# explicit_settings: SETUP_EXPLICIT_SETTINGS into SETTING_ASSIGNS (HAMH_<NAME>=value words for `env`), checked against the registry of the archive (scripts/lib/stage-settings.tsv:
# NAME|KEPT|CHANGE|PATTERN|NOTE, `#` comments; a PATTERN may hold `|`, so it is everything between the third field and the last one). Nothing else may be set this way.
explicit_settings() {
  local text line name value re registry="$ROOT/scripts/lib/stage-settings.tsv" seen=" "
  SETTING_ASSIGNS=()
  [ -n "${SETUP_EXPLICIT_SETTINGS:-}" ] || return 0
  [ -f "$registry" ] || fail "SETUP_EXPLICIT_SETTINGS is set, and this revision has no scripts/lib/stage-settings.tsv, so it cannot take stage settings"
  text=$("$PY" -I -B -c 'import base64, binascii, sys
try:
    sys.stdout.write(base64.b64decode(sys.argv[1], validate=True).decode("ascii"))
except (binascii.Error, UnicodeDecodeError, ValueError):
    sys.exit(3)' "$SETUP_EXPLICIT_SETTINGS") || fail "SETUP_EXPLICIT_SETTINGS is not base64 of ASCII lines"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%=*}
    value=${line#*=}
    [[ $line == *=* ]] && [[ $name =~ ^HAMH_[A-Z0-9_]+$ ]] || fail "SETUP_EXPLICIT_SETTINGS holds a line that is not HAMH_<NAME>=value: '$line'"
    case $seen in *" $name "*) fail "SETUP_EXPLICIT_SETTINGS names $name twice" ;; esac
    seen="$seen$name "
    re=$(awk -v n="$name" 'BEGIN { FS = "|" } /^[[:space:]]*#/ || NF < 5 { next } $1 == n { p = $4; for (i = 5; i < NF; i++) p = p "|" $i; print p; found = 1; exit } END { exit found ? 0 : 1 }' "$registry") ||
      fail "$name is not a stage setting of this revision (scripts/lib/stage-settings.tsv)"
    if [ "$value" != default ] && ! [[ $value =~ $re ]]; then
      fail "the value '$value' of $name does not match its pattern $re (scripts/lib/stage-settings.tsv)"
    fi
    SETTING_ASSIGNS+=("$name=$value")
  done <<SETTINGS
$text
SETTINGS
  return 0
}

phase_fetch() {
  CONFIG_LOCAL="$WORK/accounts-in.json"
  aws s3 cp "s3://$BUCKET/$SETUP_CONFIG_KEY" "$CONFIG_LOCAL" --only-show-errors >"$WORK/fetch.out" 2>&1 ||
    fail "cannot read $SETUP_CONFIG_KEY (SETUP_CONFIG_KEY) from s3://$BUCKET: $(last_nonblank "$WORK/fetch.out")"
  local got
  got=$(sha256sum "$CONFIG_LOCAL" | cut -d' ' -f1)
  [ "${got:0:12}" = "$CONFIG_DIGEST" ] || fail "config digest ${got:0:12} is not the $CONFIG_DIGEST of $SETUP_CONFIG_KEY: the file in the bucket is not the one setup.sh named"
  [ "$got" = "$SETUP_CONFIG_SHA256" ] || fail "config SHA-256 $got is not the SETUP_CONFIG_SHA256 $SETUP_CONFIG_SHA256 setup.sh sent: the file in the bucket is not the one setup.sh uploaded"
  local ver="${CODEBUILD_SOURCE_VERSION:-}"
  if [ -n "$ver" ] && [ "$ver" != null ]; then
    # CodeBuild extracted this version of the object: the commit check is made on the same bytes.
    aws s3api get-object --bucket "$BUCKET" --key "$SETUP_SOURCE_KEY" --version-id "$ver" --output json "$WORK/source.zip" >"$WORK/fetch.out" 2>&1 ||
      fail "cannot read version $ver of $SETUP_SOURCE_KEY (SETUP_SOURCE_KEY) from s3://$BUCKET: $(last_nonblank "$WORK/fetch.out")"
  else
    aws s3 cp "s3://$BUCKET/$SETUP_SOURCE_KEY" "$WORK/source.zip" --only-show-errors >"$WORK/fetch.out" 2>&1 ||
      fail "cannot read $SETUP_SOURCE_KEY (SETUP_SOURCE_KEY) from s3://$BUCKET: $(last_nonblank "$WORK/fetch.out")"
  fi
  got=$(sha256sum "$WORK/source.zip" | cut -d' ' -f1)
  [ "$got" = "$SETUP_SOURCE_SHA256" ] || fail "source SHA-256 $got is not the SETUP_SOURCE_SHA256 $SETUP_SOURCE_SHA256 setup.sh sent: the archive in the bucket is not the one setup.sh uploaded"
  local named
  named=$(state zip-comment "$WORK/source.zip" 2>"$WORK/fetch.out") || fail "source commit: $(last_nonblank "$WORK/fetch.out")"
  [ "$named" = "$COMMIT" ] || fail "source commit: the archive names '${named:-nothing}', SETUP_SOURCE_KEY names $COMMIT"
  cd "$ROOT" || fail "cannot enter $ROOT"
  GIT_RUN=(git -c user.name="setup runner" -c user.email="setup-runner@localhost" -c commit.gpgsign=false -c core.autocrlf=false -c core.safecrlf=false)
  { "${GIT_RUN[@]}" init -q && "${GIT_RUN[@]}" add -A && "${GIT_RUN[@]}" commit -q --allow-empty -m "source archive $COMMIT"; } >"$WORK/git.out" 2>&1 ||
    fail "git could not make a repository of the archive: $(last_nonblank "$WORK/git.out")"
  DETAIL="archive of $COMMIT, config $CONFIG_DIGEST"
  return 0
}

phase_toolchain() {
  TOOLS="$WORK/tools"
  local -a tc_env=()
  [ "$ACTION" != bootstrap-template ] || tc_env=(RUNNER_TOOLCHAIN_ONLY=template)  # the AWS CLI and the Docker daemon: no Rust and no Go for a build that prints one template
  run_logged "$WORK/toolchain.log" env ${tc_env[@]+"${tc_env[@]}"} bash "$ROOT/scripts/setup/runner-toolchain.sh" "$TOOLS" ||
    fail "${LAST_LINE:-runner-toolchain.sh failed}"
  [ -f "$TOOLS/env.sh" ] || fail "runner-toolchain.sh wrote no $TOOLS/env.sh"
  # shellcheck disable=SC1091
  . "$TOOLS/env.sh"
  export NODE_SH_TOOLS="${HAMH_TOOLS:-$TOOLS}"
  export NODE_SH_TMP="$WORK/node-tmp"
  export NODE_SH_IMAGE_MIRROR="${NODE_SH_IMAGE_MIRROR:-$NODE_IMAGE_MIRROR}"
  mkdir -p "$NODE_SH_TMP" || fail "cannot make $NODE_SH_TMP"
  DETAIL="$LAST_LINE"
  return 0
}

# The probe looks for the name of the setting, not for one spelling of the variable: the CDK app and the Rust build script read it under either prefix (COMPAT-CAH).
accounts_override_supported() {
  grep -q ACCOUNTS_FILE "$ROOT/infra/lib/config.ts" 2>/dev/null && grep -q ACCOUNTS_FILE "$ROOT/crates/harness-protocol/build.rs" 2>/dev/null
}

phase_account_file() {
  local file="$WORK/account/accounts.json" shell_out target logging cfg
  mkdir -p "$WORK/account" || fail "cannot make $WORK/account"
  cp "$CONFIG_LOCAL" "$file" || fail "cannot copy the accounts file to $file"
  chmod 0644 "$file" || fail "cannot set the mode of $file"
  shell_out=$(HAMH_ACCOUNTS_FILE="$file" HAMH_AWS_PROFILE="" CAH_AWS_PROFILE="" CAH_ACCOUNTS_FILE="" "$PY" -I -B "$ROOT/scripts/lib/aws_target.py" shell 2>&1) ||  # COMPAT-CAH
    fail "the accounts file is not one the scripts accept: $(printf '%s' "$shell_out" | tail -n 1 | cut -c1-300)"
  F_ACCOUNT=$(printf '%s\n' "$shell_out" | sed -n 's/^HAMH_ACCOUNT=//p')
  F_REGION=$(printf '%s\n' "$shell_out" | sed -n 's/^HAMH_REGION=//p')
  F_PROFILE=$(printf '%s\n' "$shell_out" | sed -n 's/^HAMH_PROFILE=//p')
  [ "$F_ACCOUNT" = "$ACCOUNT" ] || fail "the account of the accounts file ($F_ACCOUNT) does not match the account of the credentials ($ACCOUNT)"
  [ "$F_REGION" = "$REGION" ] || fail "the Region of the accounts file ($F_REGION) is not the Region of the build ($REGION)"
  logging=$(HAMH_ACCOUNTS_FILE="$file" HAMH_AWS_PROFILE="" CAH_AWS_PROFILE="" CAH_ACCOUNTS_FILE="" "$PY" -I -B "$ROOT/scripts/lib/aws_target.py" get invocationLogging 2>&1) ||  # COMPAT-CAH
    fail "the accounts file is not one the scripts accept: $(printf '%s' "$logging" | tail -n 1 | cut -c1-300)"
  [ "$logging" = read-only ] ||
    fail "invocationLogging is $logging in the accounts file; the runner role cannot write the Bedrock invocation-logging setting, so the file must say read-only"
  export HAMH_ACCOUNTS_FILE="$file"
  unset HAMH_AWS_PROFILE CAH_AWS_PROFILE CAH_ACCOUNTS_FILE  # COMPAT-CAH
  # The container credentials of CodeBuild answer to the profile name the accounts file carries, for the scripts that name a profile and for node.sh.
  mkdir -p "$WORK/aws" || fail "cannot make $WORK/aws"
  cfg="$WORK/aws/config"
  {
    if [ "$F_PROFILE" = default ]; then printf '[default]\n'; else printf '[profile %s]\n' "$F_PROFILE"; fi
    printf 'credential_source = EcsContainer\nregion = %s\n' "$REGION"
  } >"$cfg" || fail "cannot write $cfg"
  export AWS_CONFIG_FILE="$cfg"
  if accounts_override_supported; then
    DETAIL="account file $CONFIG_DIGEST through HAMH_ACCOUNTS_FILE, profile $F_PROFILE"
  else
    target="$ROOT/accounts.json"
    cp "$file" "$target" || fail "cannot copy the accounts file to $target"
    { "${GIT_RUN[@]}" -C "$ROOT" add accounts.json && "${GIT_RUN[@]}" -C "$ROOT" commit -q --allow-empty -m "setup: account file $CONFIG_DIGEST"; } >"$WORK/git.out" 2>&1 ||
      fail "git could not commit the account file: $(last_nonblank "$WORK/git.out")"
    DETAIL="The account file $CONFIG_DIGEST is copied to accounts.json and committed because this repository revision does not read HAMH_ACCOUNTS_FILE. Your profile is $F_PROFILE."
  fi
  if [ "$ACTION" = bootstrap-template ]; then return 0; fi
  # One run of the CDK CLI proves the Docker daemon, the pinned Node image, the credentials the container reaches and the install of pnpm and the CDK at their pinned integrity.
  if ! bash "$ROOT/scripts/node.sh" cdk --version >"$WORK/node.out" 2>&1; then
    fail "scripts/node.sh could not run the CDK: $(last_nonblank "$WORK/node.out")"
  fi
  return 0
}

phase_deploy() {
  local -a assigns=(${SETTING_ASSIGNS[@]+"${SETTING_ASSIGNS[@]}"})
  mkdir -p "$WORK/records"
  export DEPLOY_RECORD_DIR="$WORK/records"
  [ -z "${SETUP_DEPLOY_LOCK_TOKEN:-}" ] || assigns+=("HAMH_DEPLOY_LOCK_TOKEN=$SETUP_DEPLOY_LOCK_TOKEN")
  # the commit the user asked for (the 40 hex of SETUP_SOURCE_KEY): deploy.py writes it as commit= of the stage's record in place of the HEAD of this repository
  assigns+=("HAMH_SOURCE_COMMIT=$COMMIT")
  run_logged "$WORK/deploy.log" env ${assigns[@]+"${assigns[@]}"} bash "$ROOT/scripts/deploy.sh" --stage "$STAGE" --azs "$AZS" --fork-source build
  local rc=$? re_pass='^DEPLOY-PASS stage=[a-z]+ digest=([0-9a-f]{12}) ' re_fail='^DEPLOY-FAIL (.*)$' re_refused='^DEPLOY-REFUSED (reason=([a-z0-9-]+) wait=(yes|no) stage=unchanged :: .+)$' refused failed
  upload_records
  if [ "$rc" = 3 ]; then
    # refused before its first stack change: the stage is unchanged (CONTRACT C5)
    refused=$(grep -E '^DEPLOY-REFUSED ' "$WORK/deploy.log" | tail -n 1 | cut -c1-600)
    if [[ $refused =~ $re_refused ]]; then STAGE_UNCHANGED=1; FAILED_STEP=${BASH_REMATCH[2]}; fail_run refused "${BASH_REMATCH[1]}"; fi
  fi
  if [ "$rc" != 0 ]; then
    # the failure line is the last DEPLOY-FAIL line; for a step that failed before the first stack change deploy.py prints `DEPLOY-STAGE unchanged` on a line of its own
    # before it (A2.3). The whole log is searched for either line, so their order does not matter.
    failed=$(grep -E '^DEPLOY-FAIL ' "$WORK/deploy.log" | tail -n 1 | cut -c1-600)
    ! grep -qx 'DEPLOY-STAGE unchanged' "$WORK/deploy.log" || STAGE_UNCHANGED=1
    if [[ $failed =~ $re_fail ]]; then
      FAILED_STEP=${BASH_REMATCH[1]%% *}
      # the deploy guard (runs are active and pin the worker runtime) is its own phase: setup.sh reads failedPhase guard as exit 7
      if [[ ${BASH_REMATCH[1]} == guard\ * ]]; then STAGE_UNCHANGED=1; fail_run guard "${BASH_REMATCH[1]#guard }"; fi
      fail "${BASH_REMATCH[1]}"
    fi
    fail "deploy.sh exit $rc: ${LAST_LINE:-no output}"
  fi
  if [[ $LAST_LINE =~ $re_pass ]]; then
    DEPLOY_DIGEST12="${BASH_REMATCH[1]}"
    DETAIL="$LAST_LINE"
    return 0
  fi
  fail "deploy.sh printed no DEPLOY-PASS line (last line: ${LAST_LINE:-none})"
}

upload_records() {
  local f
  for f in "$WORK"/records/*.json; do
    [ -f "$f" ] || continue
    upload "$f" "results/$BUILD_ID/deploy-records/$(basename "$f")" || true
  done
}

phase_template() {
  local pin="$ROOT/engine/policy/cdk-bootstrap-template.pin" want out="$WORK/bootstrap" file size sha key
  [ -f "$pin" ] || fail "engine/policy/cdk-bootstrap-template.pin is missing from the source archive"
  want=$(sed -n 's/^sha256=//p' "$pin" | head -n 1)
  [[ $want =~ ^[0-9a-f]{64}$ ]] || fail "engine/policy/cdk-bootstrap-template.pin holds no SHA-256 (a line sha256=<64 hex digits>)"
  mkdir -p "$out" || fail "cannot make $out"
  file="$out/cdk-bootstrap.yaml"
  if ! bash "$ROOT/scripts/node.sh" cdk bootstrap --show-template >"$file" 2>"$WORK/template.err"; then
    fail "the CDK could not print the bootstrap template: $(last_nonblank "$WORK/template.err")"
  fi
  size=$(wc -c <"$file" | tr -d ' ')
  [ "$size" -gt 0 ] || fail "the CDK printed no bootstrap template"
  [ "$size" -le 51200 ] || fail "the bootstrap template is $size bytes, more than the 51,200 bytes that the AWS CLI deploys without a staging bucket"
  sha=$(sha256sum "$file" | cut -d' ' -f1)
  [ "$sha" = "$want" ] || fail "the bootstrap template has SHA-256 $sha; engine/policy/cdk-bootstrap-template.pin pins $want"
  key="bootstrap/cdk-bootstrap-${sha:0:12}.yaml"
  upload "$file" "$key" || fail "cannot write $key to s3://$BUCKET: $UPLOAD_ERR"
  TEMPLATE_KEY=$key TEMPLATE_SHA=$sha TEMPLATE_BYTES=$size
  DETAIL="CDK bootstrap template of $size bytes, SHA-256 ${sha:0:12}, at s3://$BUCKET/$key"
  return 0
}

phase_gomod() {
  export GOMOD_STORE_GO="${HAMH_TOOLS:-$WORK/tools}/go/bin/go"
  # --reuse-current: a store whose newest manifest was written by a fill with these modules, tool roots and Go version already holds everything the fill would put in it, so an
  # update that changes none of them ends the phase here, in seconds. A fresh store, or one with other modules, is filled and checked in full.
  run_logged "$WORK/gomod.log" bash "$ROOT/scripts/gomod-store.sh" fill --stage "$STAGE" --modules engine/rules/deps/go-approved-modules.txt \
    --tools scripts/gomod-tool-roots.txt --jobs 8 --reuse-current || fail "${LAST_LINE:-gomod-store.sh fill failed}"
  if [[ $LAST_LINE == GOMOD-FILL\ CURRENT* ]]; then
    DETAIL="$LAST_LINE"
    return 0
  fi
  [[ $LAST_LINE == GOMOD-FILL\ PASS* ]] || fail "gomod-store.sh fill did not end in GOMOD-FILL PASS or GOMOD-FILL CURRENT: $LAST_LINE"
  run_logged "$WORK/gomod-check.log" bash "$ROOT/scripts/gomod-store.sh" check --stage "$STAGE" || fail "${LAST_LINE:-gomod-store.sh check failed}"
  [[ $LAST_LINE == GOMOD-CHECK\ PASS* ]] || fail "gomod-store.sh check did not end in GOMOD-CHECK PASS: $LAST_LINE"
  DETAIL="$LAST_LINE"
  return 0
}

phase_scanner_rules() {
  local out="$WORK/rules" fetch="$WORK/rules-fetch.$$" wheel
  mkdir -p "$out"
  run_logged "$WORK/rules-build.log" "$PY" "$ROOT/scripts/scanner_kit_rules.py" build --out "$out" --work "$fetch" || fail "${LAST_LINE:-scanner_kit_rules.py build failed}"
  wheel=$(ls "$out"/hamh_semgrep_rules-*-py3-none-any.whl 2>/dev/null | head -n 1)
  [ -n "$wheel" ] || fail "scanner_kit_rules.py build wrote no wheel to $out"
  run_logged "$WORK/rules-publish.log" "$PY" "$ROOT/scripts/scanner_kit_rules.py" publish --wheel "$wheel" --apply || fail "${LAST_LINE:-scanner_kit_rules.py publish failed}"
  DETAIL="$LAST_LINE"
  return 0
}

phase_client() {
  local out="$WORK/client" raw="$WORK/client-out.json" rc line admin_line key platform bytes sha
  [ -f "$ROOT/scripts/setup/build-client.sh" ] || fail "scripts/setup/build-client.sh is missing from the source archive"
  mkdir -p "$out"
  # build-client.sh takes the client name from names.sh, which an exported HAMH_CLIENT_BIN overrides: it builds the binary this build was asked for, and the
  # operator tool is named after it (the build environment carries one name, SETUP_BINARY_NAME)
  export SETUP_BINARY_NAME="$BINARY" HAMH_CLIENT_BIN="$BINARY" HAMH_ADMIN_BIN="$BINARY-admin"
  bash "$ROOT/scripts/setup/build-client.sh" "$out" "$BUCKET" "$BUILD_ID" >"$raw" 2>"$WORK/client.err"
  rc=$?
  cat "$WORK/client.err"
  if [ "$rc" != 0 ]; then fail "build-client.sh exit $rc: $(last_nonblank "$WORK/client.err")"; fi
  line=$(state client-check "$raw" 2>"$WORK/client.err") || fail "build-client.sh printed no valid client section: $(last_nonblank "$WORK/client.err")"
  if ! aws s3api head-object --bucket "$BUCKET" --key "client/$BUILD_ID/SHA256SUMS" --output json >/dev/null 2>&1; then
    if [ -f "$out/SHA256SUMS" ]; then
      upload "$out/SHA256SUMS" "client/$BUILD_ID/SHA256SUMS" || fail "cannot write client/$BUILD_ID/SHA256SUMS: $UPLOAD_ERR"
    else
      fail "build-client.sh left no SHA256SUMS (client/$BUILD_ID/SHA256SUMS is not in the bucket)"
    fi
  fi
  while IFS="$(printf '\t')" read -r platform key bytes sha; do
    [ -n "$platform" ] || continue
    client_file_ok "$key" "$bytes" "$sha" || fail "the client file $key of $platform is not in the bucket with the size and SHA-256 build-client.sh reported"
  done <<CLIENT_LINES
$line
CLIENT_LINES
  admin_line=$(state client-check --part admin "$raw" 2>"$WORK/client.err") || fail "build-client.sh printed no valid client section: $(last_nonblank "$WORK/client.err")"
  while IFS="$(printf '\t')" read -r platform key bytes sha; do
    [ -n "$platform" ] || continue
    client_file_ok "$key" "$bytes" "$sha" || fail "the operator tool file $key of $platform is not in the bucket with the size and SHA-256 build-client.sh reported"
  done <<ADMIN_LINES
$admin_line
ADMIN_LINES
  cp "$raw" "$WORK/client.json"
  CLIENT_FILE="$WORK/client.json"
  DETAIL="client and operator tool for $(printf '%s' "$line" | cut -f1 | tr '\n' ' ')"
  return 0
}

client_file_ok() { # <key> <bytes> <sha256>: the object is in the bucket with this size and this SHA-256 (read back from the bucket)
  local f="$WORK/verify.bin" got
  aws s3 cp "s3://$BUCKET/$1" "$f" --only-show-errors >/dev/null 2>&1 || return 1
  [ "$(wc -c <"$f" | tr -d ' ')" = "$2" ] || return 1
  got=$(sha256sum "$f" | cut -d' ' -f1)
  [ "$got" = "$3" ]
}

live_param() { # <name>: the value of /hamh/<stage>/<name> in the account (what is deployed there now); a failure and nothing when the account has none
  local out
  out=$(aws ssm get-parameter --name "/hamh/$STAGE/$1" --output json 2>/dev/null) || return 1
  printf '%s' "$out" | state json-get Parameter.Value 2>/dev/null
}

source_tree() { # the SHA-256 that deploy.sh records as /hamh/<stage>/deployed-source-tree for the tree it deploys (scripts/lib/digests.py)
  "$PY" -I -B -c 'import sys; from pathlib import Path; sys.path.insert(0, sys.argv[1]); import digests; print(digests.source_tree(Path(sys.argv[2])))' "$ROOT/scripts/lib" "$ROOT" 2>/dev/null
}

# stacks_complete [after-deploy]: 0 when all nine stacks of the stage exist and are complete; else STACK_PROBLEM names the first that is not.
# Complete is CREATE_COMPLETE, UPDATE_COMPLETE or IMPORT_COMPLETE, the words the wizard's pf_stage (preflight.sh) counts, plus, after a deploy of this build,
# UPDATE_ROLLBACK_COMPLETE: a deploy that has nothing to change in a stack whose earlier update rolled back leaves the status as it was, and the stack holds
# the state from before that update, which is the template. Without the argument UPDATE_ROLLBACK_COMPLETE does not count: it tells can_skip that an update
# rolled back after the deploy the record names, so the deploy has to run again. Every other state is refused, UPDATE_ROLLBACK_FAILED, ROLLBACK_COMPLETE,
# every *_FAILED, *_IN_PROGRESS and DELETE_* state included. scripts/tests/test_setup_contract.py runs the wizard and this function over every state.
stacks_complete() {
  local mode=${1:-} stack status
  for stack in $STACKS; do
    status=$(aws cloudformation describe-stacks --stack-name "hamh-$STAGE-$stack" --output json 2>"$WORK/cfn.err" | state json-get Stacks.0.StackStatus 2>/dev/null) || {
      STACK_PROBLEM="the stack hamh-$STAGE-$stack cannot be read: $(last_nonblank "$WORK/cfn.err")"
      return 1
    }
    case "$status" in
      CREATE_COMPLETE | UPDATE_COMPLETE | IMPORT_COMPLETE) ;;
      UPDATE_ROLLBACK_COMPLETE)
        [ "$mode" = after-deploy ] || { STACK_PROBLEM="the stack hamh-$STAGE-$stack is $status: an update rolled back after the earlier deploy"; return 1; } ;;
      *) STACK_PROBLEM="the stack hamh-$STAGE-$stack is $status, not CREATE_COMPLETE, UPDATE_COMPLETE, IMPORT_COMPLETE or UPDATE_ROLLBACK_COMPLETE"; return 1 ;;
    esac
  done
  return 0
}

phase_outputs() {
  local stack out f n=0 digest ssm
  OUT_FILES=()
  mkdir -p "$WORK/outputs" || fail "cannot make $WORK/outputs"
  stacks_complete after-deploy || fail "$STACK_PROBLEM"
  for stack in $STACKS; do
    f="$WORK/outputs/$stack.json"
    out=$(aws cloudformation describe-stacks --stack-name "hamh-$STAGE-$stack" --output json 2>"$WORK/cfn.err") ||
      fail "the stack hamh-$STAGE-$stack cannot be read: $(last_nonblank "$WORK/cfn.err")"
    printf '%s' "$out" | state stack-outputs --stack "hamh-$STAGE-$stack" >"$f" 2>"$WORK/cfn.err" ||
      fail "the outputs of hamh-$STAGE-$stack cannot be read: $(last_nonblank "$WORK/cfn.err")"
    OUT_FILES+=("$f")
    n=$((n + 1))
  done
  ENDPOINT=$(state json-get-key "hamh-$STAGE-control.V1ApiUrl" <"$WORK/outputs/control.json" 2>/dev/null) ||
    fail "the stack hamh-$STAGE-control has no V1ApiUrl output"
  ssm=$(aws ssm get-parameter --name "/hamh/$STAGE/deployment-digest" --output json 2>"$WORK/ssm.err") ||
    fail "cannot read /hamh/$STAGE/deployment-digest: $(last_nonblank "$WORK/ssm.err")"
  digest=$(printf '%s' "$ssm" | state json-get Parameter.Value) || fail "/hamh/$STAGE/deployment-digest holds no value"
  DEPLOY_DIGEST12=$(printf '%s' "$digest" | cut -c1-12)
  DETAIL="$n stacks, endpoint $ENDPOINT"
  return 0
}

# ---- skipping what an earlier build finished ----
load_previous() {
  PREV_LOADED=1
  PREV_TABLE=""
  PREV_FILES=()
  [ "${RUNNER_BUILD_NO_SKIP:-0}" = 1 ] && return 0
  local listing keys key n=0 dst
  listing=$(aws s3api list-objects-v2 --bucket "$BUCKET" --prefix results/ --output json 2>/dev/null) || return 0
  keys=$(printf '%s' "$listing" | state list-keys --suffix /phases.jsonl 2>/dev/null) || return 0
  mkdir -p "$WORK/prev" || return 0
  while IFS= read -r key; do
    [ -n "$key" ] || continue
    [ "$key" = "results/$BUILD_ID/phases.jsonl" ] && continue
    n=$((n + 1))
    [ "$n" -le 200 ] || break
    dst="$WORK/prev/$n.jsonl"
    if aws s3 cp "s3://$BUCKET/$key" "$dst" --only-show-errors >/dev/null 2>&1; then PREV_FILES+=("$dst"); fi
  done <<PREV_KEYS
$keys
PREV_KEYS
  PREV_TABLE=$(state completed --stage "$STAGE" --commit "$COMMIT" --digest "$CONFIG_DIGEST" ${PREV_FILES[@]+"${PREV_FILES[@]}"} 2>/dev/null) || PREV_TABLE=""
}

prev_field() { # <phase> <column: 2 build id, 3 deployment digest, 4 binary name>
  printf '%s\n' "$PREV_TABLE" | awk -F '\t' -v p="$1" -v c="$2" '$1 == p { print $c; exit }'
}

can_skip() { # <phase>: 0 when an earlier build finished it and the account shows the result is still there; sets SKIP_FROM
  local phase=$1 from rec current tree
  [ "${RUNNER_BUILD_NO_SKIP:-0}" = 1 ] && return 1
  from=$(prev_field "$phase" 2)
  [ -n "$from" ] || return 1
  case "$phase" in
    deploy)
      # setup.sh decides from the stage's record whether a deploy is needed (SETUP_CLIENT_ONLY when it is not). A build that carries settings must hand them to deploy.sh,
      # and --force-deploy asks for the deploy: neither is skipped on the word of an earlier build.
      [ "${SETUP_FORCE_DEPLOY:-0}" != 1 ] || return 1
      [ "${#SETTING_ASSIGNS[@]}" = 0 ] || return 1
      rec=$(prev_field deploy 3)
      [ -n "$rec" ] || return 1
      current=$(live_param deployment-digest) || return 1
      [ "$(printf '%s' "$current" | cut -c1-12)" = "$rec" ] || return 1
      # The record above lies in a bucket that other principals of the account may have written. The account says what is deployed: this source tree, this account file.
      tree=$(source_tree) || return 1
      [ -n "$tree" ] && [ "$(live_param deployed-source-tree)" = "$tree" ] || return 1
      [ "$(live_param deployment-config-digest)" = "$SETUP_CONFIG_SHA256" ] || return 1
      stacks_complete || return 1
      DEPLOY_DIGEST12="$rec"
      ;;
    gomod)
      export GOMOD_STORE_GO="${HAMH_TOOLS:-$WORK/tools}/go/bin/go"
      run_logged "$WORK/gomod-skip.log" bash "$ROOT/scripts/gomod-store.sh" check --stage "$STAGE" >/dev/null || return 1
      [[ $LAST_LINE == GOMOD-CHECK\ PASS* ]] || return 1
      ;;
    *) return 1 ;;
  esac
  SKIP_FROM="$from"
  return 0
}

# ---- the client-only build ----
# phase_client_only: the stage already runs this commit (SETUP_CLIENT_ONLY=1, CONTRACT C7). The account must say so as the skip of a deploy needs it: this source tree, this
# account file and every stack complete. Then the deploy is recorded as skipped and no stack changes; gomod and scanner-rules run as in every build (each leaves a store or a
# version that is already there as it is, and finishes one that an earlier build left unfinished). Otherwise the build is refused before any change: setup.sh decided from the
# record of the stage, and the account says something else.
phase_client_only() {
  local tree current config
  CUR_PHASE=deploy
  PHASE_START=$SECONDS
  say "phase deploy: client only"
  tree=$(source_tree) || tree=""
  current=$(live_param deployed-source-tree) || current=""
  config=$(live_param deployment-config-digest) || config=""
  if [ -z "$tree" ] || [ "$current" != "$tree" ] || [ "$config" != "$SETUP_CONFIG_SHA256" ] || ! stacks_complete; then
    fail_run refused "reason=not-deployed wait=no stage=unchanged :: The stage does not run this source tree and account file (deployed-source-tree ${current:-none}, this tree ${tree:-unknown}${STACK_PROBLEM:+; $STACK_PROBLEM}), so the client alone would not match it. Run ./setup.sh --update --force-deploy to deploy it."
  fi
  # The stage runs this source tree, and a release that changes no file the stage runs still has its own VERSION. The label of the release is written alone
  # (scripts/deploy.sh --relabel: one SSM parameter, no build, no stack change), so the version the backend reports follows the release and the client it goes with.
  # deploy.py checks the record and the stage's parameters again under the lock setup holds; it never lowers a label.
  local -a assigns=("HAMH_SOURCE_COMMIT=$COMMIT")
  local rc re_refused='^DEPLOY-REFUSED (reason=([a-z0-9-]+) wait=(yes|no) stage=unchanged :: .+)$' re_label='^DEPLOY-LABEL stage=[a-z]+ version=([0-9.]+) ?(.*)$' refused failed label
  [ -z "${SETUP_DEPLOY_LOCK_TOKEN:-}" ] || assigns+=("HAMH_DEPLOY_LOCK_TOKEN=$SETUP_DEPLOY_LOCK_TOKEN")
  run_logged "$WORK/relabel.log" env "${assigns[@]}" bash "$ROOT/scripts/deploy.sh" --stage "$STAGE" --relabel
  rc=$?
  if [ "$rc" = 3 ]; then
    refused=$(grep -E '^DEPLOY-REFUSED ' "$WORK/relabel.log" | tail -n 1 | cut -c1-600)
    if [[ $refused =~ $re_refused ]]; then STAGE_UNCHANGED=1; FAILED_STEP=${BASH_REMATCH[2]}; fail_run refused "${BASH_REMATCH[1]}"; fi
  fi
  if [ "$rc" != 0 ]; then
    failed=$(grep -E '^DEPLOY-FAIL ' "$WORK/relabel.log" | tail -n 1 | cut -c1-600)
    FAILED_STEP=release-label
    fail_run deploy "release label: ${failed:-scripts/deploy.sh --relabel ended with exit $rc without a DEPLOY-FAIL line}"
  fi
  label=$(grep -E '^DEPLOY-LABEL ' "$WORK/relabel.log" | tail -n 1 | cut -c1-300)
  [[ $label =~ $re_label ]] || fail_run deploy "release label: scripts/deploy.sh --relabel ended with exit 0 and no DEPLOY-LABEL line"
  record_phase deploy skipped 0 "client only: the stage already runs this source tree and account file; release label ${BASH_REMATCH[1]}${BASH_REMATCH[2]:+ (${BASH_REMATCH[2]})}"
  DEPLOY_DIGEST12=""
}

# ---- the run ----
run_phase() { # <name> <function>
  local name=$1 func=$2
  CUR_PHASE=$name
  PHASE_START=$SECONDS
  DETAIL=""
  say "phase $name"
  case "$SKIPPABLE" in
    *" $name "*)
      [ "$PREV_LOADED" = 1 ] || load_previous
      if can_skip "$name"; then
        say "phase $name: skipped, finished by build $SKIP_FROM"
        record_phase "$name" skipped 0 "finished by an earlier build for this commit and account file" "$SKIP_FROM"
        return 0
      fi
      ;;
  esac
  "$func"
  local rc=$?
  [ "$rc" = 0 ] || fail "phase $name stopped with exit $rc"
  record_phase "$name" ok "$((SECONDS - PHASE_START))" "$DETAIL"
  say "phase $name: ok"
}

main() {
  local re_id='^[A-Za-z0-9._:-]+$' re_key='^source/([0-9a-f]{40})\.zip$' re_cfg='^config/accounts-([0-9a-f]{12})\.json$'
  [ -n "$BUILD_ID" ] || die_early inputs "CODEBUILD_BUILD_ID is not set"
  [[ $BUILD_ID =~ $re_id ]] || die_early inputs "CODEBUILD_BUILD_ID '$BUILD_ID' is not a build id"
  [ -n "$REGION" ] || die_early inputs "AWS_REGION is not set"
  WORK="${RUNNER_BUILD_WORK:-$(dirname "$ROOT")/hamh-setup-work}"
  mkdir -p "$WORK/tmp" || die_early inputs "cannot make the work folder $WORK"
  # The buildspec names the folder `$CODEBUILD_SRC_DIR/../hamh-setup-work`. Every path made from it (the account file, the profile config, the records, the folders of
  # scripts/node.sh) reaches programs that compare paths, so the folder is made a physical path (no `..`, no link, absolute) once, here, before any of them is made.
  WORK=$(cd "$WORK" && pwd -P) || die_early inputs "cannot enter the work folder $WORK"
  export TMPDIR="$WORK/tmp"
  PHASES_FILE="$WORK/phases.jsonl"
  : >"$PHASES_FILE"
  pick_python || die_early inputs "no Python 3.11 or later (HAMH_PYTHON, python3.12, python3.11 or python3); deploy.sh reads TOML with tomllib"
  [ -f "$HERE/names.sh" ] || die_early inputs "scripts/setup/names.sh is missing from the source archive"
  # shellcheck disable=SC1091
  . "$HERE/names.sh"
  BINARY="${SETUP_BINARY_NAME:-$HAMH_CLIENT_BIN}"
  [[ ${SETUP_SOURCE_KEY:-} =~ $re_key ]] && COMMIT="${BASH_REMATCH[1]}"
  [[ ${SETUP_CONFIG_KEY:-} =~ $re_cfg ]] && CONFIG_DIGEST="${BASH_REMATCH[1]}"
  say "build $BUILD_ID, action $ACTION, stage $STAGE, commit ${COMMIT:-unknown}, config ${CONFIG_DIGEST:-unknown}, Region $REGION"

  CUR_PHASE=inputs
  run_phase inputs phase_inputs
  run_phase fetch phase_fetch
  run_phase toolchain phase_toolchain
  run_phase account-file phase_account_file
  if [ "$ACTION" = bootstrap-template ]; then
    run_phase template phase_template
    CUR_PHASE=template
    trap - EXIT
    write_result SETUP-PASS "" "" "" || die_early template "cannot write results/$BUILD_ID/result.json to s3://$BUCKET: ${UPLOAD_ERR:-failed}"
    printf 'SETUP-PASS stage=%s action=bootstrap-template template=%s\n' "$STAGE" "${TEMPLATE_SHA:0:12}"
    FINISHED=1
    exit 0
  fi
  if [ "$CLIENT_ONLY" = 1 ]; then phase_client_only; else run_phase deploy phase_deploy; fi
  run_phase gomod phase_gomod
  run_phase scanner-rules phase_scanner_rules
  run_phase client phase_client
  run_phase outputs phase_outputs
  CUR_PHASE=outputs
  trap - EXIT
  write_result SETUP-PASS "" "" "$DEPLOY_DIGEST12" || die_early outputs "cannot write results/$BUILD_ID/result.json to s3://$BUCKET: ${UPLOAD_ERR:-failed}"
  printf 'SETUP-PASS stage=%s digest=%s endpoint=%s\n' "$STAGE" "$DEPLOY_DIGEST12" "$ENDPOINT"
  FINISHED=1
  exit 0
}

main "$@"
