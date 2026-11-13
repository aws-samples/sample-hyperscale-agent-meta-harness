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
#                   uncommitted changes (also with --revision: the setup files and the runner template run from the working tree), exit 2 when --revision names no
#                   commit of the clone
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
#   pf_active_runs_check  an update with a running run: exit 7 before the runner is installed or anything is built; it names --allow-active-runs when the last update of
#                   this commit failed after its deploy (pf_retry_hint)
#   pf_registry     the settings registry (scripts/lib/stage-settings.tsv) of the commit that is built: REG_HAVE, PF_REG
#   pf_fold_earlier_settings  the earlier spelling of every registry setting becomes the setting; two spellings with different values: exit 2
#   pf_explicit_settings  the registry settings you exported (HAMH_<NAME>, after pf_fold_earlier_settings folded the earlier spelling): SETUP_EXPLICIT_LINES and SETUP_EXPLICIT_SETTINGS (base64 of
#                   the same lines, the one override the runner route passes; both start empty in lib.sh, whatever your shell holds); exit 2 for a value the setting does not
#                   take, or a name of the registry's families that is no setting
#   pf_record       the stage's record of its settings, /hamh/<stage>/settings (REC_HAVE, REC_TEXT; pf_rec_get reads a key); exit 1 when it cannot be read
#   pf_instances_part  INSTANCES_PRESENT: the stage has the Instances part (the parameter /hamh/<stage>/instances-capacity-provider-id)
#   pf_default_holds  the settings whose new default the deploy will not take, because the stage follows the default, runs another value and taking the default
#                   needs consent: PF_HOLDS; on a record of an update that did not finish, what the deploy carries on (PF_TAKES) or must ask you about
#                   (PF_ASKS, which full_default_asks turns into exit 7); PF_HOLD_WHY and PF_HOLD_NAMES when the stage's values could not be read
#   pf_settings_refuse  an exported setting that would remove a part the stage has (HAMH_COMPUTE_INSTANCES=off on a stage with the Instances part): exit 7
#   pf_commit_order  an update to a commit that is older than the one the stage records, or one git cannot place: exit 7 unless --revision names it
#   pf_image_plan   whether the update needs no new worker image, a new one, or a rebuild of the Cline binary (IMG_PLAN), from the pins of the two commits

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
SRC_HEAD=""
SRC_VERSION_TEXT=""
REG_HAVE=0
PF_REG=""
REC_HAVE=0
REC_TEXT=""
INSTANCES_PRESENT=0
OP_CHANGE=""
OP_FROM=""
PF_OP_WHY=""
ZONE_FROM=""
IMG_PLAN=unknown
IMG_TEXT=""
COMMIT_CHANGE=""
# The settings whose values the deploy writes in one form (NUMBER_SETTINGS, LOWER_SETTINGS and LIST_SETTINGS of the deploy engine, scripts/lib/deploy.py;
# scripts/tests/test_setup_update_scenarios.py keeps these three lists equal to them): whole numbers without leading zeros, case-free words in lower case, comma lists without
# the spaces around their commas and without empty items. pf_canonical gives that form, so setup shows, sends and compares the value the stage's record holds (A4.8).
PF_NUMBER_SETTINGS="HAMH_INSTANCES_ROOT_FREE_GIB HAMH_INSTANCES_WORKSPACE_GIB HAMH_INSTANCES_LIFETIME_S HAMH_INSTANCES_IDLE_S HAMH_TAVILY_KEY_MAX_AGE_DAYS HAMH_LOG_RETENTION_DAYS HAMH_PR_CHECK_MINUTES HAMH_PROVIDER_SILENCE_S HAMH_SLICE_DEADLINE_S"
PF_LOWER_SETTINGS="HAMH_COMPUTE_INSTANCES HAMH_TOOLS_MODE HAMH_TAVILY_ALARMS HAMH_PR_CHECK_COMPUTE HAMH_UTILITY_MODEL HAMH_UTILITY_PURPOSES HAMH_KEEP_TAG HAMH_STAGE_KIND"
PF_LIST_SETTINGS="HAMH_INSTANCES_TYPES HAMH_UTILITY_PURPOSES HAMH_AGENTCORE_STORAGE_ACCOUNTS"
# Variables of the registry's families that are no stage setting but documented controls of the worker or the client: an exported one is not refused.
PF_CONTROLS="HAMH_RESEARCH_URL HAMH_RESEARCH_WAIT_S HAMH_TOOLS_URL HAMH_TOOLS_SERVER_BIN"
# The settings whose live value the plan can read (pf_live_get: the capacity provider of the runtimes stack, one read). A setting of a consent class without a
# reader here gets no plan line when a release changes its default; scripts/tests/test_setup_update_scenarios.py pins the defaults of those settings, so such a
# change fails the tests until it has a reader.
PF_LIVE_READERS="HAMH_INSTANCES_TYPES HAMH_INSTANCES_ROOT_FREE_GIB HAMH_INSTANCES_WORKSPACE_GIB HAMH_INSTANCES_LIFETIME_S HAMH_INSTANCES_IDLE_S"
PF_DEFAULTS=""
DEF_HAVE=0
PF_HOLDS=""
PF_TAKES=""
PF_ASKS=""
PF_REC_DEPLOYING=0
PF_HOLD_WHY=""
PF_HOLD_NAMES=""
PF_LIVE_READ=0
PF_LIVE_FLAT=""

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
  SRC_HEAD=$(git -C "$SELF_DIR" rev-parse HEAD 2>/dev/null || true)
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
    if [ -n "${REVISION:-}" ] && [ "$REVISION" != "$SRC_HEAD" ]; then
      # A4.13: the build takes the git archive of the revision, and setup.sh, its helper files, the runner template and the bootstrap pin run as they are in the working tree
      detail "--revision $REVISION builds the git archive of that commit, and this setup.sh, the runner template and the bootstrap pin still run from your working tree, so it must be clean too."
    fi
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
    # The local route deploys the working tree and compiles the client from it, so it can build only the commit that is checked out.
    if [ "${BUILD_ON:-runner}" = local ] && [ "$REVISION" != "$SRC_HEAD" ]; then
      setup_exit 2 "--build-on local deploys and compiles the working tree of this clone, which is commit $SRC_HEAD, not --revision $REVISION" \
        "Check out that commit and update from it:  git checkout $REVISION   then   ./setup.sh --update --build-on local" \
        "Or leave out --build-on local: the runner in your account builds the git archive of the revision." "Nothing was changed."
    fi
    pf_runner_contract "$REVISION"
    pf_registry_since "$REVISION"
    SRC_COMMIT=$REVISION
  else
    SRC_COMMIT=$SRC_HEAD
  fi
  SRC_KEY=source/$SRC_COMMIT.zip
  SRC_VERSION_TEXT=$(git -C "$SELF_DIR" show "$SRC_COMMIT:VERSION" 2>/dev/null | head -n 1 | tr -d ' \r')
  setup_safe_word "$SRC_VERSION_TEXT" || SRC_VERSION_TEXT=""
  ok "source commit $SRC_COMMIT (clean clone)"
}

# pf_runner_contract <revision>: the setup files run from the working tree and the build runs the revision's own, so their runner contract (HAMH_RUNNER_CONTRACT of names.sh) must be
# the same. A revision with another number, or none, is refused before anything is created, with the way that works: its own setup.sh.
pf_runner_contract() {
  local rev=$1 want=${HAMH_RUNNER_CONTRACT:-} got
  [ -n "$want" ] || return 0
  got=$(git -C "$SELF_DIR" show "$rev:scripts/setup/names.sh" 2>/dev/null | sed -n 's/^HAMH_RUNNER_CONTRACT=\([0-9][0-9]*\)$/\1/p' | head -n 1)
  [ "$got" != "$want" ] || return 0
  setup_exit 2 "commit $rev has ${got:+runner contract $got}${got:-no runner contract number}, and this setup.sh has runner contract $want" \
    "The setup files run from your working tree and the build runs the files of the revision, so the two must agree. Run the revision's own setup instead:" \
    "  git checkout $rev   then   ./setup.sh --update" "Nothing was changed."
}

# pf_registry_since <revision>: a revision whose tree has no settings registry deploys without it, so it cannot keep the settings of the stage. Refused before anything is created when
# this clone has the registry (a clone without it has nothing to compare), naming the first commit of the clone that adds it and its VERSION.
pf_registry_since() {
  local rev=$1 first ver=""
  git -C "$SELF_DIR" cat-file -e "HEAD:$SETUP_SETTINGS_REGISTRY" 2>/dev/null || return 0
  git -C "$SELF_DIR" cat-file -e "$rev:$SETUP_SETTINGS_REGISTRY" 2>/dev/null && return 0
  first=$(git -C "$SELF_DIR" log --diff-filter=A --format=%H -- "$SETUP_SETTINGS_REGISTRY" 2>/dev/null | tail -n 1)
  [ -z "$first" ] || ver=$(git -C "$SELF_DIR" show "$first:VERSION" 2>/dev/null | head -n 1 | tr -d ' \r')
  setup_exit 2 "commit $rev has no $SETUP_SETTINGS_REGISTRY, so its deploy cannot keep the settings of the stage: an update to it would return them to the defaults" \
    "The first release that keeps them is ${ver:+VERSION $ver, }commit ${first:-that adds the file}. Give that commit or a later one with --revision." "Nothing was changed."
}

# ---- the settings of the stage -------------------------------------------------------------------------------------------

# pf_registry: the registry of the commit that is built (git show, so a --revision brings its own). REG_HAVE 0: the commit has none, and no setting is read.
pf_registry() {
  local text
  REG_HAVE=0
  PF_REG=""
  text=$(git -C "$SELF_DIR" show "$SRC_COMMIT:$SETUP_SETTINGS_REGISTRY" 2>/dev/null) || return 0
  PF_REG=$(printf '%s\n' "$text" | awk '/^[ \t]*#/ { next } /^[A-Z][A-Z0-9_]*\|/ { print }')
  REG_HAVE=1
  # The code defaults of the same commit (NAME|DEFAULT); a commit without the file has none, and the plan names no new default.
  DEF_HAVE=0
  PF_DEFAULTS=""
  text=$(git -C "$SELF_DIR" show "$SRC_COMMIT:$SETUP_SETTINGS_DEFAULTS" 2>/dev/null) || return 0
  PF_DEFAULTS=$(printf '%s\n' "$text" | awk '/^[ \t]*#/ { next } /^[A-Z][A-Z0-9_]*\|/ { print }')
  DEF_HAVE=1
}

# pf_def_get <name>: the code default of a setting in the commit that is built (empty for no value); returns 1 when the defaults file has no row for it.
pf_def_get() {
  printf '%s\n' "$PF_DEFAULTS" | awk -v k="$1" 'index($0, k "|") == 1 { print substr($0, length(k) + 2); found = 1; exit } END { exit found ? 0 : 1 }'
}

# pf_reg_names: the names of the registry, in its order.
pf_reg_names() {
  printf '%s\n' "$PF_REG" | awk -F'|' 'NF { print $1 }'
}

# pf_reg_get <name> <kept|change|pattern|note>: one field of a row. NAME, KEPT and CHANGE are the first three fields and NOTE the last; the PATTERN is what lies between them, so it
# may hold | (an alternation).
pf_reg_get() {
  printf '%s\n' "$PF_REG" | awk -v n="$1" -v f="$2" '
    {
      i = index($0, "|"); if (i == 0 || substr($0, 1, i - 1) != n) next
      rest = substr($0, i + 1); i = index(rest, "|"); kept = substr(rest, 1, i - 1); rest = substr(rest, i + 1)
      i = index(rest, "|"); change = substr(rest, 1, i - 1); rest = substr(rest, i + 1)
      j = 0; for (k = length(rest); k > 0; k--) if (substr(rest, k, 1) == "|") { j = k; break }
      if (f == "kept") print kept; else if (f == "change") print change; else if (f == "pattern") print substr(rest, 1, j - 1); else print substr(rest, j + 1)
      exit
    }'
}

# pf_trim <value>: the value without the space, tab, line feed, carriage return, form feed and vertical tab around it: what the deploy strips from an exported setting before
# it reads it. A value of such characters alone is no value.
pf_trim() {
  local _v=$1
  while :; do
    case $_v in
      [[:space:]]*) _v=${_v#?} ;;
      *[[:space:]]) _v=${_v%?} ;;
      *) break ;;
    esac
  done
  printf '%s' "$_v"
}

# pf_canonical <name> <trimmed value>: the one spelling of a value that its pattern took, as canonical() of the deploy engine writes it into the stage's record: a number
# without its leading zeros, a case-free word in lower case, a comma list without the spaces around its commas and without empty items.
pf_canonical() {
  local _n=$1 _v=$2
  case " $PF_NUMBER_SETTINGS " in
    *" $_n "*)
      case $_v in
        ''|*[!0-9]*) ;;
        *) while [ "${#_v}" -gt 1 ] && [ "${_v#0}" != "$_v" ]; do _v=${_v#0}; done ;;
      esac
      printf '%s' "$_v"
      return 0 ;;
  esac
  case " $PF_LOWER_SETTINGS " in
    *" $_n "*) _v=$(printf '%s' "$_v" | tr '[:upper:]' '[:lower:]') ;;
  esac
  case " $PF_LIST_SETTINGS " in
    *" $_n "*)
      _v=$(printf '%s\n' "$_v" | awk '{ n = split($0, p, ","); out = ""; for (i = 1; i <= n; i++) { gsub(/^[ \t]+/, "", p[i]); gsub(/[ \t]+$/, "", p[i]); if (p[i] != "") out = out (out == "" ? "" : ",") p[i] } printf "%s", out }') ;;
  esac
  printf '%s' "$_v"
}

# pf_fold_earlier_settings: the earlier spelling of every registry name (the previous prefix and the same suffix) counts as the setting (Addendum 4, A4.5). One loop over the
# registry: a nonempty value under the earlier name is copied to the current name when that is unset or empty, with the notice names.sh prints for such a variable, and the
# earlier variable is unset, so that the deploy of the local route reads one spelling, the one the plan shows. Both spellings set to different values end the run with exit 2
# before anything is created: a stage setting is never chosen between two values, because the deploy would take an explicit value as your consent.
pf_fold_earlier_settings() {
  local _name _old _vold _vnew
  for _name in $(pf_reg_names); do
    _old=CAH_${_name#HAMH_}  # COMPAT-CAH: the earlier prefix of a stage setting
    _vold=$(printenv "$_old" 2>/dev/null || true)
    if [ -n "$_vold" ]; then
      _vnew=$(printenv "$_name" 2>/dev/null || true)
      if [ -z "$_vnew" ]; then
        export "$_name=$_vold"
        if type _hamh_notice >/dev/null 2>&1; then
          _hamh_notice "$_old is the previous name of $_name; both work in this release. Set $_name instead."
        fi
      elif [ "$(pf_canonical "$_name" "$(pf_trim "$_vnew")")" != "$(pf_canonical "$_name" "$(pf_trim "$_vold")")" ]; then
        setup_exit 2 "$_old=$_vold and $_name=$_vnew are both set and their values differ" \
          "$_old is the previous name of $_name. A stage setting is never chosen between two values: the deploy takes an exported value as your choice." \
          "Unset one of the two, then run setup.sh again." "Nothing was changed."
      fi
    fi
    unset "$_old"
  done
  return 0
}

# pf_explicit_settings: every registry setting you exported (a nonempty HAMH_<NAME>; pf_fold_earlier_settings has folded the earlier spelling into it), each trimmed (pf_trim)
# and checked against its pattern before the question (the word `default` returns to the code default), then put into the one form the deploy records (pf_canonical; A4.8).
# SETUP_EXPLICIT_LINES holds them as HAMH_<NAME>=value lines in the registry's order, each ending in a newline, and
# SETUP_EXPLICIT_SETTINGS the same text in base64 (setup_base64), the one override the runner route passes to its build (rn_start); the local route exports the lines into its
# deploy (full_export_settings). These two variables are the whole chain: they are emptied here first, so nothing your shell holds under their names counts. An exported HAMH_*
# name of a family of the registry (HAMH_INSTANCES_TYPES with its last letter missing) that is no setting and no documented control is refused too, because the deploy would
# ignore it.
pf_explicit_settings() {
  local _names _name _value _all _e _rest _fam _known
  SETUP_EXPLICIT_LINES=""
  SETUP_EXPLICIT_SETTINGS=""
  [ "$REG_HAVE" = 1 ] || return 0
  pf_fold_earlier_settings
  _names=$(pf_reg_names)
  for _name in $_names; do
    _value=$(pf_trim "$(printenv "$_name" 2>/dev/null || true)")
    [ -n "$_value" ] || continue
    if [ "$_value" != default ]; then
      if ! setup_ere_match "$_value" "$(pf_reg_get "$_name" pattern)"; then
        setup_exit 2 "$_name=$_value is not a value this setting takes" "$_name: $(pf_reg_get "$_name" note)" \
          "Export a value of that form, $_name=default to return to the default, or unset it, then run setup.sh again." "Nothing was changed."
      fi
      _value=$(pf_canonical "$_name" "$_value")
    fi
    SETUP_EXPLICIT_LINES="$SETUP_EXPLICIT_LINES$_name=$_value$NL"
  done
  _all=$(compgen -e)
  for _e in $_all; do
    case $_e in
      HAMH_?*_?*) ;;
      *) continue ;;
    esac
    case " $PF_CONTROLS " in *" $_e "*) continue ;; esac
    case "$NL$_names$NL" in *"$NL$_e$NL"*) continue ;; esac
    # an empty value is no setting (A2.1), and neither is a look-alike that holds nothing
    [ -n "$(printenv "$_e" 2>/dev/null || true)" ] || continue
    _rest=${_e#HAMH_}
    _fam=HAMH_${_rest%%_*}_
    _known=$(printf '%s\n' "$_names" | awk -v f="$_fam" 'index($0, f) == 1 { printf "%s%s", (n++ ? ", " : ""), $0 }')
    [ -n "$_known" ] || continue
    setup_exit 2 "$_e is exported, and it is no setting of the stage: the deploy would not read it" \
      "The settings of this kind are $_known. Unset $_e, or export the setting you meant, then run setup.sh again." \
      "Nothing was changed."
  done
  [ -n "$SETUP_EXPLICIT_LINES" ] || return 0
  SETUP_EXPLICIT_SETTINGS=$(printf '%s' "$SETUP_EXPLICIT_LINES" | setup_base64) || setup_exit 2 "an exported stage setting holds a character that is not printable ASCII" \
    "The settings you exported: $(printf '%s' "$SETUP_EXPLICIT_LINES" | tr '\n' ' ')" "Export plain ASCII values, then run setup.sh again." "Nothing was changed."
  return 0
}

# pf_explicit_get <name>: the value you exported for a registry setting, nothing when you did not.
pf_explicit_get() {
  printf '%s' "$SETUP_EXPLICIT_LINES" | awk -v k="$1" 'index($0, k "=") == 1 { print substr($0, length(k) + 2); exit }'
}

# pf_record: the record the deploy keeps of the stage, the SSM parameter /hamh/<stage>/settings (key=value lines: status, commit, tree, version, accounts, operator, azs, updated and a
# set.HAMH_<NAME> line for each setting a user chose). A stage deployed before the record existed has none (REC_HAVE 0); a read that fails ends the run before anything is created.
pf_record() {
  REC_HAVE=0
  REC_TEXT=""
  if aws_run ssm get-parameter --name "/hamh/$STAGE/settings" --query Parameter.Value --output text; then
    case $AWS_OUT in
      ''|None) return 0 ;;
    esac
    REC_TEXT=$AWS_OUT
    REC_HAVE=1
    return 0
  fi
  aws_missing && return 0
  setup_exit 1 "could not read the settings record of the stage, /hamh/$STAGE/settings: ${AWS_ERR:-no answer}" "Check that your credentials may call ssm:GetParameter. Nothing was changed."
}

# pf_rec_get <key>: one value of the record (the text after the first = of its line), nothing when the record has none.
pf_rec_get() {
  printf '%s\n' "$REC_TEXT" | awk -v k="$1" 'index($0, k "=") == 1 { print substr($0, length(k) + 2); exit }'
}

# pf_instances_part: INSTANCES_PRESENT 1 when the stage has the Instances part: its runtimes stack keeps the id of the capacity provider in /hamh/<stage>/instances-capacity-provider-id.
pf_instances_part() {
  INSTANCES_PRESENT=0
  [ "$STAGE_STATE" != none ] || return 0
  if aws_run ssm get-parameter --name "/hamh/$STAGE/instances-capacity-provider-id" --query Parameter.Value --output text; then
    case $AWS_OUT in
      ''|None) ;;
      *) INSTANCES_PRESENT=1 ;;
    esac
    return 0
  fi
  aws_missing && return 0
  setup_exit 1 "could not read whether the stage has the Instances part (/hamh/$STAGE/instances-capacity-provider-id): ${AWS_ERR:-no answer}" \
    "Check that your credentials may call ssm:GetParameter. Nothing was changed."
}

# pf_live_instances: the values of the Instances machine settings that the stage runs, read once from the deployed template of the runtimes stack (the properties of its
# capacity provider, InstancesCapacityProvider, the ones deploy.py's live reader reads too): PF_LIVE_FLAT, the json_flat lines of
# [types, root free GiB, workspace GiB, lifetime, idle timeout]. Returns 1, with PF_HOLD_WHY, when the template cannot be read or holds no provider.
pf_live_instances() {
  [ "$PF_LIVE_READ" = 0 ] || { [ -n "$PF_LIVE_FLAT" ]; return; }
  PF_LIVE_READ=1
  PF_LIVE_FLAT=""
  if ! aws_run cloudformation get-template --stack-name "hamh-$STAGE-runtimes" \
      --query 'TemplateBody.Resources.InstancesCapacityProvider.Properties.ComputeConfiguration.Ec2Configuration.[LaunchTemplateSource.LaunchParameters.InstanceRequirements.AllowedInstanceTypes, RootVolume.FreeSpaceGiB, Volumes[0].EbsConfiguration.SizeGiB, LifecycleConfiguration.MaxLifetime, LifecycleConfiguration.IdleInstanceTimeout]' \
      --output json; then
    PF_HOLD_WHY="the deployed template of the stack hamh-$STAGE-runtimes could not be read: $(printf '%s' "${AWS_ERR:-no answer}" | head -n 1)"
    return 1
  fi
  PF_LIVE_FLAT=$(printf '%s\n' "$AWS_OUT" | json_flat 2>/dev/null) || PF_LIVE_FLAT=""
  if [ -z "$(cl_flat_get "$PF_LIVE_FLAT" 1)" ]; then
    PF_LIVE_FLAT=""
    PF_HOLD_WHY="the deployed template of the stack hamh-$STAGE-runtimes holds no capacity provider setup can read"
    return 1
  fi
  return 0
}

# pf_live_get <name>: the value the live stage runs for a setting of PF_LIVE_READERS, in the form of the deploy (pf_canonical), from the read pf_live_instances made (call that
# first, in the shell that keeps its answer). Returns 2 for a setting without a reader and 1 when the provider names no value for it.
pf_live_get() {
  local _n=0 _t _all=""
  case " $PF_LIVE_READERS " in
    *" $1 "*) ;;
    *) return 2 ;;
  esac
  pf_live_instances || return 1
  case $1 in
    HAMH_INSTANCES_TYPES)
      while [ "$_n" -lt 30 ]; do
        _t=$(cl_flat_get "$PF_LIVE_FLAT" "0.$_n")
        [ -n "$_t" ] || break
        _all=${_all:+$_all,}$_t
        _n=$((_n + 1))
      done ;;
    HAMH_INSTANCES_ROOT_FREE_GIB) _all=$(cl_flat_get "$PF_LIVE_FLAT" 1) ;;
    HAMH_INSTANCES_WORKSPACE_GIB) _all=$(cl_flat_get "$PF_LIVE_FLAT" 2) ;;
    HAMH_INSTANCES_LIFETIME_S) _all=$(cl_flat_get "$PF_LIVE_FLAT" 3) ;;
    HAMH_INSTANCES_IDLE_S) _all=$(cl_flat_get "$PF_LIVE_FLAT" 4) ;;
  esac
  case $_all in
    ''|null) return 1 ;;
  esac
  pf_canonical "$1" "$_all"
}

# pf_needs_consent <class> <old> <new>: whether going from old to new needs your consent, as deploy.py's needs_consent says: any change of the class replace, a lower
# number of the class decrease, and no value or off of the class remove.
pf_needs_consent() {
  case $1 in
    replace) return 0 ;;
    decrease)
      case "$2" in ''|*[!0-9]*) return 1 ;; esac
      case "$3" in ''|*[!0-9]*) return 1 ;; esac
      [ "$3" -lt "$2" ] ;;
    remove) [ -z "$3" ] || [ "$3" = off ] ;;
    *) return 1 ;;
  esac
}

# pf_default_holds: an update of a stage that has a record. For each setting of a consent class (replace, decrease, remove) that the stage keeps (KEPT yes), that you did not
# export and that the record has no line for (the stage follows the default): when the value the stage runs differs from this commit's default and taking the default needs
# consent, the deploy keeps the stage's value instead, and PF_HOLDS gets one line NAME|kept value|default|class for the plan. A setting
# the plan has no reader for is left to the deploy, which says it on its own DEPLOY-SETTING line; a value that cannot be read leaves PF_HOLD_WHY and PF_HOLD_NAMES.
# A record of an update that did not finish (status=deploying) is read as deploy.py earlier_intent and legacy_deploying read it: a setting its explicit= line names was given
# to that update, and the deploy builds what the record holds for it (its set. line, else the default it was given), which PF_TAKES names (NAME|stage value|value
# built|class|value given) when it replaces or deletes something; a record that an earlier release wrote has no explicit= line, each set. line it holds is that choice,
# and for a setting it has no line for the deploy keeps nothing and asks for your consent: PF_ASKS (NAME|stage value|default|class).
pf_default_holds() {
  local _name _class _def _live _line _intent _given _target _named="" _legacy=0
  PF_HOLDS=""
  PF_TAKES=""
  PF_ASKS=""
  PF_REC_DEPLOYING=0
  PF_HOLD_WHY=""
  PF_HOLD_NAMES=""
  { [ "$REG_HAVE" = 1 ] && [ "$DEF_HAVE" = 1 ] && [ "$REC_HAVE" = 1 ]; } || return 0
  if [ "$(pf_rec_get status)" = deploying ]; then
    PF_REC_DEPLOYING=1
    if printf '%s\n' "$REC_TEXT" | grep -q '^explicit='; then
      _named=",$(pf_rec_get explicit),"
    else
      _legacy=1
    fi
  fi
  for _name in $(pf_reg_names); do
    _class=$(pf_reg_get "$_name" change)
    case $_class in
      replace|decrease|remove) ;;
      *) continue ;;
    esac
    [ "$(pf_reg_get "$_name" kept)" = yes ] || continue
    [ -z "$(pf_explicit_get "$_name")" ] || continue
    _line=0
    printf '%s\n' "$REC_TEXT" | grep -q "^set\.$_name=" && _line=1
    _intent=0
    case $_named in
      *",$_name,"*) _intent=1 ;;
    esac
    [ "$_legacy" = 0 ] || [ "$_line" = 0 ] || _intent=1
    # a value the record holds is the stage's choice; it is compared with the stage here only when it is the choice of an update that did not finish
    [ "$_line" = 0 ] || [ "$_intent" = 1 ] || continue
    _def=$(pf_def_get "$_name") || continue
    case " $PF_LIVE_READERS " in
      *" $_name "*)
        # an Instances machine setting has a value only on a stage with the part; the one read of the provider runs in this shell, so its answer is kept
        [ "$INSTANCES_PRESENT" = 1 ] || continue
        if ! pf_live_instances; then
          PF_HOLD_NAMES=${PF_HOLD_NAMES:+$PF_HOLD_NAMES, }$_name
          continue
        fi ;;
      *) continue ;;
    esac
    if ! _live=$(pf_live_get "$_name"); then
      PF_HOLD_NAMES=${PF_HOLD_NAMES:+$PF_HOLD_NAMES, }$_name
      [ -n "$PF_HOLD_WHY" ] || PF_HOLD_WHY="the capacity provider of the stack hamh-$STAGE-runtimes names no value for $_name"
      continue
    fi
    if [ "$_line" = 1 ]; then
      _given=$(pf_rec_get "set.$_name")
    else
      _given=default
    fi
    case $_given in
      default) _target=$(pf_canonical "$_name" "$_def") ;;
      *) _target=$(pf_canonical "$_name" "$_given") ;;
    esac
    [ "$_live" != "$_target" ] || continue
    pf_needs_consent "$_class" "$_live" "$_target" || continue
    if [ "$_intent" = 1 ]; then
      PF_TAKES="$PF_TAKES$_name|$_live|$_target|$_class|$_given$NL"
    elif [ "$_legacy" = 1 ]; then
      PF_ASKS="$PF_ASKS$_name|$_live|$_target|$_class$NL"
    else
      PF_HOLDS="$PF_HOLDS$_name|$_live|$_target|$_class$NL"
    fi
  done
  return 0
}

# pf_settings_refuse: a setting of the class refuse-off (HAMH_COMPUTE_INSTANCES) that you exported as anything but on, while the stage has that part: removing it ends every machine and
# session on it, and a deploy does not do that, so the run ends here with exit 7 and nothing created.
pf_settings_refuse() {
  local _name _value _word
  [ "$INSTANCES_PRESENT" = 1 ] && [ -n "$SETUP_EXPLICIT_LINES" ] || return 0
  for _name in $(pf_reg_names); do
    [ "$(pf_reg_get "$_name" change)" = refuse-off ] || continue
    _value=$(pf_explicit_get "$_name")
    [ -n "$_value" ] || continue
    _word=$(printf '%s' "$_value" | tr '[:upper:]' '[:lower:]' | tr -d ' \t')
    [ "$_word" != on ] || continue
    setup_exit 7 "$_name=$_value is exported, and the stage $STAGE has the Instances part (its capacity provider and the second worker runtime)" \
      "Removing the part ends every machine and session on it, so an update does not do it; the stage teardown is the step that removes it." \
      "Unset $_name (the update keeps the part) or export $_name=on, then run setup.sh again." "Nothing was changed."
  done
  return 0
}

# pf_principal <ARN>: PR_ACCOUNT, PR_KIND (role or user) and PR_NAME of an IAM role or user ARN (a path is dropped); returns 1 for any other text.
pf_principal() {
  local rest
  PR_ACCOUNT=""
  PR_KIND=""
  PR_NAME=""
  case $1 in
    arn:aws:iam::*:role/?*|arn:aws:iam::*:user/?*) ;;
    *) return 1 ;;
  esac
  rest=${1#arn:aws:iam::}
  PR_ACCOUNT=${rest%%:*}
  rest=${rest#*:}
  PR_KIND=${rest%%/*}
  PR_NAME=${rest##*/}
  [ -n "$PR_ACCOUNT" ] && [ -n "$PR_NAME" ]
}

# pf_commit_order: an update to a commit older than the one the stage records (an ancestor of it), or to one git cannot place against it (the recorded commit is not in this clone),
# ends here with exit 7 unless --revision says it is meant; COMMIT_CHANGE is the change the plan names. A target with the source tree the record names is never refused
# (Addendum 4, A4.1): it deploys nothing the stage does not run already, whatever the two commits are (a commit that changes no deployed file, or a record whose commit= is
# the one a runner build before A4.1 made of the archive, which no clone holds).
pf_commit_order() {
  local rec ver
  COMMIT_CHANGE=""
  [ "$REC_HAVE" = 1 ] || return 0
  rec=$(pf_rec_get commit)
  setup_is_commit "$rec" || return 0
  ver=$(pf_rec_get version)
  setup_safe_word "$ver" || ver=""
  [ "$rec" != "$SRC_COMMIT" ] || return 0
  COMMIT_CHANGE="$(rn_first12 "$rec")${ver:+ (VERSION $ver)} -> $(rn_first12 "$SRC_COMMIT")${SRC_VERSION_TEXT:+ (VERSION $SRC_VERSION_TEXT)}"
  [ -z "${REVISION:-}" ] || return 0
  [ "$(pf_rec_get tree)" != "$(rn_target_tree)" ] || return 0
  if git -C "$SELF_DIR" cat-file -e "$rec^{commit}" 2>/dev/null; then
    git -C "$SELF_DIR" merge-base --is-ancestor "$SRC_COMMIT" "$rec" 2>/dev/null || return 0
    setup_exit 7 "the stage $STAGE runs commit $rec${ver:+ (VERSION $ver)}, and this clone would update it to the older commit $SRC_COMMIT${SRC_VERSION_TEXT:+ (VERSION $SRC_VERSION_TEXT)}" \
      "Bring your clone up to date first:  git pull   then run ./setup.sh --update again." \
      "To go back to the older commit on purpose, name it:  ./setup.sh --update --revision $SRC_COMMIT" "Nothing was changed."
  fi
  setup_exit 7 "the stage $STAGE runs commit $rec${ver:+ (VERSION $ver)}, which is not in this clone, so setup cannot tell whether $SRC_COMMIT${SRC_VERSION_TEXT:+ (VERSION $SRC_VERSION_TEXT)} is newer" \
    "Bring your clone up to date first:  git pull   then run ./setup.sh --update again." \
    "To deploy this commit anyway, name it:  ./setup.sh --update --revision $SRC_COMMIT" "Nothing was changed."
}

# pf_pin_of <commit> <image|cline>: the worker image key of engine/policy/image-key.pin, or the binarySha256 of the [cline] table of engine/lock.toml, at that commit; nothing when
# the commit lacks the file or the line.
pf_pin_of() {
  if [ "$2" = image ]; then
    git -C "$SELF_DIR" show "$1:engine/policy/image-key.pin" 2>/dev/null | awk '/^[0-9a-f]+$/ && length($0) == 64 { print; exit }'
  else
    git -C "$SELF_DIR" show "$1:engine/lock.toml" 2>/dev/null | awk '
      /^\[/ { c = ($0 == "[cline]"); next }
      c && $1 == "binarySha256" { v = $0; sub(/^[^"]*"/, "", v); sub(/".*$/, "", v); print v; exit }'
  fi
}

# pf_image_plan: IMG_PLAN none (the worker image key and the Cline binary are the ones the stage runs), image (a new worker image, the binary reused), fork (the Cline binary is
# rebuilt too) or unknown (the stage records no commit, the commit is not in this clone, or a pin cannot be read), compared between the recorded commit and the one that is built.
pf_image_plan() {
  local rec k1 k2 b1 b2
  IMG_PLAN=unknown
  IMG_TEXT=""
  [ "$REC_HAVE" = 1 ] || return 0
  rec=$(pf_rec_get commit)
  setup_is_commit "$rec" || return 0
  git -C "$SELF_DIR" cat-file -e "$rec^{commit}" 2>/dev/null || return 0
  k1=$(pf_pin_of "$rec" image)
  k2=$(pf_pin_of "$SRC_COMMIT" image)
  b1=$(pf_pin_of "$rec" cline)
  b2=$(pf_pin_of "$SRC_COMMIT" cline)
  [ -n "$k1" ] && [ -n "$k2" ] && [ -n "$b1" ] && [ -n "$b2" ] || return 0
  if [ "$b1" != "$b2" ]; then
    IMG_PLAN=fork
    IMG_TEXT="a new worker image, and the Cline binary is rebuilt (its pin changes from $(rn_first12 "$b1") to $(rn_first12 "$b2"))"
  elif [ "$k1" != "$k2" ]; then
    IMG_PLAN=image
    IMG_TEXT="a new worker image (the image key changes from $(rn_first12 "$k1") to $(rn_first12 "$k2")); the Cline binary is reused"
  else
    IMG_PLAN=none
    IMG_TEXT="no new worker image: the image key and the Cline binary are the ones the stage runs"
  fi
}

# pf_image_refuse: with --allow-active-runs and a run active, an update that needs a new worker image would be refused by the build's own guard after the long build of the Lambda
# functions; it is refused here instead, before anything is created.
pf_image_refuse() {
  [ "${ALLOW_ACTIVE_RUNS:-0}" = 1 ] && [ "$ACTIVE_RUN_COUNT" -gt 0 ] || return 0
  case $IMG_PLAN in
    image|fork) ;;
    *) return 0 ;;
  esac
  setup_exit 7 "the update needs $IMG_TEXT, and $ACTIVE_RUN_COUNT run(s) are running on hamh-$STAGE-run: $ACTIVE_RUNS" \
    "A new worker image changes the worker runtime under those runs, so the build's guard would refuse it after the build of the Lambda functions. --allow-active-runs lets through only an update that keeps the worker runtime." \
    "You cancel runs with $HAMH_CLIENT_BIN cancel <runId> or wait until they end, then run ./setup.sh --update again." "Nothing was changed."
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
  pf_record_identity "$zones"
  # the zone of a stage without a record is kept also when the Region does not list it as available now (pf_live_zone)
  [ -z "$PF_ZONE_EXTRA" ] || zones="$zones$NL$PF_ZONE_EXTRA"
  acct_generate "$ACCOUNT" "$REGION" "$file_profile" "$zones" "$AZS" "$INV_LOGGING" "$CALLER_ARN" "$names" "$PF_WANT_ZONES" || exit 1
  ok "zone $(printf '%s' "$ACCT_DIRECT_AZ" | cut -d, -f1) (${ZONE_FROM:-the AgentCore zone id decides; the zone name differs between accounts})"
}

# pf_live_operator: PR_ACCOUNT, PR_KIND and PR_NAME of the operator the deployed stage trusts, read from the stage itself (Addendum 4, A4.6): the variable OPERATOR_PRINCIPALS
# of the control function in the deployed template of the stack hamh-<stage>-control, with one read-only get-template call. The CDK app writes the caller ARN patterns of the
# account entry's operator there (infra/lib/operator.ts): arn:aws:sts::<account>:assumed-role/<role>/* for a role, which is the IAM role Admin for an entry without an
# operator, and arn:aws:iam::<account>:user/<name> first for a user. Returns 1, with PF_OP_WHY, when the template cannot be read, names no operator or holds another form.
pf_live_operator() {
  local first rest
  PR_ACCOUNT=""
  PR_KIND=""
  PR_NAME=""
  PF_OP_WHY=""
  if ! aws_run cloudformation get-template --stack-name "hamh-$STAGE-control" \
      --query 'TemplateBody.Resources.ControlApi.Properties.Environment.Variables.OPERATOR_PRINCIPALS' --output text; then
    PF_OP_WHY="the deployed template of the stack hamh-$STAGE-control, which names the operator the stage trusts, could not be read"
    return 1
  fi
  first=${AWS_OUT%%,*}
  case $first in
    "arn:aws:sts::"*":assumed-role/"?*"/*")
      rest=${first#arn:aws:sts::}
      PR_ACCOUNT=${rest%%:*}
      rest=${rest#*:assumed-role/}
      PR_NAME=${rest%/\*}
      PR_KIND=role ;;
    "arn:aws:iam::"*":user/"?*)
      rest=${first#arn:aws:iam::}
      PR_ACCOUNT=${rest%%:*}
      PR_NAME=${rest#*:user/}
      PR_KIND=user ;;
  esac
  case $PR_NAME in
    ''|*[!A-Za-z0-9+=.@_-]*) PR_KIND="" ;;
  esac
  case $PR_ACCOUNT in
    ''|*[!0-9]*) PR_KIND="" ;;
  esac
  if [ -z "$PR_KIND" ]; then
    PR_ACCOUNT=""
    PR_NAME=""
    PF_OP_WHY="the control function of the stage names no operator setup can read (OPERATOR_PRINCIPALS in the deployed template of the stack hamh-$STAGE-control)"
    return 1
  fi
  return 0
}

# pf_record_identity <zones>: an update keeps the operator and the zone the stage has (C9). The operator is the one the stage's record names, or, for a stage without a record
# and for a record with an empty operator= line, the one the deployed stage trusts (pf_live_operator; A4.6); OP_FROM says which (record, live, or nothing when the stage names
# none: the caller then becomes the operator and the plan says so with PF_OP_WHY). A caller who is another operator stops the run with exit 7 unless --change-operator says the
# stage should trust the caller (OP_CHANGE is then the change the plan shows). PF_WANT_ZONES is the zone ids for the account file (empty: the lowest zone, as for a new stage):
# the zones of the record, kept also when the Region does not list one as available now (A4.11; the subnets of the stage's network then name it, and a network that runs
# elsewhere stops the run with exit 7, because either zone would replace subnets). A stage without a record keeps the zone of its network (pf_live_zone).
PF_WANT_ZONES=""
pf_record_identity() {
  local zones=$1 rec_op="" old new _ids _id trusts _missing=""
  OP_CHANGE=""
  OP_FROM=""
  ZONE_FROM=""
  ZONE_NOTE=""
  ZONE_LIVE=0
  PF_WANT_ZONES=""
  PF_ZONE_EXTRA=""
  [ "${FULL_ACTION:-}" = update ] || return 0
  [ "$REC_HAVE" != 1 ] || rec_op=$(pf_rec_get operator)
  if [ -n "$rec_op" ] && pf_principal "$rec_op"; then
    OP_FROM=record
    trusts="was deployed with the operator"
  elif pf_live_operator; then
    OP_FROM=live
    trusts="trusts the operator"
  fi
  if [ -n "$OP_FROM" ]; then
    acct_operator "$CALLER_ARN" "$ACCOUNT" || exit 1
    if [ "$PR_ACCOUNT $PR_KIND $PR_NAME" != "$ACCOUNT $ACCT_OP_KIND $ACCT_OP_NAME" ]; then
      old="$PR_KIND $PR_NAME"
      new="$ACCT_OP_KIND $ACCT_OP_NAME"
      [ "$PR_ACCOUNT" = "$ACCOUNT" ] || old="$old of account $PR_ACCOUNT"
      if [ "${CHANGE_OPERATOR:-0}" != 1 ]; then
        setup_exit 7 "the stage $STAGE $trusts $old, and your credentials are $new ($CALLER_ARN)" \
          "An update as another identity moves the stage's trust to it: $old would lose the admin routes and the deletes of the Go module store." \
          "Run the update as $old, or add --change-operator to make $new the operator of the stage." "Nothing was changed."
      fi
      OP_CHANGE="$old -> $new"
    fi
  fi
  if [ "$REC_HAVE" != 1 ]; then
    pf_live_zone "$zones"
    return 0
  fi
  _ids=$(pf_rec_get azs | tr ',' ' ')
  for _id in $_ids; do
    if ! printf '%s\n' "$zones" | awk -F'\t' -v z="$_id" '$2 == z { found = 1 } END { exit found ? 0 : 1 }'; then
      _missing="${_missing:+$_missing }$_id"
    fi
    PF_WANT_ZONES="${PF_WANT_ZONES:+$PF_WANT_ZONES }$_id"
  done
  [ -z "$PF_WANT_ZONES" ] || ZONE_FROM="the zone of the stage's record"
  [ -n "$_missing" ] || return 0
  # The Region does not list a zone of the record as available now. The stage keeps it all the same, as a stage without a record does (A4.11): another zone replaces the
  # stage's subnets and the Instances capacity provider with its machines. The account file needs the zone's name, which the record does not hold and the list of available
  # zones lacks: the subnets of the stage's network name it.
  PF_WANT_ZONES=""
  pf_live_zone "$zones"
  for _id in $_ids; do
    case " $PF_WANT_ZONES " in
      *" $_id "*) ;;
      *) setup_exit 7 "the stage $STAGE runs in the zone $_id (its record), $REGION does not list that zone as available now, and the subnets of its network hamh-$STAGE-network do not name it${PF_WANT_ZONES:+ (they are in $PF_WANT_ZONES)}" \
           "An update in another zone would replace the stage's subnets, so it does not start. Look at the stack hamh-$STAGE-network in the CloudFormation console, or run ./setup.sh --update again when AWS lists the zone as available." "Nothing was changed." ;;
    esac
  done
  ZONE_NOTE="kept from the stage's record ($REGION does not list it as available now; the subnets of the stage's network name it)"
  return 0
}

# pf_live_zone <zones>: an update of a stage without a record keeps the zone its network runs in (CONTRACT Addendum 3, A3.2): the distinct
# zones of the subnets that CloudFormation tagged with the stack hamh-<stage>-network, kept also when the Region does not list the zone as available now (PF_ZONE_EXTRA adds it
# to the zone list), because another zone replaces the stage's subnets and the Instances capacity provider with its machines. A network with no subnet takes the lowest zone,
# as a new stage does; a network in more zones than the stage takes stops the run with exit 7, before anything is created.
PF_ZONE_EXTRA=""
ZONE_LIVE=0
ZONE_NOTE=""
pf_live_zone() {
  local zones=$1 pairs line name zid n=0 seen=""
  if ! aws_run ec2 describe-subnets --filters "Name=tag:aws:cloudformation:stack-name,Values=hamh-$STAGE-network" \
    --query 'Subnets[].[AvailabilityZone,AvailabilityZoneId]' --output text; then
    setup_exit 1 "could not read the subnets of the stack hamh-$STAGE-network: ${AWS_ERR:-no answer}" \
      "Check that your credentials may call ec2:DescribeSubnets, then run setup.sh again. The update keeps the zone the stage's network runs in." "Nothing was changed."
  fi
  pairs=$(printf '%s\n' "$AWS_OUT" | awk -F'\t' 'NF == 2 && $1 != "" && $1 != "None" && $2 != "None" { print $1 "\t" $2 }' | sort -u -t "$TAB" -k2,2)
  [ -n "$pairs" ] || return 0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    zid=${line#*"$TAB"}
    n=$((n + 1))
    seen="${seen:+$seen, }$name ($zid)"
    PF_WANT_ZONES="${PF_WANT_ZONES:+$PF_WANT_ZONES }$zid"
    if ! printf '%s\n' "$zones" | awk -F'\t' -v z="$zid" '$2 == z { found = 1 } END { exit found ? 0 : 1 }'; then
      PF_ZONE_EXTRA="$PF_ZONE_EXTRA$name$TAB$zid$NL"
    fi
  done <<EOF
$pairs
EOF
  if [ "$n" -gt "$AZS" ]; then
    setup_exit 7 "the network of the stage $STAGE (hamh-$STAGE-network) has subnets in $n zones: $seen; the stage takes $AZS" \
      "An update in other zones would replace the stage's subnets, so it does not start. Look at the stack in the CloudFormation console." "Nothing was changed."
  fi
  ZONE_FROM="kept from the stage's network"
  ZONE_LIVE=1
  return 0
}

# pf_installer: who may write the runner bucket besides the runner role (the identity behind these credentials, as the two patterns of aws:PrincipalArn that
# match it), how long the runner role may assume the CDK roles after an install, and the length and machine of the runner build. A build takes 240 minutes at
# most unless HAMH_SETUP_BUILD_MINUTES (30 to 480) says otherwise, so the authority must outlast it by an hour: a length under the build's whole hours plus one (never under
# 5) would end a build in the middle. BUILD_MINUTES and BUILD_COMPUTE_TYPE stay empty when no setting is given, and the install then leaves the runner stack's own values.
pf_installer() {
  local hours=${HAMH_SETUP_AUTHORITY_HOURS:-12} pats minutes=${HAMH_SETUP_BUILD_MINUTES:-} compute=${HAMH_SETUP_BUILD_COMPUTE:-} least=5 build_max=240
  case $minutes in
    "") ;;
    0*|*[!0-9]*) minutes=bad ;;
  esac
  if [ -n "$minutes" ]; then
    if [ "$minutes" = bad ] || [ "$minutes" -lt 30 ] || [ "$minutes" -gt 480 ]; then
      setup_exit 2 "HAMH_SETUP_BUILD_MINUTES must be a whole number of minutes from 30 to 480 (got '${HAMH_SETUP_BUILD_MINUTES:-}')" \
        "It is the longest the build in your account may run. The default is 240 minutes." "Nothing was created."
    fi
    BUILD_MINUTES=$minutes
    build_max=$minutes
    least=$(((minutes + 59) / 60 + 1))
    [ "$least" -ge 5 ] || least=5
  fi
  if [ -n "$compute" ]; then
    compute=$(printf '%s' "$compute" | tr '[:upper:]' '[:lower:]')
    case $compute in
      large | xlarge | 2xlarge) BUILD_COMPUTE_TYPE=BUILD_GENERAL1_$(printf '%s' "$compute" | tr '[:lower:]' '[:upper:]') ;;
      *) setup_exit 2 "HAMH_SETUP_BUILD_COMPUTE must be large, xlarge or 2xlarge (got '${HAMH_SETUP_BUILD_COMPUTE:-}')" \
           "It is the size of the machine that builds in your account. The default is large." "Nothing was created." ;;
    esac
  fi
  case $hours in
    *[!0-9]*) hours=bad ;;
  esac
  if [ "$hours" = bad ] || [ "$hours" -lt "$least" ] || [ "$hours" -gt 72 ]; then
    setup_exit 2 "HAMH_SETUP_AUTHORITY_HOURS must be a whole number of hours from $least to 72 (got '${HAMH_SETUP_AUTHORITY_HOURS:-}')" \
      "It is how long the runner role can assume the CDK roles after each install. A build takes up to $build_max minutes (HAMH_SETUP_BUILD_MINUTES), so the length is at least the build's hours plus one." "Nothing was created."
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
  local url_step step_opts url_hint="" only="" next="./setup.sh --full to deploy a new stage under the current names"
  [ "$LEGACY_STATE" != none ] || return 0
  if [ "$STAGE_STATE" != none ]; then
    # a stage under the current names exists beside it: the teardown must cover only the previous names, or following the steps would delete the current stage too
    only=" --only legacy"
    next="./setup.sh --${FULL_ACTION:-full} again: the stage under the current names stays as it is"
  fi
  if [ "$LEGACY_STATE" = legacy-retained ]; then
    # nothing of the previous stage runs any more: only what an earlier teardown retained is left, so there is no endpoint to move to and no run to pull
    setup_exit 7 "$(pf_legacy_head)" \
      "This attempt created or changed nothing." \
      "1. Run ./setup.sh --teardown$only to remove what is left. If it reports RETAINED, run it again after AWS has released the network interface." \
      "2. Run $next." \
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
    "3. Run ./setup.sh --teardown$only to remove the previous stage." \
    "4. Run $next." \
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
  local shown="" n=0 name more="" hint=""
  pf_active_runs
  [ "$ACTIVE_RUN_COUNT" -gt 0 ] || return 0
  for name in $ACTIVE_RUNS; do
    n=$((n + 1))
    [ "$n" -gt 20 ] || shown=${shown:+$shown }$name
  done
  [ "$n" -le 20 ] || more=" (+$((n - 20)) more)"
  hint=$(pf_retry_hint)
  setup_exit 7 "the update is stopped: $ACTIVE_RUN_COUNT running run(s) on hamh-$STAGE-run pin the worker runtime: $shown$more" \
    "The stage is unchanged and no runner was installed." \
    "You cancel runs with $HAMH_CLIENT_BIN cancel <runId> or wait until they end." \
    ${hint:+"$hint"} "You run ./setup.sh --update again."
}

# pf_retry_hint: the sentence the active-runs refusal adds when the last update of this commit (the receipt of this machine) deployed the stage and then failed in a phase after
# the deploy: --allow-active-runs lets the retry go on while the runs keep running, because the build's own guard refuses
# only an update that changes the worker runtime. Nothing in any other case.
pf_retry_hint() {
  local phase
  type st_get >/dev/null 2>&1 || return 0
  [ "$(st_get sourceCommit)" = "${SRC_COMMIT:-}" ] || return 0
  phase=$(st_get failedPhase)
  case $phase in
    gomod|scanner-rules|client|outputs)
      printf '%s' "The last update of this commit deployed the stage and then failed in its $phase phase. ./setup.sh --update --allow-active-runs finishes it while the runs keep running: the build's deploy guard then refuses only an update that changes the worker runtime." ;;
  esac
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

# pf_bootstrap_min: the commit that is built names the lowest CDK bootstrap version its deploy works with (scripts/lib/bootstrap-min-version.txt, one integer line; A2.6). An account
# whose bootstrap is older would fail in the build, so the run ends here with exit 7 and the upgrade command. A commit without the file, or an account without a bootstrap (setup
# makes one from the pinned template), has nothing to compare.
pf_bootstrap_min() {
  local min
  case $BOOT_VERSION in
    ''|*[!0-9]*) return 0 ;;
  esac
  min=$(git -C "$SELF_DIR" show "$SRC_COMMIT:scripts/lib/bootstrap-min-version.txt" 2>/dev/null | awk 'NF { print $1; exit }')
  case $min in
    ''|*[!0-9]*) return 0 ;;
  esac
  [ "$BOOT_VERSION" -lt "$min" ] || return 0
  setup_exit 7 "your account's CDK bootstrap in $REGION is version $BOOT_VERSION (the SSM parameter $SETUP_BOOTSTRAP_PARAM), and commit $SRC_COMMIT needs version $min or later" \
    "setup.sh never changes an existing $SETUP_BOOTSTRAP_STACK stack. Upgrade it with the CDK CLI (it needs Node.js):  cdk bootstrap --qualifier $SETUP_BOOTSTRAP_QUALIFIER aws://$ACCOUNT/$REGION" \
    "then run setup.sh again." "Nothing was changed."
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
