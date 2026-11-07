# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): the full setup and the update.
#
#   full_run full|update   reads the account, shows what will happen and what it costs, asks ONE question, installs the runner, builds and deploys
#                          the backend and builds the client in the user's account, downloads and checks the client. It returns to setup.sh with
#                          FROM (a folder that holds the checked client and its SHA256SUMS), ENDPOINT, REGION and YES=1 set, and setup.sh goes on
#                          with the client steps it always does (install, `ham config set`, models, rules init, doctor). With --build-on local the
#                          deploy engine runs on this machine and the client is compiled here (BUILD=1).
#   full_local_missing     the programs of the local route that this machine lacks
#   full_summary           the closing block: endpoint, phases, what stays, how to tear down, the first command
#   full_installed         records in the receipt that the client is installed
#
# Exit codes: 4 you cancelled or the confirmation is missing, 5 bootstrap, deploy or initialization failed, 6 the build is still
# running and safe to resume, 7 a conflicting operation or a source or configuration mismatch, 8 cleanup is incomplete.

FULL_RAN=0
FULL_EXISTING=0
FULL_MODE_NOTE=""
FULL_REMOVE_RUNNER=0
ACCT_PROFILE_FILE=""
LOCAL_AWS_CONFIG=0

# full_local_profile: the profile name the generated account file carries on the local route: the one you named (--profile or the ham config), else AWS_PROFILE, else
# `hamh-setup` for credentials that are only in the AWS_* variables (full_local_flow writes a configuration file that has that profile), else `default`. The deploy scripts
# export this name as AWS_PROFILE, and the AWS CLI refuses a profile it does not find.
full_local_profile() {
  LOCAL_AWS_CONFIG=0
  if [ -n "${PROFILE_EFF:-}" ]; then
    ACCT_PROFILE_FILE=$PROFILE_EFF
  elif [ -n "${AWS_PROFILE:-}" ]; then
    ACCT_PROFILE_FILE=$AWS_PROFILE
  elif [ "${CRED_FROM_ENV:-0}" = 1 ]; then
    ACCT_PROFILE_FILE=hamh-setup
    LOCAL_AWS_CONFIG=1
  else
    ACCT_PROFILE_FILE=default
  fi
}

# ---- the plan ------------------------------------------------------------------------------------------------------------

# full_local_missing: prints the programs of the local route that are not on PATH, separated by spaces.
full_local_missing() {
  local tool out=""
  for tool in cargo cargo-zigbuild zig node docker; do
    command -v "$tool" >/dev/null 2>&1 || out="$out $tool"
  done
  printf '%s' "${out# }"
}

# full_run_names: one line of the plan that says whether this Region gets run names. A small model (the default row of crates/ham-core/assets/utility-models.json, Claude Haiku 5.5)
# writes the name and the tags of every run, and its row is available in one Region. Nothing is printed when the clone has no catalog or the catalog does not read.
full_run_names() {
  local file=$SELF_DIR/crates/ham-core/assets/utility-models.json flat def alias n=0 row="" name region
  [ -f "$file" ] || return 0
  flat=$(json_flat <"$file" 2>/dev/null) || return 0
  def=$(cl_flat_get "$flat" default)
  [ -n "$def" ] || return 0
  while [ "$n" -lt 50 ]; do
    alias=$(cl_flat_get "$flat" "models.$n.alias")
    [ -n "$alias" ] || return 0
    if [ "$alias" = "$def" ]; then row=$n; break; fi
    n=$((n + 1))
  done
  [ -n "$row" ] || return 0
  name=$(cl_flat_get "$flat" "models.$row.name")
  region=$(cl_flat_get "$flat" "models.$row.region")
  { [ -n "$name" ] && [ -n "$region" ]; } || return 0
  if [ "$region" = "$REGION" ]; then
    say "  run names:     on: $name writes a short name and tags for each run in this Region. The name appears in your run list. With --build-on local, you set HAMH_UTILITY_MODEL=off before setup to turn names and tags off."
  else
    say "  run names:     off in $REGION: $name writes run names and tags only in $region. Your runs work the same and show a short label cut from the prompt. You deploy in $region with --region $region to enable run names."
  fi
}

full_authority() {
  say ""
  say "What this setup grants"
  if [ "${BUILD_ON:-runner}" = local ]; then
    say "  - The deploy runs on this machine with the authority of your credentials: it creates IAM roles, a VPC, a firewall and the rest of the stage in your"
    say "    account, so your credentials must be allowed to create IAM roles (normally an administrator). No runner is installed."
    say "  - The CDK bootstrap stack ($SETUP_BOOTSTRAP_STACK) is made by the deploy when your account has none. It stays after the setup, because updates need it."
    say "  - Use a dedicated sandbox account."
    return 0
  fi
  say "  - A CloudFormation stack ($SETUP_RUNNER_STACK) with an encrypted, versioned bucket and a CodeBuild project, the runner. The runner role writes its own logs, the results"
  say "    and client files of its builds in that bucket, the objects of the stage it deploys (builds, the Cline engine and deploy records in the stage's bucket), the stage's"
  say "    parameters under /hamh/direct/, the stage's worker runtime endpoints and the scanner rules package in the stage's package repository, and it starts the stage's"
  say "    image-build project. It has no IAM permission of its own: a permissions boundary limits the runner role itself. Its real authority comes from the CDK roles it can assume."
  say "  - The CDK bootstrap stack ($SETUP_BOOTSTRAP_STACK), when your account has none: the runner prints its template with the CDK CLI this version pins, setup checks the"
  say "    template against $SETUP_BOOTSTRAP_PIN and creates the stack with your credentials. Its CloudFormation execution role holds AdministratorAccess (the CDK's own"
  say "    default), and the runner can use it through the CDK roles: treat the runner as an administrator of the account for ${AUTH_HOURS:-12} hours after each install. After that"
  say "    time its role cannot assume the CDK roles any more, whether or not the runner is still installed. The bootstrap roles trust the whole account and stay in the account"
  say "    after the runner is removed, because updates need them."
  say "  - The bucket is closed to everyone else: only you and the runner role may write or delete in it. Anyone who may start a build of the project (codebuild:StartBuild)"
  say "    can run commands as the runner role, and so as an administrator, until that time; give that permission to no one else in the account while the runner exists."
  say "  - The deploy creates IAM roles, a VPC, a firewall and the rest of the stage, so your credentials must be allowed to create IAM roles"
  say "    (normally an administrator). Deployment code runs with that authority in your account."
  say "  - The runner is removed after a successful setup; --keep-runner keeps it. Use a dedicated sandbox account."
}

full_costs() {
  say ""
  say "What it costs (billed to this AWS account; setup gives no estimate)"
  say "  - NAT gateway: an hourly charge and a charge per GB, while the stage exists."
  say "  - Network Firewall endpoint: an hourly charge and a charge per GB, while the stage exists."
  say "  - Interface endpoints: an hourly charge for each endpoint and zone."
  say "  - AgentCore sessions: charged while a run's session is alive."
  say "  - Bedrock calls: the tokens of the models a run uses."
  say "  - CodeBuild: build minutes for this setup and for every update."
  say "  - Storage: S3, DynamoDB, KMS and ECR."
  say "  - Logs: CloudWatch Logs."
  say "  The charges for the first items continue while the stage sits idle, until you run ./setup.sh --teardown."
  say "  Teardown takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface. AgentCore can keep that interface for up to 8 hours after its runtime is deleted. Teardown retains the network resources the interface holds. You run ./setup.sh --teardown again to remove them."
}

full_plan() {
  local action=$1 how
  step "What setup will do"
  say "  account:       $ACCOUNT ($CALLER_ARN)"
  if [ -n "${ACCT_OP_KIND:-}" ]; then
    say "  operator:      $ACCT_OP_KIND $ACCT_OP_NAME: the stage trusts exactly this identity for its admin routes and its deploys; run setup as another identity and the stage trusts that one instead"
  fi
  say "  Region:        $REGION"
  say "  zone:          $ACCT_DIRECT_AZ"
  say "  stage:         $STAGE"
  if [ -n "${ACCT_REPO_NAMES:-}" ]; then
    say "  repository names: ${ACCT_REPO_NAMES//,/, }. A run may open a pull request only in a CodeCommit repository with a matching name. The stage operator must also allow that repository."
  fi
  say "  source:        commit $SRC_COMMIT (clean clone)"
  if [ "${BUILD_ON:-runner}" = local ]; then
    say "  build:         on this machine, with scripts/deploy.sh (the account file below is passed to it)"
    say "  account file:  generated from your account id, Region, zone ids and the identity of your credentials, kept outside the clone ($ACCT_KEY)"
    say "  client:        compiled on this machine with cargo and installed, with the operator tool beside it"
  else
    say "  build:         the runner in your AWS account (a CodeBuild project), from the git archive of that commit"
    say "  account file:  $ACCT_KEY, generated from your account id, Region, zone ids and the identity of your credentials and nothing else; uploaded as its own object"
    say "  client:        built in your account for $CLIENT_PLATFORM with the operator tool $HAMH_ADMIN_BIN beside it, downloaded from your bucket, checked against its SHA-256 and installed"
  fi
  full_run_names
  case $RN_MODE in
    attach) say "  note:          a build of this commit is already running ($RN_BUILD_ID); setup attaches to it instead of starting another" ;;
    finish) say "  note:          a build of this commit already succeeded ($RN_BUILD_ID); setup reads its result instead of building again" ;;
  esac
  if [ -n "$STAGE_FAILED" ]; then
    say "  note:          the stack ${STAGE_FAILED%% *} is in ${STAGE_FAILED#* } (an earlier create did not finish); it holds nothing, and the deploy removes it and makes it again"
  fi
  if [ "$STAGE_UNFINISHED" = 1 ]; then
    say "  note:          the stage is only partly deployed: all CloudFormation stacks exist, but the deploy did not finish. The deploy runs again. Complete stacks stay as they are."
  fi
  if [ "$action" = full ]; then
    full_authority
    full_costs
  else
    how="the stage keeps its resources and gets the new revision; the runner is installed again for the build and removed afterwards"
    say "  update:        $how"
    say "  duration:      about 20 to 25 minutes with a reused Cline binary, plus about 21 to 27 minutes when the binary must be rebuilt"
    if [ "${ALLOW_ACTIVE_RUNS:-0}" = 1 ]; then
      say "  guard:         active runs are not checked before the build; the build refuses only when the worker runtime changes"
    else
      say "  guard:         Active runs stop ./setup.sh --update with exit 7 before anything is installed or built."
      say "                 The build also refuses worker runtime changes while runs are active."
    fi
  fi
}

full_would() {
  local action=$1
  [ "${BUILD_ON:-runner}" != local ] || {
    would "run scripts/deploy.sh --stage $STAGE --azs $AZS --fork-source build here, with the generated account file"
    would "compile the client and the operator tool here and install them, then configure the client"
    return 0
  }
  would "install the runner stack $SETUP_RUNNER_STACK from $SETUP_RUNNER_TEMPLATE"
  would "upload $SRC_KEY and $ACCT_KEY to the runner bucket $BUCKET"
  [ -n "$BOOT_VERSION" ] || would "have the runner print the CDK bootstrap template (a first build of a few minutes), check it against $SETUP_BOOTSTRAP_PIN and create the stack $SETUP_BOOTSTRAP_STACK from it with your credentials"
  would "take the lease of stage $STAGE, start the build and follow it"
  would "download the client and the operator tool for $CLIENT_PLATFORM, check their SHA-256 and install them"
  [ "${KEEP_RUNNER:-0}" = 1 ] || would "empty the bucket and remove the runner stack"
  would "write the client settings, run $HAMH_CLIENT_BIN models, rules init and doctor"
  [ "$action" = full ] || would "start from the stage that exists now"
}

# full_confirm <prompt> <costs|yes>: the one question. Returns 0 to go ahead; every other answer ends the run with exit 4 and nothing created.
full_confirm() {
  local prompt=$1 need=$2 answer
  if [ "$YES" = 1 ]; then
    if [ "$need" = costs ] && [ "${ACK_COSTS:-0}" != 1 ]; then
      setup_exit 4 "--yes does not accept the costs of a full setup" \
        "Add --acknowledge-costs to go ahead without a question:  ./setup.sh --full --yes --acknowledge-costs" "Nothing was created."
    fi
    return 0
  fi
  if [ "$NO_INPUT" = 1 ]; then
    setup_exit 4 "--no-input was given, so there is no one to ask" "Add --yes --acknowledge-costs to go ahead, or --dry-run to preview." "Nothing was created."
  fi
  if [ -t 0 ]; then
    printf '%s' "$prompt"
    if ! IFS= read -r answer; then
      printf '\n'
      setup_exit 4 "the input ended before you confirmed. Nothing was created."
    fi
    case $answer in
      y|Y|yes|YES) return 0 ;;
    esac
    setup_exit 4 "stopped at your request. Nothing was created."
  fi
  setup_exit 4 "there is no terminal to ask on" "Add --yes --acknowledge-costs to go ahead without a prompt, or --dry-run to preview." "Nothing was created."
}

# ---- state of this run ---------------------------------------------------------------------------------------------------------

# full_state_begin: the local state of the run (the receipt and the log), made only after the confirmation.
full_state_begin() {
  st_init "$ACCOUNT" "$REGION" "$STAGE"
  st_load
  (
    umask 077
    mkdir -p "$ST_DIR/logs" || exit 1
  ) || setup_exit 1 "could not make $ST_DIR"
  RUN_LOG=$ST_DIR/logs/setup-$(setup_stamp).log
  (
    umask 077
    : >"$RUN_LOG"
  ) || setup_exit 1 "could not write the log $RUN_LOG"
  log_line "setup ${FULL_ACTION:-full}: account $ACCOUNT Region $REGION stage $STAGE commit $SRC_COMMIT"
  [ -z "$PROFILE_EFF" ] || st_set profile "$PROFILE_EFF" || true
  st_set sourceCommit "$SRC_COMMIT" || true
  st_set sourceKey "$SRC_KEY" || true
  st_set configKey "$ACCT_KEY" || true
  st_set configDigest "$ACCT_DIGEST" || true
  st_set mode "${BUILD_ON:-runner}" || true
  st_set retained "" || true
  # what the last run left about its failure and its build is not true of this one
  st_set failedPhase "" || true
  st_set detail "" || true
  st_set buildId "" || true
  st_set state starting || true
  st_save || warn "could not write the receipt $ST_FILE"
  say "Log of this run: $RUN_LOG"
}

# ---- the runner route ------------------------------------------------------------------------------------------------------------

full_runner_flow() {
  local endpoint
  RN_ACTION=deploy
  case $RN_MODE in
    new|stale)
      rn_install
      rn_upload
      rn_bootstrap
      rn_lease_take
      if [ "$RN_MODE" = new ] || [ "$RN_MODE" = stale ]; then
        rn_start
        rn_follow
      elif [ "$RN_MODE" = attach ]; then
        say_log "Another setup started the same build meanwhile; attaching to $RN_BUILD_ID"
        rn_follow
      else
        RN_FINAL=SUCCEEDED
      fi ;;
    attach)
      say_log "Attaching to the running build $RN_BUILD_ID"
      st_set buildId "$RN_BUILD_ID" || true
      st_set state started || true
      st_save || true
      rn_follow ;;
    finish)
      say_log "The build $RN_BUILD_ID already succeeded; reading its result"
      RN_FINAL=SUCCEEDED ;;
  esac
  rn_settle
  endpoint=$(rn_res endpoint)
  if ! valid_stage_endpoint "$endpoint"; then
    rn_lease_release
    setup_exit 5 "the build reported an endpoint that is not the https URL of an API Gateway stage: ${endpoint:-none}" "Nothing was installed."
  fi
  # The client is pointed at the endpoint, so the stack of this account, which made it, has the last word: the result file only repeats it.
  pf_control_endpoint
  if [ "$CTRL_ENDPOINT" != "$endpoint" ]; then
    rn_lease_release
    setup_exit 7 "endpoint mismatch: the result of the build names $endpoint, and the stack hamh-$STAGE-control of this account has ${CTRL_ENDPOINT:-no V1ApiUrl output}" \
      "Nothing was installed. The result file of the build in your bucket is not believed."
  fi
  ENDPOINT=$endpoint
  st_set state built || true
  st_set endpoint "$ENDPOINT" || true
  st_set buildId "$RN_BUILD_ID" || true
  st_save || true
  ok "the backend is deployed: $ENDPOINT"
  # The lease stays until the client is downloaded and checked: a run that is interrupted in between finds the finished build and goes on from here.
  if ! cl_fetch "$RES_FLAT" "$RN_BUCKET" "$RN_BUILD_ID" "$CLIENT_PLATFORM"; then
    rn_lease_release
    st_set state failed || true
    st_set failedPhase client || true
    st_save || true
    err "the client from your bucket was not installed."
    detail "The backend is deployed ($ENDPOINT). Run  ./setup.sh --update  to build the client again in your account (the runner is installed again for it), or install a client you have with  setup.sh --client-only --from DIR."
    exit 5
  fi
  rn_lease_release
  ok "client for $CLIENT_PLATFORM downloaded and checked: sha256 $CL_SHA"
  ok "operator tool for $CLIENT_PLATFORM downloaded and checked: sha256 $CL_ADMIN_SHA"
  st_set client "$CLIENT_PLATFORM" || true
  st_save || true
  FROM=$CL_DIR
  if [ "${KEEP_RUNNER:-0}" = 1 ]; then
    FULL_MODE_NOTE="the runner stack $SETUP_RUNNER_STACK and its bucket $RN_BUCKET are kept (--keep-runner)"
  else
    # The checked client is nowhere but in the bucket that is emptied with the runner, and in the download folder of the receipt. The runner goes after the client is
    # installed (full_installed), so a setup that is stopped in between still has the client.
    FULL_REMOVE_RUNNER=1
  fi
}

# ---- the local route -------------------------------------------------------------------------------------------------------------

# full_quiet_cdk <line>: 0 when the line is a CDK notice that repeats for every stack and the screen leaves out: the deprecation warning of aws-cdk-lib with its two indented lines,
# the warning about `cdk synth` in Docker on a Linux 5.6 to 5.10 kernel, and the annotation that says run names are off in this Region with its two indented lines (the plan says
# that once). The log of the run keeps every line, and --verbose leaves none out. Any other line stays on the screen.
FQ_REST=0
full_quiet_cdk() {
  [ "${VERBOSE:-0}" != 1 ] || return 1
  if [ "$FQ_REST" -gt 0 ]; then
    FQ_REST=$((FQ_REST - 1))
    case $1 in
      "  "*) return 0 ;;
    esac
    FQ_REST=0
  fi
  case $1 in
    "[WARNING] aws-cdk-lib."*" is deprecated."|"INFO HAMH_UTILITY_MODEL is unset:"*) FQ_REST=2; return 0 ;;
    '`cdk synth` may hang in Docker on Linux '*) return 0 ;;
  esac
  return 1
}

full_local_flow() {
  local file rc line endpoint cfg=""
  file=$ST_DIR/accounts-local.json
  (
    umask 077
    printf '%s' "$ACCT_JSON" >"$file"
  ) || setup_exit 1 "could not write $file"
  if [ "$LOCAL_AWS_CONFIG" = 1 ]; then
    # Credentials in the environment have no profile, and the deploy scripts export the profile of the account file as AWS_PROFILE, which the AWS CLI wants to find in
    # a configuration file. This file has that profile and a Region and nothing else, so the credentials of the environment are still the ones that sign.
    cfg=$ST_DIR/aws-config
    (
      umask 077
      printf '[profile %s]\nregion = %s\n' "$ACCT_PROFILE_FILE" "$REGION" >"$cfg"
    ) || setup_exit 1 "could not write $cfg"
  fi
  say_log "Running scripts/deploy.sh on this machine (the account file is $file) ..."
  (
    cd "$SELF_DIR" || exit 1
    [ -z "$cfg" ] || export AWS_CONFIG_FILE=$cfg
    HAMH_ACCOUNTS_FILE=$file HAMH_AWS_PROFILE=$ACCT_PROFILE_FILE scripts/deploy.sh --stage "$STAGE" --azs "$AZS" --fork-source build 2>&1
  ) | while IFS= read -r line; do
    log_line "deploy: $line"
    if full_quiet_cdk "$line"; then continue; fi
    printf '  %s\n' "$line"
  done
  rc=${PIPESTATUS[0]}
  if [ "$rc" != 0 ]; then
    st_set state failed || true
    st_save || true
    setup_exit 5 "scripts/deploy.sh failed (exit $rc)" "Its output is above and in $RUN_LOG." "Run ./setup.sh --full again to retry."
  fi
  pf_control_endpoint
  endpoint=$CTRL_ENDPOINT
  if ! valid_stage_endpoint "$endpoint"; then
    setup_exit 5 "the deploy finished but the stack hamh-$STAGE-control has no V1ApiUrl output that is the https URL of an API Gateway stage: ${endpoint:-none}"
  fi
  ENDPOINT=$endpoint
  st_set state built || true
  st_set endpoint "$ENDPOINT" || true
  st_save || true
  ok "the backend is deployed: $ENDPOINT"
  BUILD=1
  BUILD_ADMIN=1
}

# ---- the stage is there already ----------------------------------------------------------------------------------------------------

# full_reuse_client: no client is installed on this machine, but an earlier setup downloaded and checked one (the folder download/verified of the receipt). That copy
# is used again when its file still hashes to the entry of the SHA256SUMS beside it, so a setup that stopped after the build, and the runner that has gone since, do not
# cost another build. Sets FROM and CL_SHA; returns 1 when there is nothing to use.
full_reuse_client() {
  local dir want got admin_want admin_got
  [ -z "${FROM:-}" ] && [ -z "$(ham_now)" ] || return 1
  st_init "$ACCOUNT" "$REGION" "$STAGE"
  dir=$ST_DIR/download/verified
  [ -f "$dir/$HAMH_CLIENT_BIN" ] && [ -f "$dir/SHA256SUMS" ] || return 1
  want=$(cl_sums_get "$(cat "$dir/SHA256SUMS")" "$HAMH_CLIENT_BIN") || return 1
  got=$(setup_sha256 <"$dir/$HAMH_CLIENT_BIN")
  [ "$got" = "$want" ] || return 1
  # a folder from a setup before the operator tool existed holds the client alone; a copy of the operator tool that no longer matches its line stops the reuse
  if [ -f "$dir/$HAMH_ADMIN_BIN" ]; then
    admin_want=$(cl_sums_get "$(cat "$dir/SHA256SUMS")" "$HAMH_ADMIN_BIN") || return 1
    admin_got=$(setup_sha256 <"$dir/$HAMH_ADMIN_BIN")
    [ "$admin_got" = "$admin_want" ] || return 1
    CL_ADMIN_SHA=$admin_got
  fi
  FROM=$dir
  CL_SHA=$got
  return 0
}

full_existing() {
  step "The stage is already deployed"
  say "The stage $STAGE is already deployed in account $ACCOUNT, Region $REGION."
  if [ -n "$STAGE_ENDPOINT" ] && valid_endpoint "$STAGE_ENDPOINT"; then
    say "Its endpoint is $STAGE_ENDPOINT. Nothing is deployed again."
    ENDPOINT=$STAGE_ENDPOINT
  else
    warn "its control stack has no endpoint output that is a URL, so the client cannot be pointed at it"
  fi
  if full_reuse_client; then
    say "No $HAMH_CLIENT_BIN is installed on this machine; the one this setup downloaded and checked earlier (sha256 $CL_SHA) is installed."
  fi
  if [ "$RN_EXISTS" = 1 ] && [ "${KEEP_RUNNER:-0}" != 1 ]; then
    # a runner from an earlier setup (its removal was stopped, or the setup was): the stage is deployed and no build is running, so it has no work left
    FULL_REMOVE_RUNNER=1
    say "The runner from an earlier setup is removed once the client is in place (--keep-runner keeps it)."
  fi
  say "To deploy a newer revision of the code run  ./setup.sh --update ."
  FULL_EXISTING=1
}

# full_existing_preview: --preflight-only on a stage that is deployed: what would be configured, and nothing is installed or written.
full_existing_preview() {
  step "Preflight finished"
  say "The stage $STAGE is already deployed in account $ACCOUNT, Region $REGION${STAGE_ENDPOINT:+, endpoint $STAGE_ENDPOINT}."
  say "Nothing is deployed again. Run again without --preflight-only to install and configure the client for it."
  exit 0
}

# ---- the run -----------------------------------------------------------------------------------------------------------------------------

# full_run full|update
full_run() {
  local action=$1 missing
  FULL_ACTION=$action
  RN_ACTION=deploy
  if [ "${BUILD_ON:-runner}" = local ]; then
    missing=$(full_local_missing)
    if [ -n "$missing" ]; then
      setup_exit 1 "--build-on local needs the toolchain on this machine, and these programs are missing: $missing" \
        "Without them use the runner in your AWS account, which needs nothing installed here:  ./setup.sh --full"
    fi
  fi
  step "Checking your account"
  pf_region
  pf_profile
  pf_aws_cli
  pf_identity
  pf_source
  if [ "${BUILD_ON:-runner}" = local ]; then
    full_local_profile
  else
    pf_templates
    pf_installer
    CLIENT_PLATFORM=$(cl_platform) || exit 1
    ACCT_PROFILE_FILE=hamh-setup
  fi
  pf_zones "$ACCT_PROFILE_FILE"
  pf_stage
  # a stage deployed under the previous names is neither updated nor replaced: the run ends here, before the runner is installed or anything is created (LEGACY-TOMBSTONE)
  pf_legacy_refuse
  if [ "${BUILD_ON:-runner}" != local ]; then
    rn_lease_check
    [ "$RN_MODE" = attach ] || rn_note_installed
  fi
  # A build of this checkout that is running or has just succeeded explains the stacks it left in whatever state, and is attached to or read first (its client is
  # still to come); only without one does the state of the stage decide.
  case $RN_MODE in
    attach|finish) ;;
    *)
      case $action in
        full)
          case $STAGE_STATE in
            complete)
              [ "${PREFLIGHT_ONLY:-0}" != 1 ] || full_existing_preview
              full_existing
              return 0 ;;
            deploying)
              setup_exit 7 "the stage $STAGE in account $ACCOUNT is being changed right now: $STAGE_BAD" \
                "No build of this checkout explains it, so another operation is working on the stage (the CloudFormation console, another tool, a build of another setup)." \
                "Wait until it has finished, then run setup.sh again. ./setup.sh --status shows the state." "Nothing was changed." ;;
            broken)
              setup_exit 7 "the stage $STAGE exists in account $ACCOUNT but is not healthy: $STAGE_BAD" \
                "Setup does not build on top of it. Look at the stack in the CloudFormation console, or remove the stage with ./setup.sh --teardown and run setup again." ;;
          esac ;;
        update)
          case $STAGE_STATE in
            none) setup_exit 7 "nothing to update: the stage $STAGE is not deployed in account $ACCOUNT, Region $REGION" "Deploy it first:  ./setup.sh --full" ;;
            deploying)
              setup_exit 7 "the stage $STAGE in account $ACCOUNT is being changed right now: $STAGE_BAD" \
                "No build of this checkout explains it, so another operation is working on the stage. Wait until it has finished, then run setup.sh again." "Nothing was changed." ;;
            broken) setup_exit 7 "the stage $STAGE exists in account $ACCOUNT but is not healthy: $STAGE_BAD" "Look at the stack in the CloudFormation console before you update." ;;
          esac ;;
      esac ;;
  esac
  # A deploy follows (a build of this checkout that is attached to or read does not start one): the stage keeps its owner, so one that HAMH_OWNER names differently is found here.
  case $RN_MODE in
    attach|finish) ;;
    *)
      pf_stage_owner
      pf_owner_check
      # an update does not start while a run is active: found here, it costs seconds and leaves the account as it was. --allow-active-runs leaves the decision to the
      # build's own deploy guard, which refuses only an update that changes the worker runtime.
      [ "$action" != update ] || [ "${ALLOW_ACTIVE_RUNS:-0}" = 1 ] || pf_active_runs_check ;;
  esac
  pf_bootstrap_stack
  if [ "${BUILD_ON:-runner}" != local ]; then
    pf_bootstrap_pin
  fi
  if [ "${PREFLIGHT_ONLY:-0}" = 1 ]; then
    step "Preflight finished"
    say "Everything above was read; nothing was created. Run again without --preflight-only to go ahead."
    exit 0
  fi
  full_plan "$action"
  if [ "$DRY" = 1 ]; then
    step "Dry run"
    full_would "$action"
    say "Nothing was created. Run again without --dry-run to do it."
    exit 0
  fi
  if [ "$action" = full ]; then
    full_confirm "Create these resources and start the deploy? [y/N] " costs
  else
    full_confirm "Update stage $STAGE to commit $SRC_COMMIT? [y/N] " yes
  fi
  YES=1
  full_state_begin
  if [ "${BUILD_ON:-runner}" = local ]; then
    full_local_flow
  else
    full_runner_flow
  fi
  FULL_RAN=1
}

# full_installed: the client is installed and runs (setup.sh calls it after `ham models`). The receipt says so, and the runner that is not kept goes now: it held the
# only copy of the client apart from the one in the receipt folder, which is installed.
full_installed() {
  if [ "$FULL_RAN" = 1 ]; then
    st_set state installed || true
    st_save || true
  fi
  if [ "$FULL_REMOVE_RUNNER" = 1 ]; then
    FULL_REMOVE_RUNNER=0
    rn_remove || true
  fi
}

# full_phase_lines: one line for each phase the result lists.
full_phase_lines() {
  local n=0 name state secs
  while :; do
    name=$(rn_res "phases.$n.name")
    [ -n "$name" ] || break
    state=$(rn_res "phases.$n.state")
    secs=$(rn_res "phases.$n.seconds")
    say "    $name${state:+ $state}${secs:+ (${secs}s)}"
    n=$((n + 1))
  done
}

# full_summary <installed client path>
full_summary() {
  step "Setup summary"
  say "  Endpoint:      $ENDPOINT"
  say "  Account:       $ACCOUNT, Region $REGION, stage $STAGE"
  say "  Source:        commit $SRC_COMMIT"
  if [ -n "${RES_FLAT:-}" ]; then
    say "  Phases:"
    full_phase_lines
  fi
  if [ -n "${1:-}" ]; then
    say "  Client:        $1${CL_SHA:+ (sha256 $CL_SHA)}"
    if [ -f "$(dirname "$1")/$HAMH_ADMIN_BIN" ]; then
      say "  Operator tool: $(dirname "$1")/$HAMH_ADMIN_BIN${CL_ADMIN_SHA:+ (sha256 $CL_ADMIN_SHA)}: $HAMH_ADMIN_BIN repos allow cc:<name> lets pull requests go to that repository"
    fi
  fi
  say "  Retained:      the CDK bootstrap stack $SETUP_BOOTSTRAP_STACK (updates need it)${FULL_MODE_NOTE:+; $FULL_MODE_NOTE}"
  if [ "$RUNNER_LEFT" = 1 ]; then
    say "                 the runner stack $SETUP_RUNNER_STACK could not be removed (see RETAINED above)"
  fi
  say "  Teardown:      ./setup.sh --teardown   (charges for the stage continue until you run it)"
  say "  First command: $HAMH_CLIENT_BIN \"Build a small command-line todo app in Python with tests\" --s3-only --direct"
}
