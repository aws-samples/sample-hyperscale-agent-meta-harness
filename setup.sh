#!/usr/bin/env bash
# setup.sh: the single entry point of the system, for a Mac or Linux machine.
#
# The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh).
# The full setup runs from a git clone, because it builds the commit it records. Setup uses the shell tools that ship with macOS and Linux.
#
# Its first question is "full (backend and client), or client only". The default is the full setup:
#   Full setup (`--full`): reads your AWS identity, account, Region and Availability Zones, shows the account, the zone, the source commit, the
#     authority it grants and the cost items, and asks ONE question; after the answer it asks nothing more. Then it installs a runner in your own AWS
#     account (a CloudFormation stack with a CodeBuild project, made with the AWS CLI alone), uploads the git archive of your clean clone and a
#     generated account file, builds and deploys the backend there, builds the client there, downloads it from your own bucket, checks its SHA-256,
#     installs it, writes its settings through `ham config set`, runs `ham models`, `ham rules init` and `ham doctor`, and prints the first command.
#     The runner is removed after a successful setup unless you give `--keep-runner`. `--build-on local` runs the same deploy engine on this machine
#     for a person who already has the toolchain.
#   Client only (`--client-only`): the steps a user of an existing backend needs: it checks git and a source of AWS credentials, installs a `ham`
#     binary (one you built with `--from`, checked against its SHA-256, one it builds with `--build`, or the one already on your PATH), writes the
#     `ham` config file only from the options you give or the answers you type, runs `ham models`, `ham rules init` and `ham doctor`.
# Later life of the system: `--status`, `--doctor`, `--update` (an explicit revision) and `--teardown`.
# A step that is already done is left as it is, so a re-run is safe; an interrupted full setup attaches to the build it started.
# `--yes` never chooses the full setup by itself, and the costs need `--acknowledge-costs`. It reads no credential value, writes no secret, edits no
# shell startup file and deletes none of your files (scripts/setup/empty-bucket.sh removes only the temporary files it makes).
# The AWS lifecycle lives in the helper files of scripts/setup/ (scripts/setup/AGENTS.md); this file keeps the options, the questions and the
# client steps.
# Compatible with bash 3.2 (the bash that ships with macOS).
#
# Exit codes: 0 done; 1 a prerequisite or step failed; 2 bad usage; 3 installed, but `ham doctor` found a problem; 4 you cancelled or a
# confirmation is missing; 5 bootstrap, deploy or initialization failed; 6 an operation is still running and safe to resume; 7 a conflicting
# operation, or a source or configuration mismatch; 8 cleanup is incomplete; 9 teardown has removed everything except the network resources that AgentCore's
# network interface holds (run it again after AWS releases the interface).

# shellcheck disable=SC2034  # some variables are set here and read only by the helper files of scripts/setup/
set -u

SELF_DIR=$(cd "$(dirname "$0")" && pwd)

# The name of the client and of its settings directory come from one definition (scripts/setup/names.sh), and the commands and the messages below read them from there. The name of the
# product is defined there too, for text that names the product (no message prints it). Names of resources (hamh-direct-*, hamh-setup-*) stay literal.
if [ -f "$SELF_DIR/scripts/setup/names.sh" ]; then
  # names.sh also reads the variables of the earlier spelling (COMPAT-CAH): a selector with two different values ends the run here, with exit 2
  # shellcheck disable=SC1091
  . "$SELF_DIR/scripts/setup/names.sh" || exit $?
fi
: "${HAMH_CLIENT_BIN:=ham}"
: "${HAMH_ADMIN_BIN:=${HAMH_CLIENT_BIN}-admin}"
: "${HAMH_PRODUCT_NAME:=Hyperscale Agent Meta Harness}"
: "${HAMH_CONFIG_DIR_NAME:=ham}"
: "${LEGACY_CLIENT_BIN:=cah}"  # COMPAT-CAH
: "${HAMH_LEGACY_CONFIG_DIR_NAME:=cah}"  # COMPAT-CAH

DRY=0
YES=0
NO_INPUT=0
BUILD=0
MODE=""
CMD=""
KEEP_RUNNER=0
ACK_COSTS=0
ACK_TEARDOWN=0
ALLOW_ACTIVE_RUNS=0
FORCE_DEPLOY=0
CHANGE_OPERATOR=0
PREFLIGHT_ONLY=0
BUILD_ON=""
STAGE=direct
AZS=1
ACCOUNT_ID_OPT=""
REVISION=""
TEARDOWN_ONLY=""
NETWORK_WAIT_OPT=""
NETWORK_WAIT_SET=0
NETWORK_WAIT_MIN=0
INV_LOGGING=read-only
REPO_NAMES=""
REPO_NAMES_SET=0
VERBOSE=0
FULL_ONLY=0
PREFIX=""
ENDPOINT=""
PROFILE=""
CRED_FROM_ENV=0
REGION=""
DEFAULT_REPO=""
FROM=""
SHA=""
FULL_VIA=""
CLIENT_ONLY_TYPED=0

# Where the AWS documentation explains what the messages below point at.
AWS_CLI_INSTALL_URL=https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html
AWS_CLI_CONFIG_URL=https://docs.aws.amazon.com/cli/latest/userguide/cli-chap-configure.html
AWS_CLI_SSO_URL=https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html

say() { printf '%s\n' "$*"; }
ok() { printf 'ok    %s\n' "$*"; }
warn() { printf 'warn  %s\n' "$*"; }
would() { printf 'would %s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
err() { printf 'setup.sh: %s\n' "$*" >&2; }
# die <message>: the message, then exit 1; after a build that succeeded, the runner that is still installed goes first (full_runner_cleanup of scripts/setup/full.sh).
die() {
  err "$*"
  if type full_runner_cleanup >/dev/null 2>&1; then full_runner_cleanup; fi
  exit 1
}
usage_die() { err "$*"; printf 'Try: setup.sh --help\n' >&2; exit 2; }

# The checks of an answer, one per kind, used by the options and by the questions alike (return 0 when the value is acceptable).
valid_endpoint() { # one URL: https, or http to this machine for tests; no spaces and no <placeholders>
  case $1 in
    *[[:space:]]*|*"<"*|*">"*) return 1 ;;
    https://?*) return 0 ;;
    http://localhost*|http://127.0.0.1*) valid_local_url "$1"; return $? ;;
  esac
  return 1
}

# network_wait_minutes <value>: prints the minutes that a --network-wait-minutes or HAMH_SETUP_NETWORK_WAIT_MINUTES value stands for (full is 480) and returns 0; returns 1
# for anything that is not a whole number of minutes from 0 to 480 or the word full.
network_wait_minutes() {
  case $1 in
    full) printf '480'; return 0 ;;
    ''|*[!0-9]*) return 1 ;;
  esac
  [ "${#1}" -le 3 ] || return 1
  [ "$((10#$1))" -le 480 ] || return 1
  printf '%s' "$((10#$1))"
}

# valid_local_url <url>: http://localhost or http://127.0.0.1, then optionally :PORT (digits only), then nothing or a path. A longer host name such as
# localhost.example, or text between the port and the path such as :80@example, is another machine and is refused.
valid_local_url() {
  local rest digits
  case $1 in
    http://localhost*) rest=${1#http://localhost} ;;
    http://127.0.0.1*) rest=${1#http://127.0.0.1} ;;
    *) return 1 ;;
  esac
  case $rest in
    :[0-9]*)
      rest=${rest#:}
      digits=${rest%%[!0-9]*}
      rest=${rest#"$digits"} ;;
  esac
  case $rest in
    ''|/*) return 0 ;;
  esac
  return 1
}

# valid_stage_endpoint <url>: the URL of an API Gateway stage as the stack makes it: https, then a host of the form <id>.execute-api.<region>.amazonaws.com, then nothing or a path.
# Called by scripts/setup/full.sh, which this script sources.
# shellcheck disable=SC2329
valid_stage_endpoint() {
  local rest host
  valid_endpoint "$1" || return 1
  case $1 in
    https://*) ;;
    *) return 1 ;;
  esac
  rest=${1#https://}
  host=${rest%%/*}
  case $host in
    ''|*[!a-z0-9.-]*) return 1 ;;
    ?*.execute-api.?*.amazonaws.com) return 0 ;;
  esac
  return 1
}

valid_region() { # looks like us-east-1
  case $1 in
    ''|*[!a-z0-9-]*) return 1 ;;
  esac
  return 0
}

valid_profile_name() { # only the characters an AWS profile name uses
  case $1 in
    ''|*[!A-Za-z0-9_.@:+-]*) return 1 ;;
  esac
  return 0
}

valid_repo_entry() { # one entry of the repository names: a name of 1 to 100 letters, digits, dot, underscore and dash, such a prefix of up to 99 followed by one *, or * alone
  local e=$1 body
  [ "$e" = '*' ] && return 0
  case $e in
    *'*') body=${e%?}; [ "${#body}" -le 99 ] || return 1 ;;
    *) body=$e; [ "${#body}" -le 100 ] || return 1 ;;
  esac
  case $body in
    ''|*[!A-Za-z0-9._-]*) return 1 ;;
  esac
  return 0
}

repo_name_covered() { # <name> <rule>: 0 when an entry of the comma separated rule equals the name, is a prefix of it (team-*) or is *. After a prefix at least one character must follow: a typo guard
  local name=$1 rule=$2 e
  local -a entries
  [ -n "$rule" ] || return 1
  IFS=, read -r -a entries <<<"$rule"
  for e in "${entries[@]}"; do
    case $e in
      '*') return 0 ;;
      *'*') case $name in "${e%?}"?*) return 0 ;; esac ;;
      *) [ "$name" = "$e" ] && return 0 ;;
    esac
  done
  return 1
}

valid_default_repo() { # cc:<name>, the name made of letters, digits, dot, underscore and dash, and covered by the repository names of the stage (--repo-name; else hamh-e2e-*)
  case $1 in
    cc:?*)
      case ${1#cc:} in
        *[!A-Za-z0-9._-]*) return 1 ;;
      esac
      repo_name_covered "${1#cc:}" "${REPO_NAMES:-hamh-e2e-*,cah-e2e-*}"  # COMPAT-CAH-DATA: repositories of a stage deployed under the previous names
      return $? ;;
  esac
  return 1
}

check_repo_names() { # --repo-name: every entry valid, 1 to 10 entries, none twice
  local e n=0 seen=, bad=""
  local -a entries
  [ -n "$REPO_NAMES" ] || usage_die "--repo-name has an empty value. Use a repository name, a prefix ending in *, or *."
  case $REPO_NAMES in
    ,*|*,|*,,*) usage_die "--repo-name contains an empty entry. Supplied value: $REPO_NAMES" ;;
  esac
  IFS=, read -r -a entries <<<"$REPO_NAMES"
  for e in "${entries[@]}"; do
    valid_repo_entry "$e" || usage_die "--repo-name entry '$e' is invalid. Use a repository name (1 to 100 letters, digits, dot, underscore or dash), such a prefix followed by one *, or * alone."
    case $seen in *",$e,"*) bad=$e ;; esac
    seen="$seen$e,"
    n=$((n + 1))
  done
  [ -z "$bad" ] || usage_die "--repo-name lists '$bad' twice. Remove the duplicate."
  [ "$n" -le 10 ] || usage_die "--repo-name takes at most 10 entries. You supplied $n entries."
}

FAILS=0
# fail <headline> [detail line ...]: report a problem on stderr and keep checking, so one run lists them all.
fail() {
  FAILS=$((FAILS + 1))
  printf 'FAIL  %s\n' "$1" >&2
  shift
  for line in "$@"; do printf '      %s\n' "$line" >&2; done
}

# detail <text>: one more indented line under the last FAIL.
detail() { printf '      %s\n' "$*" >&2; }

# show_help: the text below with @CLIENT@ replaced by the name of the client (HAMH_CLIENT_BIN). The here-document is quoted, so nothing else in it is expanded.
# It is read line by line: bash 3.2 (macOS) fails to parse a here-document inside $( ) when the text holds an apostrophe, and this text does.
show_help() {
  local line
  while IFS= read -r line; do
    printf '%s\n' "${line//@CLIENT@/$HAMH_CLIENT_BIN}"
  done <<'EOF'
setup.sh: set up the system on this machine: the backend in your own AWS account and the @CLIENT@ terminal app.

The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh).
Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them.
`--status`, `--doctor` and `--teardown` need neither git nor a clone. Setup uses the shell tools that ship with macOS and Linux.
The local route (--build-on local) also needs cargo, cargo-zigbuild, zig, node and docker on PATH.

Usage: ./setup.sh [options]
       ./setup.sh --status | --doctor | --update | --teardown [options]

The first question is "full (backend and client), or client only"; the default is the full setup.
  --full          deploy the backend in your AWS account, then install and configure the client (creates AWS resources that cost money)
  --client-only   install and configure the client for a backend that already exists (no AWS resource is created)
  --yes never chooses the full setup by itself: a run with --yes and neither option, or with --from, --build or --endpoint, is a client-only setup.

The full setup, step by step (a step that is already done is left alone, so you can run it again):
  1. read your AWS identity, the account, the Region and the Availability Zones from the AWS CLI; refuse placeholders
  2. show the account, Region, zone, source commit, the authority it grants and the cost items, and ask ONE question; after that nothing is asked
     (for scripts: --yes --acknowledge-costs)
  3. install the runner in your account: the CDK bootstrap stack when it is missing, and a small stack with a CodeBuild project and an encrypted,
     versioned bucket, made with the AWS CLI alone
  4. upload the git archive of your clean clone and a generated account file, build and deploy the backend and build the client and the operator tool
     in your account
  5. download the client and the operator tool from your own bucket, check the SHA-256 of each, install both, write the client's settings, run `@CLIENT@ models`,
     `@CLIENT@ rules init`, `@CLIENT@ doctor`
  6. the runner is removed after a successful setup (--keep-runner keeps it); print the endpoint, the phases, what stays, the teardown
     command and the first command to run
An interrupted setup attaches to the build it started when you run it again; it never starts a second one.

Full-setup options:
  --keep-runner   keep the runner stack and its bucket after a successful setup (the default removes them)
  --acknowledge-costs  with --yes: accept the costs listed on the screen (without it, --yes stops with exit 4)
  --preflight-only     read and report only: the account, the zone and the commit; create nothing
  --build-on WHERE     runner (default: the CodeBuild runner in your account) or local (scripts/deploy.sh on this machine; you need cargo,
                       cargo-zigbuild, zig, node and docker)
  --account-id ID      the account you mean; setup stops with exit 7 when your credentials are for another one
  --stage NAME         the stage (only direct is supported)
  --azs N              the number of Availability Zones of the stage (only 1 is supported)
  --invocation-logging MODE  read-only (default) or enable-and-restore: what the deploy may do with Bedrock invocation logging
  --repo-name NAME     CodeCommit repository names in which a run may open a pull request, once the operator has allowed the repository. Use a name, a prefix ending in * (team-*), or * for every name.
                       Repeat the option or separate entries with commas. Use up to 10 different entries. Default: hamh-e2e-*

Later life of the system (they read or change this deployment only; --profile, --region and --account-id select it):
  --status        what is deployed, which build runs, what the last setup did, whether the client fits the stage
  --doctor        PASS, WARN and FAIL checks of the AWS CLI, the stage, the endpoint, a runner left behind and the client's own doctor
  --update        build and deploy a revision of this clone (HEAD, or the full commit id of --revision).
                  Active runs stop ./setup.sh --update with exit 7 before anything is installed or built.
                  --allow-active-runs skips that check; the build then refuses only an update that changes the worker runtime.
                  An update takes about 20 to 25 minutes when it reuses the Cline binary, and about 21 to 27 minutes more when it rebuilds it.
  --revision ID        the full 40-digit commit id to deploy with --update (it must be in this clone). An update to a commit older than the one the stage runs
                       stops with exit 7 unless --revision names it; a commit whose setup files differ from this clone's stops with exit 2 (run its own setup.sh).
                       A commit the stage records that this clone does not hold is judged by VERSION: a lower recorded VERSION goes ahead, any other stops with exit 7
  --change-operator    with --update: make the identity of your credentials the operator of the stage. Without it an update by another identity than the one the
                       stage was deployed with stops with exit 7
  --force-deploy       with --update: deploy even when the stage already runs this commit
                  An update keeps the stage's settings, its operator, its zone and its Instances part. A stage setting you export (HAMH_TOOLS_MODE=on, for example)
                  is checked before the question, shown in the plan and passed to the deploy on both routes; NAME=default returns it to the default.
  --teardown      delete the stage and the runner; --yes --acknowledge-teardown --account-id ID skips the question. AgentCore network interfaces that hold
                  the network stack are retained and listed as RETAINED, never counted as zero resources; AWS frees them up to 8 hours later. When only those
                  are left, the run ends with exit 9 and prints the command that runs it again.
                  One plan covers every stage found, under the current names and under the previous names; --only legacy or --only current covers one of them
  --network-wait-minutes N|full   with --teardown: wait up to N minutes (0 to 480; full is 480) for AWS to release the network interface that AgentCore holds in
                  the stage's VPC, then delete the network stack. The default is 0: the network resources it holds are retained at once.
                  HAMH_SETUP_NETWORK_WAIT_MINUTES sets another default; the option beats it.

Getting a client without the full setup (the project publishes no @CLIENT@ binary; the full setup builds one in your account):
  --from PATH     a directory with `@CLIENT@` and SHA256SUMS (the output of scripts/build-macos-driver.sh),
                  or a single @CLIENT@ file together with --sha256. A checksum that does not match is refused.
  --sha256 HEX    the SHA-256 of the single file given with --from
  --build         compile @CLIENT@ on this machine: cargo build --release --locked -p @CLIENT@ --bin @CLIENT@
                  (run from a checkout of the repository).
  With none of these, setup.sh uses the @CLIENT@ already in PREFIX/bin or on your PATH.

Options:
  --prefix DIR    install to DIR/bin/@CLIENT@ (default: $HOME/.local, so ~/.local/bin/@CLIENT@)
  --endpoint URL  the stage URL: the V1ApiUrl output of the CloudFormation stack hamh-direct-control
  --profile NAME  AWS profile: static keys, credential_process, IAM Identity Center (SSO) or assume-role
                  (SSO and assume-role profiles need the AWS CLI, version 2.9.0 or later)
  --region NAME   AWS Region, when the profile or AWS_REGION does not name one (the stage deploys in us-east-1 or us-west-2)
  --default-repo REPO  the repository your terminal app preselects: cc:hamh-e2e-NAME or cc:NAME covered by your --repo-name rule. Optional. A run without one returns a zip.
  --yes, -y       do not ask before making changes
  --no-input      ask nothing at all, not even the confirmation (add --yes or --dry-run)
  --dry-run       show what would happen; create and write nothing and run no @CLIENT@ command (AWS is only read)
  --verbose       show the whole log of the build while it runs
  --help, -h      show this text

Questions: on a terminal, setup.sh asks one numbered question at a time. The numbers follow the order in your run.
You skip each question when you give its option. You first choose the full setup or the client only.
You skip this first question with --full or --client-only. For the client-only setup, you next give the stage endpoint.
The answer "deploy one for me" switches you to the full setup. You do not get this answer when you type --client-only.
For the client-only setup, you then say where @CLIENT@ comes from, unless @CLIENT@ is installed or you gave --from, --sha256 or --build.
You then choose the AWS profile. You next choose the Region. Both setups ask these questions.
For the client-only setup, you then choose an optional default repository. Setup asks this only when an endpoint is known.
For the full setup, you then choose where to build the backend.
You get the build question only when your machine has cargo, cargo-zigbuild, zig, node and docker.
You get at most six questions.
Press Enter to take the answer in [brackets]; type q to stop. Nothing is created or written until you confirm the plan.
--yes, --no-input and a missing terminal ask nothing.

What the full setup grants: deployment code runs with the authority of your credentials in your account (IAM roles, a VPC, a firewall and the
rest of the stage are created), through a runner. The runner role writes its logs, the results and client files of its builds, the objects and
parameters of the stage and its worker runtime endpoints, and starts the stage's image-build project; a permissions boundary limits that role
itself, but it can assume the CDK roles, and their CloudFormation execution role holds AdministratorAccess. Treat the runner as an administrator
of the account for 12 hours after each install (HAMH_SETUP_AUTHORITY_HOURS sets 5 to 72); after that its role cannot assume them, whether or not
it is still installed. Only you and the runner role may write the runner's bucket, and anyone who may start a build of its CodeBuild project
(codebuild:StartBuild) can act as the runner role until then. The CDK bootstrap stack and its roles, which trust the whole account, stay in the
account after the runner is removed, because updates need them. Use a dedicated sandbox account.

SSO and assume-role profiles: @CLIENT@ asks the AWS CLI (`aws configure export-credentials`) for the credentials, so the AWS CLI must be on your
PATH, or named by the environment variable HAMH_AWS_CLI. Before a session with an SSO profile run: aws sso login --profile NAME

Without --endpoint, a client-only setup writes no endpoint and skips `@CLIENT@ doctor`.
Exit codes: 0 done; 1 a prerequisite or step failed; 2 bad usage; 3 installed, but `@CLIENT@ doctor` found a problem; 4 you cancelled or a
confirmation is missing; 5 bootstrap, deploy or initialization failed; 6 an operation is still running and safe to resume (run setup.sh
again to attach); 7 a conflicting operation, or a source or configuration mismatch; 8 cleanup is incomplete.
setup.sh refuses to run as root, writes no secret and edits no shell startup file.
EOF
}

# ---- arguments -----------------------------------------------------------------------------------

ACCOUNT_ID_SET=0
set_opt() { # option value
  case $1 in
    --prefix) PREFIX=$2 ;;
    --endpoint) ENDPOINT=$2 ;;
    --profile) PROFILE=$2 ;;
    --region) REGION=$2 ;;
    --default-repo) DEFAULT_REPO=$2 ;;
    --from) FROM=$2 ;;
    --sha256) SHA=$2 ;;
    --build-on) BUILD_ON=$2; FULL_ONLY=1; FULL_VIA=${FULL_VIA:-$1} ;;
    --stage) STAGE=$2 ;;
    --azs) AZS=$2 ;;
    --account-id) ACCOUNT_ID_OPT=$2; ACCOUNT_ID_SET=1 ;;
    --revision) REVISION=$2 ;;
    --only) TEARDOWN_ONLY=$2 ;;
    --network-wait-minutes) NETWORK_WAIT_OPT=$2; NETWORK_WAIT_SET=1 ;;
    --invocation-logging) INV_LOGGING=$2; FULL_ONLY=1; FULL_VIA=${FULL_VIA:-$1} ;;
    --repo-name) REPO_NAMES=${REPO_NAMES:+$REPO_NAMES,}$2; REPO_NAMES_SET=1; FULL_ONLY=1; FULL_VIA=${FULL_VIA:-$1} ;;
  esac
}

set_cmd() { # command
  [ -z "$CMD" ] || [ "$CMD" = "$1" ] || usage_die "give one of --status, --doctor, --update and --teardown, not $CMD and $1"
  CMD=$1
}

set_mode() { # mode
  [ -z "$MODE" ] || [ "$MODE" = "$1" ] || usage_die "give --full or --client-only, not both"
  MODE=$1
}

while [ $# -gt 0 ]; do
  case $1 in
    -h|--help) show_help; exit 0 ;;
    --dry-run) DRY=1 ;;
    -y|--yes) YES=1 ;;
    --no-input) NO_INPUT=1 ;;
    --build) BUILD=1 ;;
    --verbose) VERBOSE=1 ;;
    --full) set_mode full; FULL_VIA=--full ;;
    --client-only) set_mode client; CLIENT_ONLY_TYPED=1 ;;
    --status) set_cmd status ;;
    --doctor) set_cmd doctor ;;
    --update) set_cmd update ;;
    --teardown) set_cmd teardown ;;
    --keep-runner) KEEP_RUNNER=1; FULL_ONLY=1; FULL_VIA=${FULL_VIA:-$1} ;;
    --acknowledge-costs) ACK_COSTS=1; FULL_ONLY=1; FULL_VIA=${FULL_VIA:-$1} ;;
    --acknowledge-teardown) ACK_TEARDOWN=1 ;;
    --allow-active-runs) ALLOW_ACTIVE_RUNS=1 ;;
    --force-deploy) FORCE_DEPLOY=1 ;;
    --change-operator) CHANGE_OPERATOR=1 ;;
    --preflight-only) PREFLIGHT_ONLY=1; FULL_ONLY=1; FULL_VIA=${FULL_VIA:-$1} ;;
    --prefix|--endpoint|--profile|--region|--default-repo|--from|--sha256|--build-on|--stage|--azs|--account-id|--revision|--only|--invocation-logging|--repo-name|--network-wait-minutes)
      [ $# -ge 2 ] || usage_die "$1 needs a value"
      case $2 in --*) usage_die "$1 needs a value, not the option $2" ;; esac
      set_opt "$1" "$2"
      shift
      ;;
    --prefix=*|--endpoint=*|--profile=*|--region=*|--default-repo=*|--from=*|--sha256=*|--build-on=*|--stage=*|--azs=*|--account-id=*|--revision=*|--only=*|--invocation-logging=*|--repo-name=*|--network-wait-minutes=*)
      set_opt "${1%%=*}" "${1#*=}" ;;
    *) usage_die "unknown option: $1" ;;
  esac
  shift
done

if [ -n "$PREFIX" ]; then
  case $PREFIX in /*) ;; *) usage_die "--prefix must be an absolute path (got: $PREFIX)" ;; esac
fi
if [ -n "$ENDPOINT" ]; then
  valid_endpoint "$ENDPOINT" || usage_die "--endpoint must be one https:// URL without spaces or <placeholders> (got: $ENDPOINT)"
fi
if [ -n "$PROFILE" ]; then
  valid_profile_name "$PROFILE" || usage_die "--profile has characters an AWS profile name does not use (got: $PROFILE)"
fi
if [ -n "$REGION" ]; then
  valid_region "$REGION" || usage_die "--region should look like us-east-1 (got: $REGION)"
fi
if [ "$REPO_NAMES_SET" = 1 ]; then
  check_repo_names
fi
if [ -n "$DEFAULT_REPO" ]; then
  if [ -z "$REPO_NAMES" ]; then
    valid_default_repo "$DEFAULT_REPO" || usage_die "--default-repo should look like cc:hamh-e2e-NAME (got: $DEFAULT_REPO)"
  else
    valid_default_repo "$DEFAULT_REPO" || usage_die "--default-repo should look like cc:NAME with a NAME the repository names (${REPO_NAMES//,/, }) cover (got: $DEFAULT_REPO)"
  fi
fi
if [ -n "$SHA" ]; then
  [ -n "$FROM" ] || usage_die "--sha256 goes with --from: it is the checksum of the file --from names"
  case $SHA in *[!0-9a-fA-F]*) usage_die "--sha256 must be 64 hexadecimal characters" ;; esac
  [ "${#SHA}" -eq 64 ] || usage_die "--sha256 must be 64 hexadecimal characters"
  SHA=$(printf '%s' "$SHA" | tr 'A-F' 'a-f')
fi
if [ -n "$FROM" ] && [ "$BUILD" = 1 ]; then
  usage_die "give --from or --build, not both"
fi
# the options of the full setup and of the lifecycle commands
case $STAGE in direct) ;; *) usage_die "--stage must be direct, the only stage there is (got: $STAGE)" ;; esac
case $AZS in 1) ;; *) usage_die "--azs must be 1, the number of zones the stage uses (got: $AZS)" ;; esac
case $INV_LOGGING in read-only|enable-and-restore) ;; *) usage_die "--invocation-logging must be read-only or enable-and-restore (got: $INV_LOGGING)" ;; esac
if [ -n "$BUILD_ON" ]; then
  case $BUILD_ON in runner|local) ;; *) usage_die "--build-on must be runner or local (got: $BUILD_ON)" ;; esac
fi
if [ "$ACCOUNT_ID_SET" = 1 ]; then
  case $ACCOUNT_ID_OPT in
    ''|*[!0-9]*) usage_die "--account-id must be the 12 digits of an AWS account id (got: $ACCOUNT_ID_OPT)" ;;
  esac
  [ "${#ACCOUNT_ID_OPT}" -eq 12 ] || usage_die "--account-id must be the 12 digits of an AWS account id (got: $ACCOUNT_ID_OPT)"
fi
if [ -n "$REVISION" ]; then
  case $REVISION in
    *[!0-9a-f]*) usage_die "--revision must be a full 40-digit commit id (got: $REVISION)" ;;
  esac
  [ "${#REVISION}" -eq 40 ] || usage_die "--revision must be a full 40-digit commit id (got: $REVISION)"
  [ "$CMD" = update ] || [ "$MODE" = full ] || [ "$FULL_ONLY" = 1 ] || usage_die "--revision goes with --update or --full"
fi
if [ "$ACK_TEARDOWN" = 1 ] && [ "$CMD" != teardown ]; then
  usage_die "--acknowledge-teardown goes with --teardown"
fi
if [ -n "$TEARDOWN_ONLY" ]; then
  [ "$CMD" = teardown ] || usage_die "--only goes with --teardown"
  case $TEARDOWN_ONLY in current|legacy) ;; *) usage_die "--only must be legacy or current (got: $TEARDOWN_ONLY)" ;; esac
fi
if [ "$ALLOW_ACTIVE_RUNS" = 1 ] && [ "$CMD" != update ]; then
  usage_die "--allow-active-runs goes with --update"
fi
if [ "$FORCE_DEPLOY" = 1 ] && [ "$CMD" != update ]; then
  usage_die "--force-deploy goes with --update"
fi
if [ "$CHANGE_OPERATOR" = 1 ] && [ "$CMD" != update ]; then
  usage_die "--change-operator goes with --update"
fi
# --force-deploy: a deploy even when the stage already runs this commit. The helper files read SETUP_FORCE_DEPLOY and the runner route passes it to its build as an
# override; a SETUP_FORCE_DEPLOY of the shell that started setup.sh does not count, and the variable is not exported.
unset SETUP_FORCE_DEPLOY
SETUP_FORCE_DEPLOY=$FORCE_DEPLOY
if [ "$NETWORK_WAIT_SET" = 1 ] && [ "$CMD" != teardown ]; then
  usage_die "--network-wait-minutes goes with --teardown"
fi
if [ "$CMD" = teardown ]; then
  # how long teardown waits for AWS to release the network interface AgentCore holds: the option, else HAMH_SETUP_NETWORK_WAIT_MINUTES, else 0
  if [ "$NETWORK_WAIT_SET" = 1 ]; then
    NETWORK_WAIT_MIN=$(network_wait_minutes "$NETWORK_WAIT_OPT") || usage_die "--network-wait-minutes must be a whole number of minutes from 0 to 480, or full (got: $NETWORK_WAIT_OPT)"
  elif [ -n "${HAMH_SETUP_NETWORK_WAIT_MINUTES:-}" ]; then
    NETWORK_WAIT_MIN=$(network_wait_minutes "$HAMH_SETUP_NETWORK_WAIT_MINUTES") || usage_die "HAMH_SETUP_NETWORK_WAIT_MINUTES must be a whole number of minutes from 0 to 480, or full (got: '$HAMH_SETUP_NETWORK_WAIT_MINUTES')"
  fi
fi
if [ -n "$CMD" ]; then
  [ -z "$MODE" ] || usage_die "--$CMD does not go with --full or --client-only"
  if [ -n "$FROM$ENDPOINT$SHA" ] || [ "$BUILD" = 1 ]; then
    usage_die "--$CMD works on the deployment in your AWS account; --from, --sha256, --build and --endpoint belong to the client-only setup"
  fi
  if [ "$CMD" != update ] && { [ "$KEEP_RUNNER" = 1 ] || [ "$ACK_COSTS" = 1 ] || [ "$PREFLIGHT_ONLY" = 1 ] || [ -n "$BUILD_ON" ]; }; then
    usage_die "--$CMD does not take --keep-runner, --acknowledge-costs, --preflight-only or --build-on"
  fi
  if [ "$CMD" != update ] && [ "$REPO_NAMES_SET" = 1 ]; then
    usage_die "--$CMD does not take --repo-name. --repo-name sets the repository names for the stage deployed by --full or --update."
  fi
else
  if [ "$FULL_ONLY" = 1 ]; then
    if [ "$REPO_NAMES_SET" = 1 ] && [ "$MODE" = client ]; then
      usage_die "--client-only does not take --repo-name. --repo-name sets the repository names for the stage deployed by the full setup. Set your default repository with --default-repo or $HAMH_CLIENT_BIN config set default_repo."
    fi
    [ "$MODE" != client ] || usage_die "--client-only does not take --keep-runner, --acknowledge-costs, --preflight-only, --build-on or --invocation-logging: they belong to the full setup"
    MODE=full
  fi
  if [ -n "$FROM$ENDPOINT$SHA" ] || [ "$BUILD" = 1 ]; then
    if [ "$MODE" = full ]; then
      # name the option the person typed: --full, else the first option that only the full setup takes
      case $FULL_VIA in
        ''|--full) usage_die "--full builds the client in your AWS account, so --from, --sha256, --build and --endpoint do not go with it (they belong to --client-only)" ;;
      esac
      usage_die "$FULL_VIA turns on the full setup, which builds the client in your AWS account, so --from, --sha256, --build and --endpoint do not go with it (they belong to --client-only)"
    fi
    MODE=client
  fi
fi

# ---- who and where ---------------------------------------------------------------------------------

UID_NOW=$(id -u 2>/dev/null || echo unknown)
if [ "$UID_NOW" = "0" ]; then
  die "refusing to run as root. $HAMH_CLIENT_BIN installs under your home directory: run this as your normal user."
fi
if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
  die "HOME is not set to a directory, so there is nowhere to install or configure."
fi

PREFIX_OPT=$PREFIX
[ -n "$PREFIX" ] || PREFIX=$HOME/.local
BINDIR=$PREFIX/bin
DEST=$BINDIR/$HAMH_CLIENT_BIN
ADMIN_DEST=$BINDIR/$HAMH_ADMIN_BIN
CLIENT_CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}/$HAMH_CONFIG_DIR_NAME/config.toml

[ "$DRY" = 1 ] && say "DRY RUN: nothing is written and no $HAMH_CLIENT_BIN command runs."

# ---- helpers -----------------------------------------------------------------------------------------

HASHTOOL=""
if command -v sha256sum >/dev/null 2>&1; then
  HASHTOOL=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  HASHTOOL=shasum
fi

hash_of() { # file -> lower-case SHA-256
  case $HASHTOOL in
    sha256sum) sha256sum "$1" | cut -d' ' -f1 ;;
    shasum) shasum -a 256 "$1" | cut -d' ' -f1 ;;
  esac
}

# sums_lookup <SHA256SUMS file> <file name>: the SHA-256 listed for that name, if any.
sums_lookup() {
  awk -v n="$2" '{ f = $2; sub(/^\*/, "", f); sub(/^\.\//, "", f); if (f == n && length($1) == 64) { print tolower($1); exit } }' "$1"
}

# config_dir_has_entry <folder>: 0 when the folder exists and holds an entry besides the .DS_Store of a file browser. A folder that cannot be listed counts as holding something.
config_dir_has_entry() {
  local list
  [ -d "$1" ] || return 1
  list=$(ls -A "$1" 2>/dev/null) || return 0
  # An empty list must count as empty, which grep -qv would not do.
  # shellcheck disable=SC2143
  [ -n "$(printf '%s\n' "$list" | grep -v '^\.DS_Store$')" ]
}

# config_has_content <folder>: 0 when the folder holds user content by the rule of the client (crates/ham-core/src/config_dir.rs, holds_user_content): a settings, continue or review file,
# or an engines, rules or skills folder with something in it. An empty folder, or one that holds only a file browser's file, is nothing.
config_has_content() {
  [ -f "$1/config.toml" ] || [ -f "$1/continue.md" ] || [ -f "$1/review.md" ] || config_dir_has_entry "$1/engines" || config_dir_has_entry "$1/rules" || config_dir_has_entry "$1/skills"
}

# config_migration_due <earlier folder> <current folder>: 0 when the earlier folder holds settings and the current folder does not count as holding any. A copy that stopped half way leaves the
# marker .<name>-migrating in the current folder, and the client takes that folder for empty while the earlier one is there, so the copy is still due.
config_migration_due() {
  config_has_content "$1" || return 1
  if [ -e "$2/.$HAMH_CONFIG_DIR_NAME-migrating" ] || [ -L "$2/.$HAMH_CONFIG_DIR_NAME-migrating" ]; then return 0; fi
  ! config_has_content "$2"
}

# read_cfg_key <camelCaseKey>: the value of that top-level key in the ham config file, if the file has it. It reads what TOML allows around a
# value: indentation, a comment after it, single or double quotes. cfg_get asks ham itself when a ham exists; this reader serves the first run
# (no ham yet) and the dry run (no ham command runs). While only the earlier settings folder holds settings (config_migration_due), it reads the file there, as the client
# itself does (COMPAT-CAH), so that a setup on a machine with only the earlier settings chooses the account and the Region they name.
read_cfg_key() {
  local file=$CLIENT_CONFIG home=${XDG_CONFIG_HOME:-$HOME/.config}
  if [ "$HAMH_LEGACY_CONFIG_DIR_NAME" != "$HAMH_CONFIG_DIR_NAME" ] && config_migration_due "$home/$HAMH_LEGACY_CONFIG_DIR_NAME" "$home/$HAMH_CONFIG_DIR_NAME"; then
    file=$home/$HAMH_LEGACY_CONFIG_DIR_NAME/config.toml
  fi
  [ -f "$file" ] || return 0
  awk -v want="$1" '
    /^[ \t]*\[/ { exit }
    {
      line = $0; sub(/^[ \t]+/, "", line)
      if (substr(line, 1, 1) == "#") next
      eq = index(line, "=")
      if (eq == 0) next
      k = substr(line, 1, eq - 1); sub(/[ \t]+$/, "", k)
      if (k != want) next
      v = substr(line, eq + 1); sub(/^[ \t]+/, "", v)
      q = substr(v, 1, 1)
      if (q == "\"" || q == "\047") {
        rest = substr(v, 2); stop = index(rest, q)
        if (stop > 0) { print substr(rest, 1, stop - 1); exit }
      }
    }' "$file"
}

# client_on_path: the path of the client on the PATH when it is this system's client, or nothing. A program that carries the name of the client is the client only when
# it holds the name of the header the client reads the backend version from (the file is searched, never run): another program with that name is not touched or used.
client_on_path() {
  local found
  found=$(command -v "$HAMH_CLIENT_BIN" 2>/dev/null || true)
  [ -n "$found" ] && [ -f "$found" ] || return 0
  if is_current_client "$found"; then printf '%s' "$found"; fi
  return 0
}

# is_current_client <file>: 0 when the file holds the name of the header the current client reads the backend version from (x-hamh-backend-version). The file is searched, never run;
# the client of the earlier name reads another header, so it is not taken for this one.
is_current_client() {
  grep -aq 'x-hamh-backend-version' "$1" 2>/dev/null
}

# ham_now: the path of a ham that exists right now (the one installed at DEST, else the one on the PATH), or nothing.
ham_now() {
  if [ -x "$DEST" ]; then echo "$DEST"; else client_on_path; fi
}

CFG_VAL=""
CFG_BAD=""
# cfg_get <snake_case_key> <camelCaseKey>: sets CFG_VAL to the value the ham config holds for that key. With a ham at hand (and no dry run) it asks
# `ham config get`; a failure that names the config file sets CFG_BAD to ham's text, because the file is then the problem, not the binary.
cfg_get() {
  local bin err
  CFG_VAL=""
  bin=""
  [ "$DRY" = 1 ] || bin=$(ham_now)
  if [ -n "$bin" ]; then
    if CFG_VAL=$("$bin" config get "$1" 2>/dev/null); then
      CFG_VAL=$(printf '%s\n' "$CFG_VAL" | head -1)
      return 0
    fi
    CFG_VAL=""
    err=$("$bin" config get "$1" 2>&1 >/dev/null)
    case $err in
      *"is not set"*) ;;
      *"$CLIENT_CONFIG"*|*config.toml*) [ -n "$CFG_BAD" ] || CFG_BAD=$err ;;
    esac
    return 0
  fi
  CFG_VAL=$(read_cfg_key "$2")
}

AWS_CFG_FILE=${AWS_CONFIG_FILE:-$HOME/.aws/config}
AWS_CRED_FILE=${AWS_SHARED_CREDENTIALS_FILE:-$HOME/.aws/credentials}

# pick_aws_files: sets AWS_F1 and AWS_F2 to the AWS files that exist (the config file first); both stay empty when neither exists.
pick_aws_files() {
  AWS_F1=""
  AWS_F2=""
  if [ -f "$AWS_CFG_FILE" ]; then AWS_F1=$AWS_CFG_FILE; fi
  if [ -f "$AWS_CRED_FILE" ]; then
    if [ -z "$AWS_F1" ]; then AWS_F1=$AWS_CRED_FILE; else AWS_F2=$AWS_CRED_FILE; fi
  fi
}

# profile_kind <name>: missing | credential_process | static | sso | assume | unknown (section headers and key names only;
# no value is read into this script). crates/ham/src/awsprofile.rs holds the same definition for ham; the two are run over the same
# files, fixtures/aws-profiles/, by scripts/tests/test_setup_script.py and crates/ham/tests/aws_profile_kinds.rs.
profile_kind() {
  pick_aws_files
  if [ -z "$AWS_F1" ]; then
    echo missing
    return 0
  fi
  awk -v want="$1" '
    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
    FNR == 1 { cur = 0 }
    /^[ \t]*\[/ {
      line = trim($0); sub(/^\[/, "", line); sub(/\].*$/, "", line); line = trim(line); sub(/^profile[ \t]+/, "", line)
      cur = (line == want); if (cur) found = 1
      next
    }
    cur {
      k = $0; sub(/[ \t]*=.*/, "", k); k = trim(k)
      if (k == "credential_process") cp = 1
      else if (k == "aws_access_key_id") st = 1
      else if (k ~ /^sso_/) sso = 1
      else if (k == "role_arn" || k == "web_identity_token_file") assume = 1
    }
    END {
      if (!found) print "missing"; else if (cp) print "credential_process"; else if (st) print "static"
      else if (sso) print "sso"; else if (assume) print "assume"; else print "unknown"
    }' "$AWS_F1" ${AWS_F2:+"$AWS_F2"}
}

# list_profiles: the profile names the AWS files define, one per line, in file order, each once. The same names as
# `aws configure list-profiles` gives (a [name] section counts in the credentials file, [default] and [profile name] in the config file),
# found with awk alone because the AWS CLI may be absent or too old.
list_profiles() {
  pick_aws_files
  [ -n "$AWS_F1" ] || return 0
  awk -v credfile="$AWS_CRED_FILE" '
    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
    FNR == 1 { iscred = (FILENAME == credfile) }
    /^[ \t]*\[/ {
      line = trim($0); sub(/^\[/, "", line); sub(/\].*$/, "", line); line = trim(line)
      if (iscred) { name = line }
      else if (line == "default") { name = "default" }
      else if (line ~ /^profile[ \t]+/) { sub(/^profile[ \t]+/, "", line); name = trim(line) }
      else { name = "" }
      if (name != "" && !(name in seen)) { seen[name] = 1; print name }
    }' "$AWS_F1" ${AWS_F2:+"$AWS_F2"}
}

# profile_region <name>: the region = value of that profile, if it has one.
profile_region() {
  pick_aws_files
  [ -n "$AWS_F1" ] || return 0
  awk -v want="$1" '
    function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
    FNR == 1 { cur = 0 }
    /^[ \t]*\[/ {
      line = trim($0); sub(/^\[/, "", line); sub(/\].*$/, "", line); line = trim(line); sub(/^profile[ \t]+/, "", line)
      cur = (line == want)
      next
    }
    cur {
      k = $0; sub(/[ \t]*=.*/, "", k); k = trim(k)
      if (k == "region") { v = $0; sub(/^[^=]*=/, "", v); print trim(v); exit }
    }' "$AWS_F1" ${AWS_F2:+"$AWS_F2"}
}

# kind_label <kind>: the words used for a profile kind in lists.
kind_label() {
  case $1 in
    credential_process) echo "credential_process" ;;
    static) echo "static keys" ;;
    sso) echo "SSO" ;;
    assume) echo "assumes a role" ;;
    *) echo "no credentials found" ;;
  esac
}

# print_profile_choices [skip-name]: on stderr, under the last FAIL, the profiles the AWS files define with their kind and the option that
# picks each. Returns 1 and prints nothing when there is no profile to show.
print_profile_choices() {
  local names n kind shown=0 total=0 started=0
  names=$(list_profiles)
  [ -n "$names" ] || return 1
  while IFS= read -r n; do
    [ -n "$n" ] || continue
    valid_profile_name "$n" || continue
    [ "$n" != "${1:-}" ] || continue
    total=$((total + 1))
    if [ "$started" = 0 ]; then
      detail "Profiles found:"
      started=1
    fi
    [ "$shown" -lt 20 ] || continue
    shown=$((shown + 1))
    kind=$(profile_kind "$n")
    case $kind in
      credential_process|static) printf '        %-16s %-20s usable: add --profile %s\n' "$n" "$(kind_label "$kind")" "$n" >&2 ;;
      sso|assume) printf '        %-16s %-20s usable (needs the AWS CLI 2.9.0 or later): add --profile %s\n' "$n" "$(kind_label "$kind")" "$n" >&2 ;;
      *) printf '        %-16s %-20s not usable\n' "$n" "$(kind_label "$kind")" >&2 ;;
    esac
  done <<EOF
$names
EOF
  [ "$started" = 1 ] || return 1
  [ "$total" -le "$shown" ] || detail "... and $((total - shown)) more; give one of them as --profile NAME"
  return 0
}

# no_profile_guidance: on stderr, how to get a profile ham can use when there is none to choose. setup.sh runs none of these commands.
no_profile_guidance() {
  detail "Make a profile with the AWS CLI, then run setup.sh again with  --profile NAME:"
  detail "    aws configure sso     a profile that signs in through IAM Identity Center (SSO): $AWS_CLI_SSO_URL"
  detail "    aws configure         a profile with an access key pair: $AWS_CLI_CONFIG_URL"
  detail "You can also add a profile with credential_process (a program that prints credentials as JSON) to $AWS_CFG_FILE yourself,"
  detail "or set the AWS_* environment variables. setup.sh runs none of these commands and edits no AWS file."
}

# aws_cli_check: sets AWS_CLI_STATE to missing | unreadable | old | ok, and AWS_CLI_VER to the version (2.37.2) when it could be read. The program
# is the one the environment variable HAMH_AWS_CLI names, else aws on the PATH: the choice ham makes. Asked once per run; it asks for the version
# and for nothing else.
AWS_CLI_STATE=""
AWS_CLI_VER=""
AWS_CLI_PROG=""
aws_cli_check() {
  local out word major rest minor
  [ -z "$AWS_CLI_STATE" ] || return 0
  AWS_CLI_PROG=${HAMH_AWS_CLI:-aws}
  if ! command -v "$AWS_CLI_PROG" >/dev/null 2>&1; then
    AWS_CLI_STATE=missing
    return 0
  fi
  out=$("$AWS_CLI_PROG" --version 2>&1) || true
  word=$(printf '%s\n' "$out" | awk '{ for (i = 1; i <= NF; i++) if (index($i, "aws-cli/") == 1) { print substr($i, 9); exit } }')
  case $word in
    *.*) ;;
    *) AWS_CLI_STATE=unreadable; return 0 ;;
  esac
  major=${word%%.*}
  rest=${word#*.}
  minor=${rest%%.*}
  case $major in ''|*[!0-9]*) AWS_CLI_STATE=unreadable; return 0 ;; esac
  case $minor in ''|*[!0-9]*) AWS_CLI_STATE=unreadable; return 0 ;; esac
  AWS_CLI_VER=$word
  if [ "$major" -gt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -ge 9 ]; }; then
    AWS_CLI_STATE=ok
  else
    AWS_CLI_STATE=old
  fi
}

# check_aws_cli <profile> <what the profile does>: an SSO or assume-role profile is read by the AWS CLI (ham runs `aws configure
# export-credentials`), so the AWS CLI must be there and be version 2.9.0 or later. Returns 1 after a FAIL. A version line that cannot be read is
# a warning only.
check_aws_cli() {
  aws_cli_check
  case $AWS_CLI_STATE in
    ok) return 0 ;;
    unreadable)
      warn "could not read the AWS CLI version from '$AWS_CLI_PROG --version'. SSO and assume-role profiles need the AWS CLI, version 2.9.0 or later; if $HAMH_CLIENT_BIN cannot get credentials, upgrade it: $AWS_CLI_INSTALL_URL"
      return 0 ;;
    missing)
      if [ -n "${HAMH_AWS_CLI:-}" ]; then
        fail "AWS profile $1 $2, which needs the AWS CLI" \
          "HAMH_AWS_CLI names $HAMH_AWS_CLI, and that was not found." \
          "Set HAMH_AWS_CLI to the AWS CLI, version 2.9.0 or later, or unset it. Install or upgrade the AWS CLI: $AWS_CLI_INSTALL_URL"
      else
        fail "AWS profile $1 $2, which needs the AWS CLI" \
          "The AWS CLI (aws) was not found on your PATH. It must be version 2.9.0 or later." \
          "Install or upgrade it: $AWS_CLI_INSTALL_URL" \
          "If it is installed elsewhere, set HAMH_AWS_CLI to its path."
      fi
      return 1 ;;
    *)
      fail "AWS profile $1 $2, which needs the AWS CLI, version 2.9.0 or later" \
        "The AWS CLI at $AWS_CLI_PROG is aws-cli/$AWS_CLI_VER, which is older than 2.9.0 and cannot export credentials." \
        "Upgrade it: $AWS_CLI_INSTALL_URL" \
        "If a newer one is installed elsewhere, set HAMH_AWS_CLI to its path."
      return 1 ;;
  esac
}

SSO_PROFILE=""
check_profile() { # name [where the name came from]
  local kind
  kind=$(profile_kind "$1")
  case $kind in
    credential_process) ok "AWS profile $1 uses credential_process${2:+ ($2)}" ;;
    static) ok "AWS profile $1 has static keys${2:+ ($2)}" ;;
    missing)
      fail "AWS profile $1 was not found" "looked in $AWS_CFG_FILE and $AWS_CRED_FILE"
      print_profile_choices || no_profile_guidance ;;
    sso)
      if check_aws_cli "$1" "signs in through IAM Identity Center (SSO)"; then
        ok "AWS profile $1 signs in through IAM Identity Center (SSO)${2:+ ($2)}; $HAMH_CLIENT_BIN asks the AWS CLI for its credentials"
        say "      Before each session, sign in:  aws sso login --profile $1"
        SSO_PROFILE=$1
      fi ;;
    assume)
      if check_aws_cli "$1" "assumes a role"; then
        ok "AWS profile $1 assumes a role${2:+ ($2)}; $HAMH_CLIENT_BIN asks the AWS CLI for its credentials"
        say "      If the role is assumed from an SSO profile, sign in to that profile first:  aws sso login --profile <that profile>"
      fi ;;
    *)
      fail "AWS profile $1 has no credentials in it" "it needs credential_process, aws_access_key_id, IAM Identity Center (sso_*) settings or role_arn."
      print_profile_choices "$1" || true ;;
  esac
}

# endpoint_region <url>: the Region in an API Gateway host name (https://<api-id>.execute-api.<region>.amazonaws.com/...), if it has one.
endpoint_region() {
  local host r
  host=${1#*://}
  host=${host%%/*}
  host=${host%%:*}
  case $host in
    *.execute-api.*.amazonaws.com|*.execute-api.*.amazonaws.com.cn)
      r=${host#*.execute-api.}
      echo "${r%%.*}" ;;
  esac
}

# region_profile: the profile whose region = line counts for ham: --profile, the ham config, AWS_PROFILE, else default.
region_profile() {
  local p=$PROFILE
  [ -n "$p" ] || p=$CFG_PROFILE
  [ -n "$p" ] || p=${AWS_PROFILE:-}
  [ -n "$p" ] || p=default
  echo "$p"
}

# check_region: ham signs for one Region and cannot guess it. It takes --region (written to the config), the config, AWS_REGION, then the
# region of the AWS profile. Fails before anything changes when none is set.
check_region() {
  local profile found host_region
  if [ -n "$REGION" ]; then ok "Region $REGION (from --region or your answer)"; return 0; fi
  if [ -n "$CFG_REGION" ]; then ok "Region $CFG_REGION (from your $HAMH_CLIENT_BIN config)"; return 0; fi
  if [ -n "${AWS_REGION:-}" ]; then ok "Region $AWS_REGION (from AWS_REGION)"; return 0; fi
  profile=$(region_profile)
  found=$(profile_region "$profile")
  if [ -n "$found" ]; then ok "Region $found (from the AWS profile $profile)"; return 0; fi
  host_region=$(endpoint_region "$ENDPOINT_NOW")
  fail "no AWS Region was found" \
    "$HAMH_CLIENT_BIN signs every call for one Region and cannot guess it. It takes the first of: --region, region in $CLIENT_CONFIG, AWS_REGION," \
    "or the region line of the AWS profile ($profile). None of them is set."
  if [ -n "$host_region" ]; then
    detail "The endpoint's host name says $host_region. Run setup.sh again with  --region $host_region"
  else
    detail "Run setup.sh again with  --region NAME, the Region of the stage (for example us-east-1)."
  fi
}

# ---- what the ham config already holds -----------------------------------------------------------------
# A config file ham cannot read is reported as that (in step 1), not as a broken binary.

cfg_get aws_profile awsProfile
CFG_PROFILE=$CFG_VAL
cfg_get api_endpoint apiEndpoint
CFG_ENDPOINT=$CFG_VAL
cfg_get region region
CFG_REGION=$CFG_VAL

# ---- questions (a terminal only) ---------------------------------------------------------------------------
# setup.sh asks only when stdin is a terminal and neither --yes nor --no-input was given, and only for what no option gave. An answer fills the
# same variable the option fills; the checks, the plan, the confirmation and the writes after this block are the same for both. Nothing is
# written while a question is open: q, the end of the input and a refused plan all leave everything as it was.

ANSWER=""
ASKED=0
BANNER_SHOWN=0
QN=0

# question_title <what the question asks> [ (optional)]: the heading of the next question. The questions are numbered in the order this run asks them (a question whose
# option was given is not asked and takes no number), and no total is announced because the run does not know it before the answers are in.
question_title() {
  QN=$((QN + 1))
  step "Question $QN${2:-}: $1"
}

banner() { # said once, before the first question
  [ "$BANNER_SHOWN" = 0 ] || return 0
  BANNER_SHOWN=1
  say "You answer one numbered question at a time. Press Enter to take the answer in [brackets]; type q to stop."
  say "Nothing is written until you confirm the plan at the end."
  say "To skip a question, give its option instead (./setup.sh --help), or add --yes to ask nothing."
}

# ask <prompt>: print the prompt and read one line into ANSWER. q, or the end of the input (Ctrl-D), stops the run: nothing has been changed.
ask() {
  printf '%s' "$1"
  if ! IFS= read -r ANSWER; then
    printf '\n'
    err "the input ended. Nothing was changed."
    exit 1
  fi
  ANSWER=$(printf '%s' "$ANSWER" | tr -d '\r' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
  case $ANSWER in
    q|Q) err "stopped at your request. Nothing was changed."; exit 1 ;;
  esac
}

too_many() { # <the option that gives this answer without a question>
  usage_die "three answers in a row were not usable. Give this answer as an option instead: $1"
}

# existing_client: the client program that is installed already (the one this script installs, else one on the PATH), or nothing.
existing_client() {
  if [ -x "$DEST" ]; then printf '%s' "$DEST"; else client_on_path; fi
}

# fail_no_client: the problem that a client route without a client program has, with the ways out. The check in step 2 reports it for a run that gave no question to answer
# (--yes or no terminal) and question_source reports it when the person stops at the source question.
fail_no_client() {
  local other
  fail "there is no $HAMH_CLIENT_BIN binary to install" \
    "No downloadable release of $HAMH_CLIENT_BIN is published. Pick one:" \
    "  --full       deploy the backend in your AWS account; the client is built there and downloaded for you (./setup.sh --full)" \
    "  --update     when the stage is already deployed: rebuild the client in your AWS account and download it" \
    "               (on an account whose stage was deployed under the previous names, --full and --update stop with the steps to take; there use --from or --build)" \
    "  --from DIR   a directory with $HAMH_CLIENT_BIN and SHA256SUMS, as scripts/build-macos-driver.sh writes it (a Mac build made on a Linux machine)" \
    "  --build      compile $HAMH_CLIENT_BIN on this machine from a checkout of the repository (Rust toolchain needed)" \
    "or put an existing $HAMH_CLIENT_BIN on your PATH."
  other=$(command -v "$HAMH_CLIENT_BIN" 2>/dev/null || true)
  if [ -n "$other" ] && [ -z "$(client_on_path)" ]; then
    detail "A program named $HAMH_CLIENT_BIN is on your PATH at $other. It does not report a backend version, so setup does not treat it as this client and does not run it."
  fi
}

# question_backend: question 1, the stage endpoint. With an endpoint already in the ham config it asks only to keep or change it; otherwise it
# offers: I have a URL, deploy one for me (the full setup), not now.
question_backend() {
  local tries=0 stored top hint
  banner
  stored=$CFG_ENDPOINT
  if [ -n "$stored" ]; then
    question_title "the stage endpoint"
    while :; do
      ask "Endpoint URL of the stage, from the V1ApiUrl output of the CloudFormation stack hamh-direct-control [$stored]: "
      if [ -z "$ANSWER" ]; then ENDPOINT=$stored; ASKED=1; return 0; fi
      if valid_endpoint "$ANSWER"; then ENDPOINT=$ANSWER; ASKED=1; return 0; fi
      say "That is not an https URL without spaces. The shape is https://<api-id>.execute-api.<region>.amazonaws.com/direct"
      tries=$((tries + 1))
      if [ "$tries" -ge 3 ]; then too_many "--endpoint URL"; fi
    done
  fi
  question_title "the $HAMH_CLIENT_BIN backend"
  say "$HAMH_CLIENT_BIN sends your prompt to a $HAMH_CLIENT_BIN stage: a deployed copy of the service in one AWS account."
  say "Do you have the URL of one?"
  say "  1) Yes, I have its URL (it ends with /direct)"
  if [ "$CLIENT_ONLY_TYPED" = 1 ]; then
    # --client-only was typed: nothing is created in the account, so the deploy answer is not offered
    top=2
    hint="Type 1 or 2."
    say "  2) Not now; install $HAMH_CLIENT_BIN and I will set the endpoint later"
  else
    top=3
    hint="Type 1, 2 or 3."
    say "  2) No, deploy a stage into my own AWS account for me (full setup)"
    say "  3) Not now; install $HAMH_CLIENT_BIN and I will set the endpoint later"
  fi
  while :; do
    ask "Choice [$top]: "
    [ -n "$ANSWER" ] || ANSWER=$top
    case $ANSWER in
      1) break ;;
      "$top") ASKED=1; return 0 ;; # not now: the last choice
      2) if [ "$top" = 3 ]; then MODE=full; ASKED=1; return 0; fi ;;
    esac
    say "$hint"
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--endpoint URL"; fi
  done
  tries=0
  while :; do
    ask "Endpoint URL (https://..., ends with /direct): "
    if valid_endpoint "$ANSWER"; then ENDPOINT=$ANSWER; ASKED=1; return 0; fi
    say "That is not an https URL without spaces. The shape is https://<api-id>.execute-api.<region>.amazonaws.com/direct"
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--endpoint URL"; fi
  done
}

# question_source: where the client program comes from. Asked on a terminal, for the client route, when no --from, --sha256 or --build was given and no client is installed:
# nothing downloads one, so the run would ask the profile and the Region and then stop at "there is no ham binary to install". It comes after the endpoint question (a person who
# answers "deploy one for me" takes the client from their own account and is not asked) and before every question whose answer would be thrown away.
question_source() {
  local tries=0 can_build=0 dir_n stop_n
  [ -z "$FROM$SHA" ] && [ "$BUILD" = 0 ] || return 0
  [ -z "$(existing_client)" ] || return 0
  banner
  question_title "where the $HAMH_CLIENT_BIN program comes from"
  say "$HAMH_CLIENT_BIN is not installed on this machine, and no downloadable release of it is published."
  if [ -f "$SELF_DIR/Cargo.toml" ] && [ -d "$SELF_DIR/crates/$HAMH_CLIENT_BIN" ] && command -v cargo >/dev/null 2>&1; then can_build=1; fi
  if [ "$can_build" = 1 ]; then
    say "  1) Build it on this machine from this checkout (cargo compiles it; the Rust toolchain is here)"
    dir_n=2
    stop_n=3
  else
    dir_n=1
    stop_n=2
  fi
  say "  $dir_n) Directory where I built it before (it holds $HAMH_CLIENT_BIN and SHA256SUMS, as scripts/build-macos-driver.sh writes it)"
  say "  $stop_n) Stop; I will run the full setup instead (./setup.sh --full builds the client in my AWS account)"
  while :; do
    ask "Source [1]: "
    [ -n "$ANSWER" ] || ANSWER=1
    if [ "$can_build" = 1 ] && [ "$ANSWER" = 1 ]; then BUILD=1; ASKED=1; return 0; fi
    if [ "$ANSWER" = "$dir_n" ]; then break; fi
    if [ "$ANSWER" = "$stop_n" ]; then
      fail_no_client
      err "$FAILS problem(s) found above. Nothing was changed."
      exit 1
    fi
    say "Type a number from 1 to $stop_n."
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--from DIR or --build"; fi
  done
  tries=0
  while :; do
    ask "Directory with $HAMH_CLIENT_BIN and SHA256SUMS: "
    case $ANSWER in
      "~"/*) ANSWER=$HOME/${ANSWER#"~"/} ;;
    esac
    if [ -n "$ANSWER" ] && { [ -d "$ANSWER" ] || [ -f "$ANSWER" ]; }; then FROM=$ANSWER; ASKED=1; return 0; fi
    say "There is no directory or file with that name."
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--from DIR"; fi
  done
}

# question_region: question 3, the Region. The default comes from the ham config, then the Region in the endpoint's host name, then AWS_REGION,
# then the region line of the chosen profile.
question_region() {
  local def hostr ep pick tries=0 prompt
  banner
  question_title "the Region"
  say "The stage runs in one AWS Region and $HAMH_CLIENT_BIN signs its calls for that Region."
  ep=$ENDPOINT
  [ -n "$ep" ] || ep=$CFG_ENDPOINT
  hostr=$(endpoint_region "$ep")
  def=$CFG_REGION
  [ -n "$def" ] || def=$hostr
  [ -n "$def" ] || def=${AWS_REGION:-}
  [ -n "$def" ] || def=$(profile_region "$(region_profile)")
  while :; do
    if [ -n "$def" ]; then
      prompt="Region [$def]: "
    else
      prompt="Region (for example us-east-1; Enter leaves it to AWS_REGION or the profile): "
    fi
    ask "$prompt"
    [ -n "$ANSWER" ] || ANSWER=$def
    if [ -z "$ANSWER" ]; then ASKED=1; return 0; fi
    if ! valid_region "$ANSWER"; then
      say "That does not look like a Region. The shape is us-east-1."
      tries=$((tries + 1))
      if [ "$tries" -ge 3 ]; then too_many "--region NAME"; fi
      continue
    fi
    pick=$ANSWER
    if [ -n "$hostr" ] && [ "$pick" != "$hostr" ]; then
      warn "the endpoint is in $hostr, you chose $pick. $HAMH_CLIENT_BIN must sign for the Region of the stage."
      ask "Keep $pick anyway? [y/N] "
      case $ANSWER in
        y|Y|yes|YES) ;;
        *) def=$hostr; continue ;;
      esac
    fi
    REGION=$pick
    ASKED=1
    return 0
  done
}

# question_repo: question 4, an optional default repository (a prefill for the terminal app; a run without one returns a zip).
question_repo() {
  local tries=0 stored
  banner
  cfg_get default_repo defaultRepo
  stored=$CFG_VAL
  question_title "a default repository" " (optional)"
  say "A run can open a pull request in a CodeCommit repository whose name starts with hamh-e2e- and that the stage operator allowed."
  say "Write it as cc:hamh-e2e-<name> to preselect it in the terminal app, or press Enter to skip (runs then return a zip)."
  while :; do
    ask "Default repository [${stored:-none}]: "
    if [ -z "$ANSWER" ]; then DEFAULT_REPO=$stored; ASKED=1; return 0; fi
    if valid_default_repo "$ANSWER"; then DEFAULT_REPO=$ANSWER; ASKED=1; return 0; fi
    say "That is not a repository name of the form cc:hamh-e2e-<name> (letters, digits, dot, underscore and dash after hamh-e2e-)."
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--default-repo REPO"; fi
  done
}

# question_mode: the first question, full (backend and client) or client only. The default is the full setup.
question_mode() {
  local tries=0
  banner
  question_title "What do you want to set up?"
  say "  1) Full setup: deploy the backend in my AWS account, then install and configure the client (recommended; creates AWS resources that cost money)"
  say "  2) Client only: install and configure the client for a backend that already exists"
  while :; do
    ask "Choice [1]: "
    case $ANSWER in
      ''|1) MODE=full; ASKED=1; return 0 ;;
      2) MODE=client; ASKED=1; return 0 ;;
    esac
    say "Type 1 or 2."
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--full or --client-only"; fi
  done
}

# question_build: where the backend is built. Asked only when this machine has the whole toolchain of the local route; otherwise the runner is the
# only sensible answer and nothing is asked.
question_build() {
  local tries=0
  [ -z "$(full_local_missing)" ] || return 0
  banner
  question_title "Where to build the backend"
  say "The backend (Rust programs, container images and the Cline binary) has to be compiled somewhere."
  say "  1) The runner in my AWS account (recommended; needs nothing installed on this machine)"
  say "  2) This machine (it has cargo, cargo-zigbuild, zig, node and docker)"
  while :; do
    ask "Where to build [1]: "
    case $ANSWER in
      ''|1) BUILD_ON=runner; ASKED=1; return 0 ;;
      2) BUILD_ON=local; ASKED=1; return 0 ;;
    esac
    say "Type 1 or 2."
    tries=$((tries + 1))
    if [ "$tries" -ge 3 ]; then too_many "--build-on runner or --build-on local"; fi
  done
}

# usable_profile <name>: 0 when the AWS files define the profile and ham can use it (credential_process, static keys, SSO or assume-role).
usable_profile() {
  local k
  valid_profile_name "$1" || return 1
  k=$(profile_kind "$1")
  case $k in
    credential_process|static|sso|assume) return 0 ;;
  esac
  return 1
}

# question_profile: question 2, the AWS profile. A menu built from the user's own AWS files; no key or secret is ever asked for.
question_profile() {
  local names list n rn rname rkind advice mark total usable only envkeys default_name default_show tries=0 first=1 prompt chosen kind
  banner
  question_title "the AWS profile"
  while :; do
    names=$(list_profiles | while IFS= read -r n; do valid_profile_name "$n" && printf '%s\n' "$n"; done)
    envkeys=0
    if [ -n "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "${AWS_SECRET_ACCESS_KEY:-}" ]; then envkeys=1; fi
    if [ -z "$names" ] && [ "$envkeys" = 0 ]; then
      fail "no AWS profile was found" "looked in $AWS_CFG_FILE and $AWS_CRED_FILE"
      no_profile_guidance
      err "stopped. Nothing was changed."
      exit 1
    fi
    list=""
    total=0
    usable=0
    only=""
    while IFS= read -r n; do
      [ -n "$n" ] || continue
      total=$((total + 1))
      kind=$(profile_kind "$n")
      case $kind in
        credential_process|static|sso|assume) usable=$((usable + 1)); only=$n ;;
      esac
      if [ "$total" -le 20 ]; then
        list="$list$total $n $kind
"
      fi
    done <<EOF
$names
EOF
    # the default: the profile in the ham config, the AWS_* variables, AWS_PROFILE, the profile named default, the only usable profile
    default_name=""
    if [ -n "$CFG_PROFILE" ] && usable_profile "$CFG_PROFILE"; then
      default_name=$CFG_PROFILE
    elif [ "$envkeys" = 1 ]; then
      default_name=0
    elif [ -n "${AWS_PROFILE:-}" ] && usable_profile "$AWS_PROFILE"; then
      default_name=$AWS_PROFILE
    elif usable_profile default; then
      default_name=default
    elif [ "$usable" = 1 ]; then
      default_name=$only
    fi
    default_show=$default_name
    if [ -n "$default_name" ] && [ "$default_name" != 0 ]; then
      while IFS=' ' read -r rn rname rkind; do
        if [ "$rname" = "$default_name" ]; then default_show=$rn; fi
      done <<EOF
$list
EOF
    fi
    if [ "$first" = 1 ]; then
      first=0
      say "$HAMH_CLIENT_BIN signs every call with the credentials of one AWS profile, and that profile must belong to the"
      say "AWS account that hosts the stage (a session from another account is refused)."
      say "$HAMH_CLIENT_BIN reads a static-key or credential_process profile itself. For an SSO or assume-role profile it asks"
      say "the AWS CLI (version 2.9.0 or later) for the credentials. Profiles found in $AWS_CFG_FILE and $AWS_CRED_FILE:"
    else
      say "Profiles found now:"
    fi
    if [ "$envkeys" = 1 ]; then
      printf '%3d) %s\n' 0 "the AWS_* variables of this shell (nothing is saved; $HAMH_CLIENT_BIN needs them in every shell)"
    fi
    while IFS=' ' read -r rn rname rkind; do
      [ -n "$rn" ] || continue
      case $rkind in
        credential_process|static) advice="usable" ;;
        sso|assume) advice="usable (needs the AWS CLI 2.9.0 or later)" ;;
        *) advice="not usable" ;;
      esac
      mark=""
      if [ "$rname" = "${AWS_PROFILE:-}" ]; then mark="   <- AWS_PROFILE"; fi
      printf '%3d) %-14s %-20s %s%s\n' "$rn" "$rname" "$(kind_label "$rkind")" "$advice" "$mark"
    done <<EOF
$list
EOF
    if [ "$total" -gt 20 ]; then
      say "     ... and $((total - 20)) more; type a name"
    fi
    if [ -n "$default_show" ]; then
      prompt="Profile number or name, r to read the files again [$default_show]: "
    else
      prompt="Profile number or name, r to read the files again: "
    fi
    ask "$prompt"
    case $ANSWER in
      r|R) continue ;;
      '') ANSWER=$default_name ;;
    esac
    chosen=""
    if [ -z "$ANSWER" ]; then
      say "Type a number or one of the names above."
    elif [ "$ANSWER" = 0 ] && [ "$envkeys" = 1 ]; then
      chosen=ENV
    else
      case $ANSWER in
        *[!0-9]*)
          if valid_profile_name "$ANSWER" && [ "$(profile_kind "$ANSWER")" != missing ]; then chosen=$ANSWER; fi ;;
        *)
          while IFS=' ' read -r rn rname rkind; do
            if [ "$rn" = "$ANSWER" ]; then chosen=$rname; fi
          done <<EOF
$list
EOF
          ;;
      esac
      [ -n "$chosen" ] || say "There is no profile with that name in the list. Type a number or one of the names above."
    fi
    if [ -z "$chosen" ]; then
      tries=$((tries + 1))
      if [ "$tries" -ge 3 ]; then too_many "--profile NAME"; fi
      continue
    fi
    if [ "$chosen" = ENV ]; then
      if [ -n "$CFG_PROFILE" ]; then
        warn "your $HAMH_CLIENT_BIN config names the profile $CFG_PROFILE, which $HAMH_CLIENT_BIN uses before the AWS_* variables. To use the variables, run: $HAMH_CLIENT_BIN config unset aws_profile"
      fi
      ASKED=1
      return 0
    fi
    kind=$(profile_kind "$chosen")
    case $kind in
      credential_process|static|sso|assume)
        PROFILE=$chosen
        ASKED=1
        return 0 ;;
      *)
        say "The profile $chosen has no credentials in it (it needs credential_process, aws_access_key_id, SSO settings or role_arn). Pick another one."
        tries=$((tries + 1))
        if [ "$tries" -ge 3 ]; then too_many "--profile NAME"; fi ;;
    esac
  done
}

INTERACTIVE=0
if [ "$YES" = 0 ] && [ "$NO_INPUT" = 0 ] && [ -t 0 ] && [ -z "$CFG_BAD" ]; then INTERACTIVE=1; fi

# The helper files of the full setup (scripts/setup/). A client-only setup does not need them; the full setup and the commands do.
HELPERS_OK=1
for helper in lib json accounts state client runner preflight full status teardown; do
  [ -f "$SELF_DIR/scripts/setup/$helper.sh" ] || HELPERS_OK=0
done
if [ "$HELPERS_OK" = 1 ]; then
  for helper in lib json accounts state client runner preflight full status teardown; do
    # shellcheck disable=SC1090
    . "$SELF_DIR/scripts/setup/$helper.sh"
  done
fi
need_helpers() { # what needs them
  [ "$HELPERS_OK" = 1 ] || die "$1 needs the helper files in scripts/setup/ of the repository, and they are missing next to this setup.sh. Get the whole repository (git clone) and run its setup.sh. A client-only setup needs setup.sh alone."
}

if [ "$INTERACTIVE" = 1 ] && [ -z "$CMD" ]; then
  [ -n "$MODE" ] || question_mode
  if [ "$MODE" = client ]; then
    [ -n "$ENDPOINT" ] || question_backend
    # The answer "deploy one for me" turns the mode into full inside question_backend, so this test can fire although the block is the client-only one.
    # --full and the options that only the full setup takes were refused above, where the options are read; a client of your own does not go with either.
    if [ "$MODE" = full ] && { [ -n "$FROM$SHA" ] || [ "$BUILD" = 1 ]; }; then
      usage_die "you chose the full setup, but --from, --sha256 or --build names a client of your own. The full setup builds the client in your AWS account: run again without them, or use --client-only."
    fi
    # the client program: asked before the profile and the Region, so that a run without one stops before they are asked
    [ "$MODE" != client ] || question_source
  fi
  [ -n "$PROFILE" ] || question_profile
  [ -n "$REGION" ] || question_region
  if [ "$MODE" = full ]; then
    need_helpers "the full setup"
    [ -n "$BUILD_ON" ] || question_build
  elif [ -z "$DEFAULT_REPO" ] && { [ -n "$ENDPOINT" ] || [ -n "$CFG_ENDPOINT" ]; }; then
    question_repo
  fi
fi
[ -n "$CMD" ] || [ -n "$MODE" ] || MODE=client

# ---- the commands of the later life of the system ---------------------------------------------------------------------
case $CMD in
  status) need_helpers "--status"; run_status ;;
  doctor) need_helpers "--doctor"; run_doctor ;;
  teardown) need_helpers "--teardown"; run_teardown ;;
  update) need_helpers "--update" ;;
esac
[ "$MODE" != full ] || need_helpers "the full setup"

# ---- step 1: this machine ----------------------------------------------------------------------------

step "Checking this machine"
OS=$(uname -s 2>/dev/null || echo unknown)
ARCH=$(uname -m 2>/dev/null || echo unknown)
ok "platform $OS $ARCH"
case $OS in Darwin|Linux) ;; *) warn "$HAMH_CLIENT_BIN is built for macOS and Linux; $OS is not tested" ;; esac

if ! command -v git >/dev/null 2>&1; then
  fail "git was not found" "$HAMH_CLIENT_BIN clone builds a git repository from the zip a run delivers, and needs git." \
    "macOS: xcode-select --install    Linux: install git with your package manager."
elif ! git --version >/dev/null 2>&1; then
  fail "git is installed but does not run" "On macOS the stub in /usr/bin/git needs the command line tools: xcode-select --install"
else
  ok "git works"
fi

ENDPOINT_NOW=$ENDPOINT
[ -n "$ENDPOINT_NOW" ] || ENDPOINT_NOW=$CFG_ENDPOINT

if [ -n "$CFG_BAD" ]; then
  fail "$HAMH_CLIENT_BIN cannot read its config file" "$CLIENT_CONFIG:" "$CFG_BAD" \
    "The $HAMH_CLIENT_BIN binary itself runs; the file is the problem. Fix the line it names, or move the file aside, then run setup.sh again."
else
  # The credentials, in the order ham uses them: the profile in the ham config (--profile writes it there), else the AWS_* keys, else
  # AWS_PROFILE, else the default profile.
  if [ -n "$PROFILE" ]; then
    check_profile "$PROFILE"
  elif [ -n "$CFG_PROFILE" ]; then
    check_profile "$CFG_PROFILE" "the profile in your $HAMH_CLIENT_BIN config"
  elif [ -n "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "${AWS_SECRET_ACCESS_KEY:-}" ]; then
    CRED_FROM_ENV=1
    ok "AWS credentials come from the AWS_* environment variables"
  else
    CRED_PROFILE=${AWS_PROFILE:-}
    CRED_ORIGIN=""
    if [ -n "$CRED_PROFILE" ]; then CRED_ORIGIN="from AWS_PROFILE"; else CRED_PROFILE=default; fi
    if [ "$CRED_PROFILE" = default ] && [ "$(profile_kind default)" = missing ]; then
      fail "no AWS credentials source was found" \
        "$HAMH_CLIENT_BIN signs every call with an IAM identity in the account that hosts the stage." \
        "No profile was chosen, no AWS_* keys are set and there is no [default] profile (looked in $AWS_CFG_FILE and $AWS_CRED_FILE)."
      if print_profile_choices; then
        detail "Run setup.sh again with  --profile NAME  (or set AWS_PROFILE)."
      else
        no_profile_guidance
      fi
    else
      check_profile "$CRED_PROFILE" "$CRED_ORIGIN"
    fi
  fi

  # The Region matters when ham doctor will run: an endpoint is given or already stored.
  if [ -n "$ENDPOINT_NOW" ]; then check_region; fi
  case $ENDPOINT_NOW in
    ''|*/direct) ;;
    *) warn "the endpoint does not end with /direct. The V1ApiUrl of the stage does; without it every call is refused with HTTP 403." ;;
  esac
fi

command -v unzip >/dev/null 2>&1 || warn "unzip was not found; you need it only to open the zip that $HAMH_CLIENT_BIN pull downloads ($HAMH_CLIENT_BIN clone does not)"

# ---- the full setup: the backend and the client in your own AWS account ---------------------------------------------------------
# full_run asks for nothing but the one confirmation, builds and deploys, and leaves FROM (the checked client), ENDPOINT and YES=1 for the steps below.
if [ "$MODE" = full ] || [ "$CMD" = update ]; then
  if [ "$FAILS" -gt 0 ]; then
    err "$FAILS problem(s) found above. Nothing was changed."
    exit 1
  fi
  full_run "${CMD:-full}"
  ENDPOINT_NOW=$ENDPOINT
  # the client signs with the profile this run used (an option, or the settings it read), so that the profile, the endpoint and the Region it writes name one account
  [ -n "$PROFILE" ] || PROFILE=${PROFILE_EFF:-}
fi

# ---- the link with the earlier command name (COMPAT-CAH) -----------------------------------------------------------------
# setup.sh installs the client under its name and, beside it, a relative link with the name the client had before (LEGACY_CLIENT_BIN), so that the earlier command keeps
# working for a while: running it prints a notice on stderr and then behaves as the client. Only setup.sh makes the link; another way to install the client gets none.
# An existing file at that path is replaced only when it is a link to the client, or when it is recognised without being run as an earlier client of this system: its SHA-256
# is a line of a receipt of an earlier setup (download/verified/SHA256SUMS) or it holds the name of the header the earlier client reads the backend version from. Any other file
# is kept, with a warning.
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
ALIAS=$BINDIR/$LEGACY_CLIENT_BIN
ALIAS_ACTION=skip

# alias_recognised <file>: 0 when the file is an earlier client of this system. The file is searched and hashed, never run.
alias_recognised() {
  local f=$1 sum dir sums
  if grep -aq 'x-cah-backend-version' "$f" 2>/dev/null; then return 0; fi  # COMPAT-CAH
  sum=$(hash_of "$f" 2>/dev/null) || return 1
  [ -n "$sum" ] || return 1
  for dir in "$CONFIG_HOME/$HAMH_CONFIG_DIR_NAME" "$CONFIG_HOME/$HAMH_LEGACY_CONFIG_DIR_NAME"; do
    for sums in "$dir"/setup/*/*/download/verified/SHA256SUMS; do
      [ -f "$sums" ] || continue
      if awk -v h="$sum" '$1 == h { found = 1 } END { exit found ? 0 : 1 }' "$sums"; then return 0; fi
    done
  done
  return 1
}

# alias_decide: sets ALIAS_ACTION to make, replace, ok, keep or skip (the client is not installed beside the link).
alias_decide() {
  ALIAS_ACTION=skip
  if [ -z "$FROM" ] && [ "$BUILD" != 1 ] && [ "$EXISTING" != "$DEST" ]; then return 0; fi
  if [ -L "$ALIAS" ]; then
    case $(readlink "$ALIAS") in
      "$HAMH_CLIENT_BIN"|"$DEST") ALIAS_ACTION=ok; return 0 ;;
    esac
  fi
  if [ -L "$ALIAS" ] || [ -e "$ALIAS" ]; then
    if [ -f "$ALIAS" ] && alias_recognised "$ALIAS"; then ALIAS_ACTION=replace; else ALIAS_ACTION=keep; fi
  else
    ALIAS_ACTION="make"
  fi
}

# alias_apply: the step. A relative link is made under a temporary name and renamed over the old file, so that a stop in the middle leaves the old file or the new link.
alias_apply() {
  case $ALIAS_ACTION in
    make|replace)
      if [ "$DRY" = 1 ]; then
        would "link $ALIAS to $HAMH_CLIENT_BIN with a relative link"
        return 0
      fi
      mkdir -p "$BINDIR" || die "could not create $BINDIR"
      if ln -sfn "$HAMH_CLIENT_BIN" "$BINDIR/.cah.setup-new" && mv -f "$BINDIR/.cah.setup-new" "$ALIAS"; then  # COMPAT-CAH
        if [ "$ALIAS_ACTION" = replace ]; then ok "replaced the earlier $ALIAS with a link to $HAMH_CLIENT_BIN"; else ok "linked $ALIAS to $HAMH_CLIENT_BIN"; fi
        say "      $LEGACY_CLIENT_BIN is the earlier command name. Running it prints a notice on stderr and then behaves as $HAMH_CLIENT_BIN. The link stays for a limited time."
      else
        warn "could not make the link $ALIAS (a partial link may remain at $BINDIR/.cah.setup-new); $HAMH_CLIENT_BIN is installed and works under its own name"  # COMPAT-CAH
      fi ;;
    ok) ok "$ALIAS is already a link to $HAMH_CLIENT_BIN" ;;
    keep) warn "$ALIAS exists and is not a link to $HAMH_CLIENT_BIN or an earlier client of this system, so setup left it as it is and made no link. Move that file aside and run setup.sh again if you want the link." ;;
  esac
}

# ---- step 2: where the ham binary comes from -----------------------------------------------------------

SRC=""
SRC_SHA=""
ADMIN_SRC=""
ADMIN_SHA=""
PLAN_BIN=""
EXISTING=""
BUILD_OUT=""
BUILD_ADMIN_OUT=""

if [ -n "$FROM" ] || [ "$BUILD" = 1 ]; then
  if [ -z "$HASHTOOL" ]; then
    fail "no checksum tool was found" "install sha256sum (Linux) or shasum (macOS has it); setup.sh checks every binary it installs."
  fi
fi

if [ -n "$FROM" ]; then
  SUMS=""
  if [ -d "$FROM" ]; then SRC=$FROM/$HAMH_CLIENT_BIN; SUMS=$FROM/SHA256SUMS; else SRC=$FROM; SUMS=$(dirname "$FROM")/SHA256SUMS; fi
  if [ ! -f "$SRC" ] && [ -d "$FROM" ] && [ -f "$FROM/$LEGACY_CLIENT_BIN" ]; then  # COMPAT-CAH
    fail "$FROM holds $LEGACY_CLIENT_BIN, the earlier name of the client, and no $HAMH_CLIENT_BIN" \
      "This setup installs $HAMH_CLIENT_BIN. Nothing is installed." \
      "Build or obtain $HAMH_CLIENT_BIN from this repository (the full setup builds it in your AWS account), then pass the directory that holds it."
    SRC=""
  elif [ ! -f "$SRC" ]; then
    fail "no $HAMH_CLIENT_BIN binary at $SRC" "--from takes a directory that holds $HAMH_CLIENT_BIN (and SHA256SUMS), or the $HAMH_CLIENT_BIN file itself."
    SRC=""
  elif [ -n "$HASHTOOL" ]; then
    WANT=$SHA
    if [ -z "$WANT" ] && [ -f "$SUMS" ]; then WANT=$(sums_lookup "$SUMS" "$(basename "$SRC")"); fi
    if [ -z "$WANT" ]; then
      fail "no checksum to compare $SRC with" "give --sha256 HEX, or keep the SHA256SUMS file that scripts/build-macos-driver.sh wrote beside the binary."
      SRC=""
    else
      GOT=$(hash_of "$SRC")
      if [ "$GOT" != "$WANT" ]; then
        fail "checksum mismatch for $SRC" "expected $WANT" "got      $GOT" "Refusing to install it."
        SRC=""
      elif ! is_current_client "$SRC"; then
        fail "$SRC is not the current client: it lacks the header text x-hamh-backend-version that the current client reads" \
          "A file that matches its checksum is the file you named, and this one is an earlier client or another program. Nothing is installed." \
          "Build or obtain $HAMH_CLIENT_BIN from this repository (the full setup builds it in your AWS account), then pass the file or the directory that holds it."
        SRC=""
      else
        SRC_SHA=$GOT
        ok "checksum of $SRC matches ($GOT)"
        PLAN_BIN="install $SRC to $DEST"
      fi
    fi
  fi
  # The operator tool is installed beside the client when the folder holds it (the full setup's checked download does). Its checksum comes from the
  # SHA256SUMS of that folder alone, and a file that does not match it stops the setup before anything is installed.
  if [ -n "$SRC_SHA" ] && [ -d "$FROM" ] && [ -f "$FROM/$HAMH_ADMIN_BIN" ]; then
    ADMIN_WANT=""
    if [ -f "$SUMS" ]; then ADMIN_WANT=$(sums_lookup "$SUMS" "$HAMH_ADMIN_BIN"); fi
    if [ -z "$ADMIN_WANT" ]; then
      fail "no checksum to compare $FROM/$HAMH_ADMIN_BIN with" "keep the SHA256SUMS file with a line for $HAMH_ADMIN_BIN beside it, or take the file out of the folder."
    else
      ADMIN_GOT=$(hash_of "$FROM/$HAMH_ADMIN_BIN")
      if [ "$ADMIN_GOT" != "$ADMIN_WANT" ]; then
        fail "checksum mismatch for $FROM/$HAMH_ADMIN_BIN" "expected $ADMIN_WANT" "got      $ADMIN_GOT" "Refusing to install it, or $HAMH_CLIENT_BIN."
      else
        ADMIN_SRC=$FROM/$HAMH_ADMIN_BIN
        ADMIN_SHA=$ADMIN_GOT
        ok "checksum of $ADMIN_SRC matches ($ADMIN_GOT)"
      fi
    fi
  fi
elif [ "$BUILD" = 1 ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    fail "cargo was not found" "--build compiles $HAMH_CLIENT_BIN on this machine and needs the Rust toolchain (https://rustup.rs)." \
      "The repository pins Rust 1.98.1 in rust-toolchain.toml; rustup installs it on first use."
  fi
  if [ ! -f "$SELF_DIR/Cargo.toml" ] || [ ! -d "$SELF_DIR/crates/$HAMH_CLIENT_BIN" ]; then
    fail "--build must run from a checkout of the repository" "$SELF_DIR has no Cargo.toml with crates/$HAMH_CLIENT_BIN."
  fi
  BUILD_OUT=${CARGO_TARGET_DIR:-$SELF_DIR/target}
  case $BUILD_OUT in /*) ;; *) BUILD_OUT=$SELF_DIR/$BUILD_OUT ;; esac
  BUILD_ADMIN_OUT=$BUILD_OUT/release/$HAMH_ADMIN_BIN
  BUILD_OUT=$BUILD_OUT/release/$HAMH_CLIENT_BIN
  if [ "${BUILD_ADMIN:-0}" = 1 ]; then
    # the full setup on this machine (--build-on local) builds the operator tool in the same cargo run
    BUILD_CMD="cargo build --release --locked -p $HAMH_CLIENT_BIN -p $HAMH_ADMIN_BIN --bin $HAMH_CLIENT_BIN --bin $HAMH_ADMIN_BIN"
    PLAN_BIN="build $HAMH_CLIENT_BIN and the operator tool $HAMH_ADMIN_BIN here ($BUILD_CMD), then install them to $BINDIR"
  else
    BUILD_CMD="cargo build --release --locked -p $HAMH_CLIENT_BIN --bin $HAMH_CLIENT_BIN"
    PLAN_BIN="build $HAMH_CLIENT_BIN here ($BUILD_CMD), then install it to $DEST"
  fi
else
  if [ -x "$DEST" ]; then
    EXISTING=$DEST
  else
    EXISTING=$(client_on_path)
  fi
  if [ -n "$EXISTING" ]; then
    PLAN_BIN="use the $HAMH_CLIENT_BIN that is already installed ($EXISTING)"
  else
    fail_no_client
  fi
fi

if [ "$FAILS" -gt 0 ]; then
  err "$FAILS problem(s) found above. Nothing was changed."
  if type full_runner_cleanup >/dev/null 2>&1; then full_runner_cleanup; fi
  exit 1
fi

# shq <word>: the word, quoted for a shell when it needs it.
shq() {
  case $1 in
    ''|*[!A-Za-z0-9_./:@%+=,-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s' "$1" ;;
  esac
}

# again_line: this run as one command line that asks nothing, so the same setup can be repeated with the answers given as options.
again_line() {
  local line
  line=$(shq "$0")
  [ -z "$FROM" ] || line="$line --from $(shq "$FROM")"
  [ -z "$SHA" ] || line="$line --sha256 $SHA"
  [ "$BUILD" = 0 ] || line="$line --build"
  [ -z "$PREFIX_OPT" ] || line="$line --prefix $(shq "$PREFIX_OPT")"
  [ -z "$ENDPOINT" ] || line="$line --endpoint $(shq "$ENDPOINT")"
  [ -z "$PROFILE" ] || line="$line --profile $(shq "$PROFILE")"
  [ -z "$REGION" ] || line="$line --region $(shq "$REGION")"
  [ -z "$DEFAULT_REPO" ] || line="$line --default-repo $(shq "$DEFAULT_REPO")"
  printf '%s --yes\n' "$line"
}

# ---- plan and consent ------------------------------------------------------------------------------------

alias_decide
step "Plan"
say "  binary:  $PLAN_BIN"
case $ALIAS_ACTION in
  make|replace) say "  link:    $ALIAS to $HAMH_CLIENT_BIN, a relative link with the earlier command name" ;;
  keep) say "  link:    none; $ALIAS exists and is left as it is" ;;
esac
if [ -n "$ADMIN_SRC" ]; then
  say "  operator tool: install $ADMIN_SRC to $ADMIN_DEST ($HAMH_ADMIN_BIN allows the repositories that may receive pull requests; the account's operator uses it)"
fi
if [ -n "$ENDPOINT$PROFILE$REGION$DEFAULT_REPO" ]; then
  say "  config:  set${ENDPOINT:+ api_endpoint}${PROFILE:+ aws_profile}${REGION:+ region}${DEFAULT_REPO:+ default_repo} in $CLIENT_CONFIG"
else
  say "  config:  no option given, so $CLIENT_CONFIG is not touched"
fi
say "  rules:   $HAMH_CLIENT_BIN rules init (the default rules when you have none, the lifecycle rules and its seven skills; it replaces only an unedited earlier default or pack file)"
say "  checks:  $HAMH_CLIENT_BIN models, $HAMH_CLIENT_BIN --version, and $HAMH_CLIENT_BIN doctor when an endpoint is set"
if [ "$ASKED" = 1 ]; then
  say "  again:   $(again_line)"
fi
if [ "$BUILD" = 1 ]; then
  say "  build:   a first build takes about 11 minutes, needs 3 GB of disk and needs rustup (https://rustup.rs)"
fi

if [ "$DRY" = 0 ] && [ "$YES" = 0 ]; then
  if [ "$NO_INPUT" = 1 ]; then
    usage_die "--no-input was given, so there is no one to ask: add --yes to go ahead, or --dry-run to preview"
  elif [ -t 0 ]; then
    printf 'Go ahead? [y/N] '
    read -r ANSWER
    case $ANSWER in y|Y|yes|YES) ;; *) err "stopped at your request. Nothing was changed."; exit 1 ;; esac
  else
    usage_die "there is no terminal to ask on: add --yes to go ahead without a prompt, or --dry-run to preview"
  fi
fi

# client_tried <staged file>: the new client must run on this machine before it is installed: `--version` (and it names the VERSION of the commit that was built, when the commit
# has one) and, when a settings file exists, `models`, which reads it. A client that fails stays out: the one at DEST is left as it is, and the run ends with exit 5 (full_runner_cleanup removes
# a runner that is still installed, or names it RETAINED).
client_tried() {
  local out rc first
  out=$("$1" --version 2>&1)
  rc=$?
  if [ "$rc" = 0 ] && [ -n "${SRC_VERSION_TEXT:-}" ]; then
    case $out in
      *"v$SRC_VERSION_TEXT"*) ;;
      *) rc=version ;;
    esac
  fi
  if [ "$rc" = 0 ] && [ -f "$CLIENT_CONFIG" ]; then
    out=$("$1" models 2>&1)
    rc=$?
  fi
  [ "$rc" != 0 ] || return 0
  first=$(printf '%s\n' "$out" | head -n 1 | cut -c1-200)
  if [ "$rc" = version ]; then
    err "the new $HAMH_CLIENT_BIN does not print the version $SRC_VERSION_TEXT of the commit that was built (it printed: $first), so it is not installed."
  else
    err "the new $HAMH_CLIENT_BIN does not run on this machine (exit $rc: $first), so it is not installed."
  fi
  if [ -e "$DEST" ]; then
    detail "Your earlier client at $DEST stays as it is and still runs."
  else
    detail "Nothing was installed at $DEST."
  fi
  detail "The backend is updated (${ENDPOINT:-the endpoint above}). The checked client stays at $SRC for you to look at."
  detail "Run $(full_retry_command) to try again once the cause is fixed, or install a client you have with  setup.sh --client-only --from DIR."
  full_runner_cleanup
  exit 5
}

# ---- step 2 (do): the binary -------------------------------------------------------------------------------

step "Installing $HAMH_CLIENT_BIN"
HAM=""
if [ "$BUILD" = 1 ]; then
  say "Building $HAMH_CLIENT_BIN on this machine. You can instead cross-build for a Mac on a Linux machine with"
  say "scripts/build-macos-driver.sh and install the result with --from."
  if [ "$DRY" = 1 ]; then
    would "run: $BUILD_CMD (in $SELF_DIR)"
  else
    if [ "${BUILD_ADMIN:-0}" = 1 ]; then
      if ! ( cd "$SELF_DIR" && cargo build --release --locked -p "$HAMH_CLIENT_BIN" -p "$HAMH_ADMIN_BIN" --bin "$HAMH_CLIENT_BIN" --bin "$HAMH_ADMIN_BIN" ); then
        die "build failed. Nothing was installed."
      fi
    elif ! ( cd "$SELF_DIR" && cargo build --release --locked -p "$HAMH_CLIENT_BIN" --bin "$HAMH_CLIENT_BIN" ); then
      die "build failed. Nothing was installed."
    fi
    [ -f "$BUILD_OUT" ] || die "build failed: cargo finished but $BUILD_OUT does not exist."
    SRC=$BUILD_OUT
    SRC_SHA=$(hash_of "$SRC")
    ok "built $SRC ($SRC_SHA)"
    if [ "${BUILD_ADMIN:-0}" = 1 ]; then
      [ -f "$BUILD_ADMIN_OUT" ] || die "build failed: cargo finished but $BUILD_ADMIN_OUT does not exist."
      ADMIN_SRC=$BUILD_ADMIN_OUT
      ADMIN_SHA=$(hash_of "$ADMIN_SRC")
      ok "built $ADMIN_SRC ($ADMIN_SHA)"
    fi
  fi
  HAM=$DEST
elif [ -n "$FROM" ]; then
  HAM=$DEST
else
  HAM=$EXISTING
  ok "using the $HAMH_CLIENT_BIN at $EXISTING; no install needed"
fi

if [ -n "$FROM" ] || [ "$BUILD" = 1 ]; then
  if [ "$DRY" = 1 ] && [ "$BUILD" = 1 ]; then
    would "install the built $HAMH_CLIENT_BIN to $DEST"
  elif [ -f "$DEST" ] && [ "$(hash_of "$DEST")" = "$SRC_SHA" ]; then
    ok "$HAMH_CLIENT_BIN is already installed at $DEST (same checksum); left as it is"
  elif [ "$DRY" = 1 ]; then
    would "install $SRC to $DEST (sha256 $SRC_SHA)"
  else
    mkdir -p "$BINDIR" || die "could not create $BINDIR"
    TMPBIN=$BINDIR/.$HAMH_CLIENT_BIN.setup-new
    # Die is meant for a failed copy and a failed chmod alike.
    # shellcheck disable=SC2015
    cp "$SRC" "$TMPBIN" && chmod 755 "$TMPBIN" || die "could not install $HAMH_CLIENT_BIN into $BINDIR (a partial copy may remain at $TMPBIN; the next run overwrites it)"
    # the client a full setup or an update built is tried in the install folder before it takes the place of the one that runs (C10): a client that does not run here never
    # replaces it, and the one it replaces stays beside it as DEST.prev
    [ "${FULL_RAN:-0}" != 1 ] || client_tried "$TMPBIN"
    if [ -f "$DEST" ] && [ "${FULL_RAN:-0}" = 1 ]; then
      cp -p "$DEST" "$DEST.prev" || warn "could not keep the earlier client as $DEST.prev"
    fi
    if mv -f "$TMPBIN" "$DEST"; then
      ok "installed $DEST (sha256 $SRC_SHA)"
    else
      die "could not install $HAMH_CLIENT_BIN into $BINDIR (a partial copy may remain at $TMPBIN; the next run overwrites it)"
    fi
  fi
  # the operator tool goes into the same folder, by the same steps
  if [ -n "$ADMIN_SRC" ] || { [ "$DRY" = 1 ] && [ "${BUILD_ADMIN:-0}" = 1 ]; }; then
    if [ "$DRY" = 1 ] && [ "$BUILD" = 1 ]; then
      would "install the built $HAMH_ADMIN_BIN to $ADMIN_DEST"
    elif [ -f "$ADMIN_DEST" ] && [ "$(hash_of "$ADMIN_DEST")" = "$ADMIN_SHA" ]; then
      ok "$HAMH_ADMIN_BIN is already installed at $ADMIN_DEST (same checksum); left as it is"
    elif [ "$DRY" = 1 ]; then
      would "install $ADMIN_SRC to $ADMIN_DEST (sha256 $ADMIN_SHA)"
    else
      mkdir -p "$BINDIR" || die "could not create $BINDIR"
      TMPADMIN=$BINDIR/.$HAMH_ADMIN_BIN.setup-new
      if cp "$ADMIN_SRC" "$TMPADMIN" && chmod 755 "$TMPADMIN" && mv -f "$TMPADMIN" "$ADMIN_DEST"; then
        ok "installed $ADMIN_DEST (sha256 $ADMIN_SHA)"
      else
        die "could not install $HAMH_ADMIN_BIN into $BINDIR (a partial copy may remain at $TMPADMIN; the next run overwrites it)"
      fi
    fi
  fi
  case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *) warn "$BINDIR is not on your PATH. Add this line to your shell profile (setup.sh does not edit it):"
       say "        export PATH=\"$BINDIR:\$PATH\"" ;;
  esac
fi

alias_apply

# ---- step 3: prove the binary ------------------------------------------------------------------------------

step "Checking the binary (offline)"
if [ "$DRY" = 1 ]; then
  would "run: $HAMH_CLIENT_BIN models"
  would "run: $HAMH_CLIENT_BIN --version"
else
  MODELS_OUT=$("$HAM" models 2>&1)
  MODELS_RC=$?
  if [ "$MODELS_RC" -ne 0 ]; then
    printf '%s\n' "$MODELS_OUT" >&2
    case $MODELS_OUT in
      *"$CLIENT_CONFIG"*|*config.toml*)
        if "$HAM" --version >/dev/null 2>&1; then
          die "$HAMH_CLIENT_BIN could not read its config file $CLIENT_CONFIG (the line above). The $HAMH_CLIENT_BIN binary itself runs. Fix that line, or move the file aside, then run setup.sh again. Nothing else was set up."
        fi ;;
    esac
    die "$HAMH_CLIENT_BIN models failed (exit $MODELS_RC), so $HAM does not run here. Nothing else was set up."
  fi
  printf '%s\n' "$MODELS_OUT" | sed 's/^/      /'
  ok "$HAMH_CLIENT_BIN runs and lists its models"
  if type full_installed >/dev/null 2>&1; then full_installed; fi
  # the notices of the client go to its stderr (for example that only the earlier settings folder holds settings): the line below shows the version, which is on stdout
  if VERSION_OUT=$("$HAM" --version 2>/dev/null); then
    ok "installed: $(printf '%s\n' "$VERSION_OUT" | head -1)"
  else
    warn "$HAMH_CLIENT_BIN --version did not print a version (exit $?); this $HAMH_CLIENT_BIN may be older than the version tag."
  fi
fi

# ---- step 4: config ----------------------------------------------------------------------------------------

cfg_set() { # key value
  local key=$1 val=$2 cur=""
  if [ -x "$HAM" ]; then cur=$("$HAM" config get "$key" 2>/dev/null) || cur=""; fi
  if [ -n "$cur" ] && [ "$cur" = "$val" ]; then
    ok "$key is already $val"
    return 0
  fi
  if [ "$DRY" = 1 ]; then
    would "run: $HAMH_CLIENT_BIN config set $key $val"
    return 0
  fi
  "$HAM" config set "$key" "$val" >/dev/null || die "$HAMH_CLIENT_BIN config set $key failed"
  if [ -n "$cur" ]; then ok "$key changed from $cur to $val"; else ok "$key set to $val"; fi
}

step "Config"
# Settings that only the earlier folder holds are copied first (COMPAT-CAH): the client reads that folder and writes nowhere else, and a write while only the earlier folder holds
# settings is refused with exit 2. The client's own `config migrate` makes the copy and leaves the earlier folder as it is.
if [ "$HAMH_LEGACY_CONFIG_DIR_NAME" != "$HAMH_CONFIG_DIR_NAME" ] && config_migration_due "$CONFIG_HOME/$HAMH_LEGACY_CONFIG_DIR_NAME" "$CONFIG_HOME/$HAMH_CONFIG_DIR_NAME"; then
  say "Your settings are in the earlier folder $CONFIG_HOME/$HAMH_LEGACY_CONFIG_DIR_NAME. Setup copies them to $CONFIG_HOME/$HAMH_CONFIG_DIR_NAME with $HAMH_CLIENT_BIN config migrate and leaves the earlier folder as it is."
  if [ "$DRY" = 1 ]; then
    would "run: $HAMH_CLIENT_BIN config migrate"
  else
    "$HAM" config migrate || die "$HAMH_CLIENT_BIN config migrate failed (exit $?); your settings are still in $CONFIG_HOME/$HAMH_LEGACY_CONFIG_DIR_NAME"
    ok "settings copied to $CONFIG_HOME/$HAMH_CONFIG_DIR_NAME"
  fi
fi
if [ -n "$ENDPOINT$PROFILE$REGION$DEFAULT_REPO" ]; then
  say "config file: $CLIENT_CONFIG"
  [ -z "$ENDPOINT" ] || cfg_set api_endpoint "$ENDPOINT"
  [ -z "$PROFILE" ] || cfg_set aws_profile "$PROFILE"
  [ -z "$REGION" ] || cfg_set region "$REGION"
  [ -z "$DEFAULT_REPO" ] || cfg_set default_repo "$DEFAULT_REPO"
else
  say "No --endpoint, --profile, --region or --default-repo given: $CLIENT_CONFIG is not touched."
fi
# the runner of the full setup goes only now, after the client's config names the stage (full_configured of scripts/setup/full.sh)
if type full_configured >/dev/null 2>&1; then full_configured; fi

# ---- step 5: rules -----------------------------------------------------------------------------------------

step "Rules"
if [ "$DRY" = 1 ]; then
  would "run: $HAMH_CLIENT_BIN rules init"
else
  RULES_OUT=$("$HAM" rules init 2>&1)
  RULES_RC=$?
  printf '%s\n' "$RULES_OUT" | sed 's/^/      /'
  [ "$RULES_RC" -eq 0 ] || die "$HAMH_CLIENT_BIN rules init failed (exit $RULES_RC)"
  ok "rules are in place; change them with: $HAMH_CLIENT_BIN rules edit"
fi

# ---- step 6: doctor ----------------------------------------------------------------------------------------

step "Smoke check ($HAMH_CLIENT_BIN doctor)"
HAVE_ENDPOINT=0
[ -z "$ENDPOINT" ] || HAVE_ENDPOINT=1
cfg_get api_endpoint apiEndpoint
[ -z "$CFG_VAL" ] || HAVE_ENDPOINT=1
PROBLEM=0
if [ "$HAVE_ENDPOINT" = 0 ]; then
  say "Skipped: no apiEndpoint is set yet."
  say "The endpoint is the V1ApiUrl output of the CloudFormation stack hamh-direct-control, in the account that hosts the stage. Two ways to get one:"
  say "  1. Ask the person who runs the stage for its URL, then run:"
  say "       $HAMH_CLIENT_BIN config set api_endpoint <that URL>"
  say "       $HAMH_CLIENT_BIN doctor"
  say "  2. Deploy a stage into your own AWS account: run  ./setup.sh --full  (it asks for what it needs, deploys the backend there and sets this"
  say "     client up for it)."
elif [ "$DRY" = 1 ]; then
  would "run: $HAMH_CLIENT_BIN doctor"
else
  DOCTOR_OUT=$("$HAM" doctor 2>&1)
  DOCTOR_RC=$?
  printf '%s\n' "$DOCTOR_OUT" | sed 's/^/      /'
  ID_LINE=$(printf '%s\n' "$DOCTOR_OUT" | awk '$1 == "PASS" && $2 == "identity" { print; exit }')
  if [ -n "$ID_LINE" ]; then
    ID_WHO=$(printf '%s\n' "$ID_LINE" | awk '{ print $3 }')
    ID_ACCT=$(printf '%s\n' "$ID_LINE" | awk '$(NF - 1) == "account" { print $NF }')
    if [ -n "$ID_WHO" ] && [ -n "$ID_ACCT" ]; then
      say "$HAMH_CLIENT_BIN signs as $ID_WHO in account $ID_ACCT. That must be the account that hosts the stage."
    fi
  fi
  FAILED=$(printf '%s\n' "$DOCTOR_OUT" | awk '$1 == "FAIL" { print $2 }')
  WARNED=$(printf '%s\n' "$DOCTOR_OUT" | awk '$1 == "WARN" { print $2 }')
  printf '%s\n' "$WARNED" | grep -q '^versions$' &&
    warn "versions: your $HAMH_CLIENT_BIN and the stage's backend report different versions, which is advice and does not stop a run; update whichever is older when you can."
  if [ "$DOCTOR_RC" -eq 0 ]; then
    ok "$HAMH_CLIENT_BIN doctor found no failing check"
    # A WARN line is advice and never changes doctor's exit code; the catalog one says the smoke records are old.
    printf '%s\n' "$WARNED" | grep -q '^catalog$' &&
      warn "catalog: the model smoke records are more than 24 h old. That is advice for the person who runs the stage, not a setup problem; the models can still be used."
  else
    PROBLEM=1
    if [ -z "$FAILED" ]; then
      SAID=$(printf '%s\n' "$DOCTOR_OUT" | awk 'NF { last = $0 } END { print last }')
      warn "$HAMH_CLIENT_BIN doctor exited $DOCTOR_RC without a FAIL line. The last thing it said: $SAID"
    fi
    printf '%s\n' "$FAILED" | grep -q '^identity$' &&
      warn "identity: the stage refused the credentials. It accepts callers of its own AWS account only, so the profile must belong to the account that hosts the stage: ask the person who runs the stage for a role in that account. If the profile is right, the credentials may have expired: sign in again (for an SSO profile: aws sso login --profile NAME), then run $HAMH_CLIENT_BIN doctor again."
    printf '%s\n' "$FAILED" | grep -q '^api$' &&
      warn "api: check apiEndpoint in $CLIENT_CONFIG ($HAMH_CLIENT_BIN config get api_endpoint), and that your identity belongs to the account that hosts the stage."
    printf '%s\n' "$FAILED" | grep -q '^catalog$' &&
      warn "catalog: the stage lists no selectable model, no model has a smoke time, or the model list could not be read. Ask the person who runs the stage; $HAMH_CLIENT_BIN models --server shows what the stage serves."
  fi
fi

if [ "$PROBLEM" = 1 ]; then
  err "$HAMH_CLIENT_BIN is installed, but $HAMH_CLIENT_BIN doctor found a problem (see above)."
  exit 3
fi

# ---- done ----------------------------------------------------------------------------------------------------

if [ "$DRY" = 1 ]; then
  step "Dry run finished"
  say "Nothing was written. Run again without --dry-run to do it."
  exit 0
fi

if [ "${FULL_RAN:-0}" = 1 ]; then
  full_summary "$DEST"
fi
step "Next steps"
if [ -n "$SSO_PROFILE" ]; then
  say "  Before each session, sign in:   aws sso login --profile $SSO_PROFILE"
fi
cat <<EOF
  1. Look at your rules, and edit them if you like:   $HAMH_CLIENT_BIN rules show      $HAMH_CLIENT_BIN rules edit
  2. See the five models and which one a run uses:    $HAMH_CLIENT_BIN models
  3. Start a run (a deployed stage and its endpoint are needed):
       $HAMH_CLIENT_BIN "Build a small command-line todo app in Python with tests" --s3-only --direct
     or, to get a pull request in a CodeCommit repository:
       $HAMH_CLIENT_BIN "<your prompt>" --repo cc:hamh-e2e-<name> --direct
     or run $HAMH_CLIENT_BIN alone to open the terminal app and type the prompt there.
  4. Follow it, then collect the work:
       $HAMH_CLIENT_BIN watch          $HAMH_CLIENT_BIN pull <runId>          $HAMH_CLIENT_BIN clone <runId>
  More: docs/user-guide.md in this repository.
EOF
if [ "${RUNNER_LEFT:-0}" = 1 ]; then
  if [ -n "${RN_LEFT_COMMAND:-}" ]; then
    err "setup is done, but the runner could not be removed (see RETAINED above). To remove only the runner, run the command printed after RETAINED. ./setup.sh --teardown also removes the stage."
  else
    err "setup is done, but the runner could not be removed (see RETAINED above for the cause). ./setup.sh --teardown removes the runner together with the stage."
  fi
  exit 8
fi
exit 0
