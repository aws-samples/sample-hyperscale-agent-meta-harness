# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): the runner in the user's own account. Everything here is the AWS CLI
# and nothing else: the CDK bootstrap stack and the runner stack are created with `aws cloudformation deploy`, the source archive and the generated
# account file go up as two objects, the build is started with the environment variables of the contract below, followed to its end and read back from
# results/<buildId>/result.json.
#
# The contract:
#   stack hamh-setup-runner (infra/setup-runner.yaml), outputs SetupBucket, BuildProject, LogGroup, RunnerRoleArn; parameters InstallerPrincipal and
#     InstallerPrincipalWithPath (who may write the bucket besides the runner role) and AuthorityExpires (when the runner role stops being able to assume the CDK roles)
#   bucket hamh-setup-<account>-<region>: source/<commit>.zip, config/accounts-<12 digits of the SHA-256>.json, results/<buildId>/result.json,
#     results/<buildId>/phases.jsonl, client/<buildId>/<platform>/<binary>, client/<buildId>/SHA256SUMS, lease/<stage>.json
#   build environment: SETUP_SOURCE_KEY, SETUP_CONFIG_KEY, SETUP_STAGE, SETUP_AZS, SETUP_BINARY_NAME, SETUP_SOURCE_SHA256, SETUP_CONFIG_SHA256 (and
#     SETUP_ACTION=bootstrap-template)
#
# Trust: the bucket is written by this script with the user's credentials and by the runner role, and its policy refuses every other principal, but a
# bucket is still not where a decision is made. The archive and the account file are uploaded every time (an object found under the key is not believed),
# the build is pinned to the version id the upload answered with and told the SHA-256 of both, and the result of the build is believed only when its SHA-256
# is the one the build recorded in CodeBuild (the exported variable SETUP_RESULT_SHA256, which nothing that writes the bucket can change).
#
# State: AWS is authoritative. The lease object keeps two setups from racing; a repeated or interrupted setup attaches to the build the lease records
# and never starts another. Losing the connection is not a failed build: after HAMH_SETUP_POLL_RETRIES failed looks in a row (20 by default) the run
# stops with exit 6 and says how to attach again. Interrupting setup (Ctrl-C) never cancels the build.

RN_EXISTS=0
RN_BUCKET=""
RN_PROJECT=""
RN_LOGGROUP=""
RN_ROLE=""
RN_BUILD_ID=""
RN_FINAL=""
RN_MODE="new"
RN_LEASE_FLAT=""
RN_PHASES_SHOWN=0
RN_LOG_GROUP=""
RN_LOG_STREAM=""
RN_LOG_TS=0
RN_LOG_SKIP=0
RN_LOG_RESOLVED=0
RN_LAST_PHASE=""
RN_STARTED_AT=0
RN_LAST_SAID=0
RN_ACTION=""
RN_FAILED_PHASE=""
RN_FAILED_TEXT=""
SRC_VERSION=""
SRC_SHA256=""
RES_FLAT=""
RES_SHA256=""
RN_EXPIRES=""
RN_STATUS=""
INSTALLER_P1=""
INSTALLER_P2=""
AUTH_HOURS=12
RUNNER_LEFT=0
RN_LEFT_WHY=""
RN_LEFT_STATUS=""
RN_LEFT_COMMAND=""
RN_FAIL_WHY=""
RN_FAIL_FULL=""
RN_LEFT_DENIED=0

# rn_describe: does the runner stack exist, what are its outputs and until when may its role assume the CDK roles (RN_EXPIRES, empty when the stack does not say).
rn_describe() {
  local line key val
  RN_EXISTS=0
  RN_BUCKET=""
  RN_PROJECT=""
  RN_LOGGROUP=""
  RN_ROLE=""
  RN_EXPIRES=""
  if aws_run cloudformation describe-stacks --stack-name "$SETUP_RUNNER_STACK" --query 'Stacks[0].Outputs[].[OutputKey,OutputValue]' --output text; then
    RN_EXISTS=1
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      key=${line%%"$TAB"*}
      val=${line#*"$TAB"}
      case $key in
        SetupBucket) RN_BUCKET=$val ;;
        BuildProject) RN_PROJECT=$val ;;
        LogGroup) RN_LOGGROUP=$val ;;
        RunnerRoleArn) RN_ROLE=$val ;;
      esac
    done <<EOF
$AWS_OUT
EOF
    if aws_run cloudformation describe-stacks --stack-name "$SETUP_RUNNER_STACK" --query "Stacks[0].Parameters[?ParameterKey=='AuthorityExpires'].ParameterValue" --output text; then
      case $AWS_OUT in
        20??-??-??T??:??:??Z) RN_EXPIRES=$AWS_OUT ;;
      esac
    fi
  elif ! aws_missing; then
    setup_exit 1 "could not look for the runner stack: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:DescribeStacks."
  fi
}

# rn_stack_status <stack>: sets RN_STATUS to the status of a stack (empty when there is none); returns 1 when the look itself failed.
rn_stack_status() {
  RN_STATUS=""
  if aws_run cloudformation describe-stacks --stack-name "$1" --query 'Stacks[0].StackStatus' --output text; then
    RN_STATUS=$AWS_OUT
    [ "$RN_STATUS" != None ] || RN_STATUS=""
    return 0
  fi
  aws_missing
}

# rn_prepare_stack <stack>: `aws cloudformation deploy` changes a stack only from a settled state. A stack whose first create rolled back (ROLLBACK_COMPLETE, CREATE_FAILED)
# holds nothing and can only be deleted, so it is deleted and made again; a stack that is still changing is waited for (HAMH_SETUP_STACK_LOOKS looks, 120 by default, a
# pause apart); a stack CloudFormation can no longer change is named with the way out.
rn_prepare_stack() {
  local name=$1 looks=0 limit=${HAMH_SETUP_STACK_LOOKS:-120}
  while :; do
    rn_stack_status "$name" || setup_exit 1 "could not look at the stack $name: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:DescribeStacks."
    case $RN_STATUS in
      ROLLBACK_COMPLETE|CREATE_FAILED)
        say_log "The stack $name is in $RN_STATUS (an earlier create did not finish). It holds nothing, so it is deleted and made again ..."
        aws_run cloudformation delete-stack --stack-name "$name" || setup_exit 5 "could not delete the stack $name that is in $RN_STATUS: ${AWS_ERR:-no answer}" \
          "Delete it in the CloudFormation console, then run setup.sh again."
        aws_run cloudformation wait stack-delete-complete --stack-name "$name" || setup_exit 5 "the stack $name did not finish deleting: ${AWS_ERR:-no answer}" \
          "Look at it in the CloudFormation console, then run setup.sh again."
        return 0 ;;
      *_IN_PROGRESS)
        looks=$((looks + 1))
        [ "$looks" -le "$limit" ] || setup_exit 6 "the stack $name is still $RN_STATUS" "Something else is changing it. Run ./setup.sh again when it has finished."
        [ "$looks" != 1 ] || say_log "The stack $name is $RN_STATUS (it is being changed); waiting for it to settle ..."
        setup_sleep ;;
      DELETE_FAILED|ROLLBACK_FAILED|UPDATE_ROLLBACK_FAILED|UPDATE_FAILED|IMPORT_ROLLBACK_FAILED)
        setup_exit 5 "the stack $name is in $RN_STATUS, a state CloudFormation cannot change" \
          "Look at the stack in the CloudFormation console (its events say which resource is stuck), fix or delete it, then run setup.sh again." ;;
      *) return 0 ;;
    esac
  done
}

# rn_retry_command: what the user types to run the same operation again.
rn_retry_command() {
  case ${FULL_ACTION:-full} in
    update) printf '%s' "./setup.sh --update" ;;
    *) printf '%s' "./setup.sh --full" ;;
  esac
}

# rn_standing_text: the sentence that says a runner is still there, what it can do and until when, said on every run while the stack exists.
rn_standing_text() {
  printf 'the runner stack %s is still installed in account %s (bucket %s). While it exists, anyone who may start a build of its CodeBuild project (codebuild:StartBuild) runs commands with the authority of the CDK roles through it%s. Run %s to finish, or ./setup.sh --teardown to remove the stage and the runner.' \
    "$SETUP_RUNNER_STACK" "${ACCOUNT:-this}" "${RN_BUCKET:-none}" "${RN_EXPIRES:+ until $RN_EXPIRES (UTC)}" "$(rn_retry_command)"
}

# rn_note_installed: said on every run that finds the runner stack in the account and no build of this checkout using it.
rn_note_installed() {
  [ "$RN_EXISTS" = 1 ] || return 0
  warn "$(rn_standing_text)"
  log_line "runner installed: bucket ${RN_BUCKET:-none}, authority until ${RN_EXPIRES:-unknown}"
}

# rn_bootstrap: the CDK bootstrap stack when the account has none. The runner role cannot make one and the desktop has no Node, so the runner's first build
# (SETUP_ACTION=bootstrap-template, no lease) prints the template with the pinned CDK CLI and puts it in the bucket; the template is deployed with the user's own
# credentials, and only when its SHA-256 is the one this clone pins in engine/policy/cdk-bootstrap-template.pin (BOOT_PIN_SHA), the result of the build and the
# bytes downloaded from the bucket all agree, and it is no larger than the AWS CLI deploys without a staging bucket. An existing bootstrap is never changed.
rn_bootstrap() {
  local saved=${RN_ACTION:-deploy} key sha bytes dest got size commit digest
  if [ -n "$BOOT_VERSION" ]; then
    ok "CDK bootstrap version $BOOT_VERSION is used as it is"
    return 0
  fi
  [ -n "$BOOT_PIN_SHA" ] || pf_bootstrap_pin
  say_log "Your account has no CDK bootstrap. The runner prints the template with the CDK CLI this version pins (a build of a few minutes) ..."
  RN_ACTION=bootstrap-template
  rn_start nolease
  rn_follow
  rn_read_result
  if [ "$RN_FINAL" != SUCCEEDED ] || [ "$(rn_res state)" != SETUP-PASS ] || [ "$(rn_res action)" != bootstrap-template ]; then
    rn_report_failure
    setup_exit 5 "the build that prints the CDK bootstrap template failed${RN_FAILED_PHASE:+ in phase '$RN_FAILED_PHASE'}" \
      "The runner stays installed${RN_EXPIRES:+ (its authority to assume the CDK roles ends $RN_EXPIRES UTC)}. You run $(rn_retry_command) to retry. You run ./setup.sh --teardown to delete the runner and any stage." "Nothing else was created."
  fi
  commit=$(rn_res sourceCommit)
  digest=$(rn_res configDigest)
  if [ "$(rn_first12 "$commit")" != "$(rn_first12 "$SRC_COMMIT")" ] || [ "$(rn_first12 "$digest")" != "$(rn_first12 "$ACCT_DIGEST")" ]; then
    setup_exit 7 "source or configuration mismatch: the build that printed the CDK bootstrap template names another source or account file than this run uploaded" "Nothing was made from it."
  fi
  key=$(rn_res bootstrapTemplate.key)
  sha=$(rn_res bootstrapTemplate.sha256)
  bytes=$(rn_res bootstrapTemplate.bytes)
  case $key in
    bootstrap/cdk-bootstrap-????????????.yaml) ;;
    *) setup_exit 7 "the build that printed the CDK bootstrap template named an object setup does not trust: ${key:-none}" "Nothing was made from it." ;;
  esac
  setup_safe_word "$key" || setup_exit 7 "the build that printed the CDK bootstrap template named an object setup does not trust: $key" "Nothing was made from it."
  if [ "$sha" != "$BOOT_PIN_SHA" ]; then
    setup_exit 7 "the CDK bootstrap template the runner printed has SHA-256 ${sha:-none}, and $SETUP_BOOTSTRAP_PIN of this clone pins $BOOT_PIN_SHA" \
      "A different CDK than the pinned one printed it, or the runner is not running this version. Nothing was made from it."
  fi
  case $bytes in
    ''|*[!0-9]*) setup_exit 7 "the build that printed the CDK bootstrap template gave no byte count" "Nothing was made from it." ;;
  esac
  if [ "$bytes" -gt "$SETUP_BOOTSTRAP_MAX_BYTES" ]; then
    setup_exit 7 "the CDK bootstrap template is $bytes bytes, more than the 51,200 bytes that the AWS CLI deploys without a staging bucket" "Nothing was made from it."
  fi
  dest=$ST_DIR/cdk-bootstrap.yaml
  aws_run s3 cp "s3://$RN_BUCKET/$key" "$dest" || setup_exit 5 "could not download the CDK bootstrap template s3://$RN_BUCKET/$key: ${AWS_ERR:-no answer}" "Nothing was made from it."
  got=$(setup_sha256 <"$dest")
  size=$(wc -c <"$dest" | tr -d ' ')
  if [ "$got" != "$BOOT_PIN_SHA" ] || [ "$size" != "$bytes" ]; then
    setup_exit 7 "the template in your bucket (SHA-256 $got, $size bytes) is not the one the build reported and this clone pins" "Nothing was made from it."
  fi
  rn_prepare_stack "$SETUP_BOOTSTRAP_STACK"
  # A stack that was still changing has settled by now: the parameter may be there after all, and a stack that is there without it is not ours to change.
  pf_bootstrap_version
  if [ -n "$BOOT_VERSION" ]; then
    ok "CDK bootstrap version $BOOT_VERSION is used as it is"
    RN_ACTION=$saved
    return 0
  fi
  pf_bootstrap_stack late
  say_log "Creating the CDK bootstrap stack $SETUP_BOOTSTRAP_STACK from the checked template (about 2 minutes) ..."
  # The template's CloudFormation execution role has no policy of its own; the CDK CLI gives it AdministratorAccess when the caller names none, and so does this call.
  if ! aws_run cloudformation deploy --stack-name "$SETUP_BOOTSTRAP_STACK" --template-file "$dest" \
      --parameter-overrides CloudFormationExecutionPolicies=arn:aws:iam::aws:policy/AdministratorAccess \
      --capabilities CAPABILITY_NAMED_IAM --no-fail-on-empty-changeset --tags "project=$SETUP_PROJECT_TAG" stage=shared managed-by=setup.sh; then
    rn_fail_stack "creating the CDK bootstrap stack failed"
  fi
  ok "CDK bootstrap stack created from the template that this version pins (SHA-256 $(printf '%s' "$BOOT_PIN_SHA" | cut -c1-12))"
  BOOT_VERSION=created
  RN_ACTION=$saved
}

# rn_fail_stack <headline>: a stack that could not be made ends the run with exit 5 and the AWS text.
rn_fail_stack() {
  log_line "$1: $AWS_ERR"
  setup_exit 5 "$1: ${AWS_ERR:-no answer}" \
    "Creating a stack with IAM roles needs credentials that may create IAM roles, S3 buckets and ECR repositories (normally an administrator)." \
    "Nothing else was created after this step. Run setup.sh again when the cause is fixed."
}

# rn_install: the runner stack. Idempotent: CloudFormation changes nothing when the template and the parameters are the same. Every install writes the parameters
# again: the identity that runs this setup is the only one besides the runner role that may write the bucket (InstallerPrincipal and InstallerPrincipalWithPath, from
# pf_installer), and the runner role can assume the CDK roles for AUTH_HOURS from now (AuthorityExpires), so a runner that is left behind stops being able to act by itself.
rn_install() {
  local expires
  rn_prepare_stack "$SETUP_RUNNER_STACK"
  expires=$(setup_at_plus $((AUTH_HOURS * 3600)))
  say_log "Installing the runner stack $SETUP_RUNNER_STACK (about 2 minutes) ..."
  if ! aws_run cloudformation deploy --stack-name "$SETUP_RUNNER_STACK" --template-file "$SELF_DIR/$SETUP_RUNNER_TEMPLATE" \
      --parameter-overrides "InstallerPrincipal=$INSTALLER_P1" "InstallerPrincipalWithPath=$INSTALLER_P2" "AuthorityExpires=$expires" \
      --capabilities CAPABILITY_NAMED_IAM --no-fail-on-empty-changeset --tags "project=$SETUP_PROJECT_TAG" managed-by=setup.sh; then
    rn_fail_stack "installing the runner stack failed"
  fi
  rn_describe
  if [ "$RN_EXISTS" != 1 ] || [ -z "$RN_BUCKET" ] || [ -z "$RN_PROJECT" ]; then
    setup_exit 5 "the runner stack $SETUP_RUNNER_STACK has no SetupBucket or BuildProject output" "Delete the stack and run setup.sh again: ./setup.sh --teardown"
  fi
  [ -n "$RN_LOGGROUP" ] || RN_LOGGROUP=/aws/codebuild/$SETUP_RUNNER_STACK
  ok "runner installed: build project $RN_PROJECT, bucket $RN_BUCKET; its role can assume the CDK roles until $expires UTC"
}

# rn_put <file> <key>: one object into the bucket of this account, with a SHA-256 that S3 checks, and the version id it was stored as in AWS_OUT. The expected bucket owner
# is this account, so a bucket of that name in another account is never written to.
rn_put() {
  aws_run s3api put-object --bucket "$RN_BUCKET" --key "$2" --body "$1" --checksum-algorithm SHA256 --expected-bucket-owner "$ACCOUNT" --query VersionId --output text
}

# rn_upload: the source archive (git archive of the commit) and the generated account file, as two objects, every time. Another principal of the account may have
# written something under the key before, and the archive is run with deployment authority, so an object that is there is not believed and is not left: the upload makes a
# new version. The build is pinned to the version id the upload answered with (a later look at the key could name a version somebody wrote in between) and is told the
# SHA-256 of both files; runner-build.sh hashes the bytes it reads and stops when they differ. The files are written to the folder of the receipt (mode 0600).
rn_upload() {
  local archive=$ST_DIR/source.zip config=$ST_DIR/accounts-generated.json
  say_log "Packing and uploading the source archive of commit $SRC_COMMIT ..."
  ( umask 077; git -C "$SELF_DIR" archive --format=zip -o "$archive" "$SRC_COMMIT" ) || setup_exit 5 "git archive of commit $SRC_COMMIT failed"
  SRC_SHA256=$(setup_sha256 <"$archive")
  rn_put "$archive" "$SRC_KEY" || setup_exit 5 "uploading the source archive to s3://$RN_BUCKET/$SRC_KEY failed: ${AWS_ERR:-no answer}"
  case $AWS_OUT in
    ''|None|null) setup_exit 5 "S3 gave no version id for s3://$RN_BUCKET/$SRC_KEY, so the build cannot be pinned to the bytes just uploaded" \
      "Versioning of the bucket $RN_BUCKET was switched off or suspended. Turn it on, or run ./setup.sh --teardown and set up again." ;;
  esac
  setup_safe_word "$AWS_OUT" || setup_exit 5 "S3 answered with a version id setup does not trust: $AWS_OUT"
  SRC_VERSION=$AWS_OUT
  ok "uploaded $SRC_KEY (version $SRC_VERSION, SHA-256 $(rn_first12 "$SRC_SHA256"))"
  ( umask 077; printf '%s' "$ACCT_JSON" >"$config" ) || setup_exit 1 "could not write $config"
  rn_put "$config" "$ACCT_KEY" || setup_exit 5 "uploading the account file to s3://$RN_BUCKET/$ACCT_KEY failed: ${AWS_ERR:-no answer}"
  ok "uploaded $ACCT_KEY"
}

# ---- the lease --------------------------------------------------------------------------------------------------

# rn_first12 <text>: the first 12 characters.
rn_first12() { printf '%s' "$1" | cut -c1-12; }

# rn_lease_json <build id>: the text of the lease.
rn_lease_json() {
  printf '{%s  "schema": "1",%s  "owner": "%s:%s",%s  "buildId": "%s",%s  "sourceCommit": "%s",%s  "configDigest": "%s",%s  "action": "%s",%s  "startedAt": "%s"%s}%s' \
    "$NL" "$NL" "$(uname -n)" "$$" "$NL" "$1" "$NL" "$SRC_COMMIT" "$NL" "$ACCT_DIGEST" "$NL" "${RN_ACTION:-deploy}" "$NL" "$(setup_now)" "$NL" "$NL"
}

# rn_build_status <build id>: prints the status of a build, nothing when CodeBuild does not know it; returns 1 when the look itself failed.
rn_build_status() {
  if aws_run codebuild batch-get-builds --ids "$1" --query 'builds[0].buildStatus' --output text; then
    [ "$AWS_OUT" = None ] || printf '%s' "$AWS_OUT"
    return 0
  fi
  return 1
}

# rn_lease_check: decides RN_MODE from the lease of the stage and the build it records. It only reads; nothing is changed.
#   new     no lease
#   stale   a lease whose build has ended (or never started): rn_lease_take replaces it
#   attach  the lease records a build that is running with the same inputs: follow it
#   finish  the lease records a build that succeeded with the same inputs: read its result
# A lease of other inputs whose build is still running is a conflict: exit 7, nothing changed.
rn_lease_check() {
  local l_commit l_digest l_build l_owner l_action status same=0
  RN_MODE=new
  RN_LEASE_FLAT=""
  [ "$RN_EXISTS" = 1 ] && [ -n "$RN_BUCKET" ] || return 0
  if aws_run s3 cp "s3://$RN_BUCKET/lease/$STAGE.json" -; then
    RN_LEASE_FLAT=$(printf '%s' "$AWS_OUT" | json_flat)
  elif aws_missing; then
    return 0
  else
    setup_exit 1 "could not read the lease of the stage from s3://$RN_BUCKET/lease/$STAGE.json: ${AWS_ERR:-no answer}"
  fi
  l_commit=$(cl_flat_get "$RN_LEASE_FLAT" sourceCommit)
  l_digest=$(cl_flat_get "$RN_LEASE_FLAT" configDigest)
  l_build=$(cl_flat_get "$RN_LEASE_FLAT" buildId)
  l_owner=$(cl_flat_get "$RN_LEASE_FLAT" owner)
  l_action=$(cl_flat_get "$RN_LEASE_FLAT" action)
  if [ "$(rn_first12 "$l_commit")" = "$(rn_first12 "$SRC_COMMIT")" ] && [ "$(rn_first12 "$l_digest")" = "$(rn_first12 "$ACCT_DIGEST")" ] \
      && [ "${l_action:-deploy}" = "${RN_ACTION:-deploy}" ]; then
    same=1
  fi
  if [ -z "$l_build" ]; then
    # taken, and the build never started (that setup died between the two steps, or is about to start it)
    if [ "$same" = 1 ]; then RN_MODE=stale; return 0; fi
    setup_exit 7 "another setup holds the lease of stage $STAGE (taken by $l_owner, source commit $(rn_first12 "$l_commit"), no build started yet)" \
      "If no setup is running, delete the lease and run setup.sh again:  aws s3api delete-object --bucket $RN_BUCKET --key lease/$STAGE.json"
  fi
  status=$(rn_build_status "$l_build") || setup_exit 6 "could not look at the build $l_build that the lease of stage $STAGE records" \
    "The build is not affected. Run ./setup.sh again in a minute."
  case $status in
    IN_PROGRESS)
      if [ "$same" = 1 ]; then
        RN_MODE=attach
        RN_BUILD_ID=$l_build
        return 0
      fi
      setup_exit 7 "another setup is running: build $l_build for source commit $(rn_first12 "$l_commit") (started by $l_owner)" \
        "It is not the same source or account file as this run, so setup does not start a second build. Wait for it, or follow it with: ./setup.sh --status"
      ;;
    SUCCEEDED)
      if [ "$same" = 1 ]; then
        RN_MODE=finish
        RN_BUILD_ID=$l_build
        return 0
      fi
      ;;
  esac
  # the build ended some other way, or ended for other inputs: the lease is stale
  log_line "the lease of stage $STAGE recorded build $l_build (${status:-unknown}); it is stale"
  RN_MODE=stale
  return 0
}

# rn_lease_take: writes the lease, only when there is none (the condition makes the write fail when another setup was faster). When another setup
# was faster, the lease is read again: a build with the same inputs is attached to (RN_MODE becomes attach or finish), anything else is exit 7.
rn_lease_take() {
  local file=$ST_DIR/lease.json tries=0
  rn_lease_json "" >"$file"
  [ "$RN_MODE" != stale ] || rn_lease_release
  while :; do
    tries=$((tries + 1))
    if aws_run s3api put-object --bucket "$RN_BUCKET" --key "lease/$STAGE.json" --body "$file" --if-none-match '*'; then
      return 0
    fi
    case $AWS_ERR in
      *PreconditionFailed*|*"(412)"*|*ConditionalRequestConflict*|*"(409)"*)
        rn_lease_check
        case $RN_MODE in attach|finish) return 0 ;; esac
        [ "$tries" -lt 2 ] || setup_exit 7 "another setup took the lease of stage $STAGE a moment ago" "Run setup.sh again in a minute, or look with: ./setup.sh --status"
        [ "$RN_MODE" != stale ] || rn_lease_release ;;
      *"Unknown options"*|*"Invalid choice"*|*"unrecognized arguments"*)
        warn "this AWS CLI cannot write the lease with a condition; writing it without one (upgrade the AWS CLI to close the small race window)"
        if aws_run s3api head-object --bucket "$RN_BUCKET" --key "lease/$STAGE.json"; then
          setup_exit 7 "another setup took the lease of stage $STAGE a moment ago"
        fi
        aws_run s3api put-object --bucket "$RN_BUCKET" --key "lease/$STAGE.json" --body "$file" && return 0
        setup_exit 5 "could not write the lease of stage $STAGE: ${AWS_ERR:-no answer}" ;;
      *) setup_exit 5 "could not write the lease of stage $STAGE: ${AWS_ERR:-no answer}" ;;
    esac
  done
}

# rn_lease_record: the lease now names the build.
rn_lease_record() {
  local file=$ST_DIR/lease.json
  rn_lease_json "$RN_BUILD_ID" >"$file"
  aws_run s3api put-object --bucket "$RN_BUCKET" --key "lease/$STAGE.json" --body "$file" || warn "could not record the build in the lease: ${AWS_ERR:-no answer}"
}

rn_lease_release() {
  [ -n "$RN_BUCKET" ] || return 0
  aws_run s3api delete-object --bucket "$RN_BUCKET" --key "lease/$STAGE.json" || true
}

# ---- the build -----------------------------------------------------------------------------------------------------

# rn_start [nolease]: starts the build of RN_ACTION. The build of the bootstrap template takes no lease: it is repeatable and writes one object named by its hash.
rn_start() {
  local over nolease=${1:-}
  local -a source_args
  # The project's source is the placeholder object of the template: the build reads the archive setup.sh uploaded only when it is named here.
  source_args=(--source-location-override "$RN_BUCKET/$SRC_KEY")
  [ -z "$SRC_VERSION" ] || source_args+=(--source-version "$SRC_VERSION")
  over="name=SETUP_SOURCE_KEY,value=$SRC_KEY,type=PLAINTEXT name=SETUP_CONFIG_KEY,value=$ACCT_KEY,type=PLAINTEXT"
  over="$over name=SETUP_STAGE,value=$STAGE,type=PLAINTEXT name=SETUP_AZS,value=$AZS,type=PLAINTEXT"
  over="$over name=SETUP_BINARY_NAME,value=$HAMH_CLIENT_BIN,type=PLAINTEXT"
  # the SHA-256 of what was uploaded: the build hashes the bytes it reads from the bucket and stops when they differ
  over="$over name=SETUP_SOURCE_SHA256,value=$SRC_SHA256,type=PLAINTEXT name=SETUP_CONFIG_SHA256,value=$ACCT_DIGEST,type=PLAINTEXT"
  [ "${RN_ACTION:-deploy}" = deploy ] || over="$over name=SETUP_ACTION,value=$RN_ACTION,type=PLAINTEXT"
  # shellcheck disable=SC2086
  if ! aws_run codebuild start-build --project-name "$RN_PROJECT" "${source_args[@]}" --environment-variables-override $over --query build.id --output text; then
    [ -n "$nolease" ] || rn_lease_release
    setup_exit 5 "could not start the build: ${AWS_ERR:-no answer}"
  fi
  RN_BUILD_ID=$AWS_OUT
  setup_safe_word "$RN_BUILD_ID" || { [ -n "$nolease" ] || rn_lease_release; setup_exit 5 "CodeBuild answered with a build id setup does not trust: $RN_BUILD_ID"; }
  if [ -z "$nolease" ]; then
    rn_lease_record
    st_set buildId "$RN_BUILD_ID" || true
    st_set sourceCommit "$SRC_COMMIT" || true
    st_set configDigest "$ACCT_DIGEST" || true
    st_set state started || true
    st_save || true
  fi
  say_log "Build started: $RN_BUILD_ID"
}

rn_interrupted() {
  trap - INT TERM
  rn_stopped_following "you stopped it"
}

# rn_stopped_following <why>: ends the run with exit 6; the build is not cancelled.
rn_stopped_following() {
  err "Stopped following the build ($1)."
  detail "The build $RN_BUILD_ID keeps running in your AWS account; it is not cancelled."
  case ${AWS_ERR:-} in
    *xpired*|*"Unable to locate credentials"*|*SSO*|*sso*)
      detail "Your AWS sign-in may have run out. Sign in again (for an IAM Identity Center profile:  aws sso login --profile NAME) before you run setup.sh again." ;;
  esac
  if [ "${RN_ACTION:-deploy}" = bootstrap-template ]; then
    detail "Run ./setup.sh again with the same options in a few minutes. The build that prints the CDK bootstrap template takes no lease and can be repeated, so the next run starts it again."
  else
    detail "Run ./setup.sh again with the same options to attach to it, or ./setup.sh --status to look."
  fi
  log_line "stopped following $RN_BUILD_ID: $1"
  exit 6
}

# rn_show_log <message>: one line of the build log goes to the log of this run; the marker lines of the runner (SETUP-, DEPLOY-) and
# every line with --verbose go to the screen too.
rn_show_log() {
  log_line "build: $1"
  case $1 in
    SETUP-*|DEPLOY-*) say "  $1" ;;
    *) [ "${VERBOSE:-0}" != 1 ] || say "  $1" ;;
  esac
}

# rn_logs: the new lines of the build log (events are asked for by time; the ones already shown are skipped).
rn_logs() {
  local out line ts msg skip
  [ -n "$RN_LOG_GROUP" ] || return 0
  out=$(awsx logs get-log-events --log-group-name "$RN_LOG_GROUP" --log-stream-name "$RN_LOG_STREAM" --start-time "$RN_LOG_TS" --start-from-head \
    --query 'events[].[timestamp,message]' --output text 2>/dev/null) || return 0
  [ -n "$out" ] && [ "$out" != None ] || return 0
  skip=$RN_LOG_SKIP
  while IFS= read -r line; do
    case $line in
      [0-9]*"$TAB"*)
        ts=${line%%"$TAB"*}
        msg=${line#*"$TAB"}
        case $ts in *[!0-9]*) rn_show_log "$line"; continue ;; esac
        if [ "$ts" = "$RN_LOG_TS" ] && [ "$skip" -gt 0 ]; then
          skip=$((skip - 1))
          continue
        fi
        if [ "$ts" = "$RN_LOG_TS" ]; then
          RN_LOG_SKIP=$((RN_LOG_SKIP + 1))
        else
          RN_LOG_TS=$ts
          RN_LOG_SKIP=1
          skip=0
        fi
        [ -z "$msg" ] || rn_show_log "$msg" ;;
      '') ;;
      *) rn_show_log "$line" ;;
    esac
  done <<EOF
$out
EOF
}

# rn_log_location: the log group and stream CodeBuild names for the build, once it has them (the defaults are the stack's LogGroup and the build's UUID).
rn_log_location() {
  local group stream
  aws_run codebuild batch-get-builds --ids "$RN_BUILD_ID" --query 'builds[0].logs.[groupName,streamName]' --output text || return 0
  group=${AWS_OUT%%"$TAB"*}
  stream=${AWS_OUT#*"$TAB"}
  [ -n "$group" ] && [ "$group" != None ] && [ -n "$stream" ] && [ "$stream" != None ] || return 0
  RN_LOG_GROUP=$group
  RN_LOG_STREAM=$stream
  RN_LOG_RESOLVED=1
}

# rn_progress: a line for each phase the runner has finished (results/<buildId>/phases.jsonl) and the new lines of the log.
rn_progress() {
  local text n=0 line name state secs
  if text=$(awsx s3 cp "s3://$RN_BUCKET/results/$RN_BUILD_ID/phases.jsonl" - 2>/dev/null); then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      n=$((n + 1))
      [ "$n" -gt "$RN_PHASES_SHOWN" ] || continue
      name=$(printf '%s' "$line" | json_value name)
      state=$(printf '%s' "$line" | json_value state)
      secs=$(printf '%s' "$line" | json_value seconds)
      say_log "  phase $n: $name $state${secs:+ (${secs}s)}"
      RN_LAST_SAID=$(setup_epoch)
    done <<EOF
$text
EOF
    [ "$n" -le "$RN_PHASES_SHOWN" ] || RN_PHASES_SHOWN=$n
  fi
  rn_logs
}

# rn_heartbeat <CodeBuild phase>: a line every two minutes in which nothing else was said, so a long compile does not look like a hang.
rn_heartbeat() {
  local now
  now=$(setup_epoch)
  if [ $((now - RN_LAST_SAID)) -ge 120 ]; then
    RN_LAST_SAID=$now
    say_log "  still running: CodeBuild phase $1, $(((now - RN_STARTED_AT) / 60)) minutes since you started following it"
  fi
}

# rn_follow: looks at the build until it ends. Sets RN_FINAL to its status. A look that fails is tried again; HAMH_SETUP_POLL_RETRIES in a row end the
# run with exit 6 (the build is not affected).
rn_follow() {
  local fails=0 retries=${HAMH_SETUP_POLL_RETRIES:-20} status phase uuid
  uuid=${RN_BUILD_ID#*:}
  RN_PHASES_SHOWN=0
  RN_LOG_TS=0
  RN_LOG_SKIP=0
  RN_LOG_GROUP=${RN_LOGGROUP:-/aws/codebuild/$SETUP_RUNNER_STACK}
  RN_LOG_STREAM=$uuid
  RN_LOG_RESOLVED=0
  RN_LAST_PHASE=""
  RN_STARTED_AT=$(setup_epoch)
  RN_LAST_SAID=$RN_STARTED_AT
  trap rn_interrupted INT TERM
  while :; do
    if aws_run codebuild batch-get-builds --ids "$RN_BUILD_ID" --query 'builds[0].[buildStatus,currentPhase]' --output text; then
      fails=0
      status=${AWS_OUT%%"$TAB"*}
      phase=${AWS_OUT#*"$TAB"}
      if [ -z "$AWS_OUT" ] || [ "$status" = None ]; then
        trap - INT TERM
        setup_exit 5 "CodeBuild does not know the build $RN_BUILD_ID" "It may have been deleted. Run setup.sh again to start a new build."
      fi
      if [ "$phase" != "$RN_LAST_PHASE" ] && [ "$status" = IN_PROGRESS ]; then
        RN_LAST_PHASE=$phase
        log_line "build $RN_BUILD_ID: $status $phase"
      fi
      [ "$RN_LOG_RESOLVED" = 1 ] || rn_log_location
      rn_progress
      [ "$status" = IN_PROGRESS ] || { RN_FINAL=$status; break; }
      rn_heartbeat "$phase"
    else
      fails=$((fails + 1))
      log_line "looking at the build failed ($fails of $retries): ${AWS_ERR:-no answer}"
      if [ "$fails" -ge "$retries" ]; then
        trap - INT TERM
        rn_stopped_following "$retries looks at the build failed in a row: ${AWS_ERR:-no answer}"
      fi
    fi
    setup_sleep
  done
  trap - INT TERM
  return 0
}

# ---- the result --------------------------------------------------------------------------------------------------------

rn_res() { cl_flat_get "$RES_FLAT" "$1"; }

# rn_read_result: results/<buildId>/result.json into RES_FLAT (empty when the build wrote none) and the SHA-256 of its bytes into RES_SHA256. The file is downloaded
# whole, because a hash of text that went through a shell variable would lose its trailing newlines.
rn_read_result() {
  local file=$ST_DIR/result.json
  RES_FLAT=""
  RES_SHA256=""
  if aws_run s3 cp "s3://$RN_BUCKET/results/$RN_BUILD_ID/result.json" "$file"; then
    RES_SHA256=$(setup_sha256 <"$file")
    RES_FLAT=$(json_flat <"$file")
  fi
}

# rn_anchor: the bucket can have been written by another principal of the account, so what result.json says is believed only when its SHA-256 is the one the build
# recorded in CodeBuild (the exported variable SETUP_RESULT_SHA256 of the build, set by images/codebuild/setup-runner.yml after the runner wrote the file). The result
# carries the SHA-256 of the client, the endpoint and the commit that was built, so it is the one value that decides what is installed and run here. A look that fails
# is exit 6 and leaves the lease, so the next run finishes from the same build; a missing or different value releases the lease and is exit 7, nothing installed.
rn_anchor() {
  local want
  aws_run codebuild batch-get-builds --ids "$RN_BUILD_ID" --query "builds[0].exportedEnvironmentVariables[?name=='SETUP_RESULT_SHA256'].value" --output text ||
    setup_exit 6 "could not look at the record of build $RN_BUILD_ID in CodeBuild to check its result: ${AWS_ERR:-no answer}" \
      "Nothing was installed. The build is not affected: run $(rn_retry_command) again in a minute."
  want=$AWS_OUT
  case $want in
    ''|None|none|*[!0-9a-f]*)
      rn_lease_release
      setup_exit 7 "the build $RN_BUILD_ID recorded no SHA-256 of its result in CodeBuild, so the result.json in your bucket is not believed" \
        "Something other than the runner may have written it, or the build did not finish its last step. Nothing was installed. Run $(rn_retry_command) again to build once more." ;;
  esac
  if [ "$want" != "$RES_SHA256" ]; then
    rn_lease_release
    setup_exit 7 "the result.json in your bucket (SHA-256 ${RES_SHA256:-none}) is not the one build $RN_BUILD_ID recorded in CodeBuild ($want)" \
      "An object of the bucket was changed after the build wrote it. Nothing was installed. Run $(rn_retry_command) again to build once more." \
      "Who may write the bucket: only the runner role and the identity that ran this setup; look at the CloudTrail events of s3://$RN_BUCKET."
  fi
}

# rn_fail_code: the exit code of a build that ended badly: 7 when the deploy guard refused (runs are active), else 5.
rn_fail_code() {
  if [ "$(rn_res failedPhase)" = guard ]; then
    echo 7
  else
    echo 5
  fi
}

# rn_excerpt <lines|kind> <log file> [lines]: the lines of the build log that hold the cause of a failed build, or the kind of excerpt they are. The log of a run
# (RUN_LOG) holds every line of the build log as `<time> build: <line>` between lines of the program's own. The runner prints its own failure line (`SETUP-FAIL ...`,
# and `DEPLOY-FAIL ...` before it for a deploy) after the output that caused it, and CodeBuild adds its closing lines (`[Container] ... Phase complete: POST_BUILD`)
# after that, so the last lines of the log are never the cause. The kinds:
#   before  the build printed a failure line with output ahead of it: the last [lines] (15) lines before the first failure line. When none of them looks like an
#           error but one in the 45 before them does (an error message followed by a long stack), the window starts at that line instead.
#   tail    no failure line, or one with nothing ahead of it (CodeBuild stopped the build): the last [lines] lines of the build log.
#   raw     the log holds no build line at all (the build log could not be read): the last [lines] lines of the log itself.
#   none    the log is empty or missing.
# A line is cut at 240 characters and control characters other than the tab are dropped, so a build cannot write on the user's terminal.
RN_EXCERPT_AWK='
function cause(s) { return s ~ /([Ee]rror|ERROR|ENOENT|EACCES|[Ff]ailed|FAILED|[Ff]atal|FATAL|[Dd]enied|DENIED|[Pp]anicked|[Ee]xception|Traceback|[Cc]annot|[Nn]o such file|not found)/ }
function cut(s) { return length(s) > 240 ? substr(s, 1, 240) : s }
{
  total++
  raw[total] = $0
  if (match($0, /^[^ ]+ build: /)) {
    msg = substr($0, RLENGTH + 1)
    if (msg ~ /^[ \t]*$/) next
    n++
    line[n] = msg
    if (!marker && msg ~ /^(SETUP|DEPLOY)-FAIL( |$)/) marker = n
  }
}
END {
  if (total == 0) kind = "none"
  else if (n == 0) kind = "raw"
  else if (marker > 1) kind = "before"
  else kind = "tail"
  if (want == "kind") { print kind; exit }
  if (kind == "none") exit
  if (kind == "raw") { for (i = (total > max ? total - max + 1 : 1); i <= total; i++) print cut(raw[i]); exit }
  if (kind == "tail") { for (i = (n > max ? n - max + 1 : 1); i <= n; i++) print cut(line[i]); exit }
  last = marker - 1
  first = last > max ? last - max + 1 : 1
  found = 0
  for (i = first; i <= last; i++) if (cause(line[i])) { found = 1; break }
  if (!found) {
    low = last - 3 * max + 1
    if (low < 1) low = 1
    for (i = first - 1; i >= low; i--) if (cause(line[i])) { first = i; if (first + max - 1 < last) last = first + max - 1; break }
  }
  for (i = first; i <= last; i++) print cut(line[i])
}'

rn_excerpt() {
  local want=$1 log=$2 max=${3:-15}
  case $max in '' | *[!0-9]* | 0) max=15 ;; esac
  if [ ! -f "$log" ]; then
    [ "$want" != kind ] || echo none
    return 0
  fi
  LC_ALL=C awk -v want="$want" -v max="$max" "$RN_EXCERPT_AWK" "$log" | LC_ALL=C tr -d '\000-\010\013-\037\177'
}

rn_failure_excerpt() { rn_excerpt lines "$1" "${2:-15}"; }
rn_failure_kind() { rn_excerpt kind "$1" 15; }

# rn_report_failure [local]: the build ended badly. Prints the phase, the detail, where the log is and the lines of the build log that hold the cause; rn_fail_result goes on from
# there. With `local` the CloudWatch log group is not named, because it goes with the runner that is about to be removed; the heading of the excerpt names the setup log.
rn_report_failure() {
  local phase detail_text tail_lines line where kind heading
  phase=$(rn_res failedPhase)
  detail_text=$(json_text "$(rn_res detail)")
  [ -n "$RES_FLAT" ] || detail_text="the build ended ($RN_FINAL) and wrote no result"
  where=""
  [ -z "$phase" ] || where=" in phase '$phase'"
  err "the build $RN_BUILD_ID ${RN_FINAL:+($RN_FINAL) }failed$where: ${detail_text:-no detail}"
  [ "${1:-}" = local ] || detail "Build log: CloudWatch log group ${RN_LOGGROUP:-/aws/codebuild/$SETUP_RUNNER_STACK}, stream ${RN_BUILD_ID#*:}"
  if [ -n "$RUN_LOG" ] && [ -f "$RUN_LOG" ]; then
    kind=$(rn_failure_kind "$RUN_LOG")
    case $kind in
      before) heading="Lines before the build's failure message follow. The full log is here ($RUN_LOG):" ;;
      tail) heading="The last lines of the build log follow. The full log is here ($RUN_LOG):" ;;
      raw) heading="The local log contains no build output. Its last lines follow. The full log is here ($RUN_LOG):" ;;
      *) heading="" ;;
    esac
    if [ -n "$heading" ]; then
      tail_lines=$(rn_failure_excerpt "$RUN_LOG" 15)
      detail "$heading"
      while IFS= read -r line; do detail "  $line"; done <<EOF
$tail_lines
EOF
    fi
  fi
  log_line "failed: $RN_BUILD_ID phase=$phase $detail_text"
  RN_FAILED_PHASE=$phase
  RN_FAILED_TEXT=$detail_text
}

# rn_before_deploy <failed phase>: 0 when a build that failed in this phase had not touched the stage. The deploy guard refuses before the Lambda build, and the phases inputs,
# fetch, toolchain and account-file run before the deploy. A failure in the deploy or after it, and a build that left no result, may have changed the stage.
rn_before_deploy() {
  case $1 in
    guard|inputs|fetch|toolchain|account-file) return 0 ;;
  esac
  return 1
}

# rn_fail_result: the build ended badly. Reports it, writes the receipt, releases the lease and exits. An update that failed before it changed the stage removes the runner it
# installed, and says the stage is unchanged; one that may have changed the stage keeps the runner for the retry, and says the stage may hold partial changes. A first setup keeps
# its runner for the retry.
rn_fail_result() {
  local code phase detail_text unchanged=0 until="" cdk_window=""
  code=$(rn_fail_code)
  phase=$(rn_res failedPhase)
  if [ "${FULL_ACTION:-full}" = update ] && rn_before_deploy "$phase"; then unchanged=1; fi
  if [ "$unchanged" = 1 ]; then rn_report_failure local; else rn_report_failure; fi
  phase=$RN_FAILED_PHASE
  detail_text=$RN_FAILED_TEXT
  st_set state failed || true
  st_set failedPhase "${phase:-unknown}" || true
  st_set detail "$(printf '%s' "$detail_text" | tr -c '[:print:]' ' ' | tr -d '\042\134' | cut -c1-200)" || true
  st_save || true
  if [ "$unchanged" = 1 ]; then
    # The lease lives in the bucket that rn_remove empties, so it goes with the runner; a runner that stays gives the lease back.
    rn_remove || true
    [ "$RUNNER_LEFT" != 1 ] || rn_lease_release
  else
    rn_lease_release
  fi
  [ -z "$RN_EXPIRES" ] || until=" Its role can assume the CDK roles until $RN_EXPIRES UTC."
  [ -z "$RN_EXPIRES" ] || cdk_window="; its role can assume the CDK roles until $RN_EXPIRES UTC and not after"
  if [ "$unchanged" = 1 ] && [ "$RUNNER_LEFT" = 1 ]; then
    detail "The stage is unchanged. The runner could not be removed and stays installed. The runner is listed above as RETAINED.$until You run $(rn_retry_command) to retry. You run ./setup.sh --teardown to delete the stage and the runner."
  elif [ "$unchanged" = 1 ] && [ "$code" = 7 ]; then
    detail "The stage is unchanged. The runner is removed. You wait until the active runs end. You run $(rn_retry_command) to retry the update."
  elif [ "$unchanged" = 1 ]; then
    detail "The stage is unchanged. The runner is removed. You run $(rn_retry_command) to retry."
  elif [ "$code" = 7 ]; then
    detail "The runner stays installed$cdk_window. The stage is unchanged. You wait until the active runs end. You run $(rn_retry_command) to retry the update. You run ./setup.sh --teardown to delete the stage and the runner."
  else
    detail "The runner stays installed$cdk_window. The stage may contain partial deployment changes. You run $(rn_retry_command) to retry. You run ./setup.sh --teardown to delete the stage and the runner."
  fi
  exit "$code"
}

# rn_verify: the build succeeded; its result must say so and must be for this source and this account file.
rn_verify() {
  local state commit digest
  state=$(rn_res state)
  [ "$state" = SETUP-PASS ] || rn_fail_result
  rn_anchor
  commit=$(rn_res sourceCommit)
  digest=$(rn_res configDigest)
  if [ "$(rn_first12 "$commit")" != "$(rn_first12 "$SRC_COMMIT")" ]; then
    rn_lease_release
    setup_exit 7 "source mismatch: the result of the build names sourceCommit ${commit:-none}, this run uploaded $SRC_COMMIT" "Nothing was installed."
  fi
  if [ "$(rn_first12 "$digest")" != "$(rn_first12 "$ACCT_DIGEST")" ]; then
    rn_lease_release
    setup_exit 7 "configuration mismatch: the result of the build names configDigest ${digest:-none}, this run uploaded $ACCT_DIGEST" "Nothing was installed."
  fi
}

# rn_settle: the build has ended (RN_FINAL). Reads its result and judges it; returns only for a result that passed.
rn_settle() {
  rn_read_result
  [ "$RN_FINAL" = SUCCEEDED ] || rn_fail_result
  rn_verify
}

# ---- taking the runner away -----------------------------------------------------------------------------------------------

# rn_empty_bucket [bucket]: every object version and delete marker of the setup bucket (or of the bucket named, a bucket of the stage), because a versioned bucket
# cannot be deleted while it holds any. The work is scripts/setup/empty-bucket.sh, which accepts only those buckets and names the account of the bucket as its
# expected owner on every call; it runs with the Region of this run and, like every other call of the run, with its profile as --profile (HAMH_EMPTY_BUCKET_PROFILE), so
# that exported access keys cannot make this step another identity than the one the plan names. The script reads the identity of its credentials and stops when it is not
# CALLER_ARN (HAMH_EXPECT_CALLER_ARN). A bucket that is already gone is nothing to empty. On a failure AWS_ERR holds the reason.
rn_empty_bucket() {
  local out last who rc n bucket=${1:-$RN_BUCKET}
  out=$(
    export AWS_PAGER="" AWS_CLI_PROG="${AWS_CLI_PROG:-aws}" AWS_REGION="$REGION" HAMH_EXPECT_CALLER_ARN="${CALLER_ARN:-}"
    if [ -n "${PROFILE_EFF:-}" ]; then export HAMH_EMPTY_BUCKET_PROFILE="$PROFILE_EFF"; else unset HAMH_EMPTY_BUCKET_PROFILE; fi
    "${BASH:-bash}" "$SELF_DIR/scripts/setup/empty-bucket.sh" "$bucket" 2>&1
    echo "EMPTY-BUCKET-EXIT $?"
  )
  rc=$(printf '%s\n' "$out" | awk '/^EMPTY-BUCKET-EXIT / { r = $2 } END { print r }')
  last=$(printf '%s\n' "$out" | awk 'NF && !/^EMPTY-BUCKET-EXIT / { l = $0 } END { print l }')
  who=$(printf '%s\n' "$out" | awk '/^EMPTY-BUCKET identity / { print substr($0, 23); exit }')
  [ -z "$who" ] || log_line "empty-bucket.sh: identity $who"
  log_line "empty-bucket.sh: $last"
  # only the script's own success lines count, and only with its exit code 0: a failure line that happens to name an S3 error in a key is a failure
  if [ "$rc" = 0 ]; then
    case $last in
      "EMPTY-BUCKET PASS "*)
        n=${last##*deleted=}
        case $n in
          ''|*[!0-9]*) ;;
          *) say "emptied $bucket: $n versions and delete markers" ;;
        esac
        return 0 ;;
      "EMPTY-BUCKET GONE "*) return 0 ;;
    esac
  fi
  AWS_ERR=${last#EMPTY-BUCKET FAIL }
  [ -n "$AWS_ERR" ] || AWS_ERR="empty-bucket.sh gave no answer"
  return 1
}

# rn_is_denial <text>: 0 when a failure text says that THIS caller (CALLER_ARN, the identity the run printed) was refused an IAM action ("User: <caller> is not authorized to perform:
# iam:DeleteRole ... with an explicit deny in a session policy"), which a role that may do it can get around. A refusal of another principal (a role CloudFormation uses, a handler
# role), a bucket policy, a key policy or a service control policy is not that, and is not worked around.
rn_is_denial() {
  [ -n "${CALLER_ARN:-}" ] || return 1
  case $1 in
    *"service control policy"*) return 1 ;;
    *"$CALLER_ARN is not authorized to perform: iam:"*) return 0 ;;
  esac
  return 1
}

# rn_failed_reasons <stack>: RN_FAIL_WHY is what the latest delete of a stack failed to delete and why, from its events (newest first, up to the event that started that delete, each
# resource once, three at most); empty when the events could not be read or none failed. Returns 1 then. The events of an earlier try are not read: a denial that an earlier try met
# does not decide what the latest one needs.
rn_failed_reasons() {
  RN_FAIL_WHY=""
  RN_FAIL_FULL=""
  aws_run cloudformation describe-stack-events --stack-name "$1" --query "StackEvents[].[ResourceStatus,LogicalResourceId,ResourceType,ResourceStatusReason]" --output text || return 1
  # the whole text of every failure of the latest delete decides whether the caller was refused; the three shortened ones are for the screen
  RN_FAIL_FULL=$(printf '%s\n' "$AWS_OUT" | awk -F'\t' '
    $3 == "AWS::CloudFormation::Stack" && $1 == "DELETE_IN_PROGRESS" { exit }
    NF >= 4 && $1 == "DELETE_FAILED" && $3 != "AWS::CloudFormation::Stack" { r = $4; for (i = 5; i <= NF; i++) r = r " " $i; print r }')
  RN_FAIL_WHY=$(printf '%s\n' "$AWS_OUT" | awk -F'\t' '
    $3 == "AWS::CloudFormation::Stack" && $1 == "DELETE_IN_PROGRESS" { exit }
    NF >= 4 && $1 == "DELETE_FAILED" && $3 != "AWS::CloudFormation::Stack" && !seen[$2]++ && n < 3 {
      n++
      r = $4
      for (i = 5; i <= NF; i++) r = r " " $i
      if (length(r) > 400) r = substr(r, 1, 400) "..."
      printf "%s%s (%s): %s", (n > 1 ? "; " : ""), $2, $3, r
    }')
  [ -n "$RN_FAIL_WHY" ]
}

# rn_wait_deleted: waits for the runner stack to be gone and says so every few minutes. Returns the waiter's status (AWS_ERR holds its text).
rn_wait_deleted() {
  local rc
  setup_ticker_start "CloudFormation to delete the stack $SETUP_RUNNER_STACK"
  aws_run cloudformation wait stack-delete-complete --stack-name "$SETUP_RUNNER_STACK"
  rc=$?
  setup_ticker_stop
  return "$rc"
}

# rn_delete_runner [role arn]: deletes the runner stack (as that role, when one is given) and waits. Returns 0 when it is gone. Otherwise RN_LEFT_WHY is the reason and RN_LEFT_STATUS
# the status of the stack ("DELETE_FAILED" when CloudFormation tried and failed; empty when it never started).
rn_delete_runner() {
  local waiter
  RN_LEFT_WHY=""
  RN_LEFT_STATUS=""
  RN_LEFT_DENIED=0
  if [ -n "${1:-}" ]; then
    aws_run cloudformation delete-stack --stack-name "$SETUP_RUNNER_STACK" --role-arn "$1" || {
      RN_LEFT_WHY="CloudFormation refused the delete with the role $1: ${AWS_ERR:-no answer}"
      return 1
    }
  else
    aws_run cloudformation delete-stack --stack-name "$SETUP_RUNNER_STACK" || {
      RN_LEFT_WHY="the stack could not be deleted: ${AWS_ERR:-no answer}"
      return 1
    }
  fi
  rn_wait_deleted && return 0
  waiter=${AWS_ERR:-no answer}
  if rn_stack_status "$SETUP_RUNNER_STACK"; then
    case $RN_STATUS in
      '') return 0 ;;
      DELETE_FAILED)
        RN_LEFT_STATUS=DELETE_FAILED
        if rn_failed_reasons "$SETUP_RUNNER_STACK"; then
          RN_LEFT_WHY=$RN_FAIL_WHY
          if rn_is_denial "$RN_FAIL_FULL"; then RN_LEFT_DENIED=1; fi
        else
          RN_LEFT_WHY="the stack did not finish deleting: $waiter"
        fi
        return 1 ;;
    esac
  fi
  RN_LEFT_WHY="the stack did not finish deleting: $waiter"
  return 1
}

# rn_bucket_exists: 0 unless S3 says the runner bucket is not there (a failed look counts as there: the line names it).
rn_bucket_exists() {
  [ -n "${RN_BUCKET:-}" ] || return 1
  aws_run s3api head-bucket --bucket "$RN_BUCKET" --expected-bucket-owner "$ACCOUNT" && return 0
  ! aws_missing
}

# rn_remove: the bucket is emptied and the runner stack deleted. When CloudFormation fails on a refusal of the caller's own session (an IAM or session policy that denies deleting
# a role that has been used), the delete is made again with the CDK CloudFormation execution role, the role that deletes the stage's stacks. Returns 1, with RUNNER_LEFT set and the
# reason printed as RETAINED, when it cannot; RN_LEFT_WHY and RN_LEFT_COMMAND hold the reason and the command that removes the stack.
rn_remove() {
  local why="" role first second="" bucket_part="" status="" status_part="" retried=0
  RUNNER_LEFT=0
  RN_LEFT_WHY=""
  RN_LEFT_STATUS=""
  RN_LEFT_COMMAND=""
  if [ "$RN_EXISTS" != 1 ]; then return 0; fi
  role="arn:aws:iam::$ACCOUNT:role/cdk-$SETUP_BOOTSTRAP_QUALIFIER-cfn-exec-role-$ACCOUNT-$REGION"
  say_log "Removing the runner stack $SETUP_RUNNER_STACK and its bucket $RN_BUCKET ..."
  if ! rn_empty_bucket; then
    why="the bucket could not be emptied: ${AWS_ERR:-no answer}"
    RUNNER_LEFT=1
  elif ! rn_delete_runner ""; then
    RUNNER_LEFT=1
    why=$RN_LEFT_WHY
    status=$RN_LEFT_STATUS
    if [ "$status" = DELETE_FAILED ] && [ "$RN_LEFT_DENIED" = 1 ]; then
      retried=1
      first=$why
      say_log "CloudFormation cannot delete the runner stack with your own permissions: $first"
      say_log "Deletion is retried with the CDK CloudFormation execution role $role. This role deletes the stage's stacks."
      if rn_delete_runner "$role"; then
        RUNNER_LEFT=0
        why=""
        status=""
      else
        why=$first
        second=$RN_LEFT_WHY
        status=DELETE_FAILED
      fi
    fi
  fi
  if [ "$RUNNER_LEFT" = 1 ]; then
    if rn_bucket_exists; then bucket_part=" and bucket $RN_BUCKET"; fi
    RN_LEFT_WHY=$why
    RN_LEFT_COMMAND=""
    # the command that removes the stack: the plain delete; with a role that may delete the stack's IAM role when the caller was refused and the execution role did not work either;
    # none when the bucket must be emptied first
    case $why in
      "the bucket could not be emptied"*) ;;
      *)
        if [ "$retried" = 1 ]; then
          RN_LEFT_COMMAND="aws cloudformation delete-stack --stack-name $SETUP_RUNNER_STACK --role-arn <ARN of a role that may delete the stack's IAM role>"
        else
          RN_LEFT_COMMAND="aws cloudformation delete-stack --stack-name $SETUP_RUNNER_STACK"
        fi ;;
    esac
    status_part=""
    if [ -n "$status" ]; then status_part=" ($status)"; fi
    say "RETAINED stack $SETUP_RUNNER_STACK$status_part$bucket_part: $why"
    [ -z "$second" ] || say "      Deletion with the CDK CloudFormation execution role also fails: $second"
    [ -z "$RN_LEFT_COMMAND" ] || say "      You can remove the stack yourself (account $ACCOUNT, Region $REGION) with: $RN_LEFT_COMMAND"
    log_line "RETAINED $SETUP_RUNNER_STACK ${RN_BUCKET:-none}: $why"
    st_set retained "stack $SETUP_RUNNER_STACK$bucket_part" || true
    st_save || true
    return 1
  fi
  RN_EXISTS=0
  ok "runner removed"
  return 0
}
