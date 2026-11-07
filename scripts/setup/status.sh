# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): `setup.sh --status` and `setup.sh --doctor`. Both only read: they make no change
# in the account, write no file and ask no question.
#
#   run_status   what is deployed, which build runs, what the last setup did, whether the client fits the stage and whether the operator tool is installed. First line:
#                  STATUS stage=direct state=<not-deployed|partial|deployed|broken|deploying|failed|retained|legacy|legacy-retained> account=<id> region=<region>
#                Exit 0 (nothing wrong), 5 (the last build failed or the stage is broken), 6 (a build is running), 7 (the stage was deployed under the previous names: state legacy),
#                8 (resources were retained: state retained or legacy-retained).
#                state=legacy and state=legacy-retained come from pf_legacy (LEGACY-TOMBSTONE): a stage of the previous names is never reported as not-deployed.
#   run_doctor   PASS, WARN and FAIL lines for the AWS CLI, the identity, the stage, the endpoint, the operator tool, a runner that was left behind, and the client's own
#                doctor. Exit 0, or 3 when a check failed (the same code as setup.sh gives when the client's doctor finds a problem).

# status_client: prints what the client on this machine says about the stage: sets STATUS_CLIENT_STATE to missing, matches, differs or unset,
# and STATUS_CLIENT_BIN and STATUS_CLIENT_ENDPOINT. The client is compared with the endpoint of the stage; an account that holds only a stage under the previous names has the
# endpoint of that stage (LEGACY_ENDPOINT), and the client is compared with that one (STATUS_COMPARE_ENDPOINT, named by STATUS_COMPARE_LABEL).
STATUS_CLIENT_STATE=missing
STATUS_CLIENT_BIN=""
STATUS_CLIENT_ENDPOINT=""
STATUS_COMPARE_ENDPOINT=""
STATUS_COMPARE_LABEL="the stage"
status_client() {
  STATUS_CLIENT_STATE=missing
  STATUS_CLIENT_ENDPOINT=""
  STATUS_COMPARE_ENDPOINT=$STAGE_ENDPOINT
  STATUS_COMPARE_LABEL="the stage"
  if [ -z "$STATUS_COMPARE_ENDPOINT" ] && [ -n "${LEGACY_ENDPOINT:-}" ]; then
    STATUS_COMPARE_ENDPOINT=$LEGACY_ENDPOINT
    STATUS_COMPARE_LABEL="the stage under the previous names"
  fi
  STATUS_CLIENT_BIN=$(ham_now)
  [ -n "$STATUS_CLIENT_BIN" ] || return 0
  cfg_get api_endpoint apiEndpoint
  STATUS_CLIENT_ENDPOINT=$CFG_VAL
  if [ -z "$STATUS_CLIENT_ENDPOINT" ]; then
    STATUS_CLIENT_STATE="unset"
  elif [ -n "$STATUS_COMPARE_ENDPOINT" ] && [ "$STATUS_CLIENT_ENDPOINT" = "$STATUS_COMPARE_ENDPOINT" ]; then
    STATUS_CLIENT_STATE=matches
  else
    STATUS_CLIENT_STATE=differs
  fi
}

# status_admin: where the operator tool is: the file beside the installed client. Sets STATUS_ADMIN_BIN to its path, or to nothing when there is none.
STATUS_ADMIN_BIN=""
status_admin() {
  local candidate
  STATUS_ADMIN_BIN=""
  [ -n "$STATUS_CLIENT_BIN" ] || return 0
  candidate=$(dirname "$STATUS_CLIENT_BIN")/$HAMH_ADMIN_BIN
  if [ -x "$candidate" ]; then STATUS_ADMIN_BIN=$candidate; fi
}

# status_admin_advice: what to do about a missing operator tool. ./setup.sh --update builds it, but --update is refused on an account that holds a stage under the previous names (exit 7),
# where a new stage brings the tool.
status_admin_advice() {
  if [ "${LEGACY_STATE:-none}" != none ]; then
    printf 'a new stage brings it: ./setup.sh --teardown, then ./setup.sh --full'
  elif [ "${1:-status}" = doctor ]; then
    printf 'run ./setup.sh --update to build it in your account and install it'
  else
    printf './setup.sh --update builds it in your account and installs it'
  fi
}

# status_lease_build: the build the lease of the stage records, as "id<TAB>status<TAB>phase", or nothing.
status_lease_build() {
  local flat build
  [ "$RN_EXISTS" = 1 ] && [ -n "$RN_BUCKET" ] || return 0
  aws_run s3 cp "s3://$RN_BUCKET/lease/$STAGE.json" - || return 0
  flat=$(printf '%s' "$AWS_OUT" | json_flat)
  build=$(cl_flat_get "$flat" buildId)
  [ -n "$build" ] || return 0
  if aws_run codebuild batch-get-builds --ids "$build" --query 'builds[0].[buildStatus,currentPhase]' --output text; then
    printf '%s\t%s\n' "$build" "$AWS_OUT"
  else
    printf '%s\tunknown\t\n' "$build"
  fi
}

run_status() {
  local state code=0 lease build_id build_status phase retained failed_phase failed_detail last unfinished
  pf_region
  pf_profile
  pf_aws_cli
  pf_identity
  pf_stage
  st_init "$ACCOUNT" "$REGION" "$STAGE"
  st_load
  retained=$(st_get retained)
  last=$(st_get state)
  lease=$(status_lease_build)
  build_id=${lease%%"$TAB"*}
  if [ -n "$lease" ]; then
    build_status=$(printf '%s' "$lease" | cut -f2)
    phase=$(printf '%s' "$lease" | cut -f3)
  else
    build_status=""
    phase=""
  fi
  state=not-deployed
  case $STAGE_STATE in
    complete) state=deployed ;;
    partial) state=partial ;;
    deploying) state=deploying; code=6 ;;
    broken) state=broken; code=5 ;;
  esac
  if [ "$state" = not-deployed ] || [ "$state" = partial ]; then
    [ "$last" != failed ] || { state=failed; code=5; }
  fi
  [ "$state" != partial ] || code=5
  [ -z "$retained" ] || { [ "$state" != not-deployed ] || state=retained; code=8; }
  if [ "$state" = not-deployed ] && [ "$LEGACY_STATE" != none ]; then
    state=$LEGACY_STATE
    if [ "$state" = legacy ]; then code=7; else code=8; fi
  fi
  if [ "$build_status" = IN_PROGRESS ]; then
    state=deploying
    code=6
  fi
  say "STATUS stage=$STAGE state=$state account=$ACCOUNT region=$REGION"
  unfinished=""
  [ "$STAGE_UNFINISHED" != 1 ] || unfinished="; the deploy did not finish"
  case $STAGE_STATE in
    none)
      if [ "$LEGACY_STATE" != none ]; then say "  stage:    $(pf_legacy_head)"; else say "  stage:    no stack of the stage exists"; fi ;;
    deploying) say "  stage:    deploying ($(printf '%s\n' "$STAGE_STACKS" | awk -F'\t' 'NF == 2 { n++ } END { print n + 0 }') stacks; being changed: $STAGE_BAD)" ;;
    *) say "  stage:    $STAGE_STATE ($(printf '%s\n' "$STAGE_STACKS" | awk -F'\t' 'NF == 2 { n++ } END { print n + 0 }') stacks)${STAGE_BAD:+; not healthy: $STAGE_BAD}${STAGE_FAILED:+; made again by the next deploy: $STAGE_FAILED}${unfinished}" ;;
  esac
  [ -z "$STAGE_ENDPOINT" ] || say "  endpoint: $STAGE_ENDPOINT"
  if [ "$LEGACY_STATE" != none ]; then
    [ "$STAGE_STATE" = none ] || say "  previous: $(pf_legacy_head)"
    [ -z "$LEGACY_ENDPOINT" ] || say "  previous endpoint: $LEGACY_ENDPOINT"
  fi
  if [ -n "$build_id" ]; then
    say "  build:    $build_id ${build_status:-unknown}${phase:+ ($phase)}"
  fi
  if [ "$RN_EXISTS" = 1 ]; then
    say "  runner:   $(rn_standing_text)"
  else
    say "  runner:   not installed"
  fi
  status_client
  case $STATUS_CLIENT_STATE in
    missing) say "  client:   not installed on this machine" ;;
    unset) say "  client:   installed at $STATUS_CLIENT_BIN; its endpoint is not set" ;;
    matches) say "  client:   installed at $STATUS_CLIENT_BIN; its endpoint matches $STATUS_COMPARE_LABEL" ;;
    differs) say "  client:   installed at $STATUS_CLIENT_BIN; its endpoint points to another stage (${STATUS_CLIENT_ENDPOINT})" ;;
  esac
  status_admin
  if [ -n "$STATUS_ADMIN_BIN" ]; then
    say "  operator tool: installed at $STATUS_ADMIN_BIN"
  else
    say "  operator tool: not installed${STATUS_CLIENT_BIN:+ beside $STATUS_CLIENT_BIN} ($(status_admin_advice))"
  fi
  if [ -f "$ST_FILE" ]; then
    say "  last setup: $last${last:+, }commit $(st_get sourceCommit) (receipt $ST_FILE)"
    failed_phase=$(st_get failedPhase)
    failed_detail=$(st_get detail)
    if [ "$last" = failed ] && [ -n "$failed_phase" ]; then
      say "  failed in phase $failed_phase: $failed_detail"
    fi
  fi
  if [ -n "$retained" ]; then
    say "RETAINED $retained"
  fi
  exit "$code"
}

run_doctor() {
  local bad=0 bin out rc line
  pf_region
  pf_profile
  aws_cli_check
  case $AWS_CLI_STATE in
    ok) say "PASS aws-cli aws-cli/$AWS_CLI_VER" ;;
    unreadable) say "WARN aws-cli could not read the version of $AWS_CLI_PROG" ;;
    *) say "FAIL aws-cli $AWS_CLI_STATE: setup needs the AWS CLI, version 2 ($AWS_CLI_INSTALL_URL)"; exit 3 ;;
  esac
  if ! aws_run sts get-caller-identity --query '[Account,Arn]' --output text; then
    say "FAIL identity ${AWS_ERR:-no answer}"
    exit 3
  fi
  ACCOUNT=${AWS_OUT%%"$TAB"*}
  CALLER_ARN=${AWS_OUT#*"$TAB"}
  BUCKET=hamh-setup-$ACCOUNT-$REGION
  say "PASS identity $CALLER_ARN account $ACCOUNT"
  pf_stage
  case $STAGE_STATE in
    complete) say "PASS stage $STAGE is deployed ($(printf '%s\n' "$STAGE_STACKS" | awk -F'\t' 'NF == 2 { n++ } END { print n + 0 }') stacks complete)" ;;
    none)
      if [ "$LEGACY_STATE" != none ]; then
        say "FAIL stage $STAGE: $(pf_legacy_head)"
      else
        say "FAIL stage $STAGE is not deployed in account $ACCOUNT, Region $REGION: run ./setup.sh --full"
      fi
      bad=1 ;;
    partial)
      if [ "$STAGE_UNFINISHED" = 1 ]; then
        say "FAIL stage $STAGE is only partly deployed (all $(printf '%s\n' "$STAGE_STACKS" | awk -F'\t' 'NF == 2 { n++ } END { print n + 0 }') stacks exist, but the deploy did not finish): run ./setup.sh --full to finish it"
      else
        say "FAIL stage $STAGE is only partly deployed (no control stack${STAGE_FAILED:+; $STAGE_FAILED}): run ./setup.sh --full to finish it"
      fi
      bad=1 ;;
    deploying) say "FAIL stage $STAGE is being changed right now: $STAGE_BAD"; bad=1 ;;
    *) say "FAIL stage $STAGE has a stack that is not healthy: $STAGE_BAD"; bad=1 ;;
  esac
  if [ "$LEGACY_STATE" != none ] && [ "$STAGE_STATE" != none ]; then
    say "FAIL previous-stage $(pf_legacy_head)"
    bad=1
  fi
  status_client
  case $STATUS_CLIENT_STATE in
    missing) say "FAIL client is not installed on this machine"; bad=1 ;;
    unset) say "FAIL endpoint the client has no api_endpoint set"; bad=1 ;;
    matches) say "PASS endpoint the client points at $STATUS_COMPARE_LABEL ($STATUS_COMPARE_ENDPOINT)" ;;
    differs)
      if [ -z "$STATUS_COMPARE_ENDPOINT" ]; then
        say "FAIL endpoint the stage has no endpoint to compare with (client: $STATUS_CLIENT_ENDPOINT)"
      else
        say "FAIL endpoint the client points at $STATUS_CLIENT_ENDPOINT, $STATUS_COMPARE_LABEL is at $STATUS_COMPARE_ENDPOINT"
      fi
      bad=1 ;;
  esac
  status_admin
  if [ -n "$STATUS_ADMIN_BIN" ]; then
    if "$STATUS_ADMIN_BIN" --version >/dev/null 2>&1; then
      say "PASS operator-tool $STATUS_ADMIN_BIN runs"
    else
      say "FAIL operator-tool $STATUS_ADMIN_BIN did not run when asked for its version"
      bad=1
    fi
  elif [ -n "$STATUS_CLIENT_BIN" ]; then
    say "WARN operator-tool $HAMH_ADMIN_BIN is not installed beside $STATUS_CLIENT_BIN: $(status_admin_advice doctor)"
  fi
  if [ "$RN_EXISTS" = 1 ]; then
    say "WARN runner $(rn_standing_text)"
  fi
  bin=$STATUS_CLIENT_BIN
  if [ -n "$bin" ]; then
    out=$("$bin" doctor 2>&1)
    rc=$?
    while IFS= read -r line; do
      [ -z "$line" ] || say "      $line"
    done <<EOF
$out
EOF
    if [ "$rc" = 0 ]; then
      say "PASS client $HAMH_CLIENT_BIN doctor found no failing check"
    else
      say "FAIL client $HAMH_CLIENT_BIN doctor exited $rc"
      bad=1
    fi
  fi
  [ "$bad" = 0 ] || exit 3
  exit 0
}
