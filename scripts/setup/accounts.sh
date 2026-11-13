# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): the generator of the account file.
#
# acct_generate <account> <region> <profile> <zones> <azs> <logging> [<caller arn> [<repo names> [<direct zone ids>]]]
#   <zones> is what `aws ec2 describe-availability-zones` gave for the Region: one line per zone, the zone name, a tab, the zone id.
#   <caller arn> is the Arn of `aws sts get-caller-identity`: the identity behind the credentials, who runs setup and later operates the stage. The file
#   names it in the entry's `operator` (see acct_operator), so the stage trusts that identity and not an IAM role called Admin that a new account does not have.
#   Without the seventh argument the entry has no `operator` and the stage keeps the IAM role Admin; setup always passes it, and an empty one is a refusal.
#   <repo names> is the stage's repository-name rule as comma separated entries (setup.sh --repo-name, or the rule an update finds on the stage). A nonempty one is written as the
#   entry's `repoNames`; without it the entry has no such field and the stage keeps the default rule, hamh-e2e-*. setup.sh has checked every entry; the characters are checked here again
#   because they go into a JSON file.
#   <direct zone ids> are the zones of the stage that an update keeps (its record names them): the entry's direct and drill zones are these, in this order,
#   in place of the lowest available ones; each must be a supported zone of the list, else nothing is made. Empty: the lowest zones, as for a new stage.
#   Sets ACCT_JSON (the file, ending in a newline), ACCT_DIGEST (its SHA-256), ACCT_KEY (config/accounts-<first 12 digits of the digest>.json),
#   ACCT_DIRECT_AZ ("<name> <id>" of each zone the stage uses, separated by ", ") and ACCT_REPO_NAMES (the <repo names> the file carries, empty for the default rule).
#   Returns 0, or 1 after one line on stderr that says why nothing was made. It makes no AWS call and writes no file.
#
# The file has the schema of the committed accounts.json (schemaVersion 1, one entry with the role `active`). Placeholders are refused, a Region the
# stage does not support is refused, and the zones are the ones AgentCore accepts in that Region, taken in the order of their zone ids, so the same
# account gives the same file on every run.

# acct_supported_ids <region>: the AgentCore zone ids the stage can use in that Region; returns 1 for a Region the stage does not support.
acct_supported_ids() {
  case $1 in
    us-east-1) echo "use1-az1 use1-az2 use1-az4" ;;
    us-west-2) echo "usw2-az1 usw2-az2 usw2-az3" ;;
    *) return 1 ;;
  esac
}

# acct_placeholder <account>: 0 when the value is not an account id a person could have: not 12 digits, one digit repeated, or one of the ids that
# the AWS documentation uses in its examples (written in pieces so that this file holds no account id).
acct_placeholder() {
  local doc_a doc_b first rest
  doc_a=$(printf '%s%s%s' 1234 5678 9012)
  doc_b=$(printf '%s%s%s' 1111 2222 3333)
  case $1 in
    ''|*[!0-9]*) return 0 ;;
  esac
  [ "${#1}" -eq 12 ] || return 0
  [ "$1" != "$doc_a" ] || return 0
  [ "$1" != "$doc_b" ] || return 0
  first=$(printf '%s' "$1" | cut -c1)
  rest=$(printf '%s' "$1" | tr -d "$first")
  [ -n "$rest" ] || return 0
  return 1
}

# acct_operator <caller arn> <account>: the operator the entry names for the identity behind the credentials. Sets ACCT_OP_KIND (role or user) and ACCT_OP_NAME and returns 0,
# or says on stderr why the identity cannot be the operator of a stage and returns 1. The identity is whoever `sts get-caller-identity` answers with:
#   arn:aws:sts::<account>:assumed-role/<role>/<session>   a role session, an IAM Identity Center one included (the role is AWSReservedSSO_<permission set>_<hash>): kind role, the role's name
#   arn:aws:iam::<account>:user/[<path>/]<name>            an IAM user: kind user, the user's name without the path
# The root user and a federated user (GetFederationToken) have no role or user name the stage could trust, so they are refused with the way out. A name of 1 to 64 letters,
# digits or + = . @ _ - is the only kind the account file accepts (scripts/lib/aws_target.py, infra/lib/operator.ts; the table infra/test/fixtures/operator-identity.json).
ACCT_OP_KIND=""
ACCT_OP_NAME=""
acct_operator() {
  local arn=$1 account=$2 rest name
  ACCT_OP_KIND=""
  ACCT_OP_NAME=""
  case $arn in
    "arn:aws:iam::$account:root")
      err "the credentials are for the root user of the account ($arn). A stage needs an IAM user or an IAM role as its operator, and the root user is neither."
      detail "Sign in as an IAM user, an IAM role or an IAM Identity Center user (aws sso login --profile NAME) and run setup again. Nothing was created."
      return 1 ;;
    "arn:aws:sts::$account:federated-user/"*)
      err "the credentials are for a federated user ($arn), made with GetFederationToken. A stage needs an IAM user or an IAM role as its operator."
      detail "Sign in as an IAM user, an IAM role or an IAM Identity Center user (aws sso login --profile NAME) and run setup again. Nothing was created."
      return 1 ;;
    "arn:aws:sts::$account:assumed-role/"?*/?*)
      rest=${arn#"arn:aws:sts::$account:assumed-role/"}
      name=${rest%%/*}
      case ${rest#*/} in
        */*) rest="" ;;
      esac
      [ -n "$rest" ] && [ -n "$name" ] && ACCT_OP_KIND=role ;;
    "arn:aws:iam::$account:user/"?*)
      rest=${arn#"arn:aws:iam::$account:user/"}
      name=${rest##*/}
      case /$rest/ in
        *//*) name="" ;;
      esac
      [ -n "$name" ] && ACCT_OP_KIND=user ;;
    arn:aws:sts::*:*|arn:aws:iam::*:*)
      # an ARN of the same shape in another account is a different problem from one setup cannot read
      rest=${arn#arn:aws:*::}
      rest=${rest%%:*}
      if [ "$rest" != "$account" ]; then
        err "the credentials are for another account than $account: '$arn'."
        return 1
      fi ;;
  esac
  if [ -z "$ACCT_OP_KIND" ]; then
    err "setup cannot name the identity behind your credentials as the operator of the stage: '$arn'."
    detail "It knows IAM users (arn:aws:iam::<account>:user/<name>) and role sessions (arn:aws:sts::<account>:assumed-role/<role>/<session>), IAM Identity Center ones included."
    return 1
  fi
  case $name in
    ''|*[!A-Za-z0-9+=.@_-]*)
      err "the name '$name' of the identity behind your credentials has a character that an IAM role or user name of the account file may not have (1 to 64 letters, digits or + = . @ _ -; no path, no wildcard, no comma)."
      ACCT_OP_KIND=""
      return 1 ;;
  esac
  if [ "${#name}" -gt 64 ]; then
    err "the name '$name' of the identity behind your credentials has ${#name} characters; the account file takes 1 to 64."
    ACCT_OP_KIND=""
    return 1
  fi
  ACCT_OP_NAME=$name
  return 0
}

# acct_list "<word> <word>...": the words as a JSON list of strings: ["a", "b"].
acct_list() {
  local out="" w
  for w in $1; do
    if [ -z "$out" ]; then out="\"$w\""; else out="$out, \"$w\""; fi
  done
  printf '[%s]' "$out"
}

# acct_names_list "<entry>,<entry>...": the entries as a JSON list of strings: ["a", "b*"]. The star is not expanded: the entries are split with read, not by the shell's word splitting.
acct_names_list() {
  local out="" w
  local -a words
  IFS=, read -r -a words <<<"$1"
  for w in "${words[@]}"; do
    if [ -z "$out" ]; then out="\"$w\""; else out="$out, \"$w\""; fi
  done
  printf '[%s]' "$out"
}

ACCT_JSON=""
ACCT_DIGEST=""
ACCT_KEY=""
ACCT_DIRECT_AZ=""
ACCT_REPO_NAMES=""

acct_generate() {
  local account=$1 region=$2 profile=$3 zones=$4 want=$5 logging=$6 caller=${7-} names=${8-} _direct_want=${9-} named=0
  if [ "$#" -ge 7 ]; then named=1; fi
  local ids name zid line n ok_ids="" sorted live_names="" live_ids="" direct_names="" direct_ids="" count=0 direct_pairs=""
  ACCT_JSON=""
  ACCT_DIGEST=""
  ACCT_KEY=""
  ACCT_DIRECT_AZ=""
  ACCT_REPO_NAMES=""
  if acct_placeholder "$account"; then
    err "the account id '$account' is a placeholder or is not 12 digits. Setup reads the account id from your AWS credentials and never takes one from a template."
    return 1
  fi
  if ! ids=$(acct_supported_ids "$region"); then
    err "the stage deploys only in us-east-1 or us-west-2 (got Region '$region')."
    return 1
  fi
  case $profile in
    ''|*[!A-Za-z0-9._-]*) err "the profile name '$profile' is not a profile name."; return 1 ;;
  esac
  case $logging in
    read-only|enable-and-restore) ;;
    *) err "the invocation logging mode must be read-only or enable-and-restore (got '$logging')."; return 1 ;;
  esac
  case $want in
    1) ;;
    *) err "the stage uses 1 Availability Zone (got '$want')."; return 1 ;;
  esac
  case $names in
    *[!A-Za-z0-9._*,-]*) err "the repository names '$names' hold a character that a repository name, a prefix with * or a comma may not."; return 1 ;;
  esac
  if [ "$named" -eq 1 ]; then
    acct_operator "$caller" "$account" || return 1
  fi
  # every line must be a zone of this Region with a zone id of the usual shape, or nothing is trusted
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    zid=${line#*"$TAB"}
    if [ "$name" = "$line" ] || [ -z "$zid" ]; then
      err "the zone list has a line that is not 'name<TAB>id': $line"
      return 1
    fi
    case $name in
      "$region"[a-z]) ;;
      *) err "the zone '$name' is not a zone of $region."; return 1 ;;
    esac
    case $zid in
      *[!a-z0-9-]*|*-az|*-az*[!0-9]*|"") err "the zone id '$zid' of '$name' is not a zone id."; return 1 ;;
    esac
    case $zid in
      *-az[0-9]*) ;;
      *) err "the zone id '$zid' of '$name' is not a zone id."; return 1 ;;
    esac
  done <<EOF
$zones
EOF
  # the zones AgentCore accepts, in the order of their ids
  sorted=$(printf '%s\n' "$zones" | sort -t "$TAB" -k2,2)
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    zid=${line#*"$TAB"}
    case " $ids " in
      *" $zid "*) ok_ids="$ok_ids$name $zid$NL" ;;
    esac
  done <<EOF
$sorted
EOF
  if [ -z "$ok_ids" ]; then
    err "this account has no Availability Zone in $region that AgentCore supports. The stage needs one with the id $(printf '%s' "$ids" | sed 's/ / or /g')."
    return 1
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line% *}
    zid=${line#* }
    if [ "$count" -lt 3 ]; then
      live_names="$live_names $name"
      live_ids="$live_ids $zid"
    fi
    if [ -z "$_direct_want" ] && [ "$count" -lt "$want" ]; then
      direct_names="$direct_names $name"
      direct_ids="$direct_ids $zid"
      if [ -z "$direct_pairs" ]; then direct_pairs="$name $zid"; else direct_pairs="$direct_pairs, $name $zid"; fi
    fi
    count=$((count + 1))
  done <<EOF
$ok_ids
EOF
  # an update keeps the zone the stage was deployed in (the record names its id): it must be a zone the stage can use that the Region lists as available
  count=0
  for zid in $_direct_want; do
    [ "$count" -lt "$want" ] || break
    name=$(printf '%s' "$ok_ids" | awk -v z="$zid" '$2 == z { print $1; exit }')
    if [ -z "$name" ]; then
      err "the zone id '$zid' is not an AgentCore zone of $region that is available now."
      return 1
    fi
    direct_names="$direct_names $name"
    direct_ids="$direct_ids $zid"
    if [ -z "$direct_pairs" ]; then direct_pairs="$name $zid"; else direct_pairs="$direct_pairs, $name $zid"; fi
    count=$((count + 1))
  done
  live_names=$(acct_list "$live_names")
  live_ids=$(acct_list "$live_ids")
  direct_names=$(acct_list "$direct_names")
  direct_ids=$(acct_list "$direct_ids")
  n=$NL
  ACCT_JSON="{${n}"
  ACCT_JSON="$ACCT_JSON  \"schemaVersion\": 1,${n}"
  ACCT_JSON="$ACCT_JSON  \"selector\": \"HAMH_AWS_PROFILE\",${n}"
  ACCT_JSON="$ACCT_JSON  \"accounts\": [${n}"
  ACCT_JSON="$ACCT_JSON    {${n}"
  ACCT_JSON="$ACCT_JSON      \"account\": \"$account\",${n}"
  ACCT_JSON="$ACCT_JSON      \"region\": \"$region\",${n}"
  ACCT_JSON="$ACCT_JSON      \"profile\": \"$profile\",${n}"
  ACCT_JSON="$ACCT_JSON      \"role\": \"active\",${n}"
  if [ "$named" -eq 1 ]; then
    ACCT_JSON="$ACCT_JSON      \"operator\": {\"kind\": \"$ACCT_OP_KIND\", \"name\": \"$ACCT_OP_NAME\"},${n}"
  fi
  ACCT_JSON="$ACCT_JSON      \"azs\": {\"live\": $live_names, \"drill\": $direct_names, \"direct\": $direct_names},${n}"
  ACCT_JSON="$ACCT_JSON      \"azIds\": {\"live\": $live_ids, \"drill\": $direct_ids, \"direct\": $direct_ids},${n}"
  if [ -n "$names" ]; then
    ACCT_JSON="$ACCT_JSON      \"invocationLogging\": \"$logging\",${n}"
    ACCT_JSON="$ACCT_JSON      \"repoNames\": $(acct_names_list "$names")${n}"
    ACCT_REPO_NAMES=$names
  else
    ACCT_JSON="$ACCT_JSON      \"invocationLogging\": \"$logging\"${n}"
  fi
  ACCT_JSON="$ACCT_JSON    }${n}"
  ACCT_JSON="$ACCT_JSON  ]${n}"
  ACCT_JSON="$ACCT_JSON}${n}"
  ACCT_DIGEST=$(printf '%s' "$ACCT_JSON" | setup_sha256)
  ACCT_KEY=config/accounts-$(printf '%s' "$ACCT_DIGEST" | cut -c1-12).json
  ACCT_DIRECT_AZ=$direct_pairs
  return 0
}
