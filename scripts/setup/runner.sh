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
#     results/<buildId>/phases.jsonl, client/<buildId>/<platform>/<binary>, client/<buildId>/SHA256SUMS
#   build environment: SETUP_SOURCE_KEY, SETUP_CONFIG_KEY, SETUP_STAGE, SETUP_AZS, SETUP_BINARY_NAME, SETUP_SOURCE_SHA256, SETUP_CONFIG_SHA256 (and
#     SETUP_ACTION=bootstrap-template); for a deploy build also SETUP_DEPLOY_LOCK_TOKEN, SETUP_EXPLICIT_SETTINGS (when the user chose settings), SETUP_FORCE_DEPLOY=1
#     (--force-deploy) and SETUP_CLIENT_ONLY=1
#     (when the stage already runs the commit)
#   SSM /hamh/<stage>/deploy-lock (the deploy lock, below) and /hamh/<stage>/settings (the record of the stage that deploy.py writes; rn_same_deployment reads it)
#
# Trust: the bucket is written by this script with the user's credentials and by the runner role, and its policy refuses every other principal, but a
# bucket is still not where a decision is made. The archive and the account file are uploaded every time (an object found under the key is not believed),
# the build is pinned to the version id the upload answered with and told the SHA-256 of both, and the result of the build is believed only when its SHA-256
# is the one the build recorded in CodeBuild (the exported variable SETUP_RESULT_SHA256, which nothing that writes the bucket can change).
#
# State: AWS is authoritative. The deploy lock keeps two setups from racing; a repeated or interrupted setup attaches to the build the lock records
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
BUILD_MINUTES=""
BUILD_COMPUTE_TYPE=""
RUNNER_LEFT=0
RN_LEFT_WHY=""
RN_LEFT_STATUS=""
RN_LEFT_COMMAND=""
RN_FAIL_WHY=""
RN_FAIL_FULL=""
RN_LEFT_DENIED=0
RN_LOCK_TEXT=""
RN_LOCK_SEEN=""
RN_LOCK_TOKEN=""
RN_LOCK_STARTED=""
RN_LOCK_ROUTE=runner
RN_LOCK_HELD=0
RN_LOCK_LEGACY=0
RN_LOCK_BUILD=""
RN_LOCK_STALE=""
RN_LOCK_INTERRUPTED=""
RN_STARTING=0
RN_INSTALLED_NOW=0
RN_SAME_WHY=""
RN_BLOCKED_WHY=""
# RN_BLOCKED_FAILED is 1 when the runner stays because a look or a write of the removal guard failed (that is RETAINED, exit 8), 0 when another setup uses it.
# RN_LOCK_LEFT is 1 once this run has left a shared lock to another setup (rn_lock_leave).
RN_BLOCKED_FAILED=0
RN_LOCK_LEFT=0
RN_LOCK_PARTIES=""
RN_LOCK_OTHERS=""
# RN_LOCK_DEPLOYS is `no` while this run's hold of the lock changes nothing on the stage: an update that builds only the client (SETUP_CLIENT_ONLY), the removal of a
# runner that was left (rn_lease_take_soft), and the time after the build has succeeded. The lock then carries the line deploys=no, and the stage's API service, which starts
# no new runs while a deploy holds the lock, lets runs start. A lock without the line deploys. The line is never taken back: a run that finds it in the lock it shares keeps it.
RN_LOCK_DEPLOYS=""
RN_LOCK_AT=0
# RN_LOCK_OPEN is 1 when the lock in the account says deploys=no as far as this run knows: it wrote the lock that way, or read it so.
RN_LOCK_OPEN=0
# RN_LOCK_BUSY is 1 while scripts/deploy.sh of the local route runs under this run's lock: the lock then stays through an exit (rn_lock_on_exit).
RN_LOCK_BUSY=0
# RN_RELEASE is what the last rn_lease_release did with this run's lock: `released` (deleted, or gone already), `shared` (another setup that follows the same build
# is still at work under it, so the lock stays for that setup), `other` (the lock in its place holds another token and is left alone), `kept` (the read or the delete
# failed, so the lock stays as this run left it; RN_RELEASE_ERR is the error and RN_RELEASE_HINT what it names) or `none` (this run held no lock).
RN_RELEASE=none
RN_RELEASE_ERR=""
RN_RELEASE_HINT=""
# The explicit settings of this run are SETUP_EXPLICIT_LINES (one HAMH_<NAME>=value line each) and SETUP_EXPLICIT_SETTINGS (base64 of these lines):
# pf_explicit_settings of preflight.sh collects, checks and encodes them, and lib.sh empties both when it is sourced, so nothing here comes from the caller's shell. The
# build receives SETUP_EXPLICIT_SETTINGS as one override (rn_start), and the deploy lock records it in its `inputs` (rn_inputs_digest).

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
  [ "${BUILD_ON:-runner}" != local ] || printf ' --build-on local'
}

# rn_standing_text: the sentence that says a runner is still there, what it can do and until when, said on every run while the stack exists. The way out it names only removes
# the runner when the stage is deployed: `./setup.sh --full` on a deployed stage builds nothing and removes a runner that no build uses (full_existing); an update or a
# rebuild would cost a whole build. A stage that is not deployed yet is finished with `./setup.sh --full`.
rn_standing_text() {
  local way
  case ${STAGE_STATE:-} in
    complete|deploying|broken) way="To remove only the runner, run ./setup.sh --full: the stage is deployed, so that run builds nothing and removes the runner. ./setup.sh --teardown removes the stage and the runner." ;;
    *) way="Run ./setup.sh --full to finish the setup, or ./setup.sh --teardown to remove the stage and the runner." ;;
  esac
  printf 'the runner stack %s is still installed in account %s (bucket %s). While it exists, anyone who may start a build of its CodeBuild project (codebuild:StartBuild) runs commands with the authority of the CDK roles through it%s. %s' \
    "$SETUP_RUNNER_STACK" "${ACCOUNT:-this}" "${RN_BUCKET:-none}" "${RN_EXPIRES:+ until $RN_EXPIRES (UTC)}" "$way"
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
  case $RN_MODE in attach|finish) return 0 ;; esac  # another setup started the build of these inputs: it made the bootstrap
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
    # The quotes are part of the message; the expansion is in the double-quoted string around them.
    # shellcheck disable=SC2016
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
  local why="$1: ${AWS_ERR:-no answer}" hint
  hint=$(rn_aws_hint)
  log_line "$why"
  trap - INT TERM
  rn_lease_release
  if [ "${FULL_ACTION:-full}" = update ]; then
    setup_exit 5 "$why" ${hint:+"$hint"} \
      "Creating a stack with IAM roles needs credentials that may create IAM roles, S3 buckets and ECR repositories (normally an administrator)." \
      "The stage is unchanged: no build was started." "Run $(rn_retry_command) again when the cause is fixed."
  fi
  setup_exit 5 "$why" ${hint:+"$hint"} \
    "Creating a stack with IAM roles needs credentials that may create IAM roles, S3 buckets and ECR repositories (normally an administrator)." \
    "Nothing else was created after this step. Run setup.sh again when the cause is fixed."
}

# rn_install: the runner stack. Idempotent: CloudFormation changes nothing when the template and the parameters are the same. Every install writes the parameters
# again: the identity that runs this setup is the only one besides the runner role that may write the bucket (InstallerPrincipal and InstallerPrincipalWithPath, from
# pf_installer), and the runner role can assume the CDK roles for AUTH_HOURS from now (AuthorityExpires), so a runner that is left behind stops being able to act by itself.
rn_install() {
  local expires
  local -a build_params=()
  case $RN_MODE in attach|finish) [ "$RN_EXISTS" = 1 ] || rn_describe; return 0 ;; esac  # the build of these inputs is another setup's: its runner stays as it is
  rn_prepare_stack "$SETUP_RUNNER_STACK"
  expires=$(setup_at_plus $((AUTH_HOURS * 3600)))
  # HAMH_SETUP_BUILD_MINUTES and HAMH_SETUP_BUILD_COMPUTE (pf_installer) are passed only when set: a parameter left out keeps the value the stack has, or the template default (240 minutes, large).
  [ -z "$BUILD_MINUTES" ] || build_params+=("BuildTimeoutMinutes=$BUILD_MINUTES")
  [ -z "$BUILD_COMPUTE_TYPE" ] || build_params+=("BuildComputeType=$BUILD_COMPUTE_TYPE")
  say_log "Installing the runner stack $SETUP_RUNNER_STACK (about 2 minutes) ..."
  if ! aws_run cloudformation deploy --stack-name "$SETUP_RUNNER_STACK" --template-file "$SELF_DIR/$SETUP_RUNNER_TEMPLATE" \
      --parameter-overrides "InstallerPrincipal=$INSTALLER_P1" "InstallerPrincipalWithPath=$INSTALLER_P2" "AuthorityExpires=$expires" ${build_params[@]+"${build_params[@]}"} \
      --capabilities CAPABILITY_NAMED_IAM --no-fail-on-empty-changeset --tags "project=$SETUP_PROJECT_TAG" managed-by=setup.sh; then
    rn_fail_stack "installing the runner stack failed"
  fi
  rn_describe
  if [ "$RN_EXISTS" != 1 ] || [ -z "$RN_BUCKET" ] || [ -z "$RN_PROJECT" ]; then
    setup_exit 5 "the runner stack $SETUP_RUNNER_STACK has no SetupBucket or BuildProject output" "Delete the stack and run setup.sh again: ./setup.sh --teardown"
  fi
  [ -n "$RN_LOGGROUP" ] || RN_LOGGROUP=/aws/codebuild/$SETUP_RUNNER_STACK
  RN_INSTALLED_NOW=1
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
  case $RN_MODE in attach|finish) return 0 ;; esac  # the build of these inputs is another setup's, and it uploaded them
  say_log "Packing and uploading the source archive of commit $SRC_COMMIT ..."
  ( umask 077; git -C "$SELF_DIR" archive --format=zip -o "$archive" "$SRC_COMMIT" ) || { AWS_ERR="git archive failed"; rn_fail_before_build "git archive of commit $SRC_COMMIT failed"; }
  SRC_SHA256=$(setup_sha256 <"$archive")
  rn_put "$archive" "$SRC_KEY" || rn_fail_before_build "uploading the source archive to s3://$RN_BUCKET/$SRC_KEY failed"
  case $AWS_OUT in
    ''|None|null)
      AWS_ERR="no version id"
      rn_fail_before_build "S3 gave no version id for s3://$RN_BUCKET/$SRC_KEY, so the build cannot be pinned to the bytes just uploaded. Versioning of the bucket $RN_BUCKET was switched off or suspended. Turn it on, or run ./setup.sh --teardown and set up again" ;;
  esac
  setup_safe_word "$AWS_OUT" || { AWS_ERR=$AWS_OUT; rn_fail_before_build "S3 answered with a version id setup does not trust"; }
  SRC_VERSION=$AWS_OUT
  ok "uploaded $SRC_KEY (version $SRC_VERSION, SHA-256 $(rn_first12 "$SRC_SHA256"))"
  ( umask 077; printf '%s' "$ACCT_JSON" >"$config" ) || setup_exit 1 "could not write $config"
  rn_put "$config" "$ACCT_KEY" || rn_fail_before_build "uploading the account file to s3://$RN_BUCKET/$ACCT_KEY failed"
  ok "uploaded $ACCT_KEY"
}

# ---- the deploy lock --------------------------------------------------------------------------------------------
# (CONTRACT C6): the SSM String /hamh/<stage>/deploy-lock keeps two deploys of a stage from running at once, whatever the route, and keeps a setup from rewriting the
# runner (its bucket policy names the identity that installed it) while another setup uses it. setup.sh reads it in preflight (rn_lease_check) and takes it right after the
# confirmation, before the runner is installed (rn_lease_take); the runner route records the build in it after start-build (rn_lease_record) and passes its token to the build
# (SETUP_DEPLOY_LOCK_TOKEN; runner-build.sh hands it to deploy.py as HAMH_DEPLOY_LOCK_TOKEN). Its value is key=value lines: token, holder (<host>:<pid>), identity (the caller
# ARN), route (runner, local or manual), started (UTC), commit, inputs (rn_inputs_digest), build (the CodeBuild id, empty until the build starts) and, after a build that ended
# while it was changing the stage, mark=deploy-interrupted. It is created without --overwrite, so ParameterAlreadyExists means someone else holds it, and only its holder (the
# token decides) overwrites or deletes it. The holder keeps it until it has removed its runner (or reported it RETAINED): the release is its last step (full_runner_end of
# full.sh), and while it downloads and installs the client it writes a new `started` (rn_lock_renew), so that no other setup rewrites or removes the runner it still uses
# (Addendum 4, A4.4). Two setups of the same commit and inputs share one lock: the second follows the first one's build and writes itself into the lock (rn_lock_join), which
# then has one more line, parties=<host>:<pid>@<UTC time>,...: every setup at work under the lock with the time it wrote the lock last. A setup that ends while another
# one of that line is still at work (it wrote the lock within 30 minutes and is not a process of this machine that has ended) leaves the runner and the lock to it
# (rn_remove_blocked, rn_lease_release), and every setup writes the lock again while it follows the build (rn_lock_keep). A lock of one setup has no such line.
# Stale: a recorded build that ended without success; a recorded build that succeeded when the lock was not renewed for 30 minutes or its holder is a setup
# process of this machine that has ended (rn_lock_fresh, which asks it of every setup the lock names); no build and `started` more than 30 minutes ago; no build and a holder on this machine whose process has ended.
# Releases kept a lease at lease/<stage>.json of the runner bucket; it is still read (rn_legacy_lease_check), so a build such a setup started is
# attached to or waited for, never raced.
# SSM has no conditional write: two setups that write the shared lock in the same moment can each replace the other's value. A setup that writes itself into a
# shared lock reads it back after a short pause and writes itself in again when its entry is gone (rn_lock_renew); the setup that is about to remove the runner waits twice
# that pause before its guard reads the lock (full_runner_end); and a setup that has left the lock to another one reads it once more, because that one may have ended
# at the same moment and left it back (rn_lock_left_back). The pause is HAMH_SETUP_LOCK_SETTLE_SECONDS (5 by default; the tests set 0).
# The number is the one integer line of scripts/lib/deploy-lock-stale-seconds.txt. The stage's API service embeds the same file for its own use of the rule: a lock that was
# written within that time closes the stage to new runs. So the two cannot differ. A partial copy of the tree without the file
# gets the same number.
RN_LOCK_STALE_SECONDS=$(setup_number "$(head -n 1 "${SELF_DIR:-.}/scripts/lib/deploy-lock-stale-seconds.txt" 2>/dev/null)" 1800)

# rn_lock_settle [times]: the pause between a write of the shared lock and the read that checks it (HAMH_SETUP_LOCK_SETTLE_SECONDS, 5 by default), times the number given.
rn_lock_settle() {
  local s
  s=$(setup_number "${HAMH_SETUP_LOCK_SETTLE_SECONDS:-5}" 5)
  s=$((s * ${1:-1}))
  [ "$s" -eq 0 ] || sleep "$s"
}

# rn_lock_names_me: 0 when the lock just read (RN_LOCK_TEXT) names this run, as its holder or in its `parties` line.
rn_lock_names_me() {
  rn_lock_parties | awk -F@ -v me="$(uname -n):$$" '$1 == me { f = 1 } END { exit f ? 0 : 1 }'
}

# rn_first12 <text>: the first 12 characters.
rn_first12() { printf '%s' "$1" | cut -c1-12; }

# the lock of the stage of the generation in use (GEN_ROOT of lib.sh): a teardown of the previous generation reads that generation's names only
rn_lock_name() { printf '%s/%s/deploy-lock' "${GEN_ROOT:-/hamh}" "$STAGE"; }

# rn_lock_read: RN_LOCK_TEXT is the value of the lock, empty when there is none. Returns 1 when the read failed (AWS_ERR says why): a failed read never counts as no lock.
rn_lock_read() {
  RN_LOCK_TEXT=""
  if aws_run ssm get-parameter --name "$(rn_lock_name)" --query Parameter.Value --output text; then
    RN_LOCK_TEXT=$AWS_OUT
    return 0
  fi
  aws_missing
}

# rn_kv <text> <key>: the value of the first line `key=value` of the text (split at the first =): the lock and the record of the stage use this form.
rn_kv() {
  printf '%s\n' "$1" | awk -v k="$2" 'index($0, k "=") == 1 { print substr($0, length(k) + 2); exit }'
}

rn_lock_get() { rn_kv "$RN_LOCK_TEXT" "$1"; }

# rn_inputs_digest: what the lock records as `inputs`: the SHA-256 of the account file this run uploads and of the explicit settings it sends. A run attaches only to a build of
# the same commit and the same inputs, because a build with other settings would deploy other values.
rn_inputs_digest() {
  printf 'config=%s\nsettings=%s\n' "$ACCT_DIGEST" "$SETUP_EXPLICIT_SETTINGS" | setup_sha256
}

# rn_lock_token_new: 32 hexadecimal digits that name this run's hold of the lock.
rn_lock_token_new() {
  printf '%s:%s:%s:%s%s%s%s:%s:%s' "$(uname -n)" "$$" "$(setup_now)" "$RANDOM" "$RANDOM" "$RANDOM" "$RANDOM" "${CALLER_ARN:-}" "$SRC_COMMIT" | setup_sha256 | cut -c1-32
}

# rn_lock_value <build id> [mark]: the text of the lock this run holds, with the line `parties` when other setups are at work under it (RN_LOCK_PARTIES, rn_lock_sync).
rn_lock_value() {
  printf 'token=%s\nholder=%s:%s\nidentity=%s\nroute=%s\nstarted=%s\ncommit=%s\ninputs=%s\nbuild=%s' "$RN_LOCK_TOKEN" "$(uname -n)" "$$" "${CALLER_ARN:-unknown}" \
    "$RN_LOCK_ROUTE" "$RN_LOCK_STARTED" "$SRC_COMMIT" "$(rn_inputs_digest)" "$1"
  [ "$RN_LOCK_DEPLOYS" != no ] || printf '\ndeploys=no'
  [ -z "${2:-}" ] || printf '\nmark=%s' "$2"
  [ -z "$RN_LOCK_PARTIES" ] || printf '\nparties=%s' "$RN_LOCK_PARTIES"
}

# rn_epoch_of <UTC time, 2026-11-10T16:00:00Z>: the seconds since 1970 (GNU date -d, else BSD date -j -f); returns 1 for a text that is no such time.
rn_epoch_of() {
  case $1 in
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]Z) ;;
    *) return 1 ;;
  esac
  date -u -d "$1" +%s 2>/dev/null || date -j -u -f '%Y-%m-%dT%H:%M:%SZ' "$1" +%s 2>/dev/null
}

# rn_party_gone <host>:<pid>: 0 when that setup is a process of this machine that has ended.
rn_party_gone() {
  local host=${1%:*} pid=${1##*:} out
  [ "$host" = "$(uname -n)" ] || return 1
  case $pid in ''|*[!0-9]*) return 1 ;; esac
  out=$(kill -0 "$pid" 2>&1) && return 1
  case $out in *"No such process"*) return 0 ;; esac
  return 1
}

# rn_lock_holder_gone: 0 when the holder of the lock is a setup process of this machine that has ended.
rn_lock_holder_gone() { rn_party_gone "$(rn_lock_get holder)"; }

# rn_party_at_work <host>:<pid> <UTC time of its last write>: 0 while that setup may still be at work: it wrote the lock at most 30 minutes ago, and it is not a setup process
# of this machine that has ended.
rn_party_at_work() {
  local at
  at=$(rn_epoch_of "$2") || return 1
  [ $(($(setup_epoch) - at)) -le "$RN_LOCK_STALE_SECONDS" ] || return 1
  ! rn_party_gone "$1"
}

# rn_lock_parties: one line <host>:<pid>@<UTC time> for every setup the lock (RN_LOCK_TEXT) names: its `parties` line, or, for a lock without one (a single setup), its holder
# with its start time.
rn_lock_parties() {
  local line holder
  line=$(rn_lock_get parties)
  if [ -n "$line" ]; then
    printf '%s\n' "$line" | tr ',' '\n'
    return 0
  fi
  holder=$(rn_lock_get holder)
  [ -z "$holder" ] || printf '%s@%s\n' "$holder" "$(rn_lock_get started)"
}

# rn_lock_others: RN_LOCK_OTHERS becomes the entries (<host>:<pid>@<UTC time>, comma-separated) of the lock just read (RN_LOCK_TEXT) for the setups other than this run that
# are still at work; 0 when there is one.
rn_lock_others() {
  local entry me
  me="$(uname -n):$$"
  RN_LOCK_OTHERS=""
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    [ "${entry%@*}" != "$me" ] || continue
    rn_party_at_work "${entry%@*}" "${entry##*@}" || continue
    RN_LOCK_OTHERS="${RN_LOCK_OTHERS:+$RN_LOCK_OTHERS,}$entry"
  done <<<"$(rn_lock_parties)"
  [ -n "$RN_LOCK_OTHERS" ]
}

# rn_lock_fresh: 0 while a setup under the lock may still be at work: one of the setups the lock names (its holder, or every setup of its `parties` line) wrote the lock at most
# 30 minutes ago, which a setup does again while it works, and is not a setup process of this machine that has ended.
rn_lock_fresh() {
  local entry
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    if rn_party_at_work "${entry%@*}" "${entry##*@}"; then return 0; fi
  done <<<"$(rn_lock_parties)"
  return 1
}

# rn_lock_holder_text: who holds the lock, in words.
rn_lock_holder_text() {
  local holder here=""
  holder=$(rn_lock_get holder)
  [ "${holder%:*}" != "$(uname -n)" ] || here=" (this machine)"
  printf '%s%s, identity %s, route %s, started %s UTC, commit %s' "${holder:-an unknown setup}" "$here" "$(rn_lock_get identity)" "$(rn_lock_get route)" \
    "$(rn_lock_get started)" "$(rn_lock_get commit)"
  # the setups that follow one build under this lock (its `parties` line), without their times
  [ -z "$(rn_lock_get parties)" ] || printf '; setups under this lock: %s' "$(rn_lock_parties | sed 's/@.*//' | tr '\n' ' ' | sed 's/ *$//; s/ /, /g')"
}

# rn_lock_held_exit <what the holder is doing>: someone else holds the lock. Exit 7 before anything is created, naming the holder, that it may be the user's own earlier
# setup, and, when it is a setup on the runner route whose commit is in this clone, how to attach to it. A deploy of scripts/deploy.sh (route manual) and a setup that
# deploys from its own machine (route local) run no build that another setup can follow, so for them the way on is to wait.
rn_lock_held_exit() {
  local commit hint=""
  commit=$(rn_lock_get commit)
  case $(rn_lock_get route) in
    manual) hint=" It is a deploy that scripts/deploy.sh runs (route manual), which no setup can attach to. Wait for it to end: it gives the lock back when it ends." ;;
    local) hint=" It is a setup that deploys from its own machine (route local, --build-on local), which no other setup can attach to. Wait for it to end: it gives the lock back when it ends." ;;
    *)
      if setup_is_commit "$commit" && git -C "$SELF_DIR" cat-file -e "$commit^{commit}" 2>/dev/null; then
        hint=" If it is, run  git checkout $commit  and then  ./setup.sh --update  to attach to it."
      fi ;;
  esac
  log_line "the deploy lock of stage $STAGE is held: $RN_LOCK_TEXT"
  setup_exit 7 "the deploy lock $(rn_lock_name) of stage $STAGE is held: $1" \
    "Holder: $(rn_lock_holder_text)." \
    "It may be your own earlier setup on this machine or another one.$hint" \
    "Otherwise wait for it to end; ./setup.sh --status shows the stage. Nothing was changed."
}

# rn_lock_adopt: a build of this commit and these inputs is running or has succeeded under the lock: this run follows it and gives the lock back at the end, as its own.
rn_lock_adopt() {
  # the build first: a lock that is held without a build is given back when the run ends (rn_lock_on_exit)
  RN_LOCK_BUILD=$RN_BUILD_ID
  RN_LOCK_TOKEN=$(rn_lock_get token)
  RN_LOCK_STARTED=$(rn_lock_get started)
  RN_LOCK_ROUTE=$(rn_lock_get route)
  RN_LOCK_HELD=1
  # A hold that deploys nothing stays one
  RN_LOCK_DEPLOYS=""
  RN_LOCK_OPEN=0
  if [ "$(rn_lock_get deploys)" = no ]; then
    RN_LOCK_DEPLOYS=no
    RN_LOCK_OPEN=1
  fi
}

# rn_build_status <build id>: prints the status of a build, nothing when CodeBuild does not know it; returns 1 when the look itself failed.
rn_build_status() {
  if aws_run codebuild batch-get-builds --ids "$1" --query 'builds[0].buildStatus' --output text; then
    [ "$AWS_OUT" = None ] || printf '%s' "$AWS_OUT"
    return 0
  fi
  return 1
}

# rn_lock_find_build <token>: a build that the setup holding the lock started and did not record (it stopped between start-build and the record): the newest builds of the
# runner project whose environment carries the token. Prints "<id><TAB><status>", nothing when there is none; returns 1 when a look failed.
rn_lock_find_build() {
  local ids id n=0 token
  [ -n "$RN_PROJECT" ] && [ -n "$1" ] || return 0
  aws_run codebuild list-builds-for-project --project-name "$RN_PROJECT" --sort-order DESCENDING --query ids --output text || return 1
  ids=$AWS_OUT
  [ -n "$ids" ] && [ "$ids" != None ] || return 0
  for id in $ids; do
    n=$((n + 1))
    [ "$n" -le 5 ] || break
    aws_run codebuild batch-get-builds --ids "$id" --query "builds[0].environment.environmentVariables[?name=='SETUP_DEPLOY_LOCK_TOKEN'].value" --output text || return 1
    token=$AWS_OUT
    [ "$token" = "$1" ] || continue
    token=$(rn_build_status "$id") || return 1
    printf '%s\t%s' "$id" "$token"
    return 0
  done
  return 0
}

# rn_lease_check: decides RN_MODE from the deploy lock of the stage (or the lease of an earlier release) and the build it records. It only reads; nothing is changed.
#   new     no lock
#   stale   the build the lock records ended without success; or it succeeded, with other inputs than this run's, and the lock was not renewed for 30 minutes or its holder
#           process on this machine has ended; or the lock records no build and is older than 30 minutes or its holder process on this machine has ended. rn_lease_take
#           replaces it; RN_LOCK_STALE says why, for the plan and the log
#   attach  the lock records a build of the same commit and inputs that is running (or names none, and such a build carries its token): follow it
#   finish  the same, for a build that succeeded: read its result
# Any other lock is held: exit 7 before the confirmation, naming the holder. When the build of a stale lock ended while it was changing the stage (mark=deploy-interrupted, or a
# build that ended badly while a stack of the stage is still changing), that setup is named and a stack that is still changing is waited for (rn_wait_interrupted).
# A runner stack that an earlier removal left in DELETE_FAILED is named here too, before the confirmation (rn_runner_check).
rn_lease_check() {
  local l_build l_commit l_started l_mark status same=0 age started_at found
  RN_MODE=new
  RN_LEASE_FLAT=""
  RN_LOCK_STALE=""
  RN_LOCK_INTERRUPTED=""
  RN_LOCK_LEGACY=0
  rn_runner_check
  rn_lock_read || setup_exit 6 "could not read the deploy lock $(rn_lock_name) of stage $STAGE: ${AWS_ERR:-no answer}" \
    "A failed read is not taken as no lock. Nothing was changed. Check that your credentials may call ssm:GetParameter, then run $(rn_retry_command) again."
  RN_LOCK_SEEN=$RN_LOCK_TEXT
  if [ -z "$RN_LOCK_TEXT" ]; then
    rn_legacy_lease_check
    return 0
  fi
  l_build=$(rn_lock_get build)
  l_commit=$(rn_lock_get commit)
  l_started=$(rn_lock_get started)
  l_mark=$(rn_lock_get mark)
  if [ "$l_commit" = "$SRC_COMMIT" ] && [ "$(rn_lock_get inputs)" = "$(rn_inputs_digest)" ]; then same=1; fi
  if [ -z "$l_build" ]; then
    if [ "$same" = 1 ]; then
      found=$(rn_lock_find_build "$(rn_lock_get token)") || setup_exit 6 "could not look for the build of the deploy lock of stage $STAGE: ${AWS_ERR:-no answer}" \
        "Nothing was changed. Run $(rn_retry_command) again in a minute."
      case ${found#*"$TAB"} in
        IN_PROGRESS) RN_MODE=attach; RN_BUILD_ID=${found%%"$TAB"*}; rn_lock_adopt; return 0 ;;
        SUCCEEDED) RN_MODE=finish; RN_BUILD_ID=${found%%"$TAB"*}; rn_lock_adopt; return 0 ;;
      esac
    fi
    started_at=$(rn_epoch_of "$l_started") || started_at=0
    age=$(($(setup_epoch) - started_at))
    if [ "$age" -gt "$RN_LOCK_STALE_SECONDS" ]; then
      RN_MODE=stale
      RN_LOCK_STALE="the deploy lock taken by $(rn_lock_get holder) at ${l_started:-an unknown time} never recorded a build and is older than 30 minutes"
      log_line "$RN_LOCK_STALE"
      return 0
    fi
    if rn_lock_holder_gone; then
      RN_MODE=stale
      RN_LOCK_STALE="the deploy lock was taken by the setup process $(rn_lock_get holder) of this machine, which ended before it started a build"
      log_line "$RN_LOCK_STALE"
      return 0
    fi
    rn_lock_held_exit "no build is recorded in it yet (its holder is about to start one, or stopped before it did)"
  fi
  status=$(rn_build_status "$l_build") || setup_exit 6 "could not look at the build $l_build that the deploy lock of stage $STAGE records" \
    "The build is not affected. Nothing was changed. Run $(rn_retry_command) again in a minute."
  case $status in
    IN_PROGRESS)
      if [ "$same" = 1 ]; then RN_MODE=attach; RN_BUILD_ID=$l_build; rn_lock_adopt; return 0; fi
      rn_lock_held_exit "its build $l_build is running" ;;
    SUCCEEDED)
      if [ "$same" = 1 ]; then RN_MODE=finish; RN_BUILD_ID=$l_build; rn_lock_adopt; return 0; fi
      # The build of another setup succeeded: that setup still downloads its client from the runner's bucket and removes the runner as its last step before it gives the lock
      # back (A4.4). The lock is its holder's while the holder renews it; a run that replaced it would rewrite the runner's bucket policy under that download.
      if rn_lock_fresh; then
        rn_lock_held_exit "its build $l_build has succeeded, and the setup that started it is still installing its client and removing its runner"
      fi
      RN_MODE=stale
      RN_LOCK_STALE="the deploy lock recorded build $l_build, which succeeded, and its holder $(rn_lock_get holder) has ended or has not renewed the lock for 30 minutes"
      log_line "$RN_LOCK_STALE"
      return 0 ;;
  esac
  RN_MODE=stale
  RN_LOCK_STALE="the deploy lock recorded build $l_build, which has ended (${status:-unknown to CodeBuild})"
  log_line "$RN_LOCK_STALE"
  if [ "$l_mark" = deploy-interrupted ] || { [ "$status" != SUCCEEDED ] && [ "${STAGE_STATE:-}" = deploying ]; }; then
    RN_LOCK_INTERRUPTED="the build $l_build of your earlier setup ($(rn_lock_get holder), started ${l_started:-at an unknown time} UTC) ended (${status:-unknown}) while it was changing the stage"
    rn_wait_interrupted
  fi
  return 0
}

# rn_wait_interrupted: the build of an earlier setup ended while it was changing the stage. That setup is named as the cause, and a stack of the
# stage that is still changing is waited for (HAMH_SETUP_STACK_LOOKS looks, a pause apart) before this run goes on; pf_stage then says what the stage is.
rn_wait_interrupted() {
  local looks=0 limit=${HAMH_SETUP_STACK_LOOKS:-120}
  warn "$RN_LOCK_INTERRUPTED. CloudFormation may still be finishing or rolling back that change; ./setup.sh --status shows the stacks."
  while [ "${STAGE_STATE:-}" = deploying ]; do
    looks=$((looks + 1))
    if [ "$looks" -gt "$limit" ]; then
      setup_exit 6 "the stack ${STAGE_BAD%% *} is still ${STAGE_BAD#* } after the build of your earlier setup ended" \
        "It is your own earlier update that CloudFormation is still finishing, not another operation. Run ./setup.sh --status to look, and $(rn_retry_command) again once it has settled. Nothing was changed."
    fi
    [ "$looks" != 1 ] || say "Waiting for the stack ${STAGE_BAD%% *} (${STAGE_BAD#* }) to settle before this run deploys again ..."
    setup_sleep
    pf_stage
  done
  ok "the stage has settled after the build of your earlier setup ($STAGE_STATE)"
}

# rn_runner_check: a runner stack that an earlier removal left in DELETE_FAILED cannot be installed again (CloudFormation cannot change it). Named before the confirmation with
# what removes it; on a deployed stage `./setup.sh --full` removes it (full_existing), and the commands are printed for a stage that is not.
rn_runner_check() {
  local role
  # the local route installs no runner, so a runner stack that is stuck does not stop it
  [ "${BUILD_ON:-runner}" != local ] || return 0
  [ "$RN_EXISTS" = 1 ] || return 0
  rn_stack_status "$SETUP_RUNNER_STACK" || setup_exit 1 "could not look at the stack $SETUP_RUNNER_STACK: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:DescribeStacks."
  [ "$RN_STATUS" = DELETE_FAILED ] || return 0
  # a deployed stage with a full setup: full_existing removes the runner, no build needed
  [ "${FULL_ACTION:-full}" != full ] || [ "${STAGE_STATE:-}" != complete ] || return 0
  role="arn:aws:iam::$ACCOUNT:role/cdk-$SETUP_BOOTSTRAP_QUALIFIER-cfn-exec-role-$ACCOUNT-$REGION"
  setup_exit 7 "the runner stack $SETUP_RUNNER_STACK is in DELETE_FAILED: an earlier removal did not finish, and CloudFormation cannot install it again" \
    "On a deployed stage, run ./setup.sh --full: it builds nothing, empties the bucket ${RN_BUCKET:-of the runner} and deletes the stack (with the CDK CloudFormation execution role when your own permissions are refused)." \
    "Or remove it yourself: empty the bucket ${RN_BUCKET:-of the runner} (every version), then run  aws cloudformation delete-stack --stack-name $SETUP_RUNNER_STACK --role-arn $role" \
    "Then run $(rn_retry_command) again. Nothing was changed."
}

# rn_legacy_lease_check: the lease of a release (lease/<stage>.json in the runner bucket), read when the stage has no deploy lock: a build such a setup started
# with the same inputs is attached to (or finished from), one of other inputs that is running stops this run; anything else is no lock.
rn_legacy_lease_check() {
  local l_commit l_digest l_build l_owner l_action status same=0
  [ "$RN_EXISTS" = 1 ] && [ -n "$RN_BUCKET" ] || return 0
  if aws_run s3 cp "s3://$RN_BUCKET/lease/$STAGE.json" -; then
    RN_LEASE_FLAT=$(printf '%s' "$AWS_OUT" | json_flat)
  elif aws_missing; then
    return 0
  else
    setup_exit 6 "could not read the lease of the stage from s3://$RN_BUCKET/lease/$STAGE.json: ${AWS_ERR:-no answer}" "Nothing was changed. Run $(rn_retry_command) again in a minute."
  fi
  l_commit=$(cl_flat_get "$RN_LEASE_FLAT" sourceCommit)
  l_digest=$(cl_flat_get "$RN_LEASE_FLAT" configDigest)
  l_build=$(cl_flat_get "$RN_LEASE_FLAT" buildId)
  l_owner=$(cl_flat_get "$RN_LEASE_FLAT" owner)
  l_action=$(cl_flat_get "$RN_LEASE_FLAT" action)
  if [ "$(rn_first12 "$l_commit")" = "$(rn_first12 "$SRC_COMMIT")" ] && [ "$(rn_first12 "$l_digest")" = "$(rn_first12 "$ACCT_DIGEST")" ] \
      && [ "${l_action:-deploy}" = "${RN_ACTION:-deploy}" ] && [ -z "$SETUP_EXPLICIT_LINES" ]; then
    same=1
  fi
  [ -n "$l_build" ] || return 0
  status=$(rn_build_status "$l_build") || setup_exit 6 "could not look at the build $l_build that the lease of stage $STAGE records" \
    "The build is not affected. Run ./setup.sh again in a minute."
  case $status in
    IN_PROGRESS)
      if [ "$same" = 1 ]; then RN_MODE=attach; RN_BUILD_ID=$l_build; RN_LOCK_LEGACY=1; return 0; fi
      setup_exit 7 "another setup is running: build $l_build for source commit $(rn_first12 "$l_commit") (started by $l_owner, a setup of an earlier release)" \
        "It may be your own earlier setup. It is not the same source or account file as this run, so setup does not start a second build. Wait for it, or follow it with: ./setup.sh --status" ;;
    SUCCEEDED)
      if [ "$same" = 1 ]; then RN_MODE=finish; RN_BUILD_ID=$l_build; RN_LOCK_LEGACY=1; return 0; fi ;;
  esac
  log_line "the lease of stage $STAGE of an earlier release recorded build $l_build (${status:-unknown}); it is stale"
  return 0
}

# rn_lock_build_line: RN_LOCK_LINE is "<build id><TAB><status><TAB><phase>" of the build the deploy lock of the stage records (without a lock: the lease of an earlier release),
# "none<TAB>HELD<TAB>" for a lock that records no build yet and is not stale (a setup is about to start one), empty when there is neither; returns 1 when a read failed
# (AWS_ERR; RN_LOCK_WHAT names what could not be read). status.sh (status_lease_build) and teardown.sh (td_busy) read it, so both see a setup that is running and
# teardown never deletes under it.
RN_LOCK_LINE=""
RN_LOCK_WHAT=""
rn_lock_build_line() {
  local build="" started_at
  RN_LOCK_LINE=""
  RN_LOCK_WHAT="the deploy lock $(rn_lock_name)"
  rn_lock_read || return 1
  if [ -n "$RN_LOCK_TEXT" ]; then
    build=$(rn_lock_get build)
    if [ -z "$build" ]; then
      started_at=$(rn_epoch_of "$(rn_lock_get started)") || started_at=0
      [ $(($(setup_epoch) - started_at)) -gt "$RN_LOCK_STALE_SECONDS" ] || RN_LOCK_LINE=$(printf 'none\tHELD\t')
      return 0
    fi
  elif [ "$RN_EXISTS" = 1 ] && [ -n "$RN_BUCKET" ]; then
    RN_LOCK_WHAT="the lease of stage $STAGE that an earlier release left in the runner bucket (s3://$RN_BUCKET/lease/$STAGE.json)"
    if aws_run s3 cp "s3://$RN_BUCKET/lease/$STAGE.json" -; then
      build=$(printf '%s' "$AWS_OUT" | json_flat | awk -F '\t' '$1 == "buildId" { print $2; exit }')
    elif ! aws_missing; then
      return 1
    fi
  fi
  [ -n "$build" ] || return 0
  if aws_run codebuild batch-get-builds --ids "$build" --query 'builds[0].[buildStatus,currentPhase]' --output text; then
    RN_LOCK_LINE=$(printf '%s\t%s' "$build" "$AWS_OUT")
  else
    RN_LOCK_LINE=$(printf '%s\tunknown\t' "$build")
  fi
}

# rn_lock_replace_stale: the stale lock preflight read is deleted when it is still that lock. A lock that changed since then belongs to a setup that came in between: it is left,
# and the create that follows fails and is judged again.
rn_lock_replace_stale() {
  rn_lock_read || return 0
  [ -n "$RN_LOCK_TEXT" ] && [ "$RN_LOCK_TEXT" = "$RN_LOCK_SEEN" ] || return 0
  if aws_run ssm delete-parameter --name "$(rn_lock_name)" || aws_missing; then
    log_line "replaced the stale deploy lock: ${RN_LOCK_STALE:-stale}"
  fi
  return 0
}

# rn_lease_take: takes the deploy lock, right after the confirmation and before the runner is installed or its bucket policy rewritten. The create succeeds only when no lock
# exists; when another setup was faster the lock is read again: a build of the same commit and inputs is attached to (RN_MODE attach or finish), a stale lock is replaced once
# more, and anything else is exit 7 naming the holder. A lock without a build is stale only by the rules of rn_lease_check, so two setups that start at the same moment never
# both deploy. From here until the build is recorded, Ctrl-C ends the run through rn_lock_interrupted. [route]: runner (the default) or local; the local route
# (--build-on local) takes the lock the same way, gives deploy.sh HAMH_DEPLOY_LOCK_TOKEN=$RN_LOCK_TOKEN, renews the lock while the deploy prints and releases it when the
# deploy has ended (full_local_lock_check, full_local_flow, full_local_lock_refresh and full_local_interrupted of full.sh).
rn_lease_take() {
  local tries=0
  [ "$RN_LOCK_HELD" != 1 ] || return 0
  case $RN_MODE in attach|finish) return 0 ;; esac
  RN_LOCK_TOKEN=$(rn_lock_token_new)
  RN_LOCK_STARTED=$(setup_now)
  RN_LOCK_ROUTE=${1:-runner}
  RN_LOCK_PARTIES=""
  RN_LOCK_AT=$SECONDS
  # An update of the commit the stage already runs builds only the client and changes nothing on the stage, so its lock does not close the stage to new runs
  RN_LOCK_DEPLOYS=""
  [ "${SETUP_CLIENT_ONLY:-0}" != 1 ] || RN_LOCK_DEPLOYS=no
  while :; do
    tries=$((tries + 1))
    [ "$RN_MODE" != stale ] || rn_lock_replace_stale
    if aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "")" \
        --tags "Key=project,Value=$SETUP_PROJECT_TAG" "Key=stage,Value=$STAGE" "Key=managed-by,Value=setup.sh"; then
      RN_LOCK_HELD=1
      RN_LOCK_OPEN=0
      [ "$RN_LOCK_DEPLOYS" != no ] || RN_LOCK_OPEN=1
      # Whichever way this run ends, a lock without a build goes back (rn_lock_on_exit)
      trap rn_lock_on_exit EXIT
      trap rn_lock_interrupted INT TERM
      log_line "took the deploy lock $(rn_lock_name) for commit $SRC_COMMIT"
      return 0
    fi
    case $AWS_ERR in
      *ParameterAlreadyExists*)
        rn_lease_check
        case $RN_MODE in attach|finish) return 0 ;; esac
        [ "$tries" -lt 2 ] || { rn_lock_read || true; rn_lock_held_exit "another setup took it a moment ago"; } ;;
      *) rn_fail_before_build "could not take the deploy lock $(rn_lock_name) of stage $STAGE" ;;
    esac
  done
}

# rn_lease_take_soft: takes the deploy lock for the removal of a runner that an earlier setup left behind (a run without a build of its own: `./setup.sh --full` on a deployed
# stage, an update that installs the client it downloaded earlier; A4.4). One try and no exit: 0 when this run holds the lock now, 1 with RN_BLOCKED_WHY when another setup
# holds it or it cannot be written. A stale lock that preflight read is replaced first.
rn_lease_take_soft() {
  [ "$RN_LOCK_HELD" != 1 ] || return 0
  RN_LOCK_TOKEN=$(rn_lock_token_new)
  RN_LOCK_STARTED=$(setup_now)
  RN_LOCK_ROUTE=runner
  RN_LOCK_PARTIES=""
  # The removal of a runner changes nothing on the stage
  RN_LOCK_DEPLOYS=no
  [ "$RN_MODE" != stale ] || rn_lock_replace_stale
  if aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "")" \
      --tags "Key=project,Value=$SETUP_PROJECT_TAG" "Key=stage,Value=$STAGE" "Key=managed-by,Value=setup.sh"; then
    RN_LOCK_HELD=1
    RN_LOCK_OPEN=1
    trap rn_lock_on_exit EXIT
    log_line "took the deploy lock $(rn_lock_name) to remove the runner an earlier setup left"
    return 0
  fi
  case $AWS_ERR in
    *ParameterAlreadyExists*)
      rn_lock_read || true
      RN_BLOCKED_FAILED=0
      RN_BLOCKED_WHY="another setup holds the deploy lock $(rn_lock_name) ($(rn_lock_holder_text))" ;;
    *) RN_BLOCKED_FAILED=1; RN_BLOCKED_WHY="the deploy lock $(rn_lock_name) could not be taken (${AWS_ERR:-no answer})" ;;
  esac
  RN_LOCK_TOKEN=""
  return 1
}

# rn_lock_sync: before this run writes its lock again, RN_LOCK_PARTIES becomes the `parties` line of that write: the other setups at work that the lock names now, then this
# run with the time of the write (RN_LOCK_STARTED). It is empty for a setup that is alone under its lock: such a lock has no `parties` line. A read that fails, or a lock that
# is not this run's, keeps what was known.
rn_lock_sync() {
  rn_lock_read || return 0
  [ -n "$RN_LOCK_TEXT" ] && [ "$(rn_lock_get token)" = "$RN_LOCK_TOKEN" ] || return 0
  # Another setup under this lock saw the build succeed first and wrote deploys=no; this write must not take it back
  if [ "$(rn_lock_get deploys)" = no ]; then
    RN_LOCK_DEPLOYS=no
    RN_LOCK_OPEN=1
  fi
  RN_LOCK_PARTIES=""
  if rn_lock_others; then RN_LOCK_PARTIES="$RN_LOCK_OTHERS,$(uname -n):$$@$RN_LOCK_STARTED"; fi
  return 0
}

# rn_lock_renew: this run writes a new start time into its lock (with the build it records and the setups that share it, rn_lock_sync), so that the lock does not count as
# stale while the run still downloads and installs the client and removes the runner (A4.4). A lease of an earlier release has no renewal.
# While other setups share the lock, a write of one of them in the same moment can replace this write and drop this run's entry:
# the lock is read back after a pause, and this run writes itself in again when its entry is gone, three writes at most, so that the other setup never ends believing
# it is alone and removes the runner this run still downloads its client from.
rn_lock_renew() {
  local tries=0
  [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] || return 0
  while :; do
    RN_LOCK_STARTED=$(setup_now)
    RN_LOCK_AT=$SECONDS
    rn_lock_sync
    if ! aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "${RN_LOCK_BUILD:-$RN_BUILD_ID}")" --overwrite; then
      log_line "could not renew the deploy lock: ${AWS_ERR:-no answer}"
      return 0
    fi
    [ "$RN_LOCK_DEPLOYS" != no ] || RN_LOCK_OPEN=1
    [ -n "$RN_LOCK_PARTIES" ] || return 0
    tries=$((tries + 1))
    [ "$tries" -lt 3 ] || return 0
    rn_lock_settle
    rn_lock_read || return 0
    [ -n "$RN_LOCK_TEXT" ] && [ "$(rn_lock_get token)" = "$RN_LOCK_TOKEN" ] || return 0
    rn_lock_names_me && return 0
    log_line "a write of another setup at the same moment dropped this run's entry from the deploy lock; writing it again"
  done
}

# rn_lock_open: CodeBuild says that the build of the lock has succeeded, so nothing changes the stage any more: this run writes its lock with the line deploys=no, and
# the stage starts runs again. A write that failed leaves a lock that still closes the stage: it is said here and made again with the next renewal of this run.
rn_lock_open() {
  RN_LOCK_DEPLOYS=no
  rn_lock_renew
  [ "$RN_LOCK_HELD" != 1 ] || [ "$RN_LOCK_LEGACY" = 1 ] || [ "$RN_LOCK_OPEN" = 1 ] ||
    warn "could not write the deploy lock $(rn_lock_name) to say that the build has succeeded (${AWS_ERR:-no answer}). The stage starts no new runs until this run ends, or until $((RN_LOCK_STALE_SECONDS / 60)) minutes after the last write of the lock."
}

# rn_lock_join: this run follows the build of another setup of the same commit and inputs (RN_MODE attach or finish) and shares its lock. It writes itself into the lock,
# whose `parties` line then names both setups, so that the one that ends first leaves the runner and the lock to the other (rn_remove_blocked, rn_lease_release). When the
# setup that took the lock is no longer at work, this run is alone under it.
rn_lock_join() { rn_lock_renew; }

# rn_lock_keep: while this run follows the build of the lock it writes the lock again every HAMH_SETUP_LOCK_REFRESH_SECONDS seconds (600 by default), so that a setup that
# shares the lock, or joins the build later, finds this one at work also when the build runs for longer than 30 minutes. The build that prints the CDK bootstrap template is
# not the build of the lock.
rn_lock_keep() {
  [ "$RN_LOCK_HELD" = 1 ] && [ "${RN_ACTION:-deploy}" = deploy ] || return 0
  [ $((SECONDS - RN_LOCK_AT)) -ge "$(setup_number "${HAMH_SETUP_LOCK_REFRESH_SECONDS:-600}" 600)" ] || return 0
  rn_lock_renew
}

# rn_lock_verify: just before the build starts, the lock is still this run's. A setup that replaced a stale lock at the same moment as this one would otherwise run beside it.
rn_lock_verify() {
  [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] || return 0
  rn_lock_read || rn_fail_before_build "could not read the deploy lock $(rn_lock_name) before the build"
  [ "$(rn_lock_get token)" != "$RN_LOCK_TOKEN" ] || return 0
  RN_LOCK_HELD=0
  trap - INT TERM
  [ -n "$RN_LOCK_TEXT" ] || setup_exit 7 "the deploy lock $(rn_lock_name) of this run was removed by someone else before its build started" \
    "This run starts no build, so it cannot race another deploy. Run $(rn_retry_command) again. The stage is unchanged."
  rn_lock_held_exit "another setup took it while this run prepared its build, so this run starts no build"
}

# rn_lease_record: the lock now names the build.
rn_lease_record() {
  trap - INT TERM
  [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] || return 0
  rn_lock_sync
  aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "$RN_BUILD_ID")" --overwrite ||
    warn "could not record the build in the deploy lock: ${AWS_ERR:-no answer}. A later run still finds the build: it carries the token of the lock."
}

# rn_lock_mark <mark>: the lock stays, with its build and a mark the next run reads (deploy-interrupted: the build ended while it was changing the stage).
rn_lock_mark() {
  [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] || return 0
  rn_lock_sync
  aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "$RN_BUILD_ID" "$1")" --overwrite ||
    warn "could not mark the deploy lock: ${AWS_ERR:-no answer}"
}

# rn_lock_leave: another setup that follows the same build is still at work under this run's lock (RN_LOCK_OTHERS, of the lock just read): the lock stays for it. This run
# takes itself out: the lock then names the setup that wrote it last as its holder, with that setup's time, and keeps its other lines.
rn_lock_leave() {
  local entry at best=-1 last="" text
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    at=$(rn_epoch_of "${entry##*@}") || at=0
    if [ "$at" -gt "$best" ]; then best=$at; last=$entry; fi
  done <<<"$(printf '%s\n' "$RN_LOCK_OTHERS" | tr ',' '\n')"
  text=$(printf '%s\n' "$RN_LOCK_TEXT" | awk -v holder="${last%@*}" -v started="${last##*@}" '
    index($0, "holder=") == 1 { print "holder=" holder; next }
    index($0, "started=") == 1 { print "started=" started; next }
    index($0, "parties=") == 1 { next }
    { print }')
  [ "$RN_LOCK_OTHERS" = "$last" ] || text="$text
parties=$RN_LOCK_OTHERS"
  if aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$text" --overwrite; then
    RN_LOCK_LEFT=1
    log_line "left the deploy lock $(rn_lock_name) to the setup ${last%@*}, which follows the same build"
    return 0
  fi
  log_line "could not write the deploy lock for the setup ${last%@*}, which follows the same build: ${AWS_ERR:-no answer}"
  return 1
}

# rn_lock_left_back: this run has left the shared lock to another setup (rn_lock_leave), and that setup may have ended at the same moment and left the lock to this
# run. After a pause the lock is read again: 0 when it is still this build's lock and names this run as its holder with no other setup at work, so this run is the last
# one under it and removes the runner.
rn_lock_left_back() {
  rn_lock_settle
  rn_lock_read || return 1
  [ -n "$RN_LOCK_TEXT" ] && [ "$(rn_lock_get token)" = "$RN_LOCK_TOKEN" ] || return 1
  [ "$(rn_lock_get holder)" = "$(uname -n):$$" ] || return 1
  ! rn_lock_others
}

# rn_lock_take_out: a run that stops following a build takes itself out of the lock while another setup that follows the same build is still at work under it, so
# that setup removes the runner when it ends and nobody waits 30 minutes for this run's entry to age. 0 when it did; alone under the lock this run keeps it
# (the next run attaches to the build by its token), and a write that fails (a lapsed sign-in) leaves the entry to age.
rn_lock_take_out() {
  [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] && [ "${RN_ACTION:-deploy}" = deploy ] || return 1
  rn_lock_read || return 1
  [ -n "$RN_LOCK_TEXT" ] && [ "$(rn_lock_get token)" = "$RN_LOCK_TOKEN" ] || return 1
  rn_lock_others || return 1
  rn_lock_leave || return 1
  RN_LOCK_HELD=0
}

# rn_lease_release [quiet]: gives the lock back, only when it is still this run's (the token decides). While another setup that follows the same build is still at work under it, the
# lock is not deleted: this run takes itself out of it (rn_lock_leave). The lease of an earlier release that this run attached through goes the same way as the lock.
# The stage's API service starts no new runs while the lock is there, so a lock that stays is said. RN_RELEASE (above) keeps what happened, and the function returns
# 1 when the lock stays although this run meant to give it back (`kept`: the read that checks the token, or the delete, failed). Nothing says "released" without a delete
# that went through (rn_release_said). A lock that stays is said here as a warning (rn_release_kept); with `quiet` the caller says it, under its own message.
rn_lease_release() {
  RN_RELEASE=none
  RN_RELEASE_ERR=""
  RN_RELEASE_HINT=""
  if [ "$RN_LOCK_LEGACY" = 1 ]; then
    RN_LOCK_LEGACY=0
    [ -z "$RN_BUCKET" ] || aws_run s3api delete-object --bucket "$RN_BUCKET" --key "lease/$STAGE.json" || true
  fi
  [ "$RN_LOCK_HELD" = 1 ] || return 0
  # RN_LOCK_HELD stays 1 until the outcome is known: a run that a second signal ends in the middle of these calls still gives its lock back as it ends (rn_lock_on_exit)
  if ! rn_lock_read; then
    rn_release_failed "${1:-}"
  elif [ -z "$RN_LOCK_TEXT" ]; then
    RN_RELEASE=released
  elif [ "$(rn_lock_get token)" != "$RN_LOCK_TOKEN" ]; then
    RN_RELEASE=other
  elif rn_lock_others; then
    # the lock stays for the other setup, also when this run could not take itself out of it: its entry then ages
    rn_lock_leave || true
    RN_RELEASE=shared
  elif aws_run ssm delete-parameter --name "$(rn_lock_name)" || aws_missing; then
    RN_RELEASE=released
  else
    rn_release_failed "${1:-}"
  fi
  RN_LOCK_HELD=0
  [ "$RN_RELEASE" != kept ]
}

# rn_release_failed [quiet]: the lock could not be given back; the AWS call that just failed says why.
rn_release_failed() {
  RN_RELEASE=kept
  RN_RELEASE_ERR=${AWS_ERR:-no answer}
  RN_RELEASE_HINT=$(rn_aws_hint)
  log_line "could not release the deploy lock $(rn_lock_name): $RN_RELEASE_ERR"
  [ "${1:-}" = quiet ] || rn_release_kept warn
}

# rn_release_kept <warn|detail>: the lines of a lock that stays (RN_RELEASE kept): why, what the stage does with new runs meanwhile, and how the lock goes. The delete
# command is for an account in which no other setup or deploy of the stage is at work: after a read that failed, this run cannot tell whose lock is there.
rn_release_kept() {
  local out=$1 name
  name=$(rn_lock_name)
  "$out" "The deploy lock $name could not be released: $RN_RELEASE_ERR"
  [ -z "$RN_RELEASE_HINT" ] || "$out" "$RN_RELEASE_HINT"
  if [ "$RN_LOCK_OPEN" = 1 ]; then
    "$out" "It stays in your account. It says deploys=no, so the stage keeps starting runs, and the next setup replaces it."
    return 0
  fi
  "$out" "It stays in your account, so the stage starts no new runs until $((RN_LOCK_STALE_SECONDS / 60)) minutes after its last write."
  "$out" "To remove it sooner, run $(rn_retry_command) again on this machine. Or, when no other setup or deploy of the stage is running, delete it:  aws ssm delete-parameter --name $name"
}

# rn_release_said [words]: what became of this run's lock at the last rn_lease_release, as detail lines under the caller's message. "Released" is said only for a lock that
# is gone; [words] go between "is released" and the full stop.
rn_release_said() {
  case $RN_RELEASE in
    released) detail "The deploy lock of the stage is released${1:-}." ;;
    shared) detail "The deploy lock stays with the other setup that follows the same build." ;;
    other) detail "The deploy lock of the stage belongs to another setup by now and is left alone." ;;
    kept) rn_release_kept detail ;;
  esac
}

# rn_lock_on_exit: the EXIT handler of a run that took the deploy lock. The stage's API service starts no new runs
# while the lock is there, so a run that ends while it holds the lock, with no build of the lock recorded or under way, gives the lock back whichever way it ends (a step
# that failed before the build, a signal that nothing handles, a closed terminal) and says what became of it. The lock stays when it records a build (the next run attaches
# to that build or finishes from it), when a start-build call was under way (RN_STARTING: the build may have started, and it carries the token) and while scripts/deploy.sh
# of the local route runs under it (RN_LOCK_BUSY). The handler never calls exit, so the exit code of the run, or the signal that ended it, stays what it was.
rn_lock_on_exit() {
  trap - EXIT
  [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] || return 0
  [ -z "$RN_LOCK_BUILD" ] && [ "$RN_STARTING" != 1 ] && [ "$RN_LOCK_BUSY" != 1 ] || return 0
  rn_lease_release quiet
  rn_release_said
  log_line "the run ended while it held the deploy lock without a build: $RN_RELEASE"
  return 0
}

# rn_lock_interrupted: Ctrl-C between taking the lock and recording the build. A start-build that was under way may have started the build, so the lock stays and the next run
# finds that build by its token; otherwise the lock is given back. Exit 6 either way.
rn_lock_interrupted() {
  trap - INT TERM
  if [ -n "$RN_LOCK_BUILD" ]; then
    rn_lease_record
    rn_stopped_following "you stopped it"
  fi
  err "Stopped before the build was recorded (you stopped it)."
  if [ "$RN_STARTING" = 1 ]; then
    detail "The build may have started. The deploy lock stays, so the next run finds that build and attaches to it instead of starting a second one."
  else
    rn_lease_release quiet
    detail "No build was started. The stage is unchanged."
    rn_release_said
  fi
  detail "$(rn_runner_now_text)"
  detail "Run $(rn_retry_command) again to go on."
  log_line "stopped before the build was recorded"
  exit 6
}

# rn_runner_now_text: one sentence on the runner as it is now, for the messages of a run that ends early.
rn_runner_now_text() {
  if [ "$RUNNER_LEFT" = 1 ]; then
    printf 'The runner could not be removed and is listed above as RETAINED%s.' "${RN_EXPIRES:+; its role can assume the CDK roles until $RN_EXPIRES UTC}"
  elif [ "$RN_EXISTS" = 1 ]; then
    printf 'The runner stack %s stays installed%s.' "$SETUP_RUNNER_STACK" "${RN_EXPIRES:+ (its role can assume the CDK roles until $RN_EXPIRES UTC)}"
  else
    printf 'No runner is installed.'
  fi
}

# rn_aws_hint: one sentence that names the permission or the quota the last failed AWS call ran into (from its error code and text); nothing when it names neither.
rn_aws_hint() {
  local code action
  code=$(aws_error_code)
  case $AWS_ERR in
    *"is not authorized to perform: "*)
      action=${AWS_ERR#*"is not authorized to perform: "}
      action=${action%% *}
      printf 'Your identity (%s) lacks the permission %s.' "${CALLER_ARN:-the caller}" "$action"
      return 0 ;;
  esac
  case $code in
    AccessDenied|AccessDeniedException|UnauthorizedOperation|AuthorizationError|UnauthorizedException)
      printf 'Your identity (%s) was refused (%s): it needs the permission for this call.' "${CALLER_ARN:-the caller}" "$code" ;;
    AccountLimitExceededException)
      printf 'A CodeBuild quota of the account was reached (%s): a new account allows few builds at a time. Wait for other builds to end, or raise the quota in Service Quotas.' "$code" ;;
    *LimitExceeded*|ServiceQuotaExceededException|TooManyRequestsException|ThrottlingException|Throttling)
      printf 'A quota or rate limit of the account was reached (%s). Wait a few minutes, or raise the quota in Service Quotas.' "$code" ;;
    ExpiredToken|ExpiredTokenException|RequestExpired)
      printf 'Your AWS sign-in has run out (%s). Sign in again before you run setup.sh again.' "$code" ;;
  esac
  return 0
}

# rn_fail_before_build <headline>: a step after the confirmation and before the build started failed. Nothing of the stage
# was changed. An update removes the runner this run installed (or lists it RETAINED) and gives the lock back; a first setup keeps its runner for the retry. Exit 5 with the
# cause, the permission or quota it names, and the runner as it is now.
rn_fail_before_build() {
  local why="$1: ${AWS_ERR:-no answer}" hint
  hint=$(rn_aws_hint)
  trap - INT TERM
  log_line "$why"
  err "$why"
  [ -z "$hint" ] || detail "$hint"
  if [ "${FULL_ACTION:-full}" = update ] && [ "$RN_INSTALLED_NOW" = 1 ]; then rn_remove || true; fi
  rn_lease_release
  if [ "${BUILD_ON:-runner}" = local ]; then
    detail "The stage is unchanged: scripts/deploy.sh did not start."
  else
    detail "The stage is unchanged: no build was started."
    detail "$(rn_runner_now_text)"
  fi
  detail "Run $(rn_retry_command) again when the cause is fixed."
  exit 5
}

# rn_read_failed <what>: a read after the build started failed, and the thing is not missing. Exit 6; the lock and the runner stay, so the next run finishes from the
# same build without building again.
rn_read_failed() {
  trap - INT TERM
  log_line "could not read $1: ${AWS_ERR:-no answer}"
  err "could not read $1: ${AWS_ERR:-no answer}"
  detail "Nothing was installed, and the build ${RN_BUILD_ID:-} is not affected."
  case ${AWS_ERR:-} in
    *xpired*|*"Unable to locate credentials"*|*SSO*|*sso*) detail "Your AWS sign-in may have run out. Sign in again before you run setup.sh again." ;;
  esac
  if [ "$RN_LOCK_HELD" = 1 ] && [ -n "$RN_LOCK_BUILD" ]; then
    detail "The deploy lock and the runner stay, so the next run finishes from the same build without building again: run $(rn_retry_command) again."
    rn_lock_stays_said
  else
    # no build of the lock (the build that prints the bootstrap template): a lock this run holds goes back as the run ends (rn_lock_on_exit)
    detail "$(rn_runner_now_text) Run $(rn_retry_command) again."
  fi
  exit 6
}

# rn_lock_stays_said: a run ends and leaves its lock with the build it records. One line says what the stage does with new runs meanwhile. After a build that
# succeeded the lock says deploys=no (full_runner_flow writes it as soon as CodeBuild says so), and the stage starts runs. A lock that could not be written that way, and
# the lock of a build that did not succeed, close the stage until the next run or until 30 minutes after their last write. A build that only makes the client never closed it.
rn_lock_stays_said() {
  if [ "$RN_LOCK_OPEN" = 1 ]; then
    [ "$RN_FINAL" != SUCCEEDED ] || [ "${SETUP_CLIENT_ONLY:-0}" = 1 ] || detail "The build has succeeded, so the stage starts runs again."
    return 0
  fi
  detail "The stage starts no new runs until you run $(rn_retry_command) again, or until $((RN_LOCK_STALE_SECONDS / 60)) minutes after the last write of the deploy lock $(rn_lock_name)."
}

# ---- the stage already runs this commit -------------------------------------------------------------------------
# RN_DEPLOYABLE_PATHS is DEPLOYABLE_PATHS of scripts/lib/digests.py (scripts/tests/test_setup_update_runner.py keeps the two equal).
RN_DEPLOYABLE_PATHS="crates infra images engine statemachines schemas fixtures/policy accounts.json"

# rn_target_tree: the SHA-256 that deploy.sh records as /hamh/<stage>/deployed-source-tree for SRC_COMMIT, made with git from this clone as scripts/lib/digests.py source_tree
# makes it ("<path>:<git tree hash, or none>" and a newline for each deployable path, hashed).
rn_target_tree() {
  local path tree
  for path in $RN_DEPLOYABLE_PATHS; do
    tree=$(git -C "$SELF_DIR" rev-parse "$SRC_COMMIT:$path" 2>/dev/null) || tree=none
    printf '%s:%s\n' "$path" "$tree"
  done | setup_sha256
}

# rn_same_deployment: 0 when the stage already runs the target: its record /hamh/<stage>/settings says status=deployed, names this commit or this source tree, the account file
# this update uploads (ACCT_DIGEST) and every explicit setting with the value it has, and every stack of the stage is CREATE_COMPLETE or UPDATE_COMPLETE (STAGE_STACKS of
# pf_stage). The explicit values are in the form the deploy records (pf_canonical), so another spelling of the recorded value is the recorded value; an explicit value of a
# setting the stage does not keep (KEPT no in the registry) has no line in the record and is applied or removed only by a deploy, so it always counts as a change (A4.8).
# With --build-on local the two parameters in which the stage itself names the deployed source tree and account file must be the record's too (rn_live_matches; A4.9).
# --force-deploy (SETUP_FORCE_DEPLOY=1) always deploys. RN_SAME_WHY says why not, for the log.
rn_same_deployment() {
  local rec line name value recorded stack status
  RN_SAME_WHY=""
  if [ "${SETUP_FORCE_DEPLOY:-0}" = 1 ]; then RN_SAME_WHY="--force-deploy"; return 1; fi
  if ! aws_run ssm get-parameter --name "${GEN_ROOT:-/hamh}/$STAGE/settings" --query Parameter.Value --output text; then
    if aws_missing; then RN_SAME_WHY="the stage has no record of its deploy"; else RN_SAME_WHY="the record of the stage could not be read: ${AWS_ERR:-no answer}"; fi
    return 1
  fi
  rec=$AWS_OUT
  [ "$(rn_kv "$rec" status)" = deployed ] || { RN_SAME_WHY="the record of the stage says status $(rn_kv "$rec" status)"; return 1; }
  if [ "$(rn_kv "$rec" commit)" != "$SRC_COMMIT" ] && [ "$(rn_kv "$rec" tree)" != "$(rn_target_tree)" ]; then
    RN_SAME_WHY="the stage runs commit $(rn_kv "$rec" commit)"
    return 1
  fi
  [ "$(rn_kv "$rec" accounts)" = "$ACCT_DIGEST" ] || { RN_SAME_WHY="the stage was deployed from another account file"; return 1; }
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%=*}
    value=${line#*=}
    if [ "$(pf_reg_get "$name" kept)" != yes ]; then
      RN_SAME_WHY="$name is exported and the stage does not keep it: only a deploy applies it"
      return 1
    fi
    recorded=$(rn_kv "$rec" "set.$name")
    if [ "$value" = default ]; then
      [ -z "$recorded" ] || { RN_SAME_WHY="$name returns from $recorded to its default"; return 1; }
    elif [ "$value" != "$recorded" ]; then
      RN_SAME_WHY="$name changes from ${recorded:-its default} to $value"
      return 1
    fi
  done <<EOF
$SETUP_EXPLICIT_LINES
EOF
  for stack in network security data packages control orchestration runtimes delivery observability; do
    status=$(printf '%s\n' "${STAGE_STACKS:-}" | awk -F '\t' -v n="hamh-$STAGE-$stack" '$1 == n { print $2; exit }')
    case $status in
      CREATE_COMPLETE|UPDATE_COMPLETE) ;;
      *) RN_SAME_WHY="the stack hamh-$STAGE-$stack is ${status:-not there}"; return 1 ;;
    esac
  done
  if [ "${BUILD_ON:-runner}" = local ]; then
    # The build of the runner route looks at the account again before it skips its deploy (phase_client_only of runner-build.sh). The local route has no build, so the two
    # parameters in which the stage itself names what was deployed are read here (A4.9): a deploy that writes no record (an earlier release) changes them and leaves the
    # record as it was.
    rn_live_matches deployed-source-tree "$(rn_kv "$rec" tree)" || return 1
    rn_live_matches deployment-config-digest "$(rn_kv "$rec" accounts)" || return 1
  fi
  return 0
}

# rn_live_matches <parameter> <value of the record>: 0 when the stage's parameter /hamh/<stage>/<parameter> holds that value; else 1 and RN_SAME_WHY.
rn_live_matches() {
  if ! aws_run ssm get-parameter --name "${GEN_ROOT:-/hamh}/$STAGE/$1" --query Parameter.Value --output text; then
    RN_SAME_WHY="the stage's parameter $1 could not be read (${AWS_ERR:-no answer})"
    return 1
  fi
  [ "$AWS_OUT" != "$2" ] || return 0
  RN_SAME_WHY="the stage's $1 is $(rn_first12 "$AWS_OUT"), and its record names $(rn_first12 "$2"): a deploy changed the stage after the record was written"
  return 1
}

# rn_version_pair <text>: 0 when the text is N.NN, with RN_VP set to the two numbers as one integer (the counter has two digits), else 1.
RN_VP=0
rn_version_pair() {
  local text=$1 major minor
  case $text in
    *[!0-9.]*|''|.*|*.|*.*.*) return 1 ;;
    *.*) ;;
    *) return 1 ;;
  esac
  major=${text%.*}
  minor=${text#*.}
  [ "${#major}" -le 6 ] && [ "${#minor}" = 2 ] || return 1
  RN_VP=$((10#$major * 100 + 10#$minor))
}

# rn_label_plan: the release label of an update that deploys nothing: what the backend reports now (RN_LABEL_NOW: the stage's label parameter when it names this source
# tree, else the VERSION of the stage's record, which is what control-api was compiled with) and this clone's VERSION (RN_LABEL_NEXT). Both are empty when the plan has nothing to
# say (equal versions, or a version this function cannot read); RN_LABEL_KEEP is 1 when the label is newer than this clone's VERSION, which an update never lowers.
RN_LABEL_NOW=""
RN_LABEL_NEXT=""
RN_LABEL_KEEP=0
rn_label_plan() {
  local held now="" next=${SRC_VERSION_TEXT:-} at_now at_next
  RN_LABEL_NOW=""
  RN_LABEL_NEXT=""
  RN_LABEL_KEEP=0
  rn_version_pair "$next" || return 0
  at_next=$RN_VP
  if aws_run ssm get-parameter --name "${GEN_ROOT:-/hamh}/$STAGE/release-version" --query Parameter.Value --output text; then
    held=$AWS_OUT
    [ "${held#* }" != "$(rn_target_tree)" ] || now=${held%% *}
  fi
  rn_version_pair "$now" || now=$(pf_rec_get version)
  rn_version_pair "$now" || return 0
  at_now=$RN_VP
  [ "$at_now" != "$at_next" ] || return 0
  RN_LABEL_NOW=$now
  RN_LABEL_NEXT=$next
  [ "$at_now" -gt "$at_next" ] && RN_LABEL_KEEP=1
  return 0
}

# rn_same_deployment_check: for an update (full_run calls it before the active-runs check): when the stage already runs the target, the build only makes the client
# (SETUP_CLIENT_ONLY=1: no stage resource changes, so runs on the stage do not stop the update) and the run says so.
rn_same_deployment_check() {
  SETUP_CLIENT_ONLY=0
  RN_LABEL_NOW=""
  RN_LABEL_NEXT=""
  RN_LABEL_KEEP=0
  if rn_same_deployment; then
    SETUP_CLIENT_ONLY=1
    rn_label_plan
    if [ -n "$RN_LABEL_NEXT" ] && [ "$RN_LABEL_KEEP" != 1 ]; then
      # The one thing the update writes to the stage is the release label (a single parameter), and it says so
      say_log "This update builds only the client: the stage already runs this commit (its record names this commit, the same account file and the same settings), so no stack, image or run changes, only the release label of the backend is written, and active runs do not stop it. --force-deploy deploys anyway."
    else
      say_log "This update builds only the client: the stage already runs this commit (its record names this commit, the same account file and the same settings), so nothing on the stage changes and active runs do not stop it. --force-deploy deploys anyway."
    fi
  else
    log_line "the update deploys: $RN_SAME_WHY"
  fi
}

# ---- the build -----------------------------------------------------------------------------------------------------

# rn_start [nolease]: starts the build of RN_ACTION. The build of the bootstrap template takes no lease: it is repeatable and writes one object named by its hash.
rn_start() {
  local over nolease=${1:-} hint
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
  if [ -z "$nolease" ]; then
    # The settings the user chose, as pf_explicit_settings encoded them (C4), the token of the deploy lock for deploy.py (C6), a build that only makes the
    # client (C7) and --force-deploy, which keeps the build from skipping a deploy that an earlier build of this commit finished
    [ -z "$SETUP_EXPLICIT_SETTINGS" ] || over="$over name=SETUP_EXPLICIT_SETTINGS,value=$SETUP_EXPLICIT_SETTINGS,type=PLAINTEXT"
    [ "$RN_LOCK_HELD" != 1 ] || [ "$RN_LOCK_LEGACY" = 1 ] || over="$over name=SETUP_DEPLOY_LOCK_TOKEN,value=$RN_LOCK_TOKEN,type=PLAINTEXT"
    [ "${SETUP_CLIENT_ONLY:-0}" != 1 ] || over="$over name=SETUP_CLIENT_ONLY,value=1,type=PLAINTEXT"
    [ "${SETUP_FORCE_DEPLOY:-0}" != 1 ] || over="$over name=SETUP_FORCE_DEPLOY,value=1,type=PLAINTEXT"
    rn_lock_verify
    [ "$RN_LOCK_HELD" != 1 ] || trap rn_lock_interrupted INT TERM
  fi
  RN_STARTING=1
  # shellcheck disable=SC2086
  if ! aws_run codebuild start-build --project-name "$RN_PROJECT" "${source_args[@]}" --environment-variables-override $over --query build.id --output text; then
    RN_STARTING=0
    [ -n "$nolease" ] || rn_fail_before_build "could not start the build"
    hint=$(rn_aws_hint)
    setup_exit 5 "could not start the build: ${AWS_ERR:-no answer}" ${hint:+"$hint"}
  fi
  RN_BUILD_ID=$AWS_OUT
  # RN_STARTING stays 1 until the build of the lock is noted (RN_LOCK_BUILD): a run that ends in between leaves the lock to the build that has started (rn_lock_on_exit)
  setup_safe_word "$RN_BUILD_ID" || { RN_STARTING=0; [ -n "$nolease" ] || rn_lease_release; setup_exit 5 "CodeBuild answered with a build id setup does not trust: $RN_BUILD_ID"; }
  [ -n "$nolease" ] || RN_LOCK_BUILD=$RN_BUILD_ID
  RN_STARTING=0
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
  if [ "${RN_ACTION:-deploy}" != bootstrap-template ]; then
    if rn_lock_take_out; then
      detail "Another setup follows the same build: this run took itself out of the deploy lock, so that setup removes the runner when it ends."
    else
      detail "$(rn_runner_now_text) The deploy lock keeps the build for that run."
      # The lock closes the stage to new runs, and the build's deploy keeps it that way (renew_caller_lock of scripts/lib/deploy.py)
      if [ "$RN_LOCK_HELD" = 1 ] && [ "$RN_LOCK_LEGACY" != 1 ] && [ "$RN_LOCK_OPEN" != 1 ]; then
        detail "The stage starts no new runs while this build deploys: the build renews the deploy lock by itself."
        detail "The stage starts runs again when that run sees the build end, or $((RN_LOCK_STALE_SECONDS / 60)) minutes after the build's last write of the lock."
      fi
    fi
  fi
  # the build that prints the bootstrap template is no build of the lock: a lock this run holds goes back as the run ends (rn_lock_on_exit)
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
      rn_lock_keep
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
  elif ! aws_missing; then
    # a read that failed is not a build that wrote no result
    rn_read_failed "the result of build $RN_BUILD_ID (s3://$RN_BUCKET/results/$RN_BUILD_ID/result.json)"
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

# rn_refusal <reason|wait|action>: a part of a refusal before any stack change. Its detail is what deploy.py printed after
# DEPLOY-REFUSED: `reason=<id> wait=<yes|no> stage=unchanged :: <one sentence with the action>`.
rn_refusal() {
  local d
  d=$(json_text "$(rn_res detail)")
  case $1 in
    reason) d=${d#*reason=}; printf '%s' "${d%% *}" ;;
    wait) case " $d " in *" wait=yes "*) printf yes ;; *) printf no ;; esac ;;
    action) case $d in *" :: "*) printf '%s' "${d#*" :: "}" ;; *) printf '%s' "$d" ;; esac ;;
  esac
}

# rn_fail_code: the exit code of a build that ended badly: 7 when the deploy guard refused (runs are active) or a refusal that waiting cures, else 5.
rn_fail_code() {
  case $(rn_res failedPhase) in
    guard)
      echo 7 ;;
    refused) if [ "$(rn_refusal wait)" = yes ]; then echo 7; else echo 5; fi ;;
    *) echo 5 ;;
  esac
}

# rn_ended_mid_deploy: 0 when a build that wrote no result ended in its deploy phase (its phases.jsonl lists account-file and no deploy that finished): CodeBuild killed it (a
# timeout, a stop) while CloudFormation may still be changing a stack.
rn_ended_mid_deploy() {
  local text
  text=$(awsx s3 cp "s3://$RN_BUCKET/results/$RN_BUILD_ID/phases.jsonl" - 2>/dev/null) || return 1
  printf '%s\n' "$text" | grep -q '"name": *"account-file"' || return 1
  ! printf '%s\n' "$text" | grep -E '"name": *"deploy"' | grep -qE '"state": *"(ok|skipped)"'
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
# An awk program: its $ belong to awk.
# shellcheck disable=SC2016
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
    guard|refused|inputs|fetch|toolchain|account-file) return 0 ;;
  esac
  return 1
}

# rn_finish_command: what finishes a run whose deploy finished and whose later step failed: the stage runs the commit, so an update builds only what is missing
# (C7); `./setup.sh --full` on that stage builds nothing (full_existing).
rn_finish_command() {
  printf '%s' "./setup.sh --update"
  [ "${BUILD_ON:-runner}" != local ] || printf ' --build-on local'
}

# rn_fail_result: the build ended badly. Reports it, writes the receipt, releases the lease and exits. An update that failed before it changed the stage removes the runner it
# installed, and says the stage is unchanged; one that may have changed the stage keeps the runner for the retry, and says the stage may hold partial changes. A first setup keeps
# its runner for the retry.
rn_fail_result() {
  local code phase detail_text unchanged=0 until="" cdk_window="" interrupted=0 action="" step stage_same=0 gone="The runner is removed."
  code=$(rn_fail_code)
  phase=$(rn_res failedPhase)
  step=$(rn_res failedStep)
  # the stage is unchanged when the build stopped before the deploy, or the deploy stopped before its first stack change
  if rn_before_deploy "$phase" || [ "$(rn_res stageUnchanged)" = true ]; then stage_same=1; fi
  if [ "${FULL_ACTION:-full}" = update ] && [ "$stage_same" = 1 ]; then unchanged=1; fi
  if [ -z "$RES_FLAT" ] && [ "${RN_ACTION:-deploy}" = deploy ] && rn_ended_mid_deploy; then interrupted=1; fi
  if [ "$phase" = refused ]; then
    action=$(rn_refusal action)
  elif [ "$phase" = deploy ] && [ "$stage_same" = 1 ]; then
    action="The deploy stopped in its step ${step:-unknown} before it changed any stack; the lines above say why."
  fi
  if [ "$unchanged" = 1 ]; then rn_report_failure local; else rn_report_failure; fi
  phase=$RN_FAILED_PHASE
  detail_text=$RN_FAILED_TEXT
  st_set state failed || true
  st_set failedPhase "${phase:-unknown}" || true
  st_set detail "$(printf '%s' "$detail_text" | tr -c '[:print:]' ' ' | tr -d '\042\134' | cut -c1-200)" || true
  st_save || true
  [ "$unchanged" != 1 ] || rn_remove || true
  # a runner that another setup uses stays (rn_remove_blocked); that is no RETAINED
  if [ "$unchanged" = 1 ] && [ "$RUNNER_LEFT" != 1 ] && [ "$RN_EXISTS" = 1 ]; then gone="The runner stays installed: $RN_BLOCKED_WHY."; fi
  # a build that ended while it was changing the stage keeps the lock with a mark, so the next run names it and waits for a stack that is still changing (U33)
  if [ "$interrupted" = 1 ]; then rn_lock_mark deploy-interrupted; else rn_lease_release; fi
  [ -z "$RN_EXPIRES" ] || until=" Its role can assume the CDK roles until $RN_EXPIRES UTC."
  [ -z "$RN_EXPIRES" ] || cdk_window="; its role can assume the CDK roles until $RN_EXPIRES UTC and not after"
  if [ "$phase" = refused ] || { [ "$phase" = deploy ] && [ "$stage_same" = 1 ]; }; then
    # refused, or a failed step, before any stack change (C5, A2.3): the stage is unchanged on every route, and the sentence of the refusal or the step is the way on
    if [ "$unchanged" = 1 ] && [ "$RUNNER_LEFT" = 1 ]; then
      detail "The stage is unchanged. $action The runner could not be removed and stays installed; it is listed above as RETAINED.$until"
    elif [ "$unchanged" = 1 ]; then
      detail "The stage is unchanged. $action $gone"
    else
      detail "The stage is unchanged. $action The runner stays installed$cdk_window."
    fi
  elif [ "$phase" = deploy ] && [ "$step" = release-label ]; then
    # The build deployed nothing and was writing the release label alone (deploy.sh --relabel) when it failed; the label may or may not have been written, the stage runs what it ran
    detail "The stage runs the source tree it ran before: nothing was deployed, and only its version label is in question. The runner stays installed$cdk_window. You run $(rn_retry_command) to try again; it writes the label and builds only the client."
  elif [ "$phase" = gomod ] || [ "$phase" = scanner-rules ] || [ "$phase" = client ] || [ "$phase" = outputs ]; then
    # the deploy finished before this phase (A2.3): the stage runs the new commit, and only this step is to do again
    detail "The stage runs commit $SRC_COMMIT: its deploy finished, and only the step $phase failed. The runner stays installed$cdk_window. You run $(rn_finish_command) to finish; the stage already runs this commit, so the retry does not deploy it again. You run ./setup.sh --teardown to delete the stage and the runner."
  elif [ "$interrupted" = 1 ]; then
    detail "The build ended while it was changing the stage, and wrote no result. The stage may contain partial deployment changes. CloudFormation may still be finishing or rolling back that change. The runner stays installed$cdk_window, and the deploy lock keeps a mark of this build. Run ./setup.sh --status to see the stacks. $(rn_retry_command) names this build as yours, waits for a stack that is still changing and then deploys again."
  elif [ "$unchanged" = 1 ] && [ "$RUNNER_LEFT" = 1 ]; then
    detail "The stage is unchanged. The runner could not be removed and stays installed. The runner is listed above as RETAINED.$until You run $(rn_retry_command) to retry. You run ./setup.sh --teardown to delete the stage and the runner."
  elif [ "$unchanged" = 1 ] && [ "$code" = 7 ]; then
    detail "The stage is unchanged. $gone You wait until the active runs end. You run $(rn_retry_command) to retry the update."
  elif [ "$unchanged" = 1 ]; then
    detail "The stage is unchanged. $gone You run $(rn_retry_command) to retry."
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

# rn_remove_blocked: 0, with RN_BLOCKED_WHY, when the runner must stay for now (A4.4): a deploy lock exists that is not this run's (another setup installs, builds or finishes
# with this runner), or this run's lock names another setup that follows the same build and is still at work (rn_lock_others: it downloads its client from the runner's
# bucket), or a build of the runner's project is running (the ten newest builds are looked at; a build that prints the CDK bootstrap template takes no lock). A look
# that fails counts as blocked: the runner is never removed on a guess. 1 when nothing uses the runner.
rn_remove_blocked() {
  local ids id n=0 some=""
  RN_BLOCKED_WHY=""
  RN_BLOCKED_FAILED=0
  if ! rn_lock_read; then
    RN_BLOCKED_FAILED=1
    RN_BLOCKED_WHY="the deploy lock $(rn_lock_name) could not be read (${AWS_ERR:-no answer})"
    return 0
  fi
  if [ -n "$RN_LOCK_TEXT" ] && { [ "$RN_LOCK_HELD" != 1 ] || [ "$(rn_lock_get token)" != "$RN_LOCK_TOKEN" ]; }; then
    RN_BLOCKED_WHY="another setup holds the deploy lock $(rn_lock_name) ($(rn_lock_holder_text))"
    return 0
  fi
  if [ -n "$RN_LOCK_TEXT" ] && rn_lock_others; then
    RN_BLOCKED_WHY="another setup follows the same build and still uses the runner ($(printf '%s' "$RN_LOCK_OTHERS" | sed 's/@[^,]*//g; s/,/, /g'))"
    return 0
  fi
  [ -n "$RN_PROJECT" ] || return 1
  if ! aws_run codebuild list-builds-for-project --project-name "$RN_PROJECT" --sort-order DESCENDING --query ids --output text; then
    aws_missing && return 1
    RN_BLOCKED_FAILED=1
    RN_BLOCKED_WHY="the builds of the runner project $RN_PROJECT could not be listed (${AWS_ERR:-no answer})"
    return 0
  fi
  ids=$AWS_OUT
  [ -n "$ids" ] && [ "$ids" != None ] || return 1
  for id in $ids; do
    n=$((n + 1))
    [ "$n" -le 10 ] || break
    setup_safe_word "$id" || continue
    some="$some $id"
  done
  [ -n "$some" ] || return 1
  # shellcheck disable=SC2086
  if ! aws_run codebuild batch-get-builds --ids $some --query "builds[?buildStatus=='IN_PROGRESS'].id" --output text; then
    RN_BLOCKED_FAILED=1
    RN_BLOCKED_WHY="the builds of the runner project $RN_PROJECT could not be looked at (${AWS_ERR:-no answer})"
    return 0
  fi
  case $AWS_OUT in
    ''|None) return 1 ;;
  esac
  for id in $AWS_OUT; do
    RN_BLOCKED_WHY="the build $id of the runner is running"
    return 0
  done
  return 1
}

# rn_retain_unchecked: the runner stays because a look or a write of the removal guard failed (RN_BLOCKED_WHY), not because another setup uses it. A runner is
# never removed on a guess, and one that stays for that reason is a runner left behind: RETAINED, RUNNER_LEFT=1 (setup.sh ends with exit 8), and the way to remove it is
# `./setup.sh --full`, which on a deployed stage builds nothing and runs the guard again.
rn_retain_unchecked() {
  RUNNER_LEFT=1
  RN_LEFT_WHY="it was not removed because $RN_BLOCKED_WHY, and a runner is never removed on a guess"
  RN_LEFT_STATUS=""
  RN_LEFT_COMMAND="./setup.sh --full"
  say "RETAINED stack $SETUP_RUNNER_STACK: $RN_LEFT_WHY"
  say "      Run $RN_LEFT_COMMAND once the look works: on a deployed stage it builds nothing and removes the runner."
  log_line "RETAINED $SETUP_RUNNER_STACK ${RN_BUCKET:-none}: $RN_LEFT_WHY"
  st_set retained "stack $SETUP_RUNNER_STACK" || true
  st_save || true
}

# rn_remove [unguarded]: the bucket is emptied and the runner stack deleted. When CloudFormation fails on a refusal of the caller's own session (an IAM or session policy that denies deleting
# a role that has been used), the delete is made again with the CDK CloudFormation execution role, the role that deletes the stage's stacks. Returns 1, with RUNNER_LEFT set and the
# reason printed as RETAINED, when it cannot; RN_LEFT_WHY and RN_LEFT_COMMAND hold the reason and the command that removes the stack.
# A runner that another setup uses is not removed (rn_remove_blocked; A4.4): the run says so in one warning, RN_EXISTS stays 1 and the return value is 2; that is no RETAINED,
# because the setup that uses the runner removes it. `unguarded` is the teardown's word: it has looked at the lock and its build before its plan (td_busy), and the
# stage it removes is the one the runner serves.
rn_remove() {
  local why="" role first second="" bucket_part="" status="" status_part="" retried=0
  RUNNER_LEFT=0
  RN_LEFT_WHY=""
  RN_LEFT_STATUS=""
  RN_LEFT_COMMAND=""
  if [ "$RN_EXISTS" != 1 ]; then return 0; fi
  if [ "${1:-}" != unguarded ] && rn_remove_blocked; then
    if [ "$RN_BLOCKED_FAILED" = 1 ]; then
      rn_retain_unchecked
      return 1
    fi
    warn "the runner stack $SETUP_RUNNER_STACK is not removed: $RN_BLOCKED_WHY. The setup that uses the runner removes it when it ends; ./setup.sh --full removes a runner that stays on a deployed stage."
    log_line "runner not removed: $RN_BLOCKED_WHY"
    return 2
  fi
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
