#!/usr/bin/env bash
# scripts/setup/empty-bucket.sh: empty a versioned bucket before the stack that owns it is deleted.
# usage: scripts/setup/empty-bucket.sh hamh-setup-<account>-<Region>
#        AWS_REGION=<Region> scripts/setup/empty-bucket.sh hamh-direct-harness-<account>
#        AWS_REGION=<Region> scripts/setup/empty-bucket.sh hamh-direct-gomod-<account>
#        (and the same three names under the first word of the previous names, for a stage that was deployed before the rename; see FIRST_WORDS below)
#
# The buckets of infra/setup-runner.yaml and of the stage are versioned, and CloudFormation cannot delete a bucket that holds an object version or a delete marker.
# Neither template has a custom resource to do it, so the caller does, with their own credentials: this script lists one page of versions and delete markers with
# `aws s3api list-object-versions --no-paginate --max-keys 1000` (S3 returns at most 1,000 entries per call, versions and delete markers counted together) and removes
# exactly that page with `aws s3api delete-objects` (at most 1,000 keys per call), then lists again, until the listing is empty. It does not use --max-items: the AWS CLI
# counts only the versions against that limit and adds every delete marker of the pages it read, so a bucket with delete markers can give more than the 1,000 keys that
# DeleteObjects accepts. A delete that S3 answers with an Errors entry, a page that comes back unchanged after its delete, and a page of more than 1,000 entries are
# failures. The Go module store (hamh-direct-gomod-<account>) keeps S3 Object Lock in governance mode so that this step can remove it: its delete calls carry
# --bypass-governance-retention, and no other bucket is sent the flag. Its bucket policy lets only the stage operator (and the setup runner's role) delete or bypass, so
# another identity gets a refusal per version, which this script reports as a failure.
# It accepts only those three kinds of name, each under the current first word or under the previous one, and every call names the account of the name as the expected bucket owner, so it cannot empty a bucket of another account
# or of another name. The name of a stage bucket holds no Region, so AWS_REGION must name it (setup.sh passes the Region of its run). It uses only the AWS CLI and the
# shell tools of macOS and Linux. AWS_CLI_PROG names the AWS CLI program (default aws), as it does for setup.sh. The credentials are whatever the AWS CLI resolves;
# HAMH_EMPTY_BUCKET_PROFILE, when set, is passed as --profile (setup.sh sets it to the profile of its run, so that this step and the plan use one identity even when access
# keys are exported). The first line it prints is `EMPTY-BUCKET identity <arn>`, the identity of the credentials; when HAMH_EXPECT_CALLER_ARN is set and names another user, role or
# federated user (a role session under another session name is the same role), it stops before it lists anything. A throttled or interrupted call is repeated up to 5 times with a growing pause (HAMH_SETUP_RETRY_SECONDS, 2 by default).
# The last line is
#   EMPTY-BUCKET PASS bucket=<name> deleted=<n>      (exit 0; n counts versions and markers)
#   EMPTY-BUCKET GONE bucket=<name>                   (exit 0; S3 says there is no such bucket)
#   EMPTY-BUCKET FAIL <detail>                        (exit 1)
# A name that is not a setup bucket, or a wrong number of arguments, exits 2 with a message on stderr and calls nothing.
set -uo pipefail

usage() { echo "usage: empty-bucket.sh hamh-setup-<account>-<Region> | AWS_REGION=<Region> empty-bucket.sh hamh-direct-harness-<account> | hamh-direct-gomod-<account>" >&2; exit 2; }
[ $# -eq 1 ] || usage
BUCKET=$1
# The first word of the names setup gives its buckets: the current one first, then the one that releases before the rename used (the generation table of lib.sh holds both
# rows). A name-shape check is a guard that refuses everything it does not know, never the permission to delete.
FIRST_WORDS="hamh cah"  # COMPAT-CAH-STAGE: the second word is the first word of the previous names
# bash 3.2 of macOS: no [[ =~ ]]; case, grep and parameter expansion read the name.
not_ours() { echo "empty-bucket.sh: '$BUCKET' is not a setup bucket (hamh-setup-<12 digit account>-<Region>) or a bucket of the stage (hamh-direct-harness-<12 digit account>, hamh-direct-gomod-<12 digit account>), under one of the first words: $FIRST_WORDS" >&2; exit 2; }
case "$BUCKET" in
  *[!A-Za-z0-9-]*) not_ours ;;
esac
_bypass=""
KIND=""
WORD=""
# $_words is the list of first words and is split on purpose
_words=$FIRST_WORDS
# shellcheck disable=SC2086
for _w in $_words; do
  if printf '%s\n' "$BUCKET" | grep -Eq "^$_w-setup-[0-9]{12}-[a-z]{2}(-[a-z]+)+-[0-9]$"; then
    KIND=setup
    WORD=$_w
    break
  fi
  if printf '%s\n' "$BUCKET" | grep -Eq "^$_w-direct-(harness|gomod)-[0-9]{12}$"; then
    KIND=stage
    WORD=$_w
    break
  fi
done
case "$KIND" in
  setup)
    OWNER=${BUCKET#"$WORD"-setup-}
    OWNER=${OWNER%%-*}
    REGION=${BUCKET#"$WORD"-setup-????????????-} ;;
  stage)
    OWNER=${BUCKET##*-}
    case "$BUCKET" in
      "$WORD"-direct-gomod-*) _bypass="--bypass-governance-retention" ;;
    esac
    REGION=${AWS_REGION:-}
    case "$REGION" in
      ''|*[!a-z0-9-]*) echo "empty-bucket.sh: the name of a stage bucket holds no Region: set AWS_REGION (for example AWS_REGION=us-east-1)" >&2; exit 2 ;;
    esac ;;
  *) not_ours ;;
esac

AWS=${AWS_CLI_PROG:-aws}
work=$(mktemp -d "${TMPDIR:-/tmp}/ham-empty-bucket.XXXXXX") || { echo "EMPTY-BUCKET FAIL cannot make a work folder"; exit 1; }
trap 'rm -f "$work/payload.json" "$work/previous.json" "$work/answer.txt" "$work/error.txt"; rmdir "$work" 2>/dev/null' EXIT

# aws_call <aws arguments>: the AWS CLI with the Region of the bucket and, when the caller named one, the profile of its run.
aws_call() {
  if [ -n "${HAMH_EMPTY_BUCKET_PROFILE:-}" ]; then
    "$AWS" --profile "$HAMH_EMPTY_BUCKET_PROFILE" --region "$REGION" "$@"
  else
    "$AWS" --region "$REGION" "$@"
  fi
}

# call_to <file> <aws arguments>: aws_call with the answer in <file> and the error text in $work/error.txt; a throttled or interrupted call is repeated up to 5 times.
call_to() {
  local file=$1 tries=0 unit
  shift
  while :; do
    if aws_call "$@" >"$file" 2>"$work/error.txt"; then return 0; fi
    tries=$((tries + 1))
    if [ "$tries" -ge 5 ] || ! grep -Eqi 'SlowDown|Throttl|RequestLimitExceeded|ServiceUnavailable|InternalError|RequestTimeout|Could not connect|Read timeout|Too Many Requests|Connection reset' "$work/error.txt"; then
      return 1
    fi
    unit=${HAMH_SETUP_RETRY_SECONDS:-2}
    sleep "$((tries * unit))"
  done
}

fail() { echo "EMPTY-BUCKET FAIL $*"; exit 1; }
last_error() { tail -n 1 "$work/error.txt" | cut -c1-300; }

if ! call_to "$work/answer.txt" sts get-caller-identity --query Arn --output text; then
  fail "cannot read the identity of the credentials: $(last_error)"
fi
who=$(tr -d '\n' <"$work/answer.txt")
echo "EMPTY-BUCKET identity $who"
# a role session is the same identity under another session name (credential_process helpers make a new session for every call): compare the role, not the session
principal() {
  case $1 in
    arn:aws:sts::*:assumed-role/*/*) printf '%s' "${1%/*}" ;;
    *) printf '%s' "$1" ;;
  esac
}
if [ -n "${HAMH_EXPECT_CALLER_ARN:-}" ] && [ "$(principal "$who")" != "$(principal "$HAMH_EXPECT_CALLER_ARN")" ]; then
  fail "this step runs as $who. The plan names $HAMH_EXPECT_CALLER_ARN. The identities differ, so nothing was listed or deleted."
fi

deleted=0
: >"$work/previous.json"
while :; do
  # one page, exactly as S3 answers it: at most 1,000 entries, versions and delete markers together
  if ! call_to "$work/payload.json" s3api list-object-versions --bucket "$BUCKET" --expected-bucket-owner "$OWNER" --no-paginate --max-keys 1000 --output json \
      --query '{Objects: [Versions[].{Key: Key, VersionId: VersionId}, DeleteMarkers[].{Key: Key, VersionId: VersionId}][]}'; then
    if grep -q 'An error occurred (NoSuchBucket)' "$work/error.txt"; then
      echo "EMPTY-BUCKET GONE bucket=$BUCKET"
      exit 0
    fi
    fail "cannot list $BUCKET: $(last_error)"
  fi
  # the answer of the query is {"Objects": [...]}: anything else (a null, an object in place of the array) is not a listing, and an empty bucket is never inferred from it
  if ! grep -q '^ *"Objects": *\[' "$work/payload.json"; then
    fail "the answer for $BUCKET is not a listing: $(head -c 120 "$work/payload.json" | tr '\n' ' ')"
  fi
  n=$(grep -c '^ *"VersionId":' "$work/payload.json")
  keys=$(grep -c '^ *"Key":' "$work/payload.json")
  if [ "$n" -eq 0 ]; then
    # only an empty array is an empty bucket: a null, an object, or keys without versions are answers this script cannot use
    if [ "$keys" -eq 0 ] && grep -q '^ *"Objects": *\[\] *$' "$work/payload.json"; then break; fi
    fail "the answer for $BUCKET holds no list of versions and is not an empty list either: $(head -c 120 "$work/payload.json" | tr '\n' ' ')"
  fi
  if [ "$keys" -ne "$n" ]; then
    fail "the answer for $BUCKET holds $keys keys and $n version ids; nothing was sent"
  fi
  if [ "$n" -gt 1000 ]; then
    fail "the listing of $BUCKET gave $n entries in one page; S3 deletes at most 1000 keys per call, so nothing was sent"
  fi
  if cmp -s "$work/payload.json" "$work/previous.json"; then
    fail "$BUCKET still lists the $n entries that the last delete reported as deleted. Something may be writing to it, or it may not delete what it reports."
  fi
  cp "$work/payload.json" "$work/previous.json"
  # $_bypass is empty or one option, never a value with a space, and is split on purpose
  if ! call_to "$work/answer.txt" s3api delete-objects --bucket "$BUCKET" --expected-bucket-owner "$OWNER" --delete "file://$work/payload.json" $_bypass \
      --output text --query 'Errors[].[Code,Key,VersionId,Message]'; then
    fail "cannot delete from $BUCKET: $(last_error)"
  fi
  refused=$(awk 'NF && $0 != "None"' "$work/answer.txt")
  if [ -n "$refused" ]; then
    fail "$BUCKET refused a delete: $(printf '%s\n' "$refused" | head -n 1 | tr '\t' ' ' | cut -c1-300)"
  fi
  deleted=$((deleted + n))
done
echo "EMPTY-BUCKET PASS bucket=$BUCKET deleted=$deleted"
