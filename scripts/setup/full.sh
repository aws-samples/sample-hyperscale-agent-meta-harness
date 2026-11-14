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
#   full_configured        removes the runner that is not kept, once the client's config names the stage (setup.sh calls it after the config step)
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

# full_local_python: the Python that scripts/deploy.sh will run (the first of the order of scripts/lib/python.sh that starts and is 3.8 or later), printed when it is 3.11 or later
# (the deploy engine reads TOML with tomllib); returns 1 with nothing printed otherwise.
full_local_python() {
  local c m
  local -a managed=()
  if [ -n "${UV_PYTHON_INSTALL_DIR:-}" ]; then
    for m in "$UV_PYTHON_INSTALL_DIR"/cpython-3.12*/bin/python3.12; do
      [ -x "$m" ] && managed+=("$m")
    done
  fi
  for c in "${HAMH_PYTHON:-}" ${managed[@]+"${managed[@]}"} python3.12 python3.11 "$HOME/.local/bin/python3.11" python3; do
    [ -n "$c" ] || continue
    command -v "$c" >/dev/null 2>&1 || continue
    "$c" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' >/dev/null 2>&1 || continue
    "$c" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' >/dev/null 2>&1 || return 1
    printf '%s' "$c"
    return 0
  done
  return 1
}

# full_local_check: the local route needs its programs, a Python the deploy engine can run and a Docker daemon that answers; each missing piece ends the run here, before any AWS call.
full_local_check() {
  local missing
  missing=$(full_local_missing)
  if [ -n "$missing" ]; then
    setup_exit 1 "--build-on local needs the toolchain on this machine, and these programs are missing: $missing" \
      "Without them use the runner in your AWS account, which needs nothing installed here:  ./setup.sh --${FULL_ACTION:-full}"
  fi
  if ! full_local_python >/dev/null; then
    setup_exit 1 "--build-on local runs the deploy engine with Python, and it needs Python 3.11 or later; scripts/deploy.sh would start an older one, or none" \
      "Install Python 3.11 or later (macOS: brew install python@3.12; Linux: your package manager), or set HAMH_PYTHON to one, then run setup.sh again." \
      "Without it use the runner in your AWS account:  ./setup.sh --${FULL_ACTION:-full}" "Nothing was created."
  fi
  if ! docker info >/dev/null 2>&1; then
    setup_exit 1 "--build-on local builds the Lambda functions and the images with Docker, and the Docker daemon does not answer (docker info failed)" \
      "Start Docker Desktop (macOS) or the docker service (Linux), check it with  docker info , then run setup.sh again." \
      "Without it use the runner in your AWS account:  ./setup.sh --${FULL_ACTION:-full}" "Nothing was created."
  fi
}

# full_run_names: one line of the plan that says whether this Region gets run names. A small model (the default row of crates/ham-core/assets/utility-models.json, Claude Haiku 5.5)
# writes the name and the tags of every run, and its row is available in one Region. Nothing is printed when the clone has no catalog or the catalog does not read.
# The line follows the value of HAMH_UTILITY_MODEL that applies (A4.14): the one you exported, else the one the stage's record keeps, else the default; for an update of a
# stage without a record the deploy keeps what the live stage has, and the line says that. An exported setting reaches the deploy on both routes, so the line names the
# export alone as the way to turn names off.
full_run_names() {
  local file=$SELF_DIR/crates/ham-core/assets/utility-models.json flat def alias n=0 row="" name region label="run names:" value="" from="" note=""
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
  value=$(pf_explicit_get HAMH_UTILITY_MODEL)
  if [ -n "$value" ]; then
    from="exported in this shell"
  elif [ "${REC_HAVE:-0}" = 1 ]; then
    value=$(pf_rec_get set.HAMH_UTILITY_MODEL)
    from="kept: the stage's record"
  elif [ "${FULL_ACTION:-}" = update ]; then
    say "  $label     kept as the live stage has them: the stage has no record of its settings yet, so the deploy reads HAMH_UTILITY_MODEL from the live stage and keeps it. You export HAMH_UTILITY_MODEL=off before the update to turn run names and tags off, or HAMH_UTILITY_MODEL=default for the default of this Region."
    return 0
  fi
  case $value in
    off)
      say "  $label     off: HAMH_UTILITY_MODEL=off ($from). No model writes run names and tags, and each run shows a short label cut from the prompt. You export HAMH_UTILITY_MODEL=default before setup to return to the default of this Region."
      return 0 ;;
    ''|default) ;;
    *) note=" HAMH_UTILITY_MODEL=$value ($from)." ;;
  esac
  if [ "$region" = "$REGION" ]; then
    say "  run names:     on: $name writes a short name and tags for each run in this Region. The name appears in your run list.$note You export HAMH_UTILITY_MODEL=off before setup to turn names and tags off."
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

# full_minutes <n>: "1 minute" or "<n> minutes".
full_minutes() {
  if [ "$1" = 1 ]; then printf '1 minute'; else printf '%s minutes' "$1"; fi
}

# full_first_runs: the names of the first 20 running runs, separated by spaces, and how many more there are.
full_first_runs() {
  local _name n=0 shown=""
  for _name in ${ACTIVE_RUNS:-}; do
    n=$((n + 1))
    [ "$n" -gt 20 ] || shown=${shown:+$shown }$_name
  done
  [ "$n" -le 20 ] || shown="$shown (+$((n - 20)) more)"
  printf '%s' "$shown"
}

# full_client_lines <action>: where the client is installed, and, for an update, what the client's settings file points at now and after the update (the update writes the endpoint
# and the Region of the stage it updates, so updating another stage repoints the client).
full_client_lines() {
  local action=$1 parts=""
  say "  install to:    $DEST${CLIENT_DEST_FROM:+ ($CLIENT_DEST_FROM)}"
  [ -z "${CLIENT_SHADOW:-}" ] || say "                 another $HAMH_CLIENT_BIN, $CLIENT_SHADOW, starts first on your PATH; the warning above says what to do"
  [ "$action" = update ] || return 0
  if [ -n "${STAGE_ENDPOINT:-}" ] && [ "${CFG_ENDPOINT:-}" != "$STAGE_ENDPOINT" ]; then
    parts="api_endpoint ${CFG_ENDPOINT:-none} -> $STAGE_ENDPOINT"
  fi
  if [ "${CFG_REGION:-}" != "$REGION" ]; then
    parts="${parts:+$parts, }region ${CFG_REGION:-none} -> $REGION"
  fi
  if [ -n "$parts" ]; then
    say "  client config: $parts in $CLIENT_CONFIG (written after the update; the client then talks to this stage)"
  else
    say "  client config: api_endpoint and region stay as they are in $CLIENT_CONFIG"
  fi
}

# full_class_note <change class>: what a value of a setting of that class can cost, for the plan line of an exported setting whose change the stage's record cannot show
# (a stage without a record, or a record with no line for the setting: the deploy reads the stage's value, the plan cannot). Nothing for a class whose change costs nothing.
full_class_note() {
  case $1 in
    replace) printf '%s' "; a value other than the stage has now replaces what the setting describes" ;;
    decrease) printf '%s' "; a lower number than the stage has now deletes what is older, and that cannot be undone" ;;
    remove) printf '%s' "; an off or empty value removes what the setting holds" ;;
  esac
}

# full_held_cost <class>: what taking a new default of that class does, in the words of deploy.py's HELD_COST.
full_held_cost() {
  case $1 in
    replace) printf '%s' "replaces the machines of the Instances part" ;;
    decrease) printf '%s' "deletes what is older than the new value" ;;
    remove) printf '%s' "removes what the setting holds now" ;;
  esac
}

# full_held_sentence <name> <kept value> <default> <class>: the sentence of a setting whose new default the deploy does not take, the words deploy.py's held_sentence prints at
# the end of its DEPLOY-SETTING line for it.
full_held_sentence() {
  printf "the stage keeps %s; this release's default is %s, and taking it %s: export %s=default to take it" "$2" "${3:-none}" "$(full_held_cost "$4")" "$1"
}

# full_takes_sentence <name> <value given> <class>, the sentence of a setting that the deploy builds as an update of the stage that did not finish was given it
# (pf_default_holds, PF_TAKES), with what building it does.
full_takes_sentence() {
  printf '%s' "an update of the stage that did not finish was given $1=$2, and this update carries it on; it $(full_held_cost "$3")"
}

# The stage's record is of an update that did not finish, written by a release that does not record the settings that update was given (no explicit= line). For a setting it
# has no line for, the deploy cannot tell whether you asked for this release's default, so it keeps nothing by itself and refuses for your consent before any stack change
# (deploy.py legacy_deploying). The plan knows that from its reads (PF_ASKS), so the run ends here with exit 7, nothing created, naming the exports in the words of the
# deploy's refusal (consent_exports).
full_default_asks() {
  local _name _live _def _class _names="" _changes="" _exports=""
  while IFS='|' read -r _name _live _def _class; do
    [ -n "$_name" ] || continue
    _names="${_names:+$_names, }$_name"
    _changes="${_changes:+$_changes; }$_name goes from $_live to ${_def:-none} ($_class), and taking it $(full_held_cost "$_class")"
    _exports="${_exports:+$_exports; }export $_name=$_live to keep what the stage has now, or $_name=default for ${_def:-none}"
  done <<EOF
${PF_ASKS:-}
EOF
  setup_exit 7 "the update cannot tell whether you asked for this release's default of $_names: the stage's settings record is of an update that did not finish, written by a release that does not record the settings that update was given" \
    "$_changes. The deploy asks for your consent to that, so the update stops here, before anything is installed." \
    "$_exports, then run setup.sh again." "Nothing was changed."
}

# full_settings_lines <action>: one line for each stage setting that the deploy gets from this run (exported) or keeps (the stage's record), the Instances part of the stage, and,
# for an update of a stage without a record, that the deploy reads the settings from the live stage and keeps them. An exported setting whose change the record shows is printed as
# old -> new with its change class; one whose change the record cannot show carries the sentence of its class (full_class_note).
full_settings_lines() {
  local action=$1 _name value rec class given shown=0
  [ "$REG_HAVE" = 1 ] || return 0
  for _name in $(pf_reg_names); do
    value=$(pf_explicit_get "$_name")
    rec=$(pf_rec_get "set.$_name")
    if [ -n "$value" ]; then
      class=$(pf_reg_get "$_name" change)
      if [ -n "$rec" ] && [ "$rec" != "$value" ]; then
        say "  setting:       $_name: $rec -> $value (explicit: exported in this shell; the stage's record had $rec; change class $class)"
      elif [ -n "$rec" ]; then
        say "  setting:       $_name=$value (explicit: exported in this shell)"
      else
        say "  setting:       $_name=$value (explicit: exported in this shell$(full_class_note "$class"))"
      fi
      shown=1
    elif [ -n "$rec" ] && [ "$(pf_reg_get "$_name" kept)" = yes ]; then
      # the value of an update that did not finish that replaces or deletes something has its own line below (PF_TAKES)
      case "$NL${PF_TAKES:-}" in
        *"$NL$_name|"*) continue ;;
      esac
      say "  setting:       $_name=$rec (kept: the stage's record)"
      shown=1
    fi
  done
  # A new default of this commit that the deploy will not take, because the stage runs another value and taking the default needs consent (pf_default_holds)
  while IFS='|' read -r _name value rec class; do
    [ -n "$_name" ] || continue
    say "  setting:       $_name=$value ($(full_held_sentence "$_name" "$value" "$rec" "$class"))"
    shown=1
  done <<EOF
${PF_HOLDS:-}
EOF
  # What the deploy builds because an update of the stage that did not finish was given it, when that replaces or deletes something (pf_default_holds)
  while IFS='|' read -r _name value rec class given; do
    [ -n "$_name" ] || continue
    say "  setting:       $_name: $value -> ${rec:-none} ($(full_takes_sentence "$_name" "$given" "$class"))"
    shown=1
  done <<EOF
${PF_TAKES:-}
EOF
  if [ -n "${PF_HOLD_NAMES:-}" ] && [ "${PF_REC_DEPLOYING:-0}" = 1 ]; then
    say "  setting:       $PF_HOLD_NAMES: the plan could not read what the stage runs (${PF_HOLD_WHY:-no answer}); the stage's record is of an update that did not finish; the deploy reads the values and says on its DEPLOY-SETTING lines what it carries on, keeps or asks your consent for"
    shown=1
  elif [ -n "${PF_HOLD_NAMES:-}" ]; then
    say "  setting:       $PF_HOLD_NAMES: the plan could not read what the stage runs (${PF_HOLD_WHY:-no answer}); the deploy reads it, keeps the stage's value where this release's default would need your consent, and says so"
    shown=1
  fi
  if [ "$action" = update ] && [ "$REC_HAVE" != 1 ]; then
    say "  settings:      read from the live stage by the deploy and kept (the stage has no record of its settings yet); a setting you export replaces its value"
  elif [ "$shown" = 0 ] && [ "$action" = update ]; then
    say "  settings:      the defaults; the stage's record names no setting of yours"
  fi
}

# full_zone_words "<name> <id>, ...": the same zones as "<name> (<id>), ..." for the plan.
full_zone_words() {
  printf '%s' "$1" | sed 's/\([^ ,][^ ,]*\) \([^ ,][^ ,]*\)/\1 (\2)/g'
}

full_plan() {
  local action=$1 how zone_note
  step "What setup will do"
  say "  account:       $ACCOUNT ($CALLER_ARN)"
  if [ -n "${OP_CHANGE:-}" ]; then
    say "  operator:      $OP_CHANGE (--change-operator): from now on the stage trusts $ACCT_OP_KIND $ACCT_OP_NAME for its admin routes and its deploys, and no longer the earlier operator"
  elif [ -n "${ACCT_OP_KIND:-}" ] && [ "$action" = update ] && [ -z "${OP_FROM:-}" ]; then
    # Addendum 4, A4.6: neither a record nor the deployed stage names the operator, so the caller becomes it, and the plan says so
    say "  operator:      $ACCT_OP_KIND $ACCT_OP_NAME (your credentials: ${PF_OP_WHY:-the stage does not say which operator it was deployed with}, so from now on the stage trusts this identity for its admin routes and its deploys)"
  elif [ -n "${ACCT_OP_KIND:-}" ] && [ "$action" = update ] && [ "$OP_FROM" = live ]; then
    say "  operator:      $ACCT_OP_KIND $ACCT_OP_NAME (kept: the operator the stage's control function trusts, which is the identity of your credentials)"
  elif [ -n "${ACCT_OP_KIND:-}" ]; then
    say "  operator:      $ACCT_OP_KIND $ACCT_OP_NAME: the stage trusts exactly this identity for its admin routes and its deploys; run setup as another identity and the stage trusts that one instead"
  fi
  say "  Region:        $REGION"
  if [ "${ZONE_LIVE:-0}" = 1 ]; then
    # (no apostrophe inside a ${...:-...} word: bash reads it as the start of a quote there)
    zone_note=${ZONE_NOTE:-}
    [ -n "$zone_note" ] || zone_note="kept from the stage's network"
    say "  zone:          $(full_zone_words "$ACCT_DIRECT_AZ"), $zone_note"
  else
    say "  zone:          $ACCT_DIRECT_AZ${ZONE_FROM:+ ($ZONE_FROM)}"
  fi
  say "  stage:         $STAGE"
  if [ -n "${ACCT_REPO_NAMES:-}" ]; then
    say "  repository names: ${ACCT_REPO_NAMES//,/, }. A run may open a pull request only in a CodeCommit repository with a matching name. The stage operator must also allow that repository."
  fi
  if [ -n "${REVISION:-}" ] && [ "$REVISION" != "${SRC_HEAD:-}" ]; then
    say "  source:        commit $SRC_COMMIT (--revision; the git archive of that commit)"
  else
    say "  source:        commit $SRC_COMMIT (clean clone)"
  fi
  if [ -n "${COMMIT_CHANGE:-}" ] && [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
    say "  commit:        $COMMIT_CHANGE (the commit the stage records, then this one; both have the source tree the stage runs, so nothing is deployed)"
  elif [ -n "${COMMIT_CHANGE:-}" ]; then
    say "  commit:        $COMMIT_CHANGE (the commit the stage records, then the one this update deploys)"
  fi
  [ -z "${COMMIT_ORDER_NOTE:-}" ] || say "  order:         $COMMIT_ORDER_NOTE"
  if [ -n "${RN_LABEL_NEXT:-}" ] && [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
    # A release that changes no file the stage runs still has a version of its own; the update writes the label the backend reports, and nothing else on the stage
    if [ "${RN_LABEL_KEEP:-0}" = 1 ]; then
      say "  label:         the backend reports v$RN_LABEL_NOW and keeps it (this clone's VERSION is $RN_LABEL_NEXT, which is older)"
    else
      say "  label:         the backend reports v$RN_LABEL_NOW and will report v$RN_LABEL_NEXT (this release changes no file the stage runs, so nothing is deployed and only its version label is written)"
    fi
  fi
  full_settings_lines "$action"
  [ "${INSTANCES_PRESENT:-0}" != 1 ] || say "  Instances part: present, kept (the capacity provider and the second worker runtime stay; the deploy keeps the part from the stage's record or the live stage)"
  full_client_lines "$action"
  if [ "${BUILD_ON:-runner}" = local ]; then
    say "  build:         on this machine, with scripts/deploy.sh (the account file below is passed to it)"
    say "  account file:  generated from your account id, Region, zone ids and the identity of your credentials, kept outside the clone ($ACCT_KEY)"
    say "  client:        compiled on this machine with cargo and installed, with the operator tool beside it"
  elif [ "${FULL_REUSE:-0}" = 1 ]; then
    say "  build:         none: the stage already runs this commit, and the client of an earlier update of it is kept on this machine"
    say "  client:        the copy downloaded and checked earlier ($ST_DIR/download/verified, sha256 $CL_SHA), checked again and installed; no runner is installed"
  else
    say "  build:         the runner in your AWS account (a CodeBuild project), from the git archive of that commit"
    say "  account file:  $ACCT_KEY, generated from your account id, Region, zone ids and the identity of your credentials and nothing else; uploaded as its own object"
    say "  client:        built in your account for $CLIENT_PLATFORM with the operator tool $HAMH_ADMIN_BIN beside it, downloaded from your bucket, checked against its SHA-256 and installed"
  fi
  full_run_names
  case $RN_MODE in
    attach) say "  note:          a build of this commit is already running ($RN_BUILD_ID); setup attaches to it instead of starting another" ;;
    finish) say "  note:          a build of this commit already succeeded ($RN_BUILD_ID); setup reads its result instead of building again" ;;
    stale) [ -z "${RN_LOCK_STALE:-}" ] || say "  deploy lock:   stale, replaced after the question: $RN_LOCK_STALE" ;;
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
    if [ "${FULL_REUSE:-0}" = 1 ]; then
      how="nothing on the stage changes; no runner is installed"
    elif [ "${SETUP_CLIENT_ONLY:-0}" = 1 ] && [ "${BUILD_ON:-runner}" = local ]; then
      how="nothing is deployed; only the client is built here, and no runner is installed"
    elif [ "${SETUP_CLIENT_ONLY:-0}" = 1 ] && [ "${KEEP_RUNNER:-0}" = 1 ]; then
      how="nothing is deployed; the runner is installed again, builds only the client and is kept (--keep-runner)"
    elif [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
      how="nothing is deployed; the runner is installed again, builds only the client and is removed afterwards"
    elif [ "${BUILD_ON:-runner}" = local ]; then
      how="the stage keeps its resources and gets the new revision; no runner is installed"
    elif [ "${KEEP_RUNNER:-0}" = 1 ]; then
      how="the stage keeps its resources and gets the new revision; the runner is installed again for the build and kept (--keep-runner)"
    else
      how="the stage keeps its resources and gets the new revision; the runner is installed again for the build and removed afterwards"
    fi
    say "  update:        $how"
    # An update that deploys nothing builds no image and runs no deploy, so its plan names neither the image nor a deploy's duration.
    [ "${SETUP_CLIENT_ONLY:-0}" = 1 ] || case ${IMG_PLAN:-unknown} in
      none|image)
        say "  image:         $IMG_TEXT"
        say "  duration:      about 20 to 25 minutes" ;;
      fork)
        say "  image:         $IMG_TEXT"
        say "  duration:      about 41 to 52 minutes (about 21 to 27 of them to rebuild the Cline binary)" ;;
      *)
        # A4.14: without a record the stage names no commit, so the pins of the two commits cannot be compared and the order of the commits is not known
        if [ "${REC_HAVE:-0}" != 1 ]; then
          say "  image:         not compared: the stage has no record of the commit it runs (a release before the record deployed it), so setup cannot tell whether this update needs a new worker image or a rebuild of the Cline binary, or whether this commit is newer than the one the stage runs"
        elif [ "${SETUP_CLIENT_ONLY:-0}" != 1 ] && [ -n "${IMG_TEXT:-}" ]; then
          # The recorded commit is not in this clone, so its pins cannot be read (the VERSION rule of pf_commit_order let the update go ahead)
          say "  image:         $IMG_TEXT"
        fi
        say "  duration:      about 20 to 25 minutes with a reused Cline binary, plus about 21 to 27 minutes when the binary must be rebuilt" ;;
    esac
    [ "${FORCE_DEPLOY:-0}" != 1 ] || say "  deploy:        --force-deploy: the deploy runs even when the stage already runs this commit"
    if [ "${ALLOW_ACTIVE_RUNS:-0}" = 1 ]; then
      say "  guard:         active runs are not checked before the build; the build refuses only when the worker runtime changes"
      if [ "${ACTIVE_RUN_COUNT:-0}" -gt 0 ]; then
        say "  active runs:   $ACTIVE_RUN_COUNT running run(s): $(full_first_runs). They keep running (--allow-active-runs), and the Lambda functions and the run state machine change under them; their worker runtime stays."
      fi
    elif [ -n "${WAIT_FOR_RUNS:-}" ] && [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
      say "  guard:         this update changes nothing on the stage, so it waits for no run (--wait-for-runs) and the stage keeps starting runs."
    elif [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
      say "  guard:         this update deploys nothing, so active runs do not stop it and the stage keeps starting runs."
    elif [ -n "${WAIT_FOR_RUNS:-}" ]; then
      say "  guard:         the update takes the stage's deploy lock, so the stage starts no new runs, and waits up to $(full_minutes "$WAIT_FOR_RUNS") for the active runs to end (--wait-for-runs). No run is cancelled."
      say "                 The build also refuses worker runtime changes while runs are active."
      if [ "${ACTIVE_RUN_COUNT:-0}" -gt 0 ]; then
        say "  active runs:   $ACTIVE_RUN_COUNT running run(s): $(full_first_runs). The update waits up to $(full_minutes "$WAIT_FOR_RUNS") for them to end; new runs are refused meanwhile. No run is cancelled."
      fi
    else
      say "  guard:         Active runs stop ./setup.sh --update with exit 7 before anything is installed or built."
      say "                 The build also refuses worker runtime changes while runs are active."
    fi
  fi
}

full_would() {
  local action=$1
  [ "${BUILD_ON:-runner}" != local ] || {
    would "take the deploy lock of stage $STAGE for the deploy and release it when the deploy has ended"
    would "run scripts/deploy.sh --stage $STAGE --azs $AZS --fork-source build here, with the generated account file"
    would "compile the client and the operator tool here and install them, then configure the client"
    return 0
  }
  if [ "${FULL_REUSE:-0}" = 1 ]; then
    would "install the client and the operator tool downloaded and checked earlier for this commit ($ST_DIR/download/verified), with no runner and no build"
    would "write the client settings, run $HAMH_CLIENT_BIN models, rules init and doctor"
    return 0
  fi
  would "install the runner stack $SETUP_RUNNER_STACK from $SETUP_RUNNER_TEMPLATE"
  would "upload $SRC_KEY and $ACCT_KEY to the runner bucket $BUCKET"
  [ -n "$BOOT_VERSION" ] || would "have the runner print the CDK bootstrap template (a first build of a few minutes), check it against $SETUP_BOOTSTRAP_PIN and create the stack $SETUP_BOOTSTRAP_STACK from it with your credentials"
  would "take the deploy lock of stage $STAGE, start the build and follow it"
  would "download the client and the operator tool for $CLIENT_PLATFORM, check their SHA-256 and install them"
  [ "${KEEP_RUNNER:-0}" = 1 ] || would "empty the bucket and remove the runner stack"
  would "write the client settings, run $HAMH_CLIENT_BIN models, rules init and doctor"
  [ "$action" = full ] || would "start from the stage that exists now"
}

# full_confirm <prompt> <costs|yes>: the one question. Returns 0 to go ahead; every other answer ends the run with exit 4 and nothing created.
full_confirm() {
  local prompt=$1 need=$2 answer flags="--yes --acknowledge-costs"
  # an update needs only --yes; the full setup needs the acknowledgement of the costs too
  [ "$need" = costs ] || flags="--yes"
  if [ "$YES" = 1 ]; then
    if [ "$need" = costs ] && [ "${ACK_COSTS:-0}" != 1 ]; then
      setup_exit 4 "--yes does not accept the costs of a full setup" \
        "Add --acknowledge-costs to go ahead without a question:  ./setup.sh --full --yes --acknowledge-costs" "Nothing was created."
    fi
    return 0
  fi
  if [ "$NO_INPUT" = 1 ]; then
    setup_exit 4 "--no-input was given, so there is no one to ask" "Add $flags to go ahead, or --dry-run to preview." "Nothing was created."
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
  setup_exit 4 "there is no terminal to ask on" "Add $flags to go ahead without a prompt, or --dry-run to preview." "Nothing was created."
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
  # where the client is installed: the next update replaces the client there (C10)
  st_set bindir "$BINDIR" || true
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
  if [ "${FULL_REUSE:-0}" = 1 ]; then
    full_reuse_flow
    return 0
  fi
  case $RN_MODE in
    new|stale)
      rn_lease_take
      # The stage starts no new runs while this lock is held, so the look for active runs that decides comes now. A run that attached to the build of another
      # setup meanwhile makes none: that setup made it.
      case $RN_MODE in new|stale) full_runs_gate ;; esac
      rn_install
      rn_upload
      rn_bootstrap
      if [ "$RN_MODE" = new ] || [ "$RN_MODE" = stale ]; then
        rn_start
        rn_follow
      elif [ "$RN_MODE" = attach ]; then
        say_log "Another setup started the same build meanwhile; attaching to $RN_BUILD_ID"
        rn_lock_join
        rn_follow
      else
        # The build has succeeded, so this run's first write of the lock it shares already says deploys=no
        RN_LOCK_DEPLOYS=no
        rn_lock_join
        RN_FINAL=SUCCEEDED
      fi ;;
    attach)
      say_log "Attaching to the running build $RN_BUILD_ID"
      rn_lock_join
      st_set buildId "$RN_BUILD_ID" || true
      st_set state started || true
      st_save || true
      rn_follow ;;
    finish)
      say_log "The build $RN_BUILD_ID already succeeded; reading its result"
      # A run that finishes from a build that succeeded never closes the stage again
      RN_LOCK_DEPLOYS=no
      rn_lock_join
      RN_FINAL=SUCCEEDED ;;
  esac
  # CodeBuild says the build has succeeded: this run still reads and checks the result, downloads and installs the client and removes the runner, and keeps the lock until
  # then (A4.4). Every step that changes the stage ran inside the build, so from this write on the lock says deploys=no and the stage starts runs again, whatever
  # becomes of this run: a read that fails after it (rn_read_failed) leaves a lock that no longer closes the stage.
  [ "$RN_FINAL" != SUCCEEDED ] || rn_lock_open
  rn_settle
  endpoint=$(rn_res endpoint)
  if ! valid_stage_endpoint "$endpoint"; then
    rn_lease_release
    setup_exit 5 "the build reported an endpoint that is not the https URL of an API Gateway stage: ${endpoint:-none}" "Nothing was installed."
  fi
  # The client is pointed at the endpoint, so the stack of this account, which made it, has the last word: the result file only repeats it.
  pf_control_endpoint
  [ -n "$CTRL_ENDPOINT" ] || [ "$AWS_RC" = 0 ] || aws_missing || rn_read_failed "the endpoint of the stage (output V1ApiUrl of the stack hamh-$STAGE-control)"
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
  # The lock stays until the client is downloaded, checked and installed and the runner is removed (full_runner_end): a run that is interrupted in between finds the finished
  # build and goes on from here, and no other setup rewrites or removes the runner meanwhile.
  if ! cl_fetch "$RES_FLAT" "$RN_BUCKET" "$RN_BUILD_ID" "$CLIENT_PLATFORM"; then
    [ "$AWS_RC" = 0 ] || aws_missing || rn_read_failed "the client files of build $RN_BUILD_ID from s3://$RN_BUCKET"
    rn_lease_release
    st_set state failed || true
    st_set failedPhase client || true
    st_save || true
    err "the client from your bucket was not installed."
    detail "The backend is deployed ($ENDPOINT). $(rn_runner_now_text) Run  ./setup.sh --update  to build the client again in your account (the stage already runs this commit, so the update builds only the client), or install a client you have with  setup.sh --client-only --from DIR."
    exit 5
  fi
  # the lease of an earlier release lies in the bucket that goes with the runner: it is given back here; the deploy lock is renewed and kept
  if [ "$RN_LOCK_LEGACY" = 1 ]; then rn_lease_release; else rn_lock_renew; fi
  ok "client for $CLIENT_PLATFORM downloaded and checked: sha256 $CL_SHA"
  ok "operator tool for $CLIENT_PLATFORM downloaded and checked: sha256 $CL_ADMIN_SHA"
  st_set client "$CLIENT_PLATFORM" || true
  # the copy in download/verified is of this commit and this account file: a later update of the same commit installs it without a build (full_update_reuse_check)
  st_set clientCommit "$SRC_COMMIT" || true
  st_set clientConfig "$ACCT_DIGEST" || true
  st_save || true
  FROM=$CL_DIR
  if [ "${KEEP_RUNNER:-0}" = 1 ]; then
    FULL_MODE_NOTE="the runner stack $SETUP_RUNNER_STACK and its bucket $RN_BUCKET are kept (--keep-runner)"
  else
    # The checked client is nowhere but in the bucket that is emptied with the runner, and in the download folder of the receipt. The runner goes after the client is
    # installed and its config names the stage (full_configured), so a setup that is stopped in between still has a configured client.
    FULL_REMOVE_RUNNER=1
  fi
}

# full_reuse_flow: the runner route of an update that installs the checked client of an earlier update of this commit (FULL_REUSE, full_update_reuse_check): no runner is
# installed and no build runs; the endpoint comes from the stage's control stack. A runner that an earlier setup left goes at the end, under the deploy lock (full_runner_end).
full_reuse_flow() {
  pf_control_endpoint
  if [ -z "$CTRL_ENDPOINT" ] && [ "$AWS_RC" != 0 ] && ! aws_missing; then
    setup_exit 6 "could not read the endpoint of the stage (output V1ApiUrl of the stack hamh-$STAGE-control): ${AWS_ERR:-no answer}" \
      "Nothing was installed. Run the same command again."
  fi
  if ! valid_stage_endpoint "$CTRL_ENDPOINT"; then
    setup_exit 5 "the stack hamh-$STAGE-control has no V1ApiUrl output that is the https URL of an API Gateway stage: ${CTRL_ENDPOINT:-none}" "Nothing was installed."
  fi
  ENDPOINT=$CTRL_ENDPOINT
  st_set state built || true
  st_set endpoint "$ENDPOINT" || true
  st_save || true
  ok "the stage already runs this commit: $ENDPOINT"
  ok "client for $CLIENT_PLATFORM: the copy downloaded and checked earlier for this commit, sha256 $CL_SHA"
  # a runner that an earlier setup left behind goes once the client is configured
  if [ "$RN_EXISTS" = 1 ] && [ "${KEEP_RUNNER:-0}" != 1 ]; then FULL_REMOVE_RUNNER=1; fi
  return 0
}

# full_runner_end: the last step of a run on the runner route (A4.4). The runner that is not kept goes, and only then the deploy lock is given back, so that no other setup
# installs, rewrites or removes the runner while this run still uses it. A runner that an earlier setup left (this run has no build of its own: `--full` on a deployed stage,
# an update that installs the client it downloaded earlier) is removed under the lock too: the lock is taken first, and when another setup holds it the runner stays.
# A run that shares its lock with another setup of the same build (rn_lock_join) leaves the runner and the lock to that setup while it is still at work.
# Before its guard reads the lock, a run that followed a build waits twice the settle pause (rn_lock_settle), so that a setup of the same build whose entry a write
# at the same moment dropped has written itself in again; a lock write that fails is RETAINED (exit 8) like a failed look of the guard; and a run that left the lock to
# another setup reads it once more after the release, because that setup may have ended at the same moment and left it back (rn_lock_left_back): then this run is the
# last one under the lock and removes the runner.
full_runner_end() {
  local wanted=0
  if [ "${FULL_REMOVE_RUNNER:-0}" = 1 ]; then
    FULL_REMOVE_RUNNER=0
    wanted=1
    [ -z "${RN_BUILD_ID:-}" ] || rn_lock_settle 2
    if rn_lease_take_soft; then
      rn_lock_renew
      rn_remove || true
    elif [ "$RN_BLOCKED_FAILED" = 1 ]; then
      rn_retain_unchecked
    else
      warn "the runner stack $SETUP_RUNNER_STACK is not removed: $RN_BLOCKED_WHY. The setup that uses the runner removes it when it ends; ./setup.sh --full removes a runner that stays on a deployed stage."
      log_line "runner not removed: $RN_BLOCKED_WHY"
    fi
  fi
  RN_LOCK_LEFT=0
  rn_lease_release
  if [ "$wanted" = 1 ] && [ "$RN_LOCK_LEFT" = 1 ] && [ "$RN_EXISTS" = 1 ] && rn_lock_left_back; then
    log_line "the setup this run left the deploy lock to ended at the same moment and left it back: this run is the last one under it"
    RN_LOCK_HELD=1
    rn_remove || true
    rn_lease_release
  fi
}

# full_runner_cleanup: a run whose build succeeded and that ends before full_configured (a client that does not run here, a settings file the client refuses, a failed step of
# setup.sh) removes the runner it still holds, or names it RETAINED, and gives the deploy lock back; setup.sh calls it on those exits (die, client_tried). It is not an exit
# trap: the progress line of the runner's removal runs in the background, and a background job started while the shell exits outlives it.
full_runner_cleanup() { full_runner_end; }

# ---- the look for active runs that decides, and the wait for them ------------------------------------------------------------

# full_runs_gate: the look for the runs of every user that decides whether an update deploys. It is made after this run has taken the stage's deploy lock (rn_lease_take), on
# both routes. From the moment the lock exists the stage's API service starts no new runs (it reads the lock right before it creates a run and answers 409 stage-updating;
# crates/control-api/src/admission.rs), so a run that is not there at this look cannot start before the deploy. The early look of the preflight (pf_active_runs_check) stays:
# it stops a plain update in seconds, before the question.
#   1. A pause (HAMH_SETUP_RUN_SETTLE_SECONDS, 45 by default; the tests set 0): a start that read "no lock" a moment before the lock was created ends inside the time limit
#      of the API function (28 seconds, infra/lib/stacks/control.ts; a test holds the pause above it), and the listing of executions can lag behind a start.
#   2. A look every HAMH_SETUP_RUN_POLL_SECONDS (30 by default; the tests set 0). The update goes on after two looks in a row that answer and show no run, and after the
#      lock was read back with this run's token (full_gate_own).
# Without --wait-for-runs a run ends the update: the lock is given back and the run ends with exit 7, the stage unchanged and no runner installed. With it (WAIT_FOR_RUNS,
# minutes) this run waits: one line for each look that shows runs, with their number and the minutes left; the lock written again every HAMH_SETUP_LOCK_REFRESH_SECONDS
# (full_gate_keep), so that the API service keeps the stage closed; and at the limit the lock is given back and the run ends with exit 7: nothing was deployed or installed,
# and no run was cancelled. Setup never cancels a run. Ctrl-C during the looks, or a terminal that is closed, gives the lock back and ends with exit 4 (full_gate_interrupted).
# A look that fails is never taken as "no run": it is said and made again. Three in a row end a plain update with exit 6; with the option they are made again until the limit.
# The looks hold only while the lock closes the stage: a lock this run could not write for 30 minutes ends the update with exit 6 (full_gate_closed), and before the update
# goes on a renewal that is due is made (three tries) and the lock is read back (full_gate_own).
# Every end here gives the lock back first and then says what became of it: `@lock@` in the lines for full_gate_stop is the place of that sentence (rn_release_said), and
# "released" is said only for a lock that is gone. A lock that could not be given back is named, with what the stage does meanwhile and how the lock goes.
# One case keeps the rule of the early look: credentials that may not list executions (pf_active_runs_look says denied) cannot look at all, so the update goes on, and the
# build's own deploy guard looks again with the runner's role before it changes the worker runtime.
# Not made for a full setup, for an update that deploys nothing (SETUP_CLIENT_ONLY), or with --allow-active-runs. HAMH_SETUP_RUN_MINUTE_SECONDS is the length of a minute of
# the limit (60; a test shortens it).
full_runs_gate() {
  local settle poll minute limit=0 started=$SECONDS left=0 empty=0 failed=0 waited=0 pause tries=0
  [ "${FULL_ACTION:-full}" = update ] || return 0
  [ "${SETUP_CLIENT_ONLY:-0}" != 1 ] && [ "${ALLOW_ACTIVE_RUNS:-0}" != 1 ] || return 0
  settle=$(setup_number "${HAMH_SETUP_RUN_SETTLE_SECONDS:-45}" 45)
  poll=$(setup_number "${HAMH_SETUP_RUN_POLL_SECONDS:-30}" 30)
  minute=$(setup_number "${HAMH_SETUP_RUN_MINUTE_SECONDS:-60}" 60)
  [ "$minute" -ge 1 ] || minute=1
  [ -z "${WAIT_FOR_RUNS:-}" ] || limit=$((WAIT_FOR_RUNS * minute))
  trap full_gate_interrupted INT TERM HUP
  say_log "The stage starts no new runs while this update holds its deploy lock. Looking for runs that are still active ..."
  [ "$settle" -eq 0 ] || sleep "$settle"
  while :; do
    pf_active_runs_look
    case $ACTIVE_RUNS_LOOK in
      denied)
        [ -z "${WAIT_FOR_RUNS:-}" ] ||
          warn "--wait-for-runs cannot wait: these credentials may not list the running executions of hamh-$STAGE-run. The update continues. The build checks again before it changes the worker runtime."
        log_line "the running executions cannot be listed with these credentials ($ACTIVE_RUNS_ERR); the update goes on"
        break ;;
      failed)
        empty=0
        failed=$((failed + 1))
        if [ -z "${WAIT_FOR_RUNS:-}" ] && [ "$failed" -ge 3 ]; then
          full_gate_stop 6 "could not look for the active runs of stage $STAGE: $ACTIVE_RUNS_ERR" \
            "A look that fails is not taken as no run. The stage is unchanged and no runner was installed." @lock@ \
            "Run $(rn_retry_command) again."
        fi
        warn "could not look for the active runs of stage $STAGE ($ACTIVE_RUNS_ERR); looking again" ;;
      *)
        failed=0
        if [ "$ACTIVE_RUN_COUNT" -eq 0 ]; then
          empty=$((empty + 1))
          [ "$empty" -lt 2 ] || break
        else
          empty=0
          if [ -z "${WAIT_FOR_RUNS:-}" ]; then
            full_gate_stop 7 "the update is stopped: $ACTIVE_RUN_COUNT running run(s) on hamh-$STAGE-run: $(full_first_runs)" \
              "The stage is unchanged and no runner was installed." @lock@ \
              "You cancel runs with $HAMH_CLIENT_BIN cancel <runId> or wait until they end; ./setup.sh --update --wait-for-runs MINUTES waits for them." \
              "You run ./setup.sh --update again."
          fi
        fi ;;
    esac
    pause=$poll
    if [ -n "${WAIT_FOR_RUNS:-}" ] && [ "$empty" -eq 0 ]; then
      left=$((limit - (SECONDS - started)))
      if [ "$left" -le 0 ]; then
        if [ "$failed" -gt 0 ]; then
          full_gate_stop 6 "could not look for the active runs of stage $STAGE at the end of the wait of $(full_minutes "$WAIT_FOR_RUNS"): $ACTIVE_RUNS_ERR" \
            "A look that fails is not taken as no run. No deployment or client change was made. No run was cancelled." \
            @lock@ "Run $(rn_retry_command) again."
        fi
        full_gate_stop 7 "$ACTIVE_RUN_COUNT run(s) are still active after $(full_minutes "$WAIT_FOR_RUNS"): $(full_first_runs)" \
          "No deployment or client change was made. No run was cancelled." \
          @lock-open@ \
          "Run $(rn_retry_command) again when they have ended, or give a longer --wait-for-runs."
      fi
      if [ "$failed" -eq 0 ]; then
        say "Waiting for $ACTIVE_RUN_COUNT active run(s) to end: $(((left + minute - 1) / minute)) min left. New runs are refused meanwhile. No run is cancelled."
        log_line "waiting for $ACTIVE_RUN_COUNT active run(s): $ACTIVE_RUNS"
        waited=1
      fi
      [ "$pause" -le "$left" ] || pause=$left
    fi
    full_gate_keep || true
    [ "$pause" -le 0 ] || sleep "$pause"
  done
  # before the update goes on, a renewal that is due is made: the runner install and the upload take minutes before the build is recorded in the lock
  until full_gate_keep; do
    tries=$((tries + 1))
    [ "$tries" -lt 3 ] || full_gate_stop 6 "could not renew the deploy lock $(rn_lock_name) of stage $STAGE before the update goes on: ${AWS_ERR:-no answer}" \
      "No deployment or client change was made. No run was cancelled." @lock@ "Run $(rn_retry_command) again."
    [ "$poll" -le 0 ] || sleep "$poll"
  done
  full_gate_own "$poll"
  trap - HUP
  trap rn_lock_interrupted INT TERM
  if [ "$waited" = 1 ]; then
    ok "the active runs have ended (waited $(full_minutes $(((SECONDS - started + minute - 1) / minute))))"
  else
    log_line "no run is active on the stage"
  fi
}

# full_gate_keep: while this run looks and waits it writes its lock again every HAMH_SETUP_LOCK_REFRESH_SECONDS seconds (600 by default), after it has read that the lock
# is still its own (full_gate_mine). The lock then never counts as one nobody renews (30 minutes), neither for another setup nor for the stage's API service. No build is
# recorded yet, and no other setup shares a lock without a build. A read that fails here does not end the wait: it is said, nothing is written, and the renewal is due
# again at the next look. The read-back that lets the update go on stays strict (full_gate_own). Returns 1 when a renewal was due and was not made (the read or the write
# failed; RN_LOCK_AT moves only with a write that went through), and ends the update when the lock was not written for 30 minutes (full_gate_closed).
full_gate_keep() {
  full_gate_closed
  [ $((SECONDS - RN_LOCK_AT)) -ge "$(setup_number "${HAMH_SETUP_LOCK_REFRESH_SECONDS:-600}" 600)" ] || return 0
  if ! rn_lock_read; then
    warn "could not read the deploy lock $(rn_lock_name) of stage $STAGE (${AWS_ERR:-no answer}); reading it again at the next look"
    return 1
  fi
  full_gate_mine
  RN_LOCK_STARTED=$(setup_now)
  if aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "")" --overwrite; then
    RN_LOCK_AT=$SECONDS
    return 0
  fi
  log_line "could not renew the deploy lock: ${AWS_ERR:-no answer}"
  warn "could not write the deploy lock $(rn_lock_name) of stage $STAGE again (${AWS_ERR:-no answer}); writing it again at the next look"
  return 1
}

# full_gate_closed: the stage's API service takes a lock that was not written for 30 minutes (RN_LOCK_STALE_SECONDS) for one nobody renews, and starts runs again. When this
# run could not write its lock for that long (reads or writes that failed, a machine that slept), what its looks found no longer holds: the update ends with exit 6, and
# the lock is given back.
full_gate_closed() {
  [ $((SECONDS - RN_LOCK_AT)) -gt "$RN_LOCK_STALE_SECONDS" ] || return 0
  full_gate_stop 6 "the deploy lock $(rn_lock_name) of stage $STAGE was not written for more than $((RN_LOCK_STALE_SECONDS / 60)) minutes while this run looked for active runs" \
    "The stage has started runs again meanwhile, so this run deploys nothing. No deployment or client change was made. No run was cancelled." @lock@ \
    "Run $(rn_retry_command) again."
}

# full_gate_own [pause]: the read-back before the update goes on: the lock is still this run's. A read that fails never counts as "still mine": it is said and made again
# after the pause of the looks, and the third in a row ends the update with exit 6.
full_gate_own() {
  local tries=0
  while ! rn_lock_read; do
    tries=$((tries + 1))
    [ "$tries" -lt 3 ] || full_gate_stop 6 "could not read the deploy lock $(rn_lock_name) of stage $STAGE: ${AWS_ERR:-no answer}" \
      "A failed read is not taken as this run's lock. No deployment or client change was made. No run was cancelled." @lock@ "Run $(rn_retry_command) again."
    warn "could not read the deploy lock $(rn_lock_name) of stage $STAGE (${AWS_ERR:-no answer}); reading it again"
    [ "${1:-0}" -le 0 ] || sleep "$1"
  done
  full_gate_mine
}

# full_gate_mine: the lock just read (RN_LOCK_TEXT) is still this run's (the token decides). A machine that slept through the 30 minutes of a lock nobody renews may find
# the lock of another setup in its place: this run then deploys nothing and leaves that lock alone. Exit 7.
full_gate_mine() {
  [ "$(rn_lock_get token)" != "$RN_LOCK_TOKEN" ] || return 0
  RN_LOCK_HELD=0
  trap - INT TERM HUP
  st_set state failed || true
  st_save || true
  [ -n "$RN_LOCK_TEXT" ] || setup_exit 7 "the deploy lock $(rn_lock_name) of this run was removed by someone else while it looked for active runs" \
    "This run deploys nothing, so it cannot race another deploy. No deployment or client change was made. No run was cancelled." "Run $(rn_retry_command) again."
  rn_lock_held_exit "another setup took it while this run looked for active runs, so this run deploys nothing"
}

# full_gate_stop <exit code> <message> [detail line ...]: the looks end the update before anything was installed or deployed: the lock is given back first. A detail line
# `@lock@` is the place of the sentence that says what became of the lock (rn_release_said); `@lock-open@` adds what a released lock means for new runs.
full_gate_stop() {
  local code=$1 line
  trap - INT TERM HUP
  rn_lease_release quiet
  st_set state failed || true
  st_save || true
  log_line "the update stopped before anything was installed or deployed: $2"
  err "$2"
  shift 2
  for line in "$@"; do
    case $line in
      @lock@) rn_release_said ;;
      @lock-open@) rn_release_said ", so the stage starts runs again" ;;
      *) detail "$line" ;;
    esac
  done
  exit "$code"
}

# full_gate_interrupted: Ctrl-C, a TERM or a closed terminal (HUP) while this run looks or waits for active runs. Nothing was installed or deployed; the lock is given
# back, and the lines say what became of it. Exit 4.
full_gate_interrupted() {
  trap - INT TERM HUP
  rn_lease_release quiet
  st_set state failed || true
  st_save || true
  err "Stopped while it looked for active runs (you stopped it)."
  detail "No deployment or client change was made. No run was cancelled."
  rn_release_said
  detail "Run $(rn_retry_command) again to go on."
  log_line "stopped during the look for active runs"
  exit 4
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
  # The backticks are text of the CDK notice that the case matches.
  # shellcheck disable=SC2016
  case $1 in
    "[WARNING] aws-cdk-lib."*" is deprecated."|"INFO HAMH_UTILITY_MODEL is unset:"*) FQ_REST=2; return 0 ;;
    '`cdk synth` may hang in Docker on Linux '*) return 0 ;;
  esac
  return 1
}

# full_export_settings: in the subshell of the local deploy, the settings you exported (SETUP_EXPLICIT_LINES of pf_explicit_settings, exported again, so the deploy gets
# exactly the lines the plan showed and the runner route sends as SETUP_EXPLICIT_SETTINGS) and nothing else (A4.17): a value setup added would count as your explicit
# choice in the deploy and in its record. The deploy keeps the Instances part from the stage's record or the live stage, as it does on the runner route.
full_export_settings() {
  local line
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    # Each line is NAME=value: export takes the assignment it holds.
    # shellcheck disable=SC2163
    export "$line"
  done <<<"$SETUP_EXPLICIT_LINES"
}

# full_retry_command: what you type to run this setup again on the route it took.
full_retry_command() { rn_retry_command; }

# full_local_lock_check: the local route reads the deploy lock of the stage before the question, as the runner route does (rn_lease_check; C6): a lock another setup holds, or
# whose build is running, ends this run with exit 7 and nothing changed, and a stale one is replaced after the question (rn_lease_take). A deploy on this machine cannot follow
# the build of a setup on the runner route: a lock whose build is running stops this run also when the build is of this commit and these inputs, and so does a lock whose build
# has succeeded while its holder still renews it (that setup installs its client and removes its runner); without a renewal for 30 minutes it is stale.
full_local_lock_check() {
  rn_lease_check
  case $RN_MODE in
    attach)
      RN_LOCK_HELD=0
      RN_LOCK_LEGACY=0
      RN_MODE=new
      rn_lock_held_exit "its build $RN_BUILD_ID is running: a setup of this commit on the runner route. This run would deploy on this machine (--build-on local) and cannot follow that build; run ./setup.sh --${FULL_ACTION:-full} without --build-on local to attach to it" ;;
    finish)
      RN_LOCK_HELD=0
      if [ "$RN_LOCK_LEGACY" != 1 ] && rn_lock_fresh; then
        # the setup that started the build still installs its client and removes its runner (A4.4): its lock is its own while it renews it
        RN_MODE=new
        rn_lock_held_exit "its build $RN_BUILD_ID has succeeded, and the setup that started it is still installing its client and removing its runner. This run would deploy on this machine (--build-on local) and cannot finish from that build; run ./setup.sh --${FULL_ACTION:-full} without --build-on local to finish from it"
      fi
      RN_LOCK_LEGACY=0
      RN_LOCK_TOKEN=""
      RN_MODE=stale
      RN_LOCK_STALE="the deploy lock recorded build $RN_BUILD_ID, which has succeeded, and its holder has ended or has not renewed the lock for 30 minutes"
      log_line "$RN_LOCK_STALE" ;;
  esac
}

# full_local_lock_refresh: a lock that names no build counts as stale 30 minutes after its start (RN_LOCK_STALE_SECONDS), and a deploy on this machine can take longer. Its
# holder therefore writes a new start time into its own lock every HAMH_SETUP_LOCK_REFRESH_SECONDS seconds (600 by default) while the deploy prints: the output loop of
# full_local_flow calls this for each line. A deploy that prints nothing for 30 minutes is not covered.
FULL_LOCK_AT=0
full_local_lock_refresh() {
  [ $((SECONDS - FULL_LOCK_AT)) -ge "${HAMH_SETUP_LOCK_REFRESH_SECONDS:-600}" ] || return 0
  FULL_LOCK_AT=$SECONDS
  RN_LOCK_STARTED=$(setup_now)
  aws_run ssm put-parameter --name "$(rn_lock_name)" --type String --value "$(rn_lock_value "")" --overwrite </dev/null ||
    log_line "could not renew the deploy lock: ${AWS_ERR:-no answer}"
  return 0
}

# full_local_interrupted: Ctrl-C or a TERM while the deploy of the local route runs. scripts/deploy.sh has stopped by then (it got the same signal, or it ended); the lock is
# given back, and the stage holds what the deploy had changed until then. Exit 6.
full_local_interrupted() {
  trap - INT TERM
  RN_LOCK_BUSY=0
  rn_lease_release quiet
  st_set state failed || true
  st_save || true
  err "Stopped (you stopped it) while scripts/deploy.sh was running on this machine."
  rn_release_said
  detail "The stage may contain partial deployment changes; ./setup.sh --status shows its stacks."
  detail "Run $(rn_retry_command) to deploy again."
  log_line "stopped during the local deploy"
  exit 6
}

# full_local_failed <exit code>: scripts/deploy.sh ended badly. Every way it ends before its first stack change leaves the stage unchanged (A2.3): a refusal (its line
# `DEPLOY-REFUSED reason=<id> wait=<yes|no> stage=unchanged :: <action>`, exit 3) is exit 7 when waiting cures it, else 5, with the action; the guard of the running runs
# (`DEPLOY-FAIL guard ...`) is exit 7; a failed step before that change (`DEPLOY-FAIL <step> ...` followed by `DEPLOY-STAGE unchanged`) is exit 5 with the step and its text.
# Any other failure may have changed the stage.
full_local_failed() {
  local rc=$1 refused fail unchanged action wait
  refused=$(full_deploy_line DEPLOY-REFUSED)
  fail=$(full_deploy_line DEPLOY-FAIL)
  unchanged=$(full_deploy_line "DEPLOY-STAGE unchanged")
  if [ -n "$refused" ] && [ "$rc" = 3 ]; then
    action=${refused#* :: }
    [ "$action" != "$refused" ] || action=""
    wait=5
    case " $refused " in *" wait=yes "*) wait=7 ;; esac
    setup_exit "$wait" "scripts/deploy.sh refused before it changed anything: ${refused%% :: *}" \
      "The stage is unchanged." ${action:+"$action"} "Then run $(full_retry_command) again."
  fi
  case $fail in
    "DEPLOY-FAIL guard"*)
      setup_exit 7 "scripts/deploy.sh stopped: ${fail#DEPLOY-FAIL guard }" \
        "The stage is unchanged." "You cancel runs with $HAMH_CLIENT_BIN cancel <runId> or wait until they end, then run $(full_retry_command) again." ;;
  esac
  if [ -n "$unchanged" ]; then
    setup_exit 5 "scripts/deploy.sh stopped before it changed the stage: ${fail:-a step failed}" \
      "The stage is unchanged." "Fix what that line names (its output is above and in $RUN_LOG), then run $(full_retry_command) again."
  fi
  setup_exit 5 "scripts/deploy.sh failed (exit $rc)" "Its output is above and in $RUN_LOG." \
    "The stage may contain partial deployment changes. Run $(full_retry_command) to retry."
}

# full_deploy_line <start>: the last line of the deploy's output (in the log of this run) that starts with these words, nothing when it printed none.
full_deploy_line() {
  awk -v w=" deploy: $1" '{ i = index($0, w); if (i) l = substr($0, i + 9) } END { print l }' "$RUN_LOG" 2>/dev/null
}

full_local_flow() {
  local file rc line endpoint cfg="" records
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
  # the deploy's records go to the receipt folder of this run, and it reads no criteria file of its own: nothing is written outside the folders setup owns
  records=$ST_DIR/deploy-records
  (
    umask 077
    mkdir -p "$records" || exit 1
  ) || setup_exit 1 "could not make $records"
  if [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
    # the stage already runs this commit with these settings (rn_same_deployment_check; C7): nothing is deployed, so setup takes no lock, and only the client is built here.
    # scripts/deploy.sh writes the label of this release alone.
    say_log "scripts/deploy.sh deploys nothing: the stage already runs this commit. It writes the release label, and the client is built on this machine."
    (
      cd "$SELF_DIR" || exit 1
      [ -z "$cfg" ] || export AWS_CONFIG_FILE=$cfg
      export DEPLOY_RECORD_DIR="$records" DEPLOY_CRITERIA="$ST_DIR/deploy-checks.json"
      HAMH_ACCOUNTS_FILE=$file HAMH_AWS_PROFILE=$ACCT_PROFILE_FILE scripts/deploy.sh --stage "$STAGE" --relabel 2>&1
    ) | while IFS= read -r line; do
      log_line "deploy: $line"
      printf '  %s\n' "$line"
    done
    rc=${PIPESTATUS[0]}
    if [ "$rc" = 3 ]; then
      full_local_failed "$rc"
    elif [ "$rc" != 0 ]; then
      st_set state failed || true
      st_save || true
      setup_exit 5 "scripts/deploy.sh could not write the release label (exit $rc)" "Its output is above and in $RUN_LOG." \
        "The stage runs the source tree it ran before; only its version label is in question. Run $(full_retry_command) to try again."
    fi
  else
    # The deploy lock of the stage (C6), taken right after the question and before anything in the account changes. scripts/deploy.sh gets its token and only checks it
    # (HAMH_DEPLOY_LOCK_TOKEN); this run gives the lock back however the deploy ends, also when you stop it.
    rn_lease_take local
    case $RN_MODE in
      attach|finish)
        # another setup created the lock in the same moment and its build runs or has ended: this run does not deploy beside it
        RN_LOCK_HELD=0
        RN_LOCK_LEGACY=0
        rn_lock_held_exit "another setup took it a moment ago, so this run deploys nothing" ;;
    esac
    # With the lock held the stage starts no new runs; the look for active runs that decides comes before the deploy
    full_runs_gate
    trap full_local_interrupted INT TERM
    # The deploy runs under the lock from here, so an exit that nothing handles leaves the lock to it (rn_lock_on_exit)
    RN_LOCK_BUSY=1
    FULL_LOCK_AT=$SECONDS
    say_log "Running scripts/deploy.sh on this machine (the account file is $file) ..."
    (
      cd "$SELF_DIR" || exit 1
      [ -z "$cfg" ] || export AWS_CONFIG_FILE=$cfg
      export DEPLOY_RECORD_DIR="$records" DEPLOY_CRITERIA="$ST_DIR/deploy-checks.json"
      full_export_settings
      HAMH_ACCOUNTS_FILE=$file HAMH_AWS_PROFILE=$ACCT_PROFILE_FILE HAMH_DEPLOY_LOCK_TOKEN=$RN_LOCK_TOKEN scripts/deploy.sh --stage "$STAGE" --azs "$AZS" --fork-source build 2>&1
    ) | while IFS= read -r line; do
      log_line "deploy: $line"
      full_local_lock_refresh
      if full_quiet_cdk "$line"; then continue; fi
      printf '  %s\n' "$line"
    done
    rc=${PIPESTATUS[0]}
    RN_LOCK_BUSY=0
    trap - INT TERM
    rn_lease_release
    if [ "$rc" != 0 ]; then
      st_set state failed || true
      st_save || true
      full_local_failed "$rc"
    fi
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
  [ -z "${FROM:-}" ] && [ -z "$(ham_now)" ] || return 1
  st_init "$ACCOUNT" "$REGION" "$STAGE"
  full_verified_copy
}

# full_verified_copy: the client (and the operator tool, when the folder holds it) in download/verified of the receipt still hash to their SHA256SUMS lines. Sets FROM, CL_SHA
# and CL_ADMIN_SHA; returns 1 when there is nothing to use.
full_verified_copy() {
  local dir want got admin_want admin_got
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
    [ "$FULL_DEST_LATER" != 1 ] || full_dest_writable
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

# full_update_reuse_check: an update of the commit the stage already runs (SETUP_CLIENT_ONLY) needs only the client. When an earlier update of this commit and this account file
# downloaded and checked one (the receipt's clientCommit and clientConfig) that it did not install (the receipt does not say installed), and that copy in download/verified still
# hashes to its SHA256SUMS lines, the copy is installed and no runner or build is needed: FULL_REUSE=1, FROM, CL_SHA. A client that was installed gets the client-only
# build of C7, and --force-deploy and the local route build as before.
FULL_REUSE=0
full_update_reuse_check() {
  FULL_REUSE=0
  [ "${SETUP_CLIENT_ONLY:-0}" = 1 ] && [ "${BUILD_ON:-runner}" != local ] && [ -z "${FROM:-}" ] || return 0
  # While the backend still reports an older version, the client-only build is what writes the release label (rn_label_plan), so the kept copy is not
  # installed alone: without the build the update would end with the older version still reported.
  [ -z "${RN_LABEL_NEXT:-}" ] || [ "${RN_LABEL_KEEP:-0}" = 1 ] || return 0
  [ "$(st_get clientCommit)" = "$SRC_COMMIT" ] && [ "$(st_get clientConfig)" = "$ACCT_DIGEST" ] && [ "$(st_get state)" != installed ] || return 0
  full_verified_copy || return 0
  FULL_REUSE=1
  say_log "The client this setup downloaded and checked earlier for this commit (sha256 $CL_SHA) is used again: no runner and no build are needed."
}

# ---- the run -----------------------------------------------------------------------------------------------------------------------------

# full_run full|update
full_run() {
  local action=$1
  FULL_ACTION=$action
  RN_ACTION=deploy
  [ "${BUILD_ON:-runner}" != local ] || full_local_check
  step "Checking your account"
  pf_region
  pf_profile
  pf_aws_cli
  pf_identity
  pf_source
  # the stage settings: the registry of the commit that is built and the ones you exported (a value a setting does not take ends the run here, before the question), and for an
  # update the stage's record of what it was deployed with (its settings, its commit, its operator and its zone)
  pf_registry
  pf_explicit_settings
  full_repo_names_env
  [ "$action" != update ] || pf_record
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
  full_client_dest
  # the deploy lock of the stage is read before the question on both routes (C6): a setup that holds it ends this run here, with nothing changed
  if [ "${BUILD_ON:-runner}" != local ]; then
    rn_lease_check
    [ "$RN_MODE" = attach ] || rn_note_installed
    # a build of this checkout is followed or read, and its client is installed: the folder is checked now
    case $RN_MODE in attach|finish) [ "$FULL_DEST_LATER" != 1 ] || full_dest_writable ;; esac
  else
    full_local_lock_check
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
            none) full_no_stage_here ;;
            deploying)
              setup_exit 7 "the stage $STAGE in account $ACCOUNT is being changed right now: $STAGE_BAD" \
                "No build of this checkout explains it, so another operation is working on the stage. Wait until it has finished, then run setup.sh again." "Nothing was changed." ;;
            broken)
              setup_exit 7 "the stage $STAGE exists in account $ACCOUNT but is not healthy: $STAGE_BAD" \
                "Setup does not update on top of it. Look at the stack in the CloudFormation console, or remove the stage with ./setup.sh --teardown and set it up again with ./setup.sh --full." \
                "Nothing was changed." ;;
          esac ;;
      esac ;;
  esac
  # A deploy follows (a build of this checkout that is attached to or read does not start one): the stage keeps its owner, so one that HAMH_OWNER names differently is found here.
  case $RN_MODE in
    attach|finish) ;;
    *)
      pf_stage_owner
      pf_owner_check
      pf_instances_part
      pf_settings_refuse
      if [ "$action" = update ]; then
        # the stage already runs this commit with these settings: nothing is deployed, so no run is stopped or asked about, and only the client is fetched (SETUP_CLIENT_ONLY).
        # It is found first (C7): the order of the commits (C8) is a question only for an update that deploys another source tree.
        rn_same_deployment_check
        # A new default the deploy will not take, for the plan (nothing to say when only the client is fetched); a retry whose record cannot say what its
        # unfinished update was given, which the deploy would refuse for your consent after the build, ends here instead
        if [ "${SETUP_CLIENT_ONLY:-0}" != 1 ]; then
          pf_default_holds
          [ -z "$PF_ASKS" ] || full_default_asks
        fi
        pf_commit_order
        pf_image_plan
        full_update_reuse_check
        # an update does not start while a run is active: found here, it costs seconds and leaves the account as it was. --allow-active-runs leaves the decision to the
        # build's own deploy guard, which refuses only an update that changes the worker runtime; the runs are listed in the plan, and a new worker image is refused here.
        if [ "${SETUP_CLIENT_ONLY:-0}" = 1 ]; then
          :
        elif [ "${ALLOW_ACTIVE_RUNS:-0}" = 1 ]; then
          pf_active_runs
          pf_image_refuse
        elif [ -n "${WAIT_FOR_RUNS:-}" ]; then
          # --wait-for-runs: the runs are listed for the plan; the wait comes after the question and the lock (full_runs_gate)
          pf_active_runs
        else
          pf_active_runs_check
        fi
      fi ;;
  esac
  pf_bootstrap_stack
  pf_bootstrap_min
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

# full_repo_names_env: HAMH_REPO_NAMES is the same setting as --repo-name (the CDK app reads it before the account file), so it becomes the rule of the plan and of the account file
# on both routes; with --repo-name as well the two must agree.
full_repo_names_env() {
  local env_names=${HAMH_REPO_NAMES:-}
  [ -n "$env_names" ] || return 0
  if [ "$REPO_NAMES_SET" = 1 ]; then
    [ "$REPO_NAMES" = "$env_names" ] || setup_exit 2 "HAMH_REPO_NAMES=$env_names and --repo-name $REPO_NAMES name different repository rules" \
      "The stage takes one rule: give it once, as --repo-name or as HAMH_REPO_NAMES. Nothing was changed."
    return 0
  fi
  REPO_NAMES=$env_names
  REPO_NAMES_SET=1
  check_repo_names
}

# full_client_dest: the client is installed where you run it (C10): --prefix, else the folder the last setup of this stage installed it in (the receipt), else the folder of the
# $HAMH_CLIENT_BIN your PATH finds when it is this client, else PREFIX/bin. A different $HAMH_CLIENT_BIN that starts first on the PATH is named with both paths, a folder you
# cannot write to (or create) ends the run with exit 1 before anything is created (full_dest_writable), and a program of the
# client's name in the install folder that is not this client is never overwritten (exit 1 before anything is created).
CLIENT_DEST_FROM=""
CLIENT_SHADOW=""
FULL_DEST_LATER=0
full_client_dest() {
  local rp found first
  CLIENT_DEST_FROM=""
  CLIENT_SHADOW=""
  st_init "$ACCOUNT" "$REGION" "$STAGE"
  st_load
  if [ -n "${PREFIX_OPT:-}" ]; then
    CLIENT_DEST_FROM="--prefix"
  else
    rp=$(st_get bindir)
    found=$(client_on_path)
    if [ -n "$rp" ] && [ -d "$rp" ]; then
      BINDIR=$rp
      CLIENT_DEST_FROM="where the last setup of this stage installed it"
    elif [ -n "$found" ]; then
      BINDIR=$(dirname "$found")
      CLIENT_DEST_FROM="the $HAMH_CLIENT_BIN your PATH finds"
    fi
    DEST=$BINDIR/$HAMH_CLIENT_BIN
    ADMIN_DEST=$BINDIR/$HAMH_ADMIN_BIN
  fi
  first=$(command -v "$HAMH_CLIENT_BIN" 2>/dev/null || true)
  if [ -n "$first" ] && [ "$first" != "$DEST" ]; then
    CLIENT_SHADOW=$first
    warn "another $HAMH_CLIENT_BIN, $first, starts first on your PATH, and this setup installs the new client to $DEST. Run $DEST, or put $BINDIR before $(dirname "$first") on your PATH."
  fi
  # `./setup.sh --full` on a deployed stage installs a client only when it follows a build of this checkout (RN_MODE attach or finish) or no client is installed
  # here (full_existing, full_reuse_client); the folder is checked then, so a run that only removes a runner that is left is not stopped by a folder it does not write to.
  if [ "${FULL_ACTION:-full}" = full ] && [ "${STAGE_STATE:-}" = complete ]; then
    FULL_DEST_LATER=1
  else
    full_dest_writable
  fi
  full_dest_check
}

# full_dest_writable: the folder the client goes to exists and you can write to it, or it can be created (its nearest existing parent is a folder you can write to). Else the
# run ends here with exit 1, before the question and before anything is created, and names --prefix (A4.10): found after the deploy, the same problem would cost the whole
# update and leave the old client in place.
full_dest_writable() {
  local dir=$BINDIR
  while [ ! -e "$dir" ] && [ ! -L "$dir" ]; do
    case $dir in
      /*/*) dir=${dir%/*} ;;
      /*) dir=/ ;;
      */*) dir=${dir%/*} ;;
      *) dir=. ;;
    esac
  done
  if [ -d "$dir" ] && [ -w "$dir" ] && [ -x "$dir" ]; then return 0; fi
  if [ "$dir" = "$BINDIR" ]; then
    setup_exit 1 "you cannot write to the folder $BINDIR, where the client is installed${CLIENT_DEST_FROM:+ ($CLIENT_DEST_FROM)}" \
      "Give --prefix DIR to install the client in DIR/bin, a folder you can write to, or make $BINDIR writable, then run setup.sh again." "Nothing was created."
  fi
  setup_exit 1 "the folder $BINDIR, where the client is installed${CLIENT_DEST_FROM:+ ($CLIENT_DEST_FROM)}, does not exist and cannot be created: you cannot write to $dir" \
    "Give --prefix DIR to install the client in DIR/bin, a folder you can write to, or make $dir writable, then run setup.sh again." "Nothing was created."
}

# full_dest_check: a file at the install path that is not this client (it lacks the header name the client reads, and no download of an earlier setup of this stage has its SHA-256)
# is a program of somebody else's: setup stops before anything is created.
full_dest_check() {
  local sum sums=$ST_DIR/download/verified/SHA256SUMS
  [ -e "$DEST" ] || [ -L "$DEST" ] || return 0
  is_current_client "$DEST" && return 0
  if [ -f "$DEST" ] && [ -f "$sums" ]; then
    sum=$(setup_sha256 <"$DEST")
    awk -v h="$sum" '$1 == h { f = 1 } END { exit f ? 0 : 1 }' "$sums" && return 0
  fi
  setup_exit 1 "$DEST exists and is not this system's $HAMH_CLIENT_BIN (it lacks the header name x-hamh-backend-version that the client reads), so setup does not overwrite it" \
    "Move that file aside, or give --prefix DIR to install the client in DIR/bin, then run setup.sh again." "Nothing was created."
}

# full_no_stage_here: an update found no stage in this account and Region. Before it suggests a new stage it looks where the stage may be: the other supported Region of this
# account (one list of stacks there) and the receipts of earlier setups on this machine, which name their account, Region and profile.
full_no_stage_here() {
  local _r f dir r acct prof opts
  local -a hints=()
  for _r in $SETUP_REGIONS; do
    [ "$_r" != "$REGION" ] || continue
    if full_stage_in_region "$_r"; then
      opts=""
      [ -z "${PROFILE_EFF:-}" ] || opts=" --profile $PROFILE_EFF"
      hints+=("The stage $STAGE is deployed in $_r in this account. Update it there:  ./setup.sh --update$opts --region $_r")
    fi
  done
  for f in "${XDG_CONFIG_HOME:-$HOME/.config}/$HAMH_CONFIG_DIR_NAME"/setup/*/*/"$STAGE".json; do
    [ -f "$f" ] || continue
    dir=${f%/*}
    r=${dir##*/}
    acct=${dir%/*}
    acct=${acct##*/}
    [ "$acct" != "$ACCOUNT" ] || [ "$r" != "$REGION" ] || continue
    if [ "$acct" = "$ACCOUNT" ]; then
      case " ${hints[*]+${hints[*]}} " in *"--region $r"*) continue ;; esac
      hints+=("A setup on this machine deployed the stage $STAGE in $r in this account. Update it there:  ./setup.sh --update --region $r")
    else
      prof=$(json_flat <"$f" 2>/dev/null | awk -F'\t' '$1 == "profile" { print $2; exit }')
      setup_safe_word "$prof" || prof=""
      opts=""
      [ -z "$prof" ] || opts=" --profile $prof"
      hints+=("A setup on this machine deployed the stage $STAGE in account $acct, Region $r${prof:+, with the profile $prof}. Update it there:  ./setup.sh --update$opts --region $r")
    fi
  done
  setup_exit 7 "nothing to update: the stage $STAGE is not deployed in account $ACCOUNT, Region $REGION" ${hints[@]+"${hints[@]}"} \
    "To deploy a new stage in this account and Region instead:  ./setup.sh --full" "Nothing was changed."
}

# full_stage_in_region <Region>: 0 when that Region of this account holds a stack of the stage; a list that fails counts as none.
full_stage_in_region() {
  local saved=$REGION rc=1
  REGION=$1
  # shellcheck disable=SC2086
  if aws_run cloudformation list-stacks --stack-status-filter $SETUP_LIVE_STATUSES \
      --query "StackSummaries[?starts_with(StackName,'hamh-$STAGE-')].[StackName,StackStatus]" --output text; then
    case $AWS_OUT in
      ''|None) ;;
      *) rc=0 ;;
    esac
  fi
  REGION=$saved
  return "$rc"
}

# full_installed: the client is installed and runs (setup.sh calls it after `ham models`). The receipt says so.
full_installed() {
  if [ "$FULL_RAN" = 1 ]; then
    st_set state installed || true
    st_save || true
  fi
}

# full_configured: the client is installed and its config names the stage. The runner that is not kept goes now: it held
# the only copy of the client apart from the one in the receipt folder, and a setup that is stopped while it goes leaves a client that already points at the stage. The deploy
# lock is given back after it (full_runner_end).
full_configured() { full_runner_end; }

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
