#!/usr/bin/env bash
# Run a CodeBuild ARM build of the committed HEAD (project hamh-direct-image-build, in the runtimes stack of stage direct).
# usage: scripts/codebuild-run.sh --buildspec <path in repo> [--env NAME=VALUE]... [--stage direct]
#
# Zips `git archive HEAD`, uploads it to s3://hamh-<stage>-harness-<account>/builds/source/<sha>.zip,
# starts the build with that source, the buildspec override and SOURCE_SHA plus every --env,
# then polls to completion. The last stdout line is
#   CODEBUILD PASS id=<build id> sha=<sha7> seconds=<n>        (exit 0)
#   CODEBUILD FAIL id=<build id> status=<status>               (exit 1, after the log tail)
# Refuses uncommitted changes to tracked files: a build must match a commit (exit 2).
# A generated account file is a separate input of the build and is
# never written into the tree: the zip gets it as one more member, `.hamh-build/accounts.json` (scripts/lib/source_bundle.py), under the key
# builds/source/<sha>-<first 12 hex of its SHA-256>.zip, and the build gets HAMH_ACCOUNTS_FILE_SHA256=<the SHA-256>, which
# scripts/lib/child-build-env.sh (the first command of every buildspec) checks before it exports HAMH_ACCOUNTS_FILE inside the build.
# Unset or empty, the zip is `git archive HEAD` alone under builds/source/<sha>.zip and the build gets no such variable.
# Every answer of the AWS CLI is read with the interpreter lib/python.sh picked ($py: HAMH_PYTHON, else the first Python 3.8+ on the PATH), so a machine whose
# only Python is python3.12 works. A start that names no build ends the run at once (exit 1). The wait for the build is bounded: CODEBUILD_MAX_WAIT_S seconds
# (default 18000, 5 hours; the project's own limit is 3 hours of build and 1 hour in the queue) and CODEBUILD_MAX_ERRORS failed looks in a row (default 30);
# either limit ends the run with exit 1 and a message, and the build itself keeps running in CodeBuild.
# Test hooks: CODEBUILD_AWS_BIN, CODEBUILD_ROOT, CODEBUILD_TMP, CODEBUILD_POLL_S, CODEBUILD_MAX_WAIT_S, CODEBUILD_MAX_ERRORS.
set -uo pipefail
usage() { echo "usage: codebuild-run.sh --buildspec <path> [--env NAME=VALUE]... [--stage direct]" >&2; exit 2; }

here="$(cd "$(dirname "$0")" && pwd)"
root="${CODEBUILD_ROOT:-$(cd "$here/.." && pwd)}"
aws_bin="${CODEBUILD_AWS_BIN:-aws}"
# The accounts.json entry HAMH_AWS_PROFILE selects, else the active one: its account, home
# Region and profile, never the instance profile of the machine that runs this script.
# shellcheck source=lib/python.sh
. "$here/lib/python.sh"
# shellcheck source=lib/aws-target.sh
. "$here/lib/aws-target.sh"
poll="${CODEBUILD_POLL_S:-20}"
max_wait="${CODEBUILD_MAX_WAIT_S:-18000}"
max_errors="${CODEBUILD_MAX_ERRORS:-30}"
case "$max_wait$max_errors" in
  ''|*[!0-9]*) echo "codebuild-run: CODEBUILD_MAX_WAIT_S and CODEBUILD_MAX_ERRORS must be whole numbers" >&2; exit 2 ;;
esac
account=$HAMH_ACCOUNT
region=$HAMH_REGION
stage=direct buildspec="" envs=()
while [ $# -gt 0 ]; do
  case "$1" in
    --buildspec) [ $# -ge 2 ] || usage; buildspec=$2; shift 2 ;;
    --env) [ $# -ge 2 ] || usage; case "$2" in *=*) envs+=("$2") ;; *) usage ;; esac; shift 2 ;;
    --stage) [ $# -ge 2 ] || usage; case "$2" in direct) stage=$2 ;; *) usage ;; esac; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$buildspec" ] || usage
[ -f "$root/$buildspec" ] || { echo "codebuild-run: no buildspec $buildspec in the repository" >&2; exit 2; }
if [ -n "$(git -C "$root" status --porcelain --untracked-files=no)" ]; then
  echo "codebuild-run: uncommitted changes to tracked files; commit first so the build matches a commit" >&2
  exit 2
fi

sha=$(git -C "$root" rev-parse HEAD)
bucket="hamh-$stage-harness-$account"
project="hamh-$stage-image-build"
# `committed`, or the SHA-256 of the generated account file in use: the key and the zip name carry it, so two files never share a bundle.
config=$("$py" "$here/lib/aws_target.py" digest) || exit 2
suffix=""
if [ "$config" != committed ]; then suffix="-${config:0:12}"; fi
key="builds/source/$sha$suffix.zip"
work="${CODEBUILD_TMP:-${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/ham/codebuild}"
mkdir -p "$work"
zip="$work/$sha$suffix.zip"
git -C "$root" archive --format=zip -o "$zip" HEAD || { echo "codebuild-run: git archive failed" >&2; exit 1; }
if [ "$config" != committed ]; then
  "$py" "$here/lib/source_bundle.py" add-accounts-file "$zip" --expect "$config" >/dev/null || { echo "codebuild-run: could not add the account file to the source bundle" >&2; exit 2; }
fi
"$aws_bin" s3 cp "$zip" "s3://$bucket/$key" --region "$region" --only-show-errors >/dev/null ||
  { echo "codebuild-run: upload to s3://$bucket/$key failed" >&2; exit 1; }

env_args=("name=SOURCE_SHA,value=$sha,type=PLAINTEXT")
if [ "$config" != committed ]; then env_args+=("name=HAMH_ACCOUNTS_FILE_SHA256,value=$config,type=PLAINTEXT"); fi
for e in ${envs[@]+"${envs[@]}"}; do env_args+=("name=${e%%=*},value=${e#*=},type=PLAINTEXT"); done
start=$(date +%s)
started=$("$aws_bin" codebuild start-build --region "$region" --project-name "$project" --source-type-override S3 \
  --source-location-override "$bucket/$key" --buildspec-override "$buildspec" \
  --environment-variables-override "${env_args[@]}" --output json) || { echo "codebuild-run: start-build failed" >&2; exit 1; }
id=$(printf '%s' "$started" | "$py" -c 'import json, sys; print(json.load(sys.stdin)["build"]["id"])' 2>/dev/null) || id=""
if [ -z "$id" ]; then
  echo "codebuild-run: start-build answered without a build id. The script follows no build and exits 1." >&2
  exit 1
fi
echo "codebuild-run: started $id for ${sha:0:7} ($buildspec)"

status=IN_PROGRESS
errors=0
while [ "$status" = IN_PROGRESS ]; do
  sleep "$poll"
  now=$(date +%s)
  waited=$((now - start))
  if [ "$waited" -ge "$max_wait" ]; then
    echo "codebuild-run: build $id was still IN_PROGRESS after ${waited} s. The script stops waiting and exits 1; the build keeps running in CodeBuild. You stop the build with \"aws codebuild stop-build --id $id\"." >&2
    exit 1
  fi
  if ! info=$("$aws_bin" codebuild batch-get-builds --region "$region" --ids "$id" --output json 2>"$work/poll.err"); then
    errors=$((errors + 1))
    if [ "$errors" -ge "$max_errors" ]; then
      echo "codebuild-run: batch-get-builds failed $errors times in a row: $(tail -n 1 "$work/poll.err" | cut -c1-300)" >&2
      exit 1
    fi
    continue
  fi
  errors=0
  status=$(printf '%s' "$info" | "$py" -c 'import json, sys; print(json.load(sys.stdin)["builds"][0]["buildStatus"])')
  phase=$(printf '%s' "$info" | "$py" -c 'import json, sys; print(json.load(sys.stdin)["builds"][0].get("currentPhase", ""))')
  echo "codebuild-run: $id $status $phase"
done
[ -n "$status" ] || echo "codebuild-run: CodeBuild gave no status for build $id." >&2

if [ "$status" = SUCCEEDED ]; then
  # nosemgrep: unquoted-command-substitution-in-command -- the date command prints digits only and the substitution sits inside an arithmetic expansion, where word splitting does not apply
  echo "CODEBUILD PASS id=$id sha=${sha:0:7} seconds=$(( $(date +%s) - start ))"
  exit 0
fi
group=$(printf '%s' "$info" | "$py" -c 'import json, sys; print(json.load(sys.stdin)["builds"][0].get("logs", {}).get("groupName", ""))')
stream=$(printf '%s' "$info" | "$py" -c 'import json, sys; print(json.load(sys.stdin)["builds"][0].get("logs", {}).get("streamName", ""))')
if [ -n "$group" ] && [ -n "$stream" ]; then
  "$aws_bin" logs get-log-events --region "$region" --log-group-name "$group" --log-stream-name "$stream" --limit 40 \
    --no-start-from-head --output json 2>/dev/null |
    "$py" -c 'import json, sys; [sys.stdout.write(e["message"]) for e in json.load(sys.stdin).get("events", [])]'
fi
echo "CODEBUILD FAIL id=$id status=$status"
exit 1
