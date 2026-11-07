# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): everything the full setup reads before it creates anything. Each function
# makes read-only AWS calls or reads this checkout, and ends the run with an exit code and a reason when it finds a problem.
#
#   pf_region       the Region (the option, the answer, the ham config, AWS_REGION, AWS_DEFAULT_REGION or the profile's region); exit 2 when it is not one
#                   the stage supports. No AWS call is made before this one passes.
#   pf_aws_cli      the AWS CLI is there and is version 2; exit 1 when not
#   pf_identity     sts get-caller-identity: ACCOUNT and CALLER_ARN; exit 1 when the credentials do not work, exit 7 when they are for another account
#                   than --account-id names or for a placeholder account
#   pf_source       this folder is a clean git clone: SRC_COMMIT, SRC_KEY (source/<commit>.zip); exit 1 when it is not a clone, exit 7 when it has
#                   uncommitted changes, exit 2 when --revision names no commit of the clone
#   pf_zones        the zones of the Region and the generated account file: ACCT_JSON, ACCT_DIGEST, ACCT_KEY, ACCT_DIRECT_AZ; exit 1 when the account
#                   has no zone the stage can use
#   pf_installer    the identity behind the credentials as the two principal patterns of the runner bucket's policy (INSTALLER_P1, INSTALLER_P2), and how long the
#                   runner's authority lasts (AUTH_HOURS, HAMH_SETUP_AUTHORITY_HOURS, 12 by default, 5 to 72); exit 2 for a length that is none of those, exit 1 for an
#                   identity it cannot name
#   pf_stage        the stage in the account: STAGE_STATE (none, partial, complete, deploying, broken), STAGE_UNFINISHED, STAGE_ENDPOINT, STAGE_BAD, STAGE_FAILED, the CDK
#                   bootstrap (BOOT_VERSION) and the runner stack (RN_EXISTS and its outputs)
#   pf_legacy       the stage of the legacy generation, the one deployed under the names of the releases before the rename: LEGACY_STATE (none, legacy, legacy-retained), LEGACY_STACKS,
#                   LEGACY_ENDPOINT and LEGACY_LEFT. pf_stage calls it. (LEGACY-TOMBSTONE: this detector never ends, so a stage of the previous names is never reported as none)
#   pf_legacy_refuse  the end of --full and --update on an account that holds such a stage: exit 7 with the message and the four steps, before anything is created
#   pf_control_endpoint  the V1ApiUrl output of the control stack of the stage (CTRL_ENDPOINT), empty when the stack or the output is not there
#   pf_stage_repo_names  the output RepoNames of the control stack (STAGE_REPO_NAMES): the repository-name rule of the stage that exists; pf_zones reads it on an update that gives no --repo-name
#   pf_stage_owner  the `owner` tag of the stage that exists (STAGE_OWNER): the tag of its network stack, else of its control stack; empty when there is none
#   pf_owner_check  a nonempty HAMH_OWNER that differs from STAGE_OWNER: exit 7 before anything is created when the build runs on this machine (--build-on local, which hands
#                   HAMH_OWNER to scripts/deploy.sh), a warning on the runner route (the runner in the user's account does not receive HAMH_OWNER)
#   pf_active_runs  the RUNNING executions of the stage's run state machine (ACTIVE_RUNS, ACTIVE_RUN_COUNT); a state machine that is not there has none, and a look that
#                   fails is a warning
#   pf_active_runs_check  an update with a running run: exit 7 before the runner is installed or anything is built

SRC_COMMIT=""
SRC_KEY=""
ACCOUNT=""
CALLER_ARN=""
STAGE_STACKS=""
STAGE_STATE="none"
STAGE_ENDPOINT=""
STAGE_BAD=""
STAGE_FAILED=""
STAGE_UNFINISHED=0
STAGE_OWNER=""
ACTIVE_RUNS=""
ACTIVE_RUN_COUNT=0
CTRL_ENDPOINT=""
BOOT_VERSION=""
BOOT_PIN_SHA=""
CLIENT_PLATFORM=""
LEGACY_STATE="none"
LEGACY_STACKS=""
LEGACY_ENDPOINT=""
LEGACY_LEFT=""
LEGACY_PREFIX=""
LEGACY_PROJECT=""
LEGACY_E2E=""

pf_region() {
  local p
  if [ -z "${REGION:-}" ]; then
    if [ -n "${CFG_REGION:-}" ]; then
      REGION=$CFG_REGION
    elif [ -n "${AWS_REGION:-}" ]; then
      REGION=$AWS_REGION
    elif [ -n "${AWS_DEFAULT_REGION:-}" ]; then
      REGION=$AWS_DEFAULT_REGION
    else
      p=${PROFILE:-${CFG_PROFILE:-${AWS_PROFILE:-default}}}
      REGION=$(profile_region "$p")
    fi
  fi
  [ -n "$REGION" ] || setup_exit 2 "setup needs an AWS Region and none was found" \
    "Give one with  --region NAME  (us-east-1 or us-west-2), or set AWS_REGION, or add a region line to your AWS profile."
  case " $SETUP_REGIONS " in
    *" $REGION "*) ;;
    *) setup_exit 2 "the stage deploys only in us-east-1 or us-west-2 (got Region '$REGION')" "Give  --region us-east-1  or  --region us-west-2." ;;
  esac
}

pf_profile() {
  PROFILE_EFF=${PROFILE:-}
  [ -n "$PROFILE_EFF" ] || PROFILE_EFF=${CFG_PROFILE:-}
}

pf_aws_cli() {
  aws_cli_check
  case $AWS_CLI_STATE in
    ok) ok "AWS CLI $AWS_CLI_VER" ;;
    unreadable) warn "could not read the AWS CLI version from '$AWS_CLI_PROG --version'. Setup needs the AWS CLI, version 2 (2.9.0 or later)." ;;
    missing) setup_exit 1 "the AWS CLI (aws) was not found on your PATH" \
      "Setup needs the AWS CLI, version 2. Install it: $AWS_CLI_INSTALL_URL" "If it is installed elsewhere, set HAMH_AWS_CLI to its path." ;;
    *) setup_exit 1 "the AWS CLI at $AWS_CLI_PROG is aws-cli/$AWS_CLI_VER, which is too old" \
      "Setup needs the AWS CLI, version 2 (2.9.0 or later). Upgrade it: $AWS_CLI_INSTALL_URL" ;;
  esac
}

pf_identity() {
  local hint="aws sts get-caller-identity"
  [ -z "$PROFILE_EFF" ] || hint="$hint --profile $PROFILE_EFF"
  if ! aws_run sts get-caller-identity --query '[Account,Arn]' --output text; then
    setup_exit 1 "could not read your AWS identity: ${AWS_ERR:-no answer}" \
      "Check that your credentials work:  $hint" \
      "For an IAM Identity Center (SSO) profile, sign in first:  aws sso login --profile NAME"
  fi
  ACCOUNT=${AWS_OUT%%"$TAB"*}
  CALLER_ARN=${AWS_OUT#*"$TAB"}
  if acct_placeholder "$ACCOUNT"; then
    setup_exit 7 "the account id '$ACCOUNT' that your credentials answer with is a placeholder, not an account" \
      "Setup refuses to build a stage for a placeholder. Use credentials of a real AWS account."
  fi
  if [ -n "${ACCOUNT_ID_OPT:-}" ] && [ "$ACCOUNT" != "$ACCOUNT_ID_OPT" ]; then
    setup_exit 7 "account mismatch: --account-id names $ACCOUNT_ID_OPT, but your credentials are for account $ACCOUNT ($CALLER_ARN)" \
      "Nothing was changed. Use the profile of the account you meant, or correct --account-id."
  fi
  ok "account $ACCOUNT, Region $REGION ($CALLER_ARN)"
  BUCKET=hamh-setup-$ACCOUNT-$REGION
}

# pf_source: this folder is a clean git clone; the commit that is built is HEAD, or the full commit id --revision names.
pf_source() {
  local top here lines n=0 line
  if ! command -v git >/dev/null 2>&1; then
    setup_exit 1 "git was not found" "The full setup uploads the git archive of your clone, so it needs git. macOS: xcode-select --install    Linux: install git with your package manager."
  fi
  here=$(cd "$SELF_DIR" && pwd -P)
  top=$(git -C "$SELF_DIR" rev-parse --show-toplevel 2>/dev/null || true)
  if [ -n "$top" ]; then top=$(cd "$top" && pwd -P); fi
  if [ "$top" != "$here" ]; then
    setup_exit 1 "this folder is not a git clone of the repository, so there is no commit to pin and build" \
      "Run the full setup from a clone:  git clone <the repository URL>  and run its setup.sh." \
      "A zip download is enough for  setup.sh --client-only  with a client you already have (--from)."
  fi
  lines=$(git -C "$SELF_DIR" status --porcelain --untracked-files=no 2>/dev/null || true)
  if [ -n "$lines" ]; then
    err "your clone has changes that are not committed, so what is built would differ from what you see:"
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      n=$((n + 1))
      [ "$n" -le 10 ] && detail "$line"
    done <<EOF
$lines
EOF
    [ "$n" -le 10 ] || detail "... and $((n - 10)) more"
    detail "Commit or stash them, then run setup.sh again. Nothing was changed."
    exit 7
  fi
  if [ -n "${REVISION:-}" ]; then
    if ! git -C "$SELF_DIR" cat-file -e "$REVISION^{commit}" 2>/dev/null; then
      setup_exit 2 "--revision $REVISION is not a commit of this clone" "Give the full 40-digit commit id of a commit you have fetched."
    fi
    # A commit from before the rename to the current names has a setup that looks for the stage under the previous names: building it would find none (or the wrong one).
    # The names.sh of a commit of this line defines HAMH_CLIENT_BIN. (COMPAT-CAH-STAGE)
    if ! git -C "$SELF_DIR" show "$REVISION:scripts/setup/names.sh" 2>/dev/null | grep -q 'HAMH_CLIENT_BIN'; then
      setup_exit 2 "commit $REVISION predates the rename to the current names and does not contain this version of setup" \
        "Building it looks for the stage under names this version no longer uses." \
        "Use a commit that contains the rename. Nothing was changed."
    fi
    SRC_COMMIT=$REVISION
  else
    SRC_COMMIT=$(git -C "$SELF_DIR" rev-parse HEAD)
  fi
  SRC_KEY=source/$SRC_COMMIT.zip
  ok "source commit $SRC_COMMIT (clean clone)"
}

# pf_templates: the CloudFormation templates the runner needs are in the clone.
pf_templates() {
  [ -f "$SELF_DIR/$SETUP_RUNNER_TEMPLATE" ] || setup_exit 5 "the runner template $SETUP_RUNNER_TEMPLATE is missing from this clone" \
    "It is part of the repository; fetch the whole repository (git pull) and run setup.sh again."
  pf_template_size "$SETUP_RUNNER_TEMPLATE"
}

# pf_template_size <path in the clone>: the AWS CLI deploys a template without a staging bucket only up to 51,200 bytes.
pf_template_size() {
  local bytes
  bytes=$(wc -c <"$SELF_DIR/$1" | tr -d ' ')
  if [ "$bytes" -gt 51200 ]; then
    setup_exit 5 "the template $1 is $bytes bytes, more than the 51,200 bytes that aws cloudformation deploy accepts without a staging bucket" \
      "This is a defect of the repository, not of your account. Report it."
  fi
}

# pf_zones <profile name for the account file> : read the zones and make the account file.
pf_zones() {
  local file_profile=$1 q names zones
  if [ "${BUILD_ON:-runner}" != local ] && [ "$INV_LOGGING" = enable-and-restore ]; then
    setup_exit 2 "--invocation-logging enable-and-restore needs --build-on local" \
      "The runner's role cannot change the Bedrock invocation-logging setting of your account, so a deploy that may change it runs on this machine with your own credentials." \
      "Nothing was changed."
  fi
  q='AvailabilityZones[].[ZoneName,ZoneId]'
  if ! aws_run ec2 describe-availability-zones --filters Name=zone-type,Values=availability-zone Name=state,Values=available --query "$q" --output text; then
    setup_exit 1 "could not list the Availability Zones of $REGION: ${AWS_ERR:-no answer}" "Check that your credentials may call ec2:DescribeAvailabilityZones."
  fi
  zones=$AWS_OUT
  names=$REPO_NAMES
  if [ "$REPO_NAMES_SET" != 1 ] && [ "${FULL_ACTION:-}" = update ]; then
    # an update keeps the repository names the stage has unless --repo-name names others
    pf_stage_repo_names
    names=$STAGE_REPO_NAMES
  fi
  [ "$names" != "hamh-e2e-*" ] || names=""
  acct_generate "$ACCOUNT" "$REGION" "$file_profile" "$zones" "$AZS" "$INV_LOGGING" "$CALLER_ARN" "$names" || exit 1
  ok "zone $(printf '%s' "$ACCT_DIRECT_AZ" | cut -d, -f1) (the AgentCore zone id decides; the zone name differs between accounts)"
}

# pf_installer: who may write the runner bucket besides the runner role (the identity behind these credentials, as the two patterns of aws:PrincipalArn that
# match it), and how long the runner role may assume the CDK roles after an install. A build takes up to 4 hours, so a length under 5 hours would end one in the middle.
pf_installer() {
  local hours=${HAMH_SETUP_AUTHORITY_HOURS:-12} pats
  case $hours in
    *[!0-9]*) hours=bad ;;
  esac
  if [ "$hours" = bad ] || [ "$hours" -lt 5 ] || [ "$hours" -gt 72 ]; then
    setup_exit 2 "HAMH_SETUP_AUTHORITY_HOURS must be a whole number of hours from 5 to 72 (got '${HAMH_SETUP_AUTHORITY_HOURS:-}')" \
      "It is how long the runner role can assume the CDK roles after each install. A build takes up to 4 hours." "Nothing was created."
  fi
  AUTH_HOURS=$hours
  pats=$(setup_installer_patterns "$CALLER_ARN") || setup_exit 1 "setup cannot name the identity behind your credentials in the policy of the runner bucket: $CALLER_ARN" \
    "Setup knows IAM users, the root user, federated users and role sessions (arn:aws:sts::<account>:assumed-role/<role>/<session>)." "Nothing was created."
  INSTALLER_P1=${pats%% *}
  INSTALLER_P2=${pats#* }
}

# pf_stage_repo_names: STAGE_REPO_NAMES, the repository-name rule of the stage that exists (the output RepoNames of the control stack, comma separated), empty when the stage
# or the output is not there (a stage deployed before the rule was a setting has the default rule, hamh-e2e-*).
STAGE_REPO_NAMES=""
pf_stage_repo_names() {
  STAGE_REPO_NAMES=""
  if aws_run cloudformation describe-stacks --stack-name "hamh-$STAGE-control" --query "Stacks[0].Outputs[?OutputKey=='RepoNames'].OutputValue" --output text; then
    case $AWS_OUT in
      ''|None) ;;
      *) STAGE_REPO_NAMES=$AWS_OUT ;;
    esac
  elif ! aws_missing; then
    setup_exit 1 "could not read the outputs of the stack hamh-$STAGE-control: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:DescribeStacks. Nothing was changed."
  fi
}

# pf_control_endpoint: the V1ApiUrl output of the control stack of the stage in this account (CTRL_ENDPOINT), empty when the stack or the output is not there.
pf_control_endpoint() {
  CTRL_ENDPOINT=""
  if aws_run cloudformation describe-stacks --stack-name "hamh-$STAGE-control" --query "Stacks[0].Outputs[?OutputKey=='V1ApiUrl'].OutputValue" --output text; then
    [ "$AWS_OUT" = None ] || CTRL_ENDPOINT=$AWS_OUT
  fi
}

# pf_stage: the stage and the bootstrap and runner stacks that exist now.
#   STAGE_STATE  none      no stack of the stage
#                partial   some stacks but not the control stack, or a stack whose first create rolled back (ROLLBACK_COMPLETE; STAGE_FAILED names it;
#                          it holds nothing, and the CDK deploy deletes it and makes it again), or every stack complete but no deploy record
#                          (STAGE_UNFINISHED is 1): running the deploy again finishes the stage
#                complete  the control stack and every other stack of the stage are complete and the deploy wrote its record; a stack whose update rolled back
#                          (UPDATE_ROLLBACK_COMPLETE) is complete: it holds the state before that update, and a deploy builds on it (stacks_complete in runner-build.sh counts it
#                          after a deploy; scripts/tests/test_setup_contract.py keeps the two lists equal)
#                deploying a stack of the stage is changing (CREATE_IN_PROGRESS, UPDATE_IN_PROGRESS, ... STAGE_BAD names it); a build of this checkout may explain it
#                broken    a stack of the stage is in a state nobody can build on (DELETE_FAILED, a failed rollback, ...; STAGE_BAD names it)
pf_stage() {
  local line name status n=0 has_control=0 bad="" working="" failed=""
  local control=hamh-$STAGE-control
  STAGE_STATE=none
  STAGE_UNFINISHED=0
  STAGE_BAD=""
  STAGE_FAILED=""
  STAGE_ENDPOINT=""
  # shellcheck disable=SC2086
  if ! aws_run cloudformation list-stacks --stack-status-filter $SETUP_LIVE_STATUSES \
      --query "StackSummaries[?starts_with(StackName,'hamh-$STAGE-')].[StackName,StackStatus]" --output text; then
    setup_exit 1 "could not list the CloudFormation stacks: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:ListStacks."
  fi
  STAGE_STACKS=$AWS_OUT
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    status=${line#*"$TAB"}
    n=$((n + 1))
    case $status in
      CREATE_COMPLETE|UPDATE_COMPLETE|IMPORT_COMPLETE|UPDATE_ROLLBACK_COMPLETE) [ "$name" != "$control" ] || has_control=1 ;;
      ROLLBACK_COMPLETE) [ -n "$failed" ] || failed="$name $status" ;;
      *_IN_PROGRESS) [ -n "$working" ] || working="$name $status" ;;
      *) [ -n "$bad" ] || bad="$name $status" ;;
    esac
  done <<EOF
$STAGE_STACKS
EOF
  STAGE_FAILED=$failed
  if [ "$n" -eq 0 ]; then
    STAGE_STATE=none
  elif [ -n "$bad" ]; then
    STAGE_STATE=broken
    STAGE_BAD=$bad
  elif [ -n "$working" ]; then
    STAGE_STATE=deploying
    STAGE_BAD=$working
  elif [ "$has_control" = 1 ] && [ -z "$failed" ]; then
    # The first pass of the two-pass deploy leaves every stack complete (no worker image yet, a placeholder runtime), so the stacks alone do not say the deploy finished.
    if pf_deploy_record; then
      STAGE_STATE=complete
    else
      STAGE_STATE=partial
      STAGE_UNFINISHED=1
    fi
  else
    STAGE_STATE=partial
  fi
  if [ "$has_control" = 1 ]; then
    pf_control_endpoint
    STAGE_ENDPOINT=$CTRL_ENDPOINT
  fi
  pf_bootstrap_version
  rn_describe
  pf_legacy
}

# pf_legacy: LEGACY_STATE, the stage of the legacy generation in this account and Region (LEGACY-TOMBSTONE: the detector for the stages that releases before the rename deployed;
# it never ends, so that such a stage is never reported as none, and the shims around it are the ones that end, COMPAT-CAH-STAGE).
#   none             no stack of the stage under the previous names, and nothing of that generation left
#   legacy           stacks named <previous prefix>-<stage>-* that carry the previous project tag and the stage tag; LEGACY_STACKS holds them (name, status), LEGACY_ENDPOINT the
#                    V1ApiUrl of the control stack when it can be read
#   legacy-retained  no such stack, but a VPC, a stage bucket or a runner stack of that generation is left (an earlier teardown retained it); LEGACY_LEFT names what
# A stage belongs to the generation only when the name prefix and the project tag are both from it: a stack with the name and another project tag is foreign and is not counted.
# A read that fails is a warning, never the answer "none": the other reads still run, and the state is the strongest the successful ones show.
pf_legacy() {
  local saved=$GEN line name status tags n=0 b v
  LEGACY_STATE=none
  LEGACY_STACKS=""
  LEGACY_ENDPOINT=""
  LEGACY_LEFT=""
  setup_gen_use legacy || return 0
  LEGACY_PREFIX=$GEN_PREFIX
  LEGACY_PROJECT=$GEN_PROJECT
  LEGACY_E2E=$GEN_E2E
  # shellcheck disable=SC2086
  if ! aws_run cloudformation list-stacks --stack-status-filter $SETUP_LIVE_STATUSES \
      --query "StackSummaries[?starts_with(StackName,'$GEN_PREFIX-$STAGE-')].[StackName,StackStatus]" --output text; then
    warn "could not look for a stage under the previous names ($GEN_PREFIX-$STAGE-*): ${AWS_ERR:-no answer}"
  else
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      name=${line%%"$TAB"*}
      status=${line#*"$TAB"}
      case $name in "$GEN_PREFIX-$STAGE-"*) ;; *) continue ;; esac
      if aws_run cloudformation describe-stacks --stack-name "$name" --query 'Stacks[0].Tags[].[Key,Value]' --output text; then
        tags=$AWS_OUT
        if [ "$(printf '%s\n' "$tags" | awk -F'\t' '$1 == "project" { print $2; exit }')" = "$GEN_PROJECT" ] &&
            [ "$(printf '%s\n' "$tags" | awk -F'\t' '$1 == "stage" { print $2; exit }')" = "$STAGE" ]; then
          LEGACY_STACKS="$LEGACY_STACKS$name$TAB$status$NL"
          n=$((n + 1))
        fi
      elif ! aws_missing; then
        warn "could not read the tags of the stack $name: ${AWS_ERR:-no answer}"
      fi
    done <<EOF
$AWS_OUT
EOF
  fi
  if [ "$n" -gt 0 ]; then
    LEGACY_STATE=legacy
    if aws_run cloudformation describe-stacks --stack-name "$GEN_PREFIX-$STAGE-control" --query "Stacks[0].Outputs[?OutputKey=='V1ApiUrl'].OutputValue" --output text; then
      [ "$AWS_OUT" = None ] || LEGACY_ENDPOINT=$AWS_OUT
    fi
  else
    if aws_run ec2 describe-vpcs --filters "Name=tag:project,Values=$GEN_PROJECT" "Name=tag:stage,Values=$STAGE" --query 'Vpcs[].VpcId' --output text; then
      for v in $AWS_OUT; do
        [ "$v" = None ] || LEGACY_LEFT="${LEGACY_LEFT:+$LEGACY_LEFT, }VPC $v"
      done
    elif ! aws_missing; then
      warn "could not look for a VPC of the stage under the previous names: ${AWS_ERR:-no answer}"
    fi
    for b in "$GEN_PREFIX-$STAGE-harness-$ACCOUNT" "$GEN_PREFIX-$STAGE-gomod-$ACCOUNT"; do
      if aws_run s3api head-bucket --bucket "$b" --expected-bucket-owner "$ACCOUNT"; then
        # a bucket without a tag set answers "does not exist" to the tag read: it carries no project tag, so it is not the stage's
        if aws_run s3api get-bucket-tagging --bucket "$b" --expected-bucket-owner "$ACCOUNT" --query 'TagSet[].[Key,Value]' --output text; then
          tags=$AWS_OUT
          if [ "$(printf '%s\n' "$tags" | awk -F'\t' '$1 == "project" { print $2; exit }')" = "$GEN_PROJECT" ]; then
            LEGACY_LEFT="${LEGACY_LEFT:+$LEGACY_LEFT, }bucket $b"
          fi
        fi
      elif ! aws_missing; then
        warn "could not look for the bucket $b under the previous names: ${AWS_ERR:-no answer}"
      fi
    done
    if aws_run cloudformation describe-stacks --stack-name "$SETUP_RUNNER_STACK" --query 'Stacks[0].Tags[].[Key,Value]' --output text; then
      if [ "$(printf '%s\n' "$AWS_OUT" | awk -F'\t' '$1 == "project" { print $2; exit }')" = "$GEN_PROJECT" ]; then
        LEGACY_LEFT="${LEGACY_LEFT:+$LEGACY_LEFT, }runner stack $SETUP_RUNNER_STACK"
      fi
    elif ! aws_missing; then
      warn "could not look for the runner stack $SETUP_RUNNER_STACK under the previous names: ${AWS_ERR:-no answer}"
    fi
    [ -z "$LEGACY_LEFT" ] || LEGACY_STATE=legacy-retained
  fi
  setup_gen_use "$saved"
  return 0
}

# pf_legacy_head: the sentence that says the account holds a stage under the previous names (status, doctor and the refusal share it).
pf_legacy_head() {
  if [ "$LEGACY_STATE" = legacy-retained ]; then
    printf 'Your account still holds %s from a stage deployed under the previous names (project tag %s), until ./setup.sh --teardown removes them; AgentCore can hold a network interface for up to 8 hours after its runtime is deleted.' "$LEGACY_LEFT" "$LEGACY_PROJECT"
  else
    printf 'Your account holds the stage under the previous names (stacks %s-%s-*, project tag %s), and this version of setup does not update or replace it.' "$LEGACY_PREFIX" "$STAGE" "$LEGACY_PROJECT"
  fi
}

# pf_legacy_refuse: --full and --update end here, with exit 7, when the account holds a stage under the previous names; nothing was created or changed.
pf_legacy_refuse() {
  local url_step step_opts url_hint=""
  [ "$LEGACY_STATE" != none ] || return 0
  if [ "$LEGACY_STATE" = legacy-retained ]; then
    # nothing of the previous stage runs any more: only what an earlier teardown retained is left, so there is no endpoint to move to and no run to pull
    setup_exit 7 "$(pf_legacy_head)" \
      "This attempt created or changed nothing." \
      "1. Run ./setup.sh --teardown to remove what is left. If it reports RETAINED, run it again after AWS has released the network interface." \
      "2. Run ./setup.sh --full to deploy a new stage under the current names." \
      "Teardown keeps CodeCommit repositories named $LEGACY_E2E*."
  fi
  # step 1 is a command that works as printed: the client-only setup reads the profile and the Region from its own options (the refused run's values), takes the client from a directory or builds it
  step_opts="--client-only"
  [ -z "${PROFILE_EFF:-}" ] || step_opts="$step_opts --profile $PROFILE_EFF"
  if [ -n "$LEGACY_ENDPOINT" ]; then
    step_opts="$step_opts --region $REGION --endpoint $LEGACY_ENDPOINT"
  else
    step_opts="$step_opts --region $REGION --endpoint <old endpoint URL>"
    url_hint="   Read the URL from the output V1ApiUrl of the CloudFormation stack $LEGACY_PREFIX-$STAGE-control."
  fi
  url_step="1. Install the new client against the old endpoint: ./setup.sh $step_opts --from <directory with $HAMH_CLIENT_BIN and SHA256SUMS>"
  setup_exit 7 "$(pf_legacy_head)" \
    "This attempt created or changed nothing. Your previous stage keeps running." \
    "$HAMH_CLIENT_BIN still talks to it because the wire format is unchanged, so $HAMH_CLIENT_BIN pull <runId> works against the old endpoint." \
    "$url_step" \
    ${url_hint:+"$url_hint"} \
    "   Instead of --from, --build compiles $HAMH_CLIENT_BIN from this checkout (a Rust toolchain is needed). This command creates nothing in your account." \
    "   Step 1 replaces the unedited rules pack of the earlier client with the current one. Until the stage is torn down and redeployed, runs on it show 'No test command was found' on the delivery card; the code and the tests are unaffected." \
    "2. Run $HAMH_CLIENT_BIN pull <runId> for each run whose result you want to keep." \
    "3. Run ./setup.sh --teardown to remove the previous stage." \
    "4. Run ./setup.sh --full to deploy a new stage under the current names." \
    "Teardown removes the previous stage's run records, delivered zips, engine settings, Tavily key and table rows, and keeps CodeCommit repositories named $LEGACY_E2E*."
}

# pf_stage_owner: STAGE_OWNER, the value of the `owner` tag of the stage that exists. Every stack of a stage carries it, and CloudFormation replaces the network stack's DNS query
# logging configuration when it changes (AWS::Route53Resolver::ResolverQueryLoggingConfig, Tags: "Update requires: Replacement"), which fails and rolls the update back. It reads the
# tag of the network stack, else of the control stack, and skips a stack whose first create rolled back (ROLLBACK_COMPLETE; it holds nothing). Empty when the stage has no such stack
# or its stacks carry no `owner` tag. It needs pf_stage first (STAGE_STATE and STAGE_STACKS) and makes at most two read-only calls.
pf_stage_owner() {
  local short name line status
  STAGE_OWNER=""
  if [ "$STAGE_STATE" = none ]; then return 0; fi
  for short in network control; do
    name=hamh-$STAGE-$short
    status=""
    while IFS= read -r line; do
      if [ "${line%%"$TAB"*}" = "$name" ]; then status=${line#*"$TAB"}; fi
    done <<LISTED
$STAGE_STACKS
LISTED
    case $status in
      ''|ROLLBACK_COMPLETE) continue ;;
    esac
    if aws_run cloudformation describe-stacks --stack-name "$name" --query "Stacks[0].Tags[?Key=='owner'].Value" --output text; then
      case $AWS_OUT in
        ''|None) ;;
        *) STAGE_OWNER=$AWS_OUT; return 0 ;;
      esac
    elif ! aws_missing; then
      setup_exit 1 "could not read the tags of the stack $name: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:DescribeStacks."
    fi
  done
  return 0
}

# pf_owner_check: a nonempty HAMH_OWNER that differs from the owner of the stage that exists. With --build-on local the build runs on this machine and reads HAMH_OWNER, and the
# deploy would stop on it after the toolchain was checked, so the run ends here with exit 7 and nothing is created. On the runner route the build does not receive HAMH_OWNER and
# the stage keeps its owner, so the run goes on after a warning.
pf_owner_check() {
  if [ -z "${HAMH_OWNER:-}" ] || [ -z "$STAGE_OWNER" ] || [ "$HAMH_OWNER" = "$STAGE_OWNER" ]; then return 0; fi
  if [ "${BUILD_ON:-runner}" = local ]; then
    setup_exit 7 "HAMH_OWNER=$HAMH_OWNER differs from the owner tag $STAGE_OWNER of stage $STAGE in account $ACCOUNT." \
      "Changing the owner tag of a deployed stage replaces the network stack's DNS query logging configuration, and CloudFormation rolls the update back." \
      "You can keep the owner: set HAMH_OWNER=$STAGE_OWNER or unset HAMH_OWNER, then run setup.sh again; or remove the stage with ./setup.sh --teardown and set it up again." \
      "Nothing was changed."
  fi
  warn "HAMH_OWNER=$HAMH_OWNER is set, and the runner that builds in your account does not receive HAMH_OWNER, so the stage keeps its owner tag $STAGE_OWNER."
}

# pf_active_runs: the RUNNING executions of hamh-<stage>-run, the state machine that holds one execution for each run of the stage (named by the run id), in ACTIVE_RUNS (the
# names, separated by spaces) and ACTIVE_RUN_COUNT. A state machine that does not exist has no running run. A look that fails for another reason (the credentials may not call
# states:ListExecutions) is a warning and no end: the build's own deploy guard looks again before it changes the worker runtime.
pf_active_runs() {
  local machine=hamh-$STAGE-run name
  ACTIVE_RUNS=""
  ACTIVE_RUN_COUNT=0
  if aws_run stepfunctions list-executions --state-machine-arn "arn:aws:states:$REGION:$ACCOUNT:stateMachine:$machine" --status-filter RUNNING \
      --query 'executions[].name' --output text; then
    case $AWS_OUT in
      ''|None) return 0 ;;
    esac
    for name in $AWS_OUT; do
      ACTIVE_RUNS=${ACTIVE_RUNS:+$ACTIVE_RUNS }$name
      ACTIVE_RUN_COUNT=$((ACTIVE_RUN_COUNT + 1))
    done
    return 0
  fi
  [ "$(aws_error_code)" != StateMachineDoesNotExist ] || return 0
  aws_missing && return 0
  warn "could not list the running executions of $machine: ${AWS_ERR:-no answer}. The update continues. The build checks again before it changes the worker runtime."
  return 0
}

# pf_active_runs_check: an update does not start while a run is active. It ends here with exit 7 and the list of the runs, before the runner is installed or anything is built or
# uploaded: the stage is as it was and the account holds nothing new. The names of the first 20 runs are listed.
pf_active_runs_check() {
  local shown="" n=0 name more=""
  pf_active_runs
  [ "$ACTIVE_RUN_COUNT" -gt 0 ] || return 0
  for name in $ACTIVE_RUNS; do
    n=$((n + 1))
    [ "$n" -gt 20 ] || shown=${shown:+$shown }$name
  done
  [ "$n" -le 20 ] || more=" (+$((n - 20)) more)"
  setup_exit 7 "the update is stopped: $ACTIVE_RUN_COUNT running run(s) on hamh-$STAGE-run pin the worker runtime: $shown$more" \
    "The stage is unchanged and no runner was installed." \
    "You cancel runs with $HAMH_CLIENT_BIN cancel <runId> or wait until they end." \
    "You run ./setup.sh --update again."
}

# pf_deploy_record: 0 when the deploy of the stage wrote its record, the SSM parameter /hamh/<stage>/deployment-digest that scripts/lib/deploy.py writes after its last stack, the checks
# of the runtimes and their endpoints; 1 when there is none (the deploy stopped before its end). A look that fails for any other reason ends the run.
pf_deploy_record() {
  if aws_run ssm get-parameter --name "/hamh/$STAGE/deployment-digest" --query Parameter.Value --output text; then
    case $AWS_OUT in
      ''|None) return 1 ;;
    esac
    return 0
  fi
  aws_missing && return 1
  setup_exit 1 "could not read the deploy record of the stage: ${AWS_ERR:-no answer}" "Check that your credentials may call ssm:GetParameter."
}

# pf_bootstrap_version: BOOT_VERSION, the version in the SSM parameter of the default CDK qualifier, empty when the account has none.
pf_bootstrap_version() {
  BOOT_VERSION=""
  if aws_run ssm get-parameter --name "$SETUP_BOOTSTRAP_PARAM" --query Parameter.Value --output text; then
    BOOT_VERSION=$AWS_OUT
  elif ! aws_missing; then
    setup_exit 1 "could not read the CDK bootstrap version: ${AWS_ERR:-no answer}" "Check that your credentials may call ssm:GetParameter."
  fi
}

# pf_bootstrap_stack [late]: an account without the bootstrap parameter may still hold a CDKToolkit stack: a bootstrap made with another qualifier (its parameter carries that
# qualifier), or one whose parameter was removed. Setup never changes an existing CDKToolkit, and the stage needs the bootstrap of the default qualifier, so a stack that is
# there and settled ends the run with exit 7. A stack that is not there, one whose first create rolled back (rn_prepare_stack deletes it and the bootstrap makes it again), one that
# is changing (rn_bootstrap waits for it and looks again with `late`) and one in a state CloudFormation cannot change (rn_prepare_stack names the way out) are left to the steps
# that know them. Early, nothing has been created; late, the runner is installed and stays.
pf_bootstrap_stack() {
  local when=${1:-early} status
  [ -z "$BOOT_VERSION" ] || return 0
  rn_stack_status "$SETUP_BOOTSTRAP_STACK" || setup_exit 1 "could not look at the stack $SETUP_BOOTSTRAP_STACK: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:DescribeStacks."
  status=$RN_STATUS
  case $status in
    ''|ROLLBACK_COMPLETE|CREATE_FAILED|*_IN_PROGRESS|DELETE_FAILED|ROLLBACK_FAILED|UPDATE_FAILED|UPDATE_ROLLBACK_FAILED|IMPORT_ROLLBACK_FAILED) return 0 ;;
  esac
  if [ "$when" = late ]; then
    setup_exit 7 "your account's $SETUP_BOOTSTRAP_STACK stack exists in $REGION with status $status. SSM parameter $SETUP_BOOTSTRAP_PARAM is missing." \
      "The bootstrap uses another CDK qualifier, or the parameter is removed. setup.sh never changes an existing $SETUP_BOOTSTRAP_STACK stack." \
      "Your stage needs a CDK bootstrap with the default qualifier $SETUP_BOOTSTRAP_QUALIFIER." \
      "Use another account or Region, or bootstrap this account and Region yourself: cdk bootstrap --qualifier $SETUP_BOOTSTRAP_QUALIFIER --toolkit-stack-name <name>. Run setup.sh again." \
      "The runner stays installed${RN_EXPIRES:+ (its authority to assume the CDK roles ends $RN_EXPIRES UTC)}. Run ./setup.sh --teardown to remove it."
  fi
  setup_exit 7 "your account's $SETUP_BOOTSTRAP_STACK stack exists in $REGION with status $status. SSM parameter $SETUP_BOOTSTRAP_PARAM is missing." \
    "The bootstrap uses another CDK qualifier, or the parameter is removed. setup.sh never changes an existing $SETUP_BOOTSTRAP_STACK stack." \
    "Your stage needs a CDK bootstrap with the default qualifier $SETUP_BOOTSTRAP_QUALIFIER." \
    "Use another account or Region, or bootstrap this account and Region yourself: cdk bootstrap --qualifier $SETUP_BOOTSTRAP_QUALIFIER --toolkit-stack-name <name>. Run setup.sh again." \
    "Nothing was created."
}

# pf_bootstrap_pin: an account without a CDK bootstrap gets one from the template its runner prints, and deploys it only when its SHA-256 is the one this clone pins.
# BOOT_PIN_SHA is that SHA-256, empty when the account has a bootstrap.
pf_bootstrap_pin() {
  local file=$SELF_DIR/$SETUP_BOOTSTRAP_PIN
  BOOT_PIN_SHA=""
  [ -z "$BOOT_VERSION" ] || return 0
  [ -f "$file" ] || setup_exit 5 "the account has no CDK bootstrap and $SETUP_BOOTSTRAP_PIN is missing from this clone" \
    "It is part of the repository; fetch the whole repository (git pull) and run setup.sh again." "Nothing was created."
  BOOT_PIN_SHA=$(sed -n 's/^sha256=//p' "$file" | head -n 1)
  case $BOOT_PIN_SHA in
    *[!0-9a-f]*|'') setup_exit 5 "$SETUP_BOOTSTRAP_PIN holds no SHA-256 (a line sha256= and 64 lower case hexadecimal digits)" "Nothing was created." ;;
  esac
  [ "${#BOOT_PIN_SHA}" -eq 64 ] || setup_exit 5 "$SETUP_BOOTSTRAP_PIN holds no SHA-256 (a line sha256= and 64 lower case hexadecimal digits)" "Nothing was created."
}
