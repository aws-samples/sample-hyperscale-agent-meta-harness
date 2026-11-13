# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): the shared definitions of the full setup. Definitions only; sourcing prints nothing and runs nothing.
# Compatible with bash 3.2 (the bash that ships with macOS): only the constructs the old bash has.
#
# What lives here: the names of the contract in one place, the output helpers (defined only when setup.sh has not defined them, so
# a helper file can be tested on its own), the AWS CLI wrapper `awsx` that adds the profile and the Region of the run, `aws_run` that keeps the
# answer and the error text apart, a SHA-256 of stdin, a clock, and the one-line log of a run.

# ---- the names of the contract -------------------------------------------------------------------
# SETUP_RUNNER_STACK and SETUP_PROJECT_TAG are the names of the generation that setup_gen_use selected (the current one until a command selects another).
SETUP_BOOTSTRAP_STACK=CDKToolkit
SETUP_BOOTSTRAP_PARAM=/cdk-bootstrap/hnb659fds/version
SETUP_BOOTSTRAP_QUALIFIER=hnb659fds
SETUP_RUNNER_TEMPLATE=infra/setup-runner.yaml
# The pin of the CDK bootstrap template (the SHA-256 of what `cdk bootstrap --show-template` prints at the pinned aws-cdk). The template is made by the runner, not committed.
SETUP_BOOTSTRAP_PIN=engine/policy/cdk-bootstrap-template.pin
SETUP_BOOTSTRAP_MAX_BYTES=51200
SETUP_REGIONS="us-east-1 us-west-2"
# The registry of the stage settings (NAME|KEPT|CHANGE|PATTERN|NOTE, read from the commit that is built; the deploy engine owns it) and the stage's record of them.
SETUP_SETTINGS_REGISTRY=scripts/lib/stage-settings.tsv
# The code defaults of the stage settings (NAME|DEFAULT, read from the same commit; the deploy engine reads the same file): the plan of an update names a new default
# that the deploy will not take because taking it needs consent.
SETUP_SETTINGS_DEFAULTS=scripts/lib/stage-defaults.tsv
# The stack states that mean a stack still exists (DELETE_COMPLETE does not).
SETUP_LIVE_STATUSES="CREATE_IN_PROGRESS CREATE_FAILED CREATE_COMPLETE ROLLBACK_IN_PROGRESS ROLLBACK_FAILED ROLLBACK_COMPLETE DELETE_IN_PROGRESS DELETE_FAILED UPDATE_IN_PROGRESS UPDATE_COMPLETE_CLEANUP_IN_PROGRESS UPDATE_COMPLETE UPDATE_FAILED UPDATE_ROLLBACK_IN_PROGRESS UPDATE_ROLLBACK_FAILED UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS UPDATE_ROLLBACK_COMPLETE REVIEW_IN_PROGRESS IMPORT_IN_PROGRESS IMPORT_COMPLETE IMPORT_ROLLBACK_IN_PROGRESS IMPORT_ROLLBACK_FAILED IMPORT_ROLLBACK_COMPLETE"

# ---- the generations of names (COMPAT-CAH-STAGE) --------------------------------------------------------------------------------------------
# A stage is deployed under one generation of names. The current generation is the names of the contract; the legacy generation is the one that releases before the
# rename used. The legacy row below is frozen: it is written once and never edited, and it carries the marker of its shim.
# setup_gen_use <current|legacy> selects a generation and sets
#   GEN             the generation id: current or legacy
#   GEN_PREFIX      the first word of the stage's names: stacks, buckets, functions, roles and the underscore names of runtimes are <GEN_PREFIX>-<stage>-... and <GEN_PREFIX>_<stage>_...
#   GEN_ROOT        the root of the parameter, role-path and log-group names: /<GEN_PREFIX>/<stage>/...
#   GEN_PROJECT     the value of the project tag on everything the stage consists of
#   GEN_SETUP       the first words of the runner's names: the stack <GEN_SETUP>-runner, the role <GEN_SETUP>-runner-role, the bucket <GEN_SETUP>-<account>-<Region>
#   GEN_E2E         the prefix of the CodeCommit repositories of that generation
#   GEN_LABEL       the words that name the generation in a message
# and points SETUP_RUNNER_STACK and SETUP_PROJECT_TAG at it. A stage belongs to a generation only when its name prefix and its project tag are both from that generation
# (setup_gen_pair); a mixed pair is foreign and is never touched.
SETUP_GENERATIONS="current legacy"
GEN=""
setup_gen_use() {
  case $1 in
    current)
      GEN=current; GEN_PREFIX=hamh; GEN_PROJECT=hyperscale-agent-meta-harness; GEN_SETUP=hamh-setup; GEN_E2E=hamh-e2e- ;;
    legacy)
      GEN=legacy; GEN_PREFIX=cah; GEN_PROJECT=cline-agentcore-harness; GEN_SETUP=cah-setup; GEN_E2E=cah-e2e- ;;  # COMPAT-CAH-STAGE: the frozen row of the previous names
    *) return 1 ;;
  esac
  GEN_ROOT=/$GEN_PREFIX
  SETUP_RUNNER_STACK=$GEN_SETUP-runner
  SETUP_PROJECT_TAG=$GEN_PROJECT
  case $GEN in
    current) GEN_LABEL="the current names ($GEN_PREFIX-${STAGE:-direct}-*)" ;;
    *) GEN_LABEL="the previous names ($GEN_PREFIX-${STAGE:-direct}-*)" ;;
  esac
  return 0
}

# setup_gen_pair <name prefix word> <project tag value>: the generation both belong to (current or legacy), nothing and return 1 when they are from different generations
# or from neither. The legacy words are read from the legacy row by selecting it, so no old word is repeated here.
setup_gen_pair() {
  local saved=$GEN g
  for g in $SETUP_GENERATIONS; do
    setup_gen_use "$g"
    if [ "$1" = "$GEN_PREFIX" ] && [ "$2" = "$GEN_PROJECT" ]; then
      printf '%s' "$g"
      setup_gen_use "$saved"
      return 0
    fi
  done
  setup_gen_use "$saved"
  return 1
}
setup_gen_use current

TAB=$(printf '\t')
NL='
'

# ---- output helpers: used as they are when setup.sh defined them first -------------------------------------------
if ! type say >/dev/null 2>&1; then
  say() { printf '%s\n' "$*"; }
  ok() { printf 'ok    %s\n' "$*"; }
  warn() { printf 'warn  %s\n' "$*"; }
  would() { printf 'would %s\n' "$*"; }
  step() { printf '\n== %s\n' "$*"; }
  err() { printf 'setup.sh: %s\n' "$*" >&2; }
  detail() { printf '      %s\n' "$*" >&2; }
fi
if ! type fail >/dev/null 2>&1; then
  FAILS=0
  # fail <headline> [detail line ...]: a problem on stderr, with its detail lines indented under it.
  fail() {
    FAILS=$((FAILS + 1))
    printf 'FAIL  %s\n' "$1" >&2
    shift
    local line
    for line in "$@"; do printf '      %s\n' "$line" >&2; done
  }
fi

# setup_exit <code> <message> [detail line ...]: the message on stderr (setup.sh: ...), its detail lines under it, then the exit.
setup_exit() {
  local code=$1 line
  shift
  err "$1"
  shift
  for line in "$@"; do detail "$line"; done
  exit "$code"
}

# ---- a clock -----------------------------------------------------------------------------------------------------
setup_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
setup_stamp() { date -u +%Y%m%dT%H%M%SZ; }
setup_epoch() { date +%s; }
# setup_at_plus <seconds>: the UTC time that many seconds from now, as 2030-01-01T00:00:00Z (GNU date -d @N, else BSD date -r N).
setup_at_plus() {
  local secs=$1 now t
  now=$(setup_epoch)
  t=$((now + secs))
  date -u -d "@$t" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -r "$t" +%Y-%m-%dT%H:%M:%SZ
}
# setup_sleep: the pause between two looks at a build (HAMH_SETUP_POLL_SECONDS, 15 by default; the tests set 0).
setup_sleep() { sleep "${HAMH_SETUP_POLL_SECONDS:-15}"; }

# setup_number <text> <default>: the text as a decimal number with its leading zeros dropped ("08" is 8, not an octal error), the default when the text is no number.
setup_number() {
  local _n=$1
  case $_n in
    ''|*[!0-9]*) printf '%s' "$2"; return 0 ;;
  esac
  while [ "${#_n}" -gt 1 ] && [ "${_n#0}" != "$_n" ]; do _n=${_n#0}; done
  printf '%s' "$_n"
}

# setup_elapsed_text <seconds>: "45 seconds" below two minutes, else "7 minutes".
setup_elapsed_text() {
  local _n=$1
  if [ "$_n" -lt 120 ]; then printf '%s seconds' "$_n"; else printf '%s minutes' "$((_n / 60))"; fi
}

# A wait for a stack that takes minutes says so: setup_ticker_start <what> prints "still waiting for <what> (N minutes so far)" every HAMH_SETUP_PROGRESS_SECONDS (180 by
# default; 0 turns it off) until setup_ticker_stop. The ticker is a background job of this shell. Its sleep runs in the background of the ticker with no standard output,
# and stopping the ticker ends that sleep too, so nothing is left behind and no sleep holds a pipe. When the parent shell is gone (killed, or Ctrl-C reached only the parent: a
# background job ignores it), the ticker notices within HAMH_SETUP_TICKER_CHECK_SECONDS (5 by default), prints nothing and ends.
SETUP_TICKER_PID=""
setup_ticker_start() {
  local every check parent=$$
  setup_ticker_stop
  every=$(setup_number "${HAMH_SETUP_PROGRESS_SECONDS:-180}" 0)
  [ "$every" -gt 0 ] || return 0
  check=$(setup_number "${HAMH_SETUP_TICKER_CHECK_SECONDS:-5}" 5)
  [ "$check" -gt 0 ] || check=5
  [ "$check" -le "$every" ] || check=$every
  (
    nap=""
    trap 'kill $nap 2>/dev/null; exit 0' TERM
    n=0
    waited=0
    while :; do
      # The sleep is detached from the terminal on purpose: no input, no output.
      # shellcheck disable=SC2217
      sleep "$check" </dev/null >/dev/null 2>&1 &
      nap=$!
      wait "$nap" 2>/dev/null
      kill -0 "$parent" 2>/dev/null || exit 0
      waited=$((waited + check))
      [ "$waited" -ge "$every" ] || continue
      n=$((n + waited))
      waited=0
      say "  still waiting for $1 ($(setup_elapsed_text "$n") so far)"
      log_line "still waiting for $1 ($(setup_elapsed_text "$n") so far)"
    done
  ) &
  SETUP_TICKER_PID=$!
}

setup_ticker_stop() {
  if [ -n "${SETUP_TICKER_PID:-}" ]; then
    kill "$SETUP_TICKER_PID" 2>/dev/null
    wait "$SETUP_TICKER_PID" 2>/dev/null
  fi
  SETUP_TICKER_PID=""
  return 0
}

# setup_sha256: the SHA-256 of stdin, lower case.
setup_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | cut -d' ' -f1
  else
    shasum -a 256 | cut -d' ' -f1
  fi
}

# ---- what this run decides itself ----
# SETUP_EXPLICIT_LINES holds one HAMH_<NAME>=value line for each stage setting you exported and SETUP_EXPLICIT_SETTINGS the same lines as base64: pf_explicit_settings of
# preflight.sh fills both, and nothing else does. SETUP_CLIENT_ONLY is 1 when the stage already runs the commit (rn_same_deployment_check of runner.sh). Whatever the shell
# that started setup.sh holds under these names is dropped here, and none of them is exported: the runner route hands them to its build as overrides, and the local route
# exports the setting lines themselves into its deploy.
unset SETUP_EXPLICIT_LINES SETUP_EXPLICIT_SETTINGS SETUP_CLIENT_ONLY
SETUP_EXPLICIT_LINES=""
SETUP_EXPLICIT_SETTINGS=""
SETUP_CLIENT_ONLY=0

# setup_base64: stdin as base64 on one line (RFC 4648, no line wrap), with awk alone. It is the one encoder of the setting lines: the base64 program of macOS has no option
# that turns line wrapping off and the one of Linux wraps unless told not to, so neither is used. Every line of the input ends in a newline in the output. A character that
# is not printable ASCII, a tab or a newline is refused (exit 1, nothing printed), as the patterns of the registry refuse it before this.
setup_base64() {
  LC_ALL=C awk '
    BEGIN { b = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; for (i = 32; i < 127; i++) ord[sprintf("%c", i)] = i; ord["\t"] = 9; ord["\n"] = 10 }
    { s = s $0 "\n" }
    END {
      n = length(s); out = ""
      for (i = 1; i <= n; i++) if (!(substr(s, i, 1) in ord)) exit 1
      for (i = 1; i <= n; i += 3) {
        c1 = ord[substr(s, i, 1)]; c2 = (i + 1 <= n) ? ord[substr(s, i + 1, 1)] : 0; c3 = (i + 2 <= n) ? ord[substr(s, i + 2, 1)] : 0
        v = c1 * 65536 + c2 * 256 + c3
        out = out substr(b, int(v / 262144) % 64 + 1, 1) substr(b, int(v / 4096) % 64 + 1, 1)
        out = out ((i + 1 <= n) ? substr(b, int(v / 64) % 64 + 1, 1) : "=") ((i + 2 <= n) ? substr(b, v % 64 + 1, 1) : "=")
      }
      printf "%s", out
    }'
}

# setup_ere_match <value> <anchored POSIX ERE>: 0 when the one-line value matches (grep -E: bash 3.2 has no portable =~ in these files).
setup_ere_match() {
  case $1 in
    *"$NL"*) return 1 ;;
  esac
  printf '%s\n' "$1" | grep -Eq -e "$2" 2>/dev/null
}

# ---- the AWS CLI of this run ---------------------------------------------------------------------------------------
# AWS_CLI_PROG is the program setup.sh checked (HAMH_AWS_CLI, else aws). PROFILE_EFF is the profile the run uses (empty: the AWS CLI's own chain),
# REGION the Region. Nothing here reads or prints a credential.
awsx() {
  if [ -n "${PROFILE_EFF:-}" ]; then
    AWS_PAGER="" "${AWS_CLI_PROG:-aws}" --profile "$PROFILE_EFF" --region "$REGION" "$@"
  else
    AWS_PAGER="" "${AWS_CLI_PROG:-aws}" --region "$REGION" "$@"
  fi
}

AWS_OUT=""
AWS_ERR=""
AWS_RC=0
# aws_run <aws arguments>: runs awsx; the answer goes to AWS_OUT, the error text to AWS_ERR, the exit code to AWS_RC (also the return value). Trailing newlines are cut
# from both texts. No file is made, because a file needs a name in a folder that other users can write to and a clean-up that no signal can be trusted to run (the helper
# files delete nothing). The call's answer is captured by an inner command substitution; its error stream goes straight into the outer one (file descriptor 3 is the outer
# standard output), and the exit code and the answer follow the error text after a separator that is drawn for this call. The error text comes first and is short, so
# cutting the pieces apart takes time in proportion to its length, not the answer's.
aws_run() {
  local raw rest sep="@@aws-run-$$-$RANDOM@@"
  raw=$( { out=$(awsx "$@" 2>&3 3>&-); rc=$?; } 3>&1; printf '%s%s%s%s' "$sep" "$rc" "$sep" "$out" )
  AWS_ERR=${raw%%"$sep"*}
  rest=${raw#*"$sep"}
  AWS_RC=${rest%%"$sep"*}
  AWS_OUT=${rest#*"$sep"}
  AWS_ERR=$(printf '%s' "$AWS_ERR")
  case $AWS_RC in
    ''|*[!0-9]*) AWS_RC=1 ;;
  esac
  return "$AWS_RC"
}

# aws_error_code: prints the code of the last failed aws_run, the word in `An error occurred (<Code>) when calling ...` (nothing when the text has none).
aws_error_code() {
  local rest
  case $AWS_ERR in
    *"An error occurred ("*) ;;
    *) return 0 ;;
  esac
  rest=${AWS_ERR#*"An error occurred ("}
  printf '%s' "${rest%%")"*}"
}

# aws_code_is_missing <code>: 0 for the codes of a thing that is not there: a name with NotFound in it (ResourceNotFoundException, ParameterNotFound, InvalidVpcID.NotFound),
# NoSuch... (NoSuchKey, NoSuchBucket, NoSuchEntity) and the 404 of a HEAD request.
aws_code_is_missing() {
  case $1 in
    *NotFound*|NoSuch*|404) return 0 ;;
  esac
  return 1
}

# aws_missing: 0 when the last aws_run failed because the thing asked for does not exist (a stack, an object, a parameter). The code of the error decides: a word of the
# message (the name of a stack or a parameter holds `NoSuchBucket` or `does not exist` in a refusal that names the resource) does not, and a text without a code (a local
# failure such as an unreachable endpoint or an expired SSO token) never says that a thing is gone. CloudFormation answers an unknown stack with ValidationError and a
# message that ends in "does not exist".
aws_missing() {
  local code
  code=$(aws_error_code)
  [ -n "$code" ] || return 1
  aws_code_is_missing "$code" && return 0
  if [ "$code" = ValidationError ]; then
    case $AWS_ERR in
      *" does not exist") return 0 ;;
    esac
  fi
  return 1
}

# ---- the log of a run ----------------------------------------------------------------------------------------------
RUN_LOG=""
# log_line <text>: one timestamped line in the log of this run, when there is one.
log_line() {
  [ -n "$RUN_LOG" ] || return 0
  printf '%s %s\n' "$(setup_now)" "$*" >>"$RUN_LOG"
}

# say_log <text>: said on the screen and written to the log.
say_log() {
  say "$*"
  log_line "$*"
}

# ---- small checks ----------------------------------------------------------------------------------------------------
# setup_is_commit <text>: 0 when it is a full 40-digit commit id (lower case hexadecimal).
setup_is_commit() {
  case $1 in
    *[!0-9a-f]*|'') return 1 ;;
  esac
  [ "${#1}" -eq 40 ]
}

# setup_safe_word <text>: 0 when the text holds only characters that are safe to put into a key, a name or a receipt.
setup_safe_word() {
  case $1 in
    ''|*[!A-Za-z0-9_./:@=+,-]*) return 1 ;;
  esac
  return 0
}

# setup_installer_patterns <caller arn>: the two values of aws:PrincipalArn that the policy of the runner bucket compares with, so that the identity behind these
# credentials is the only one besides the runner role that may write the bucket. Prints them separated by a space; returns 1 for an ARN it cannot turn into a
# principal. A user, the root user and a federated user are named exactly. A role session (arn:aws:sts::<account>:assumed-role/<role>/<session>) is named by its
# role, which the policy sees as arn:aws:iam::<account>:role[/<path>]/<role>: the first value has no path, the second any path (an IAM Identity Center role has one).
setup_installer_patterns() {
  local arn=$1 rest acct name
  setup_safe_word "$arn" || return 1
  case $arn in
    arn:aws:iam::*:user/*|arn:aws:iam::*:root|arn:aws:sts::*:federated-user/*)
      printf '%s %s' "$arn" "$arn" ;;
    arn:aws:sts::*:assumed-role/*/*)
      rest=${arn#arn:aws:sts::}
      acct=${rest%%:*}
      name=${rest#*:assumed-role/}
      name=${name%%/*}
      [ -n "$acct" ] && [ -n "$name" ] || return 1
      printf 'arn:aws:iam::%s:role/%s arn:aws:iam::%s:role/*/%s' "$acct" "$name" "$acct" "$name" ;;
    *) return 1 ;;
  esac
}
