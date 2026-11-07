# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): `setup.sh --teardown`. It removes this deployment's stage and the runner, and says plainly
# what it could not remove and what it leaves.
#
# It runs here, with the credentials of the person who typed the command. The authority to delete the stage is theirs already (they deployed it, and the
# deployment is the larger act), so nothing is delegated: no runner is installed, no source is uploaded, no build is started, no role is assumed and no
# authority window opens. It needs no git clone and no clean tree. CloudFormation deletes each stack with the execution role it stored with the stack, so the
# caller needs no permission to pass that role.
#
# What belongs to the stage is decided by the stage's own tags and its own stacks, never by a name prefix alone:
#   - a stack is the stage's when its name starts with <prefix>-<stage>- AND it carries the tags project=<project> and stage=<stage> (the CDK app puts both on every stack);
#     a stack with the name and without the tags is named in the plan and left alone;
#   - everything found through a stack (buckets, image repositories, state machines, rules, the sessions table, the research secret, the runtimes) comes from the
#     resource list of that stack, and a thing outside every stack (a runtime, a bucket, a VPC, SSM parameters) is the stage's only with the stage tags;
#   - log groups have no stage tag: one is removed only under a prefix that ends in the delimiter of the stage's names and only when the function, project or
#     runtime that writes to it no longer exists.
# Every read the result depends on goes through td_read: a throttled read is repeated, and a read that fails is named and makes the run INCOMPLETE. The inventory
# (td_inventory) is the one function that reads the account; it runs before the plan and again after the deletes, and the result line is made from the second run, so
# the `checked:` sentence names exactly the reads that were made and that passed.
#
# What CloudFormation cannot do, this does first, with the AWS CLI alone:
#   - stop what is running (EventBridge rules, Step Functions executions, AgentCore sessions) so that nothing starts again or keeps billing;
#   - delete the `v<N>` endpoints that a deploy made on the stage's AgentCore runtimes (CloudFormation did not create them, and a runtime that has them cannot go);
#   - empty the versioned buckets of the stage (the Go module store with the governance bypass) and delete the images in its repositories;
#   - delete the research secret without its 30-day recovery window, which would keep its fixed name from a new deploy. It goes right after its stack: CloudFormation cannot delete
#     the resource policy of a secret that is marked for deletion, and the secret stays marked for about 30 seconds after the delete. The result waits until it is gone.
#   - put the Bedrock model invocation logging setting of the Region back to the state a deploy in the mode enable-and-restore recorded at <root>/<stage>/invocation-logging-prior, before the
#     first stack is deleted (the log group the setting names goes with the stage). Without the record, or when the setting names another destination, the setting is left as it is, and the
#     output says why. A restore that fails is a failed step (exit 8) and the record stays for a second run.
# Then the stacks are deleted one at a time, importers before exporters (the order is read from the exports and imports CloudFormation holds, with the order the
# stage was deployed in reversed as the base), and, when no stack of the stage is left, what the stacks did not own goes: the log groups AgentCore, Lambda and
# CodeBuild made and the parameters a deploy wrote.
#   - A stack in DELETE_FAILED because AgentCore still holds network interfaces in its subnets is seen as such after its first failed round (the interface is looked at), and is
#     deleted again at once with --retain-resources for the network resources that failed and the VPC they hold up (AgentCore frees the interfaces up to 8 hours after the
#     runtime is deleted); the retained resources are named as RETAINED with the commands that remove them by hand, are never counted as zero resources, and the next
#     `--teardown` finds their VPC by the project and stage tags and the id of the stack that made it, and removes it. The VPC is swept only when no stack owns it and no
#     stack of the stage is left, and only the resources in it that carry the stage tags are deleted.
#   - The runner stack and its bucket (emptied first) go before the long deletes begin, so that the authority of an old runner ends at once. When the caller's own session may
#     not delete a role that has been used, the delete is made again with the CDK CloudFormation execution role (rn_remove).
# The CDK bootstrap stack, the files and images the deploy published to its bucket and repository, the client on this machine and the receipt stay, and the output
# says so. Keys and secrets that CloudFormation scheduled for deletion are listed with their dates. Exit 0 when nothing is left, 8 when something is or when a read failed.
#
# A stage can be deployed under the current names or under the previous ones (COMPAT-CAH-STAGE: the generation table of lib.sh holds both rows). One run covers both generations:
# td_use_generation selects the names of one (stack prefix, runtime prefix, parameter root, project tag, runner and bucket names, repository prefix), every read and every delete
# below takes its names from there, and the run shows ONE plan, asks ONE question and deletes generation by generation with the same functions. A name and a project tag of two
# generations are not a stage of either: such a stack is named on a kept: line and never touched. --only current or --only legacy (TEARDOWN_ONLY) covers one generation, and the
# result then says that the other was not read. A failed or denied read of any covered generation makes the run INCOMPLETE (exit 8); the zero line needs every covered generation
# to read zero.

TD_FAILED=0
TD_FAILED_SEEN=""
TD_WARNED_SEEN=""
TD_UNREAD_RUN=""
TD_PLAN_STACKS=""
TD_PLAN_LOGS=""
TD_PLAN_PARAMS=""
TD_PLAN_VPCS=""
TD_PLAN_BUCKETS=""
TD_LEFT_N=0
TD_RETAINED=""
TD_RETAINED_N=0
TD_KEYS=""
TD_SECRETS=""
TD_FORCE_SECRETS=""
TD_SECRETS_FORCED=""
TD_RETAINED_VPCS=""
TD_RETAINED_RES=""
RN_BUCKET_GONE=0
TD_REASONS=""
TD_IMPORTERS=""
TD_UNTRUSTED=0
TD_OPERATOR=""
TD_OPERATOR_NOTE=""
TD_PROVIDER=""
TD_PROVIDER_READ=0
TD_NO_MANAGED=0
TD_TRANSIENT=0
TD_ENI_AGENTIC=0
TD_PREPARED_VPC=""
TD_HELD_LOOKED=""
TD_LOG_MODE=""
TD_LOG_PRIOR=""
TD_LOG_WHAT=""
TD_LOG_NOW=""
TD_LOG_WHY=""
TD_LOG_KEEP=0

# what td_inventory finds (all lists are one entry per line; fields are separated by a tab)
TD_STACKS=""
TD_FOREIGN=""
TD_RES=""
TD_RES_FAILED=""
TD_RUNTIMES=""
TD_RUNTIMES_ORPHAN=""
TD_RUNTIMES_OTHER=""
TD_BUCKETS_ORPHAN=""
TD_BUCKETS_OTHER=""
TD_VPCS=""
TD_VPCS_OWNED=""
TD_RUNNER_OTHER=0
TD_LOGS=""
TD_LOGS_KEPT=""
TD_PARAMS=""
TD_PARAMS_OTHER=""
TD_UNREAD=""
TD_CATS_OK=""
TD_CATS_BAD=""
TD_STACKS_LISTED=0

# which generations of names this run covers and holds something of; what a pass keeps for the generation it works on (td_slot_set); whether the plan is the limit of the pass
TD_COVER=""
TD_FOUND_GENS=""
TD_SLOTS_current=""
TD_SLOTS_legacy=""
TD_PLAN_CUT=0
TD_YES_GIVEN=0
TD_ANY_STACKS=0
TD_ANY_VPCS=0
TD_CLI_CHECKED=0

# td_use_generation <current|legacy>: the names of one generation of the stage (setup_gen_use of lib.sh: GEN, GEN_PREFIX, GEN_ROOT, GEN_SETUP, GEN_E2E, SETUP_PROJECT_TAG and
# SETUP_RUNNER_STACK). Every read and every delete of this file takes its names from there.
td_use_generation() { setup_gen_use "$1"; }

# td_slots_text, td_slot_set <slot> <text>, td_slot_get <slot>: text that a pass keeps for the generation in use (the frozen plan, the lines said at the end). One variable for each
# generation holds records "@@<slot>" and the lines of the text; bash 3.2 has no associative array, and these files use no eval and no indirect expansion.
td_slots_text() {
  case $GEN in
    current) printf '%s' "$TD_SLOTS_current" ;;
    *) printf '%s' "$TD_SLOTS_legacy" ;;
  esac
}

td_slot_set() {
  local all
  all=$(td_slots_text | awk -v k="@@$1" '$0 == k { skip = 1; next } /^@@/ { skip = 0 } !skip')
  all="$all$NL@@$1$NL$2"
  case $GEN in
    current) TD_SLOTS_current=$all ;;
    *) TD_SLOTS_legacy=$all ;;
  esac
}

td_slot_get() {
  td_slots_text | awk -v k="@@$1" '$0 == k { on = 1; next } /^@@/ { on = 0 } on'
}

# td_cover_begin: TD_COVER from --only (TEARDOWN_ONLY): both generations, or the one named.
td_cover_begin() {
  case ${TEARDOWN_ONLY:-} in
    '') TD_COVER=$SETUP_GENERATIONS ;;
    current|legacy) TD_COVER=$TEARDOWN_ONLY ;;
    *) setup_exit 2 "--only $TEARDOWN_ONLY is invalid; only current or legacy is accepted. Nothing is deleted." ;;
  esac
}

# td_covers_text: the generations this run covers, in words (the GEN_LABEL of each, joined by "and").
td_covers_text() {
  local g out="" saved=$GEN
  for g in $TD_COVER; do
    setup_gen_use "$g"
    out="$out${out:+ and }$GEN_LABEL"
  done
  setup_gen_use "$saved"
  printf '%s' "$out"
}

# td_skipped_gen: the generation --only left out, empty when both are covered.
td_skipped_gen() {
  local g
  for g in $SETUP_GENERATIONS; do
    td_has "$TD_COVER" "$g" || printf '%s' "$g"
  done
}

# td_gen_label <generation>: the words that name a generation.
td_gen_label() {
  local saved=$GEN
  setup_gen_use "$1"
  printf '%s' "$GEN_LABEL"
  setup_gen_use "$saved"
}

# td_words <aws text output>: the words of an answer, one per line, without the "None" the CLI prints for nothing.
td_words() { printf '%s\n' "$1" | tr '\t' '\n' | awk 'NF && $0 != "None"'; }

# td_has <space separated list> <word>: 0 when the word is in the list.
td_has() {
  case " $1 " in
    *" $2 "*) return 0 ;;
  esac
  return 1
}

# td_count <list>: the number of lines of a list.
td_count() { printf '%s' "$1" | awk 'NF { n++ } END { print n + 0 }'; }

# td_missing: 0 when the last aws_run failed because the thing asked for is not there. The error code decides (aws_missing in lib.sh): a refusal that names a resource whose
# name holds a word such as NoSuchBucket is a failed read.
td_missing() {
  aws_missing
}

# td_throttled: 0 when the last aws_run failed for a reason a repeat can cure (a throttle, a busy service, a dropped connection).
td_throttled() {
  case $AWS_ERR in
    *Throttl*|*"Rate exceeded"*|*RequestLimitExceeded*|*"Too Many Requests"*|*TooManyRequests*|*SlowDown*|*ServiceUnavailable*|*"Service Unavailable"*|*InternalFailure*|*InternalError*|*"Internal Server Error"*|*RequestTimeout*|*"Could not connect to the endpoint"*|*"Read timeout"*|*"Connect timeout"*|*"Connection reset"*) return 0 ;;
  esac
  return 1
}

# td_pause <attempt>: the pause before a repeat (HAMH_SETUP_RETRY_SECONDS times the attempt, 2 seconds by default; the tests set 0).
td_pause() {
  local n=$1 unit=${HAMH_SETUP_RETRY_SECONDS:-2}
  sleep "$((n * unit))"
}

# td_try <aws arguments>: aws_run, repeated up to 5 times with a growing pause while the error is a throttle or a transient failure. Every call of this file goes
# through it: a repeat is safe because the calls are reads and deletes of a named thing.
td_try() {
  local n=0
  while :; do
    aws_run "$@" && return 0
    n=$((n + 1))
    if [ "$n" -ge 5 ] || ! td_throttled; then return 1; fi
    td_pause "$n"
  done
}

# td_fail <text>: a step that did not work. It is said, written to the log and counted; the exit code is 8 when any was.
td_fail() {
  TD_FAILED=$((TD_FAILED + 1))
  say "FAILED: $1"
  log_line "FAILED: $1"
}

# td_fail_once <key> <text>: td_fail, but a failure with the same key is said and counted once (a stack that fails in every round is one failure).
td_fail_once() {
  if printf '%s' "$TD_FAILED_SEEN" | grep -Fqx -- "$1"; then
    log_line "FAILED again: $2"
    return 0
  fi
  TD_FAILED_SEEN="$TD_FAILED_SEEN$1$NL"
  td_fail "$2"
}

# td_warn_once <key> <text>: a warning that is said once per key and not counted: the account is read again at the end, and what is still there then is the failure.
td_warn_once() {
  if printf '%s' "$TD_WARNED_SEEN" | grep -Fqx -- "$1"; then
    log_line "again: $2"
    return 0
  fi
  TD_WARNED_SEEN="$TD_WARNED_SEEN$1$NL"
  warn "$2"
  log_line "$2"
}

# td_note_retained <text>: one more retained resource.
td_note_retained() {
  TD_RETAINED_N=$((TD_RETAINED_N + 1))
  if [ -n "$TD_RETAINED" ]; then TD_RETAINED="$TD_RETAINED; $1"; else TD_RETAINED=$1; fi
}

# td_reason <stack> <text>: why a stack is still there, for the STACKS LEFT list (the latest reason of a stack replaces the earlier one).
td_reason() {
  TD_REASONS=$(printf '%s' "$TD_REASONS" | awk -F'\t' -v n="$1" '$1 != n')
  TD_REASONS="$TD_REASONS${TD_REASONS:+$NL}$1$TAB$2$NL"
  TD_REASONS=$(printf '%s' "$TD_REASONS" | awk 'NF')$NL
}

# ---- reads ------------------------------------------------------------------------------------------------------------------------

# td_cat_phrase <category>: what the `checked:` sentence says about a category of reads.
td_cat_phrase() {
  case $1 in
    stacks) printf 'the CloudFormation stacks named %s-%s-* and their tags' "$GEN_PREFIX" "$STAGE" ;;
    resources) printf 'the resource list of each stack of the stage' ;;
    runner) printf 'the runner stack %s' "$SETUP_RUNNER_STACK" ;;
    vpcs) printf 'the VPCs that carry the stage tags' ;;
    runtimes) printf 'the AgentCore runtimes named %s_%s_*' "$GEN_PREFIX" "$STAGE" ;;
    buckets) printf 'the stage buckets %s-%s-harness-<account> and %s-%s-gomod-<account>' "$GEN_PREFIX" "$STAGE" "$GEN_PREFIX" "$STAGE" ;;
    logs) printf 'the log groups of the stage'"'"'s functions, projects and runtimes' ;;
    params) printf 'the SSM parameters under %s/%s/' "$GEN_ROOT" "$STAGE" ;;
  esac
}
TD_CATS="stacks resources runner vpcs runtimes buckets logs params"

# td_unread <category> <what>: a read that failed. The category cannot be called checked in this inventory, and the run is INCOMPLETE: the failure stays in TD_UNREAD_RUN, which no
# later inventory clears.
td_unread() {
  TD_UNREAD="$TD_UNREAD$1$TAB$2: ${AWS_ERR:-no answer}$NL"
  TD_UNREAD_RUN="$TD_UNREAD_RUN$1$TAB$2: ${AWS_ERR:-no answer}$NL"
  td_has "$TD_CATS_BAD" "$1" || TD_CATS_BAD="$TD_CATS_BAD $1"
  log_line "could not read $2: ${AWS_ERR:-no answer}"
}

# td_read <category> <what> <aws arguments>: a read that the result depends on. Returns 0 with the answer in AWS_OUT, 1 when the thing asked for is not there (AWS_OUT is
# empty), 2 when it could not be read (td_unread names it).
td_read() {
  local cat=$1 what=$2
  shift 2
  if td_try "$@"; then
    td_has "$TD_CATS_OK" "$cat" || TD_CATS_OK="$TD_CATS_OK $cat"
    return 0
  fi
  if td_missing; then
    td_has "$TD_CATS_OK" "$cat" || TD_CATS_OK="$TD_CATS_OK $cat"
    AWS_OUT=""
    return 1
  fi
  td_unread "$cat" "$what"
  AWS_OUT=""
  return 2
}

# td_checked_sentence: the reads that were made and passed, in the words of td_cat_phrase, once for each generation this run covers (the same words are said once). A category counts
# as checked only when no read of it failed in any covered generation.
td_checked_sentence() {
  local c g out="" phrase said="" saved=$GEN
  for c in $TD_CATS; do
    td_has "$TD_CATS_OK" "$c" || continue
    td_has "$TD_CATS_BAD" "$c" && continue
    for g in ${TD_COVER:-$GEN}; do
      setup_gen_use "$g"
      phrase=$(td_cat_phrase "$c")
      if printf '%s\n' "$said" | grep -Fqx -- "$phrase"; then continue; fi
      said="$said$phrase$NL"
      if [ -n "$out" ]; then out="$out; "; fi
      out="$out$phrase"
    done
  done
  setup_gen_use "$saved"
  printf '%s' "${out:-nothing}"
}

# td_tag_of <tag key> <tag rows>: the value of a tag from the rows `Key<TAB>Value` of an answer.
td_tag_of() { printf '%s\n' "$2" | awk -F'\t' -v k="$1" '$1 == k { print $2; exit }'; }

# td_ours <tag rows>: 0 when the rows carry project=<project tag> and stage=<stage>.
td_ours() {
  [ "$(td_tag_of project "$1")" = "$SETUP_PROJECT_TAG" ] && [ "$(td_tag_of stage "$1")" = "$STAGE" ]
}

# td_res_rows [type]: the resources of the stage's stacks, `stack<TAB>type<TAB>id`, of one type when it is given.
td_res_rows() {
  if [ -n "${1:-}" ]; then printf '%s' "$TD_RES" | awk -F'\t' -v t="$1" 'NF == 3 && $2 == t'; else printf '%s' "$TD_RES" | awk 'NF'; fi
}

# td_res_ids <type> [stack]: the physical ids of that type, one per line.
td_res_ids() {
  printf '%s' "$TD_RES" | awk -F'\t' -v t="$1" -v s="${2:-}" 'NF == 3 && $2 == t && (s == "" || $1 == s) { print $3 }'
}

# ---- the inventory -----------------------------------------------------------------------------------------------------------------

# td_prefixed <stack name>: 0 when the name starts with <prefix>-<stage>- of the generation in use.
td_prefixed() {
  case $1 in
    "$GEN_PREFIX-$STAGE-"*) return 0 ;;
  esac
  return 1
}

# td_date_only <timestamp>: the date of an ISO 8601 timestamp (2026-11-30T03:14:47.865000+00:00 is 2026-11-30); any other text is returned as it is.
td_date_only() {
  case $1 in
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T*) printf '%s' "${1%%T*}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# td_read_stacks: TD_STACKS (name, status) are the stacks of the stage: the name starts with <prefix>-<stage>- and the tags are project and stage (of the generation in use). TD_FOREIGN (name, status, why)
# are stacks that have the name and not the tags (or whose tags could not be read); they are named in the plan and never touched.
td_read_stacks() {
  local line name status tags rc
  TD_STACKS=""
  TD_FOREIGN=""
  TD_STACKS_LISTED=0
  # $_statuses is the list of the statuses that mean a stack exists, split on purpose
  _statuses=$SETUP_LIVE_STATUSES
  # shellcheck disable=SC2086
  td_read stacks "the CloudFormation stacks" cloudformation list-stacks --stack-status-filter $_statuses \
    --query "StackSummaries[?starts_with(StackName,'$GEN_PREFIX-$STAGE-')].[StackName,StackStatus]" --output text || return 1
  TD_STACKS_LISTED=1
  local listing=$AWS_OUT
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    status=${line#*"$TAB"}
    td_prefixed "$name" || continue
    td_read stacks "the tags of the stack $name" cloudformation describe-stacks --stack-name "$name" --query 'Stacks[0].Tags[].[Key,Value]' --output text
    rc=$?
    tags=$AWS_OUT
    case $rc in
      1) continue ;;
      2) TD_FOREIGN="$TD_FOREIGN$name$TAB$status${TAB}its tags could not be read$NL"; continue ;;
    esac
    if td_ours "$tags"; then
      if [ "$TD_PLAN_CUT" = 1 ] && ! td_has "$TD_PLAN_STACKS" "$name"; then
        td_warn_once "late-stack:$name" "the stack $name appeared after the plan was confirmed and is not touched; run ./setup.sh --teardown again to see it in a plan"
        continue
      fi
      TD_STACKS="$TD_STACKS$name$TAB$status$NL"
    else
      TD_FOREIGN="$TD_FOREIGN$name$TAB$status$TAB$(td_foreign_reason "$tags")$NL"
    fi
  done <<<"$listing"
  return 0
}

# td_foreign_reason <tag rows>: why a stack with the name of a stage stack is not the stage's. A project tag that belongs to a generation of names is named: a name of one generation
# with the project tag of the other is a mixed pair, and it is not a stage of either.
td_foreign_reason() {
  local project g other=""
  project=$(td_tag_of project "$1")
  for g in $SETUP_GENERATIONS; do
    [ "$g" != "$GEN" ] || continue
    if [ "$project" = "$(td_gen_project "$g")" ]; then other=$g; fi
  done
  if [ -n "$other" ]; then
    printf 'a name from %s with a project tag from %s does not form a stage of either generation' "$GEN_LABEL" "$(td_gen_label "$other")"
  else
    printf 'it has the name of a stage stack but not the tags project=%s and stage=%s' "$SETUP_PROJECT_TAG" "$STAGE"
  fi
}

# td_gen_project <generation>: the value of the project tag of a generation.
td_gen_project() {
  local saved=$GEN
  setup_gen_use "$1"
  printf '%s' "$GEN_PROJECT"
  setup_gen_use "$saved"
}

# td_read_resources: TD_RES, the resource list of every stack of the stage; a stack whose list could not be read is in TD_RES_FAILED.
td_read_resources() {
  local line name row rc id
  TD_RES=""
  TD_RES_FAILED=""
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    td_read resources "the resources of the stack $name" cloudformation list-stack-resources --stack-name "$name" \
      --query "StackResourceSummaries[?ResourceStatus!='DELETE_COMPLETE'].[ResourceType,PhysicalResourceId]" --output text
    rc=$?
    case $rc in
      1) continue ;;
      2) TD_RES_FAILED="$TD_RES_FAILED$name$NL"; continue ;;
    esac
    # a resource that a stack lists as deleted is history: its name may belong to somebody else now, so only the rows of resources that exist are kept
    while IFS= read -r row; do
      [ -n "$row" ] || continue
      id=${row#*"$TAB"}
      [ "$id" != "$row" ] && [ -n "$id" ] && [ "$id" != None ] || continue
      TD_RES="$TD_RES$name$TAB$row$NL"
    done <<<"$AWS_OUT"
  done <<<"$TD_STACKS"
  return 0
}

# td_read_runner: the runner stack (RN_EXISTS and its outputs, as rn_describe sets them), when it carries the project tag. A stack of that name without it is left alone.
td_read_runner() {
  local line key val tags rc
  RN_EXISTS=0
  RN_BUCKET=""
  RN_BUCKET_GONE=0
  RN_PROJECT=""
  RN_LOGGROUP=""
  RN_ROLE=""
  RN_EXPIRES=""
  TD_RUNNER_OTHER=0
  td_read runner "the runner stack $SETUP_RUNNER_STACK" cloudformation describe-stacks --stack-name "$SETUP_RUNNER_STACK" --query 'Stacks[0].Outputs[].[OutputKey,OutputValue]' --output text
  rc=$?
  [ "$rc" = 0 ] || return 0
  local outputs=$AWS_OUT
  td_read runner "the tags of the runner stack" cloudformation describe-stacks --stack-name "$SETUP_RUNNER_STACK" --query 'Stacks[0].Tags[].[Key,Value]' --output text || return 0
  tags=$AWS_OUT
  if [ "$(td_tag_of project "$tags")" != "$SETUP_PROJECT_TAG" ]; then
    TD_RUNNER_OTHER=1
    return 0
  fi
  # the bucket the stack names is emptied, so it must be the bucket the contract gives the runner of this account and Region
  if ! printf '%s
' "$outputs" | awk -F'	' -v b="$GEN_SETUP-$ACCOUNT-$REGION" '$1 == "SetupBucket" && $2 == b { found = 1 } END { exit found ? 0 : 1 }'; then
    TD_RUNNER_OTHER=1
    return 0
  fi
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
  done <<<"$outputs"
  # a stack that failed on its role may have lost its bucket already: a plan does not empty a bucket that S3 says is not there
  RN_BUCKET_GONE=0
  if [ -n "$RN_BUCKET" ] && ! td_try s3api head-bucket --bucket "$RN_BUCKET" --expected-bucket-owner "$ACCOUNT" && td_missing; then RN_BUCKET_GONE=1; fi
  if td_read runner "the parameters of the runner stack" cloudformation describe-stacks --stack-name "$SETUP_RUNNER_STACK" --query "Stacks[0].Parameters[?ParameterKey=='AuthorityExpires'].ParameterValue" --output text; then
    case $AWS_OUT in
      20??-??-??T??:??:??Z) RN_EXPIRES=$AWS_OUT ;;
    esac
  fi
}

# td_read_vpcs: TD_VPCS are the VPCs that carry the project and stage tags and that no existing stack owns (an earlier teardown retained them because AgentCore held network
# interfaces in them); TD_VPCS_OWNED (vpc, stack) are those that a stack still owns, which are never swept. A VPC belongs to the stack whose id CloudFormation tagged it with
# (aws:cloudformation:stack-id), not to the stack that has its name today: when the stage is deployed again, the new network stack has the name of the one that made a retained
# VPC. A tag with a name and no id decides by the name.
td_read_vpcs() {
  local v owner owner_id current rows
  TD_VPCS=""
  TD_VPCS_OWNED=""
  td_read vpcs "the VPCs that carry the stage tags" ec2 describe-vpcs --filters "Name=tag:project,Values=$SETUP_PROJECT_TAG" "Name=tag:stage,Values=$STAGE" \
    --query 'Vpcs[].VpcId' --output text || return 0
  local listed
  listed=$(td_words "$AWS_OUT")
  for v in $listed; do
    td_read vpcs "the owner of the VPC $v" ec2 describe-vpcs --vpc-ids "$v" --query 'Vpcs[0].Tags[].[Key,Value]' --output text
    case $? in
      1) continue ;;
      2) TD_VPCS_OWNED="$TD_VPCS_OWNED$v${TAB}unknown$NL"; continue ;;
    esac
    rows=$AWS_OUT
    owner=$(td_tag_of aws:cloudformation:stack-name "$rows")
    owner_id=$(td_tag_of aws:cloudformation:stack-id "$rows")
    if [ -z "$owner" ]; then
      # nothing shows which stack made it, so nothing shows that no stack owns it
      TD_VPCS_OWNED="$TD_VPCS_OWNED$v${TAB}unknown$NL"
      continue
    fi
    td_read vpcs "the stack $owner that the tags of the VPC $v name" cloudformation describe-stacks --stack-name "$owner" --query 'Stacks[0].StackId' --output text
    case $? in
      0)
        current=$(td_words "$AWS_OUT" | head -n 1)
        if [ -n "$owner_id" ] && [ -n "$current" ] && [ "$current" != "$owner_id" ]; then
          # a stack of that name exists, and it is not the stack that made this VPC: that one is gone
          TD_VPCS="$TD_VPCS$v$NL"
        else
          TD_VPCS_OWNED="$TD_VPCS_OWNED$v$TAB$owner$NL"
        fi ;;
      1) TD_VPCS="$TD_VPCS$v$NL" ;;
      2) TD_VPCS_OWNED="$TD_VPCS_OWNED$v${TAB}unknown$NL" ;;
    esac
  done
}

# td_runtime_owned <id> <arn>: 0 when a stack of the stage lists this runtime (CloudFormation lists an AgentCore runtime by its ARN; its id is accepted too).
td_runtime_owned() {
  td_res_ids AWS::BedrockAgentCore::Runtime | awk -v i="$1" -v a="$2" '$0 == i || $0 == a { found = 1 } END { exit found ? 0 : 1 }'
}

# td_read_runtimes: the AgentCore runtimes whose name starts with <prefix>_<stage>_. TD_RUNTIMES (id, arn, name) are those a stack of the stage lists; TD_RUNTIMES_ORPHAN are those
# that no stack lists but that carry the stage tags; TD_RUNTIMES_OTHER are the rest, which are never touched.
td_read_runtimes() {
  local line id arn name tags
  TD_RUNTIMES=""
  TD_RUNTIMES_ORPHAN=""
  TD_RUNTIMES_OTHER=""
  td_read runtimes "the AgentCore runtimes" bedrock-agentcore-control list-agent-runtimes \
    --query "agentRuntimes[?starts_with(agentRuntimeName,'${GEN_PREFIX}_${STAGE}_')].[agentRuntimeId,agentRuntimeArn,agentRuntimeName]" --output text || return 0
  local listing=$AWS_OUT
  while IFS= read -r line; do
    [ -n "$line" ] && [ "$line" != None ] || continue
    id=$(printf '%s' "$line" | cut -f1)
    arn=$(printf '%s' "$line" | cut -f2)
    name=$(printf '%s' "$line" | cut -f3)
    if td_runtime_owned "$id" "$arn"; then
      TD_RUNTIMES="$TD_RUNTIMES$id$TAB$arn$TAB$name$NL"
      continue
    fi
    if td_read runtimes "the tags of the runtime $id" bedrock-agentcore-control list-tags-for-resource --resource-arn "$arn" --query '[tags.project,tags.stage]' --output text; then
      tags=$AWS_OUT
      if [ "$(printf '%s' "$tags" | cut -f1)" = "$SETUP_PROJECT_TAG" ] && [ "$(printf '%s' "$tags" | cut -f2)" = "$STAGE" ]; then
        TD_RUNTIMES_ORPHAN="$TD_RUNTIMES_ORPHAN$id$TAB$arn$TAB$name$NL"
        continue
      fi
    fi
    TD_RUNTIMES_OTHER="$TD_RUNTIMES_OTHER$id$TAB$arn$TAB$name$NL"
  done <<<"$listing"
}

# td_read_buckets: the two buckets of the stage by their exact names, when no stack of the stage lists them. TD_BUCKETS_ORPHAN carry the stage tags (they are emptied and
# deleted); TD_BUCKETS_OTHER do not (they are never touched).
td_read_buckets() {
  local b tags
  TD_BUCKETS_ORPHAN=""
  TD_BUCKETS_OTHER=""
  for b in "$GEN_PREFIX-$STAGE-harness-$ACCOUNT" "$GEN_PREFIX-$STAGE-gomod-$ACCOUNT"; do
    if td_res_ids AWS::S3::Bucket | grep -Fqx -- "$b"; then continue; fi
    td_read buckets "the bucket $b" s3api head-bucket --bucket "$b" --expected-bucket-owner "$ACCOUNT" || continue
    # a bucket without a tag set answers "does not exist", which td_read takes as "not there": it carries no tags, so it is not the stage's
    if td_read buckets "the tags of the bucket $b" s3api get-bucket-tagging --bucket "$b" --expected-bucket-owner "$ACCOUNT" --query 'TagSet[].[Key,Value]' --output text; then
      tags=$AWS_OUT
      if td_ours "$tags"; then TD_BUCKETS_ORPHAN="$TD_BUCKETS_ORPHAN$b$NL"; continue; fi
    fi
    TD_BUCKETS_OTHER="$TD_BUCKETS_OTHER$b$NL"
  done
}

# td_runtime_id_listed <runtime id>: prints the id when a stack of the stage lists a runtime with it.
td_runtime_id_listed() {
  td_res_ids AWS::BedrockAgentCore::Runtime | awk -v i="$1" '{ n = split($0, p, "/"); if ($0 == i || p[n] == i) { print i; exit } }'
}

# td_log_by_name <log group> <function writers> <project writers> <runtime ids>: what the NAME of a log group says. gone when the function, project or runtime that the name
# points at no longer exists, stack when a stack of the stage lists it (it goes with that stack), live when it exists and no stack of the stage lists it. A list that could not be
# read is passed as "?", and the group is then live: it is not touched.
td_log_by_name() {
  local g=$1 fns=$2 projects=$3 runtimes=$4 owner
  case $g in
    /aws/lambda/*)
      owner=${g#/aws/lambda/}
      if td_res_ids AWS::Lambda::Function | grep -Fqx -- "$owner"; then printf 'stack'; return 0; fi
      if [ "$fns" = "?" ] || printf '%s\n' "$fns" | grep -Fqx -- "$owner"; then printf 'live'; else printf 'gone'; fi ;;
    /aws/codebuild/*)
      owner=${g#/aws/codebuild/}
      if td_res_ids AWS::CodeBuild::Project | grep -Fqx -- "$owner"; then printf 'stack'; return 0; fi
      if [ "$projects" = "?" ] || printf '%s\n' "$projects" | grep -Fqx -- "$owner"; then printf 'live'; else printf 'gone'; fi ;;
    /aws/bedrock-agentcore/runtimes/*)
      # the group is <runtime id>-<endpoint name>; an endpoint name has no hyphen, a runtime id has one
      owner=${g#/aws/bedrock-agentcore/runtimes/}
      owner=${owner%-*}
      if [ -n "$(td_runtime_id_listed "$owner")" ]; then printf 'stack'; return 0; fi
      if [ "$runtimes" = "?" ] || printf '%s\n' "$runtimes" | grep -Fqx -- "$owner"; then printf 'live'; else printf 'gone'; fi ;;
    *) printf 'live' ;;
  esac
}

# td_log_state <log group> <function writers> <project writers> <runtime ids>: td_log_by_name, and a group that the name calls gone is live while any function or any build project
# is set to write to it, whichever service its name belongs to (a function writes to the group of its own name or to the group in LoggingConfig, a project the same through its logs
# settings), or while a list that could say so could not be read. The writer lists hold names and destinations.
td_log_state() {
  local state
  state=$(td_log_by_name "$@")
  if [ "$state" = gone ]; then
    if [ "$2" = "?" ] || [ "$3" = "?" ] || printf '%s\n%s\n' "$2" "$3" | grep -Fqx -- "$1"; then state=live; fi
  fi
  printf '%s' "$state"
}

# td_project_batch <up to 100 project names>: the log groups that these CodeBuild projects are set to write to (None for a project that writes to the group of its own name).
td_project_batch() {
  local _names=$1
  # $_names is a list of project names and is split on purpose
  # shellcheck disable=SC2086
  td_read logs "the log settings of the CodeBuild projects" codebuild batch-get-projects --names $_names --query 'projects[].logsConfig.cloudWatchLogs.groupName' --output text
}

# td_project_writers <log groups> <function writers> <project names> <runtime ids>: sets TD_PROJECT_WRITERS to the project names and, when a group is not accounted for by the name of
# its own writer (a project can be set to write to a group of any name), to the groups that the projects are set to write to. "?" when those settings could not be read.
td_project_writers() {
  local all=$1 fns=$2 names=$3 runtimes=$4 g n _batch="" count=0 need=0 groups=""
  TD_PROJECT_WRITERS=$names
  while IFS= read -r g; do
    [ -n "$g" ] || continue
    [ "$(td_log_by_name "$g" "$fns" "$names" "$runtimes")" = gone ] && need=1
  done <<<"$all"
  [ "$need" = 1 ] || return 0
  for n in $names; do
    _batch="$_batch $n"
    count=$((count + 1))
    if [ "$count" -ge 100 ]; then
      td_project_batch "$_batch"
      case $? in
        0) groups="$groups$NL$AWS_OUT" ;;
        *) TD_PROJECT_WRITERS="?"; return 0 ;;
      esac
      _batch=""
      count=0
    fi
  done
  if [ -n "$_batch" ]; then
    td_project_batch "$_batch"
    case $? in
      0) groups="$groups$NL$AWS_OUT" ;;
      *) TD_PROJECT_WRITERS="?"; return 0 ;;
    esac
  fi
  TD_PROJECT_WRITERS="$names$NL$(td_words "$groups")"
}

# td_read_logs: TD_LOGS are the log groups to delete (the thing that wrote to them is gone, or goes with a stack of the stage), TD_LOGS_KEPT (group, reason) those that are
# not touched. The prefixes end in the delimiter of the stage's names (<prefix>_<stage>_ for runtimes, <prefix>-<stage>- for functions and projects). When a group is found, the functions
# (with the destination of each version) and the build projects of the account are read, because they decide whether a writer is left.
td_read_logs() {
  local prefix g state fns="" projects="" runtimes="" all=""
  TD_LOGS=""
  TD_LOGS_KEPT=""
  for prefix in "/aws/bedrock-agentcore/runtimes/${GEN_PREFIX}_${STAGE}_" "/aws/lambda/$GEN_PREFIX-${STAGE}-" "/aws/codebuild/$GEN_PREFIX-${STAGE}-"; do
    td_read logs "the log groups under $prefix" logs describe-log-groups --log-group-name-prefix "$prefix" --query 'logGroups[].logGroupName' --output text || continue
    all="$all$(td_words "$AWS_OUT")$NL"
  done
  # every AgentCore runtime of the account whose name starts with the stage's (the runtimes were read before): the groups of a runtime that exists are not touched
  case " $TD_CATS_BAD " in
    *" runtimes "*) runtimes="?" ;;
    *) runtimes=$(printf '%s\n%s\n%s' "$TD_RUNTIMES" "$TD_RUNTIMES_ORPHAN" "$TD_RUNTIMES_OTHER" | awk -F'\t' 'NF { print $1 }') ;;
  esac
  if [ -n "$(td_words "$all")" ]; then
    if td_read logs "the Lambda functions" lambda list-functions --function-version ALL --query 'Functions[].[FunctionName,LoggingConfig.LogGroup]' --output text; then
      fns=$(td_words "$AWS_OUT")
    else
      fns="?"
    fi
    if td_read logs "the CodeBuild projects" codebuild list-projects --query 'projects' --output text; then
      td_project_writers "$all" "$fns" "$(td_words "$AWS_OUT")" "$runtimes"
      projects=$TD_PROJECT_WRITERS
    else
      projects="?"
    fi
  fi
  while IFS= read -r g; do
    [ -n "$g" ] || continue
    state=$(td_log_state "$g" "$fns" "$projects" "$runtimes")
    case $state in
      gone|stack) TD_LOGS="$TD_LOGS$g$NL" ;;
      *) TD_LOGS_KEPT="$TD_LOGS_KEPT$g${TAB}the function, project or runtime that writes to it still exists, or could not be read$NL" ;;
    esac
  done <<<"$all"
}

# td_read_params: TD_PARAMS are the parameters under <root>/<stage>/ that carry the stage tags; TD_PARAMS_OTHER are those that do not (never touched).
td_param_root() { printf '%s/%s' "$GEN_ROOT" "$STAGE"; }

td_read_params() {
  local all owned p root
  root=$(td_param_root)
  TD_PARAMS=""
  TD_PARAMS_OTHER=""
  td_read params "the SSM parameters under $root/" ssm describe-parameters --parameter-filters "Key=Path,Option=Recursive,Values=$root" \
    --query 'Parameters[].Name' --output text || return 0
  all=$(td_words "$AWS_OUT")
  [ -n "$all" ] || return 0
  td_read params "the tags of the SSM parameters under $root/" ssm describe-parameters --parameter-filters "Key=Path,Option=Recursive,Values=$root" \
    "Key=tag:project,Values=$SETUP_PROJECT_TAG" "Key=tag:stage,Values=$STAGE" --query 'Parameters[].Name' --output text || return 0
  owned=$(td_words "$AWS_OUT")
  for p in $all; do
    if printf '%s\n' "$owned" | grep -Fqx -- "$p"; then TD_PARAMS="$TD_PARAMS$p$NL"; else TD_PARAMS_OTHER="$TD_PARAMS_OTHER$p$NL"; fi
  done
}

# td_inventory [keep]: everything of the stage (of the generation in use) that is in the account, from reads only. It forgets the reads of the run before, so that its `checked:`
# sentence is what this inventory read; `keep` leaves them, so that one sentence can be made from the reads of several generations.
td_inventory() {
  if [ "${1:-}" != keep ]; then
    TD_UNREAD=""
    TD_CATS_OK=""
    TD_CATS_BAD=""
  fi
  td_read_stacks
  td_read_resources
  td_read_runner
  td_read_vpcs
  td_read_runtimes
  td_read_buckets
  td_read_logs
  td_read_params
}

# td_no_stacks: 0 only when the inventory read the stacks and their resources and found no stack of the stage. Network resources, log groups and parameters are deleted only
# then: a list that is empty because a read failed is not "no stack".
td_no_stacks() {
  [ "$TD_STACKS_LISTED" = 1 ] || return 1
  td_has "$TD_CATS_BAD" stacks && return 1
  td_has "$TD_CATS_BAD" resources && return 1
  [ -z "$TD_STACKS" ]
}

# td_found: 0 when the inventory holds something of the stage.
td_found() {
  [ -n "$TD_STACKS$TD_VPCS$TD_RUNTIMES$TD_RUNTIMES_ORPHAN$TD_BUCKETS_ORPHAN$TD_LOGS$TD_PARAMS" ] || [ "$RN_EXISTS" = 1 ]
}

# td_list_unread: says each read that failed.
td_list_unread() {
  local line
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    say "could not read ${line#*"$TAB"}"
  done <<<"$(printf '%s' "$TD_UNREAD_RUN" | awk 'NF && !seen[$0]++')"
}

# td_vpcs_left: the rows (VPC, owner) of the VPCs that carry the stage tags and that the teardown does not touch because nothing shows that their stack is gone: the owner is not
# known, or a stack outside the stage owns the VPC. A VPC that a stack of the stage owns is that stack's, and the stack is listed.
td_vpcs_left() {
  local line stage_stacks
  stage_stacks=$(printf '%s' "$TD_STACKS" | cut -f1 | tr '\n' ' ')
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    td_has "$stage_stacks" "${line#*"$TAB"}" && continue
    printf '%s\n' "$line"
  done <<<"$TD_VPCS_OWNED"
}

# td_say_vpcs_left <rows of td_vpcs_left>: one LEFT line for each.
td_say_vpcs_left() {
  local line
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    if [ "${line#*"$TAB"}" = unknown ]; then
      say "LEFT: VPC ${line%%"$TAB"*} carries the stage tags and was not touched. No CloudFormation tag shows which stack made it, or that stack cannot be read, so nothing shows that no stack owns it. If it belongs to the stage, you can delete it yourself. If it does not, you can remove the stage tags from it."
    else
      say "LEFT: VPC ${line%%"$TAB"*} carries the stage tags and was not touched: the stack ${line#*"$TAB"} owns it, and that stack is not a stack of the stage."
    fi
  done <<<"$1"
}

td_check_cli() {
  # the teardown deletes the endpoints a deploy made on the AgentCore runtimes and stops AgentCore sessions, and an AWS CLI that predates the AgentCore commands cannot do
  # either (setup itself only needs version 2.9.0). The CLI prints the skeleton of an operation it knows without calling anything.
  local _op
  for _op in "bedrock-agentcore-control list-agent-runtimes" "bedrock-agentcore-control list-agent-runtime-endpoints" "bedrock-agentcore-control delete-agent-runtime-endpoint" \
      "bedrock-agentcore stop-runtime-session" "bedrock-agentcore delete-capacity-provider-session"; do
    # $_op is a service and an operation, split on purpose
    # shellcheck disable=SC2086
    if ! aws_run $_op --generate-cli-skeleton; then
      setup_exit 1 "this AWS CLI ($AWS_CLI_VER) has no AgentCore commands ('${_op#* }' is missing), and teardown needs them to stop the stage's sessions and delete the endpoints a deploy made" \
        "Upgrade the AWS CLI: $AWS_CLI_INSTALL_URL" "Nothing was deleted."
    fi
  done
  return 0
}

# td_busy: a setup that is still running (the lease of the stage names a build that is in progress) or a stack that is being changed stops the teardown before it
# deletes anything: it would race the build or the update. A stack that a teardown left in DELETE_IN_PROGRESS is fine; the next delete waits for it.
td_busy() {
  local line name status build
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    name=${line%%"$TAB"*}
    status=${line#*"$TAB"}
    case $status in
      DELETE_IN_PROGRESS) ;;
      *_IN_PROGRESS) setup_exit 7 "the stack $name is $status: a stack change is in progress" \
        "You wait for the change to finish. You run ./setup.sh --teardown again. Nothing is deleted." ;;
    esac
  done <<<"$TD_STACKS"
  [ "$RN_EXISTS" = 1 ] && [ -n "$RN_BUCKET" ] || return 0
  if td_try s3 cp "s3://$RN_BUCKET/lease/$STAGE.json" -; then
    build=$(printf '%s' "$AWS_OUT" | json_flat | awk -F'\t' '$1 == "buildId" { print $2; exit }')
    if [ -n "$build" ]; then
      status=$(rn_build_status "$build") || setup_exit 6 "could not look at the build $build that the lease of stage $STAGE records" \
        "Nothing was deleted. Run ./setup.sh --teardown again in a minute."
      [ "$status" != IN_PROGRESS ] || setup_exit 7 "a setup is running: build $build is in progress" \
        "You wait for the build or follow it with ./setup.sh --status. You run ./setup.sh --teardown again. Nothing is deleted."
    fi
  elif ! td_missing; then
    setup_exit 6 "could not read the lease of stage $STAGE in the runner bucket: ${AWS_ERR:-no answer}" \
      "The lease says whether a setup is running, and teardown does not delete under a running setup. Nothing was deleted. Run ./setup.sh --teardown again in a minute."
  fi
  return 0
}

# td_check_go_store: the Go module store lets only the stage operator (and the runner's role) delete its objects. Whether this caller is named in the policy decides
# what the plan says; the delete is tried either way, because the policy, not this guess, decides. Sets TD_UNTRUSTED and TD_OPERATOR (the identity the policy names).
td_check_go_store() {
  local bucket=$GEN_PREFIX-$STAGE-gomod-$ACCOUNT kind="" name="" rest policy
  TD_UNTRUSTED=0
  TD_OPERATOR=""
  TD_OPERATOR_NOTE=""
  td_res_ids AWS::S3::Bucket | grep -Fqx -- "$bucket" || return 0
  td_try s3api head-bucket --bucket "$bucket" --expected-bucket-owner "$ACCOUNT" || return 0
  td_try s3api get-bucket-policy --bucket "$bucket" --expected-bucket-owner "$ACCOUNT" --query Policy --output text || return 0
  policy=$AWS_OUT
  TD_OPERATOR=$(printf '%s\n' "$policy" | grep -oE "arn:aws:iam::$ACCOUNT:(user|role)/[A-Za-z0-9+=,.@_/-]+" | grep -v "/$GEN_SETUP-runner-role\$" | grep -v '/\*/' | sed "s|^arn:aws:iam::$ACCOUNT:||" | sort -u | head -n 1)
  if [ -n "$TD_OPERATOR" ]; then TD_OPERATOR_NOTE="($TD_OPERATOR)"; fi
  case $CALLER_ARN in
    arn:aws:sts::*:assumed-role/*/*) rest=${CALLER_ARN#*:assumed-role/}; name=${rest%%/*}; kind=role ;;
    arn:aws:iam::*:user/*) name=${CALLER_ARN##*/}; kind=user ;;
  esac
  if [ -z "$kind" ] || ! printf '%s\n' "$policy" | grep -Fq "\"arn:aws:iam::$ACCOUNT:$kind/$name\""; then
    TD_UNTRUSTED=1
  fi
}

# td_again_text: what a second run does, said wherever a run leaves something for it.
td_again_text() {
  printf 'After AWS releases the network interface, run ./setup.sh --teardown again to remove the resources this run retains; the next plan lists everything it would delete, including any stage deployed after this run.'
  if [ "$TD_YES_GIVEN" = 1 ]; then
    printf ' With --yes --acknowledge-teardown, teardown deletes everything in its plan without asking, including any stage deployed after this run.'
  else
    printf ' Without --yes, teardown asks before it deletes; answer n to keep that stage.'
  fi
}

# ---- the Bedrock model invocation logging setting ---------------------------------------------------------------------------------

# A deploy in the mode enable-and-restore (scripts/lib/deploy.py `invocation_logging`) turns the account's model invocation logging on for the stage's log group when the
# setting is unset, and writes the state it found to a parameter of the stage first. The log group goes with the stage's stacks, so the setting is put back before the first
# stack is deleted. Without the record nothing was changed and nothing is written; a setting that names another destination is somebody else's and is left as it is.
#   td_read_logging      reads the record and the setting and sets TD_LOG_MODE (reads only, so the plan and the dry run can name the restore):
#                          restore-delete   the record says unset and the setting names the stage's log group: the configuration is deleted
#                          restore-put      the record holds a configuration and the setting names the stage's log group: that configuration is put back
#                          already          the setting is the recorded state already
#                          foreign          the setting names another destination (or none while the record holds a configuration)
#                          none, untagged   no record, or a parameter without the stage tags (not read)
#                          unreadable, bad, setting-unreadable   a read failed, or the record is not a state: a failed step, and the record stays
#   td_restore_logging   makes the restore of the mode and says what it did.
td_logging_param() { printf '%s/invocation-logging-prior' "$(td_param_root)"; }

# td_logging_group: the log group a deploy points the setting at.
td_logging_group() { printf '%s/%s/bedrock-invocations' "$GEN_ROOT" "$STAGE"; }

# td_logging_dest <path prefix> <json_flat rows>: the destination a configuration names (its CloudWatch log group, else its S3 bucket); empty when it names none.
td_logging_dest() {
  local prefix=$1 rows=$2 d
  d=$(printf '%s\n' "$rows" | awk -F'\t' -v p="${prefix}cloudWatchConfig.logGroupName" '$1 == p { print $2; exit }')
  [ -n "$d" ] || d=$(printf '%s\n' "$rows" | awk -F'\t' -v p="${prefix}s3Config.bucketName" '$1 == p { print $2; exit }')
  printf '%s' "$d"
}

td_read_logging() {
  local param rec rows inner now_rows group
  param=$(td_logging_param)
  group=$(td_logging_group)
  TD_LOG_MODE=none
  TD_LOG_PRIOR=""
  TD_LOG_WHAT=""
  TD_LOG_NOW=""
  TD_LOG_WHY=""
  TD_LOG_KEEP=0
  if printf '%s\n' "$TD_PARAMS_OTHER" | grep -Fqx -- "$param"; then
    TD_LOG_MODE=untagged
    return 0
  fi
  printf '%s\n' "$TD_PARAMS" | grep -Fqx -- "$param" || return 0
  td_read params "the state recorded at $param" ssm get-parameter --name "$param" --query Parameter.Value --output text
  case $? in
    0) rec=$AWS_OUT ;;
    1) return 0 ;;
    *) TD_LOG_MODE=unreadable; TD_LOG_WHY=${AWS_ERR:-no answer}; TD_LOG_KEEP=1; return 0 ;;
  esac
  rows=$(printf '%s' "$rec" | json_flat 2>/dev/null) || rows=""
  if printf '%s\n' "$rows" | awk -F'\t' '$1 == "loggingConfig" && $2 == "null" { f = 1 } END { exit !f }'; then
    TD_LOG_WHAT=unset
  elif printf '%s\n' "$rows" | awk -F'\t' 'index($1, "loggingConfig.") == 1 { f = 1 } END { exit !f }'; then
    # the record is {"loggingConfig": <configuration>}: the configuration is what put-model-invocation-logging-configuration takes
    inner=${rec#*\"loggingConfig\"}
    inner=${inner#*:}
    inner=${inner%\}*}
    TD_LOG_PRIOR=$inner
    TD_LOG_WHAT=$(td_logging_dest "" "$(printf '%s' "$inner" | json_flat 2>/dev/null)")
  fi
  if [ -z "$TD_LOG_WHAT" ]; then
    TD_LOG_MODE=bad
    TD_LOG_WHY="it is neither {\"loggingConfig\": null} nor a configuration with a CloudWatch log group or an S3 bucket"
    TD_LOG_KEEP=1
    return 0
  fi
  if ! td_try bedrock get-model-invocation-logging-configuration --output json; then
    TD_LOG_MODE=setting-unreadable
    TD_LOG_WHY=${AWS_ERR:-no answer}
    TD_LOG_KEEP=1
    return 0
  fi
  now_rows=$(printf '%s' "$AWS_OUT" | json_flat 2>/dev/null) || now_rows=""
  TD_LOG_NOW=$(td_logging_dest "loggingConfig." "$now_rows")
  if [ "$TD_LOG_WHAT" = unset ] && [ -z "$TD_LOG_NOW" ]; then
    TD_LOG_MODE=already
  elif [ "$TD_LOG_WHAT" != unset ] && [ "$TD_LOG_NOW" = "$TD_LOG_WHAT" ]; then
    TD_LOG_MODE=already
  elif [ "$TD_LOG_NOW" = "$group" ]; then
    if [ "$TD_LOG_WHAT" = unset ]; then TD_LOG_MODE=restore-delete; else TD_LOG_MODE=restore-put; fi
  else
    TD_LOG_MODE=foreign
  fi
}

# td_logging_stays: why the setting is left as it is, for the plan and for the step (modes that write nothing and fail nothing).
td_logging_stays() {
  local param group
  param=$(td_logging_param)
  group=$(td_logging_group)
  case $TD_LOG_MODE in
    none) printf 'there is no record at %s, so there is nothing to put back' "$param" ;;
    untagged) printf '%s does not carry the stage tags, so teardown does not read it' "$param" ;;
    already)
      if [ "$TD_LOG_WHAT" = unset ]; then printf 'it is already unset, as recorded at %s' "$param"; else printf 'it already names %s, as recorded at %s' "$TD_LOG_WHAT" "$param"; fi ;;
    foreign) printf 'it names %s, not the stage'"'"'s log group %s, so the state recorded at %s is not put back' "${TD_LOG_NOW:-no destination}" "$group" "$param" ;;
  esac
}

# td_say_logging_plan: the line of the plan.
td_say_logging_plan() {
  local param
  param=$(td_logging_param)
  case $TD_LOG_MODE in
    restore-delete) say "  Bedrock model invocation logging in Region $REGION: restored to unset, as recorded at $param before the deploy turned it on. This is done before the first stack is deleted, because the stage's log group goes with its stack." ;;
    restore-put) say "  Bedrock model invocation logging in Region $REGION: restored to $TD_LOG_WHAT, as recorded at $param before the deploy turned it on. This is done before the first stack is deleted, because the stage's log group goes with its stack." ;;
    none|untagged|already|foreign) say "  Bedrock model invocation logging stays as it is: $(td_logging_stays)." ;;
    unreadable|bad) warn "the state recorded at $param could not be used (${TD_LOG_WHY}); teardown keeps the record and does not touch the Bedrock model invocation logging setting" ;;
    setting-unreadable) warn "the Bedrock model invocation logging setting in Region $REGION could not be read (${TD_LOG_WHY}); teardown keeps the record at $param and does not touch the setting" ;;
  esac
}

# td_restore_logging: the step. A failure is counted (exit 8) and the record stays, so that a second ./setup.sh --teardown can try again; the stacks are deleted all the same.
td_restore_logging() {
  local param by_hand
  param=$(td_logging_param)
  [ -n "$TD_LOG_MODE" ] || return 0
  step "Bedrock model invocation logging"
  case $TD_LOG_MODE in
    restore-delete)
      by_hand="aws bedrock delete-model-invocation-logging-configuration --region $REGION"
      if td_try bedrock delete-model-invocation-logging-configuration; then
        say_log "Restored Bedrock model invocation logging in Region $REGION to unset, as recorded at $param before the deploy turned it on."
      else
        TD_LOG_KEEP=1
        td_fail "could not restore Bedrock model invocation logging in Region $REGION to unset: ${AWS_ERR:-no answer}. The record stays at $param so that a second ./setup.sh --teardown can try again. By hand: $by_hand"
      fi ;;
    restore-put)
      by_hand="aws bedrock put-model-invocation-logging-configuration --region $REGION --logging-config '$TD_LOG_PRIOR'"
      if td_try bedrock put-model-invocation-logging-configuration --logging-config "$TD_LOG_PRIOR"; then
        say_log "Restored Bedrock model invocation logging in Region $REGION to $TD_LOG_WHAT, as recorded at $param before the deploy turned it on."
      else
        TD_LOG_KEEP=1
        td_fail "could not restore Bedrock model invocation logging in Region $REGION to $TD_LOG_WHAT: ${AWS_ERR:-no answer}. The record stays at $param so that a second ./setup.sh --teardown can try again. By hand: $by_hand"
      fi ;;
    none|untagged|already|foreign) say_log "Left Bedrock model invocation logging as it is: $(td_logging_stays)." ;;
    unreadable|bad) td_fail "could not read the state recorded at $param: ${TD_LOG_WHY}. The record stays, and the Bedrock model invocation logging setting was not touched." ;;
    setting-unreadable) td_fail "could not read the Bedrock model invocation logging setting in Region $REGION: ${TD_LOG_WHY}. The record stays at $param so that a second ./setup.sh --teardown can try again, and the setting was not touched." ;;
  esac
}

# ---- before anything is deleted ------------------------------------------------------------------------------------------------

# td_list_names <label> <list>: the names of a list (the first field of each line) as plan lines.
td_say_each() {
  local prefix=$1 line
  shift
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    say "$prefix${line%%"$TAB"*}"
  done <<<"$1"
}

# td_rule_name <physical id of a rule>: the name of the rule (CloudFormation lists a rule on a bus of its own as <bus>|<name>).
td_rule_name() {
  case $1 in
    *"|"*) printf '%s' "${1#*"|"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# td_plan_head: the top of the plan, said once: the account, the credentials and the generations of names this plan covers.
td_plan_head() {
  step "What the teardown removes"
  say "  account: $ACCOUNT, Region $REGION, stage $STAGE"
  say "  Credentials: your own ($CALLER_ARN). You run teardown on this machine. It installs nothing in your account. It starts no build. It opens no authority window."
  say "  One teardown plan covers $(td_covers_text) with one confirmation."
}

# td_plan: what the teardown removes of the generation in use. The text of one generation, said under a heading that names it.
td_plan() {
  local line name b r ends who rid reason vpcs_left
  say "  The plan covers the generation using $GEN_LABEL:"
  say "  The stage's resources are the stacks named $GEN_PREFIX-$STAGE-* that carry the tags project=$SETUP_PROJECT_TAG and stage=$STAGE, what those stacks list, and what carries the same tags."
  if [ -n "$TD_STACKS" ]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      name=${line%%"$TAB"*}
      say "  stack $name (${line#*"$TAB"})"
    done <<<"$TD_STACKS"
    for b in $(td_res_ids AWS::S3::Bucket); do say "  bucket $b: every object version and delete marker is deleted, then the bucket"; done
    for r in $(td_res_ids AWS::ECR::Repository); do say "  image repository $r: every image is deleted, then the repository"; done
    for r in $(td_res_ids AWS::SecretsManager::Secret | grep -F ":secret:$GEN_PREFIX-$STAGE-research-tavily-api-key-" || true); do
      say "  secret ${r#*:secret:}: deleted right after its stack, without the 30-day recovery window. With that window CloudFormation keeps its fixed name for 30 days, and a new deploy could not create it"
    done
    for r in $(td_res_ids AWS::Events::Rule); do say "  EventBridge rule $(td_rule_name "$r"): disabled first, deleted with its stack"; done
    for r in $(td_res_ids AWS::StepFunctions::StateMachine); do say "  Step Functions state machine ${r##*:}: its running executions are stopped first. The state machine is deleted with its stack."; done
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      say "  AgentCore runtime ${line##*"$TAB"}: its sessions are stopped and the v<N> endpoints that deploys made on it are deleted first (id $(printf '%s' "$line" | cut -f1))"
    done <<<"$TD_RUNTIMES"
    say "  Teardown deletes the stacks one at a time. An importing stack is deleted before the stack it imports. CloudFormation uses the role stored with each stack."
  fi
  if [ "$RN_EXISTS" = 1 ]; then
    ends=""
    if [ -n "$RN_EXPIRES" ]; then ends="; its authority to assume the CDK roles would have ended $RN_EXPIRES UTC"; fi
    if [ "$RN_BUCKET_GONE" = 1 ]; then
      say "  stack $SETUP_RUNNER_STACK (the runner is deleted first$ends; its bucket ${RN_BUCKET:-none} is gone)"
    else
      say "  stack $SETUP_RUNNER_STACK and its bucket ${RN_BUCKET:-none} (the runner, deleted first$ends)"
    fi
    say "  If your own session may not delete the runner's IAM role, the runner stack is deleted again with the CDK CloudFormation execution role arn:aws:iam::$ACCOUNT:role/cdk-$SETUP_BOOTSTRAP_QUALIFIER-cfn-exec-role-$ACCOUNT-$REGION, the role that deletes the stage's stacks."
  fi
  if [ -n "$TD_VPCS" ]; then
    for r in $TD_VPCS; do
      say "  VPC $r, left by an earlier teardown because AgentCore held network interfaces in it. Teardown deletes its subnets, security groups, route tables, internet gateways and endpoints that carry the stage tags, then the VPC."
    done
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    say "  AgentCore runtime ${line##*"$TAB"} (id $(printf '%s' "$line" | cut -f1)) carries the stage tags. No stack lists it. Teardown reports it and does not delete it. You can delete it with: aws bedrock-agentcore-control delete-agent-runtime --agent-runtime-id $(printf '%s' "$line" | cut -f1)"
  done <<<"$TD_RUNTIMES_ORPHAN"
  for b in $TD_BUCKETS_ORPHAN; do say "  bucket $b carries the stage tags and no stack lists it: every object version and delete marker is deleted, then the bucket"; done
  if [ -n "$TD_LOGS" ]; then
    say "  Log groups, deleted when no stack of the stage is left and the function, project or runtime that wrote to them is gone:"
    td_say_each "    " "$TD_LOGS"
  fi
  if [ -n "$TD_PARAMS" ]; then
    say "  SSM parameters that carry the stage tags, deleted when no stack of the stage is left:"
    td_say_each "    " "$TD_PARAMS"
  fi
  td_say_logging_plan
  vpcs_left=$(td_vpcs_left)
  if [ -n "$vpcs_left" ]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      say "  VPC ${line%%"$TAB"*} carries the stage tags and stays: nothing shows that a stack of the stage made it, so it is reported as LEFT and not touched"
    done <<<"$vpcs_left"
  fi
  if [ "$TD_UNTRUSTED" = 1 ] && [ -n "$TD_STACKS" ]; then
    who="the identity that ran setup"
    ends=""
    reason=""
    if [ -n "$TD_OPERATOR" ]; then who=$TD_OPERATOR; ends="; it names $TD_OPERATOR"; reason="If that identity no longer exists, its owner can replace the policy with: aws s3api delete-bucket-policy --bucket $GEN_PREFIX-$STAGE-gomod-$ACCOUNT"; fi
    warn "the bucket policy of the Go module store does not name your identity ($CALLER_ARN)$ends. That bucket and the packages stack stay. You run teardown as $who to remove them. $reason"
  fi
}

# td_plan_tail: the end of the plan, said once for all generations: how long it takes, what stays and what is lost with a stage. TD_ANY_STACKS and TD_ANY_VPCS say what the
# generations hold; td_plan_mark sets them.
td_plan_tail() {
  local g e2e="" saved=$GEN
  if [ "$TD_ANY_STACKS" = 1 ]; then
    say "  Teardown takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface. AgentCore can keep that interface for up to 8 hours after its runtime is deleted. In that case the network stack is deleted while the resources the interface holds are retained. They appear as RETAINED. $(td_again_text)"
  elif [ "$TD_ANY_VPCS" = 1 ]; then
    # no stack to delete: a run that only retries what an earlier teardown retained takes about a minute
    say "  This run only retries removing network resources retained by an earlier teardown; resources AgentCore still holds stay and appear as RETAINED."
  fi
  say "  Stays in place: the CDK bootstrap stack, its bucket and its container repository with the files and images the deploy published; the client, the receipt and the logs on this machine. KMS keys and secrets that CloudFormation schedules for deletion appear as pending: lines with their dates."
  for g in $TD_FOUND_GENS; do
    setup_gen_use "$g"
    e2e="$e2e${e2e:+ and }$GEN_E2E*"
  done
  setup_gen_use "$saved"
  say "  Removing a stage loses its run records, delivered zips, engine settings, Tavily key and table rows; CodeCommit repositories named $e2e remain, and an open pull request keeps an approval rule naming a role of the removed stage."
  if [ -n "$(td_skipped_gen)" ]; then
    say "  The generation using $(td_gen_label "$(td_skipped_gen)") is not read or covered by this plan because of --only; TEARDOWN-INVENTORY 0 stacks does not show whether it holds any resources."
  fi
}

# td_plan_mark: remembers what the generation in use holds, for td_plan_tail.
td_plan_mark() {
  [ -z "$TD_STACKS" ] || TD_ANY_STACKS=1
  [ -z "$TD_VPCS" ] || TD_ANY_VPCS=1
}

# td_would_gen: the dry-run lines of the generation in use.
td_would_gen() {
  if [ "$RN_EXISTS" = 1 ]; then
    if [ "$RN_BUCKET_GONE" = 1 ]; then would "delete the runner stack $SETUP_RUNNER_STACK"; else would "empty the bucket ${RN_BUCKET:-of the runner} and delete the runner stack $SETUP_RUNNER_STACK"; fi
  fi
  if [ -n "$TD_STACKS" ]; then
    would "disable the EventBridge rules of the stage, stop its executions and sessions, and delete the v<N> endpoints of its runtimes"
    would "empty the buckets and the image repositories of the stage, then delete its stacks one at a time"
    would "retain the network resources that AgentCore holds, listed as RETAINED"
  fi
  case $TD_LOG_MODE in
    restore-delete) would "restore Bedrock model invocation logging in Region $REGION to unset (recorded at $(td_logging_param)), before the first stack is deleted" ;;
    restore-put) would "restore Bedrock model invocation logging in Region $REGION to $TD_LOG_WHAT (recorded at $(td_logging_param)), before the first stack is deleted" ;;
  esac
  [ -z "$TD_LOGS$TD_PARAMS" ] || would "delete the log groups and the SSM parameters listed above when no stack of the stage is left"
  [ -z "$TD_VPCS" ] || would "remove the network resources an earlier teardown retained, when AWS has released the interfaces"
}

# ---- stopping what runs --------------------------------------------------------------------------------------------------------

# td_disable_rules: the EventBridge rules that a stack of the stage lists (a rule under the stage's prefix that no stack lists is not the stage's and is not touched).
td_disable_rules() {
  local id name bus done_n=0 ok
  for id in $(td_res_ids AWS::Events::Rule); do
    name=$id
    bus=""
    case $id in
      *"|"*) bus=${id%%"|"*}; name=${id#*"|"} ;;
    esac
    ok=0
    if [ -n "$bus" ]; then
      td_try events disable-rule --name "$name" --event-bus-name "$bus" && ok=1
    else
      td_try events disable-rule --name "$name" && ok=1
    fi
    if [ "$ok" = 1 ]; then
      done_n=$((done_n + 1))
    elif ! td_missing; then
      td_fail "could not disable the rule $name: ${AWS_ERR:-no answer}"
    fi
  done
  say_log "$done_n stage EventBridge rules have been disabled."
}

td_stop_executions() {
  local machine run done_n=0
  for machine in $(td_res_ids AWS::StepFunctions::StateMachine); do
    if ! td_try stepfunctions list-executions --state-machine-arn "$machine" --status-filter RUNNING --query 'executions[].executionArn' --output text; then
      td_missing || td_fail "could not list the running executions of ${machine##*:}: ${AWS_ERR:-no answer}"
      continue
    fi
    for run in $(td_words "$AWS_OUT"); do
      if td_try stepfunctions stop-execution --execution-arn "$run" --cause "setup.sh --teardown"; then
        log_line "stopped execution $run"
        done_n=$((done_n + 1))
      else
        td_fail "could not stop the execution $run: ${AWS_ERR:-no answer}"
      fi
    done
  done
  say_log "$done_n Step Functions executions have been stopped."
}

# td_qualifier <endpoint name>: the endpoint a session is stopped on: the one its run is pinned to when that is an endpoint name, else live.
td_qualifier() {
  case $1 in
    ''|None|[!A-Za-z]*|*[!A-Za-z0-9_]*) printf 'live' ;;
    *) if [ "${#1}" -le 48 ]; then printf '%s' "$1"; else printf 'live'; fi ;;
  esac
}

td_provider_id() {
  local name
  name=$(td_param_root)/instances-capacity-provider-id
  if [ "$TD_PROVIDER_READ" = 0 ]; then
    TD_PROVIDER_READ=1
    # the parameter names the provider whose sessions are deleted: it is the stage's only with the stage tags
    if printf '%s\n' "$TD_PARAMS" | grep -Fqx -- "$name" && td_try ssm get-parameter --name "$name" --query Parameter.Value --output text; then
      TD_PROVIDER=$AWS_OUT
    fi
  fi
  [ -n "$TD_PROVIDER" ] && [ "$TD_PROVIDER" != None ]
}

# td_stop_sessions: the sessions in the stage's table that are on a runtime a stack of the stage lists. The table is the one the data stack lists.
td_stop_sessions() {
  local line arn sid endpoint q done_n=0 table=$GEN_PREFIX-$STAGE-harness
  if ! td_res_ids AWS::DynamoDB::Table | grep -Fqx -- "$table"; then
    say_log "$done_n AgentCore sessions have been stopped."
    return 0
  fi
  if ! td_try dynamodb scan --table-name "$table" --filter-expression 'begins_with(SK, :a)' --expression-attribute-values '{":a":{"S":"AGENT#"}}' \
      --query "Items[?status.S!='stopped'].[runtimeArn.S,runtimeSessionId.S,runtimeEndpoint.S]" --output text; then
    td_missing || td_fail "could not read the sessions of the stage: ${AWS_ERR:-no answer}"
    return 0
  fi
  while IFS= read -r line; do
    [ -n "$line" ] && [ "$line" != None ] || continue
    arn=$(printf '%s' "$line" | cut -f1)
    sid=$(printf '%s' "$line" | cut -f2)
    endpoint=$(printf '%s' "$line" | cut -f3)
    # the whole ARN is compared: the same runtime id in another account or Region is another runtime
    if ! printf '%s' "$TD_RUNTIMES" | awk -F'\t' -v a="$arn" '$2 == a { found = 1 } END { exit found ? 0 : 1 }'; then
      log_line "session $sid is on $arn, which is not the ARN of a runtime that a stack of the stage lists; not stopped"
      continue
    fi
    [ -n "$sid" ] && [ "$sid" != None ] || continue
    q=$(td_qualifier "$endpoint")
    if td_try bedrock-agentcore stop-runtime-session --agent-runtime-arn "$arn" --runtime-session-id "$sid" --qualifier "$q"; then
      log_line "stopped session $sid"
      done_n=$((done_n + 1))
    elif ! td_missing; then
      td_fail "could not stop the session $sid: ${AWS_ERR:-no answer}"
      continue
    fi
    # a session on the Instances runtime is a machine of the account; stopping the agent leaves the machine
    case $arn in
      *"harness_worker_python_instances-"*)
        if td_provider_id; then
          td_try bedrock-agentcore delete-capacity-provider-session --capacity-provider-id "$TD_PROVIDER" --session-id "$sid" || td_missing ||
            td_fail "could not delete the machine of the session $sid: ${AWS_ERR:-no answer}"
        else
          td_fail "the session $sid is on the Instances runtime and the stage publishes no capacity provider id that carries the stage tags; its machine is not deleted"
        fi ;;
    esac
  done <<<"$AWS_OUT"
  say_log "$done_n AgentCore sessions have been stopped."
}

# td_version_endpoints <runtime id>: TD_EP_LIST, the endpoints named v<N> of a runtime, one per line; returns 1 (with AWS_ERR) when the list could not be read.
td_version_endpoints() {
  local name
  TD_EP_LIST=""
  td_try bedrock-agentcore-control list-agent-runtime-endpoints --agent-runtime-id "$1" --query 'runtimeEndpoints[].name' --output text || return 1
  for name in $(td_words "$AWS_OUT"); do
    case $name in
      v[1-9]*[!0-9]*) ;;
      v[1-9]*) TD_EP_LIST="$TD_EP_LIST$name$NL" ;;
    esac
  done
  return 0
}

td_delete_version_endpoints() {
  local rid name ids rc=0
  ids=$(printf '%s' "$TD_RUNTIMES" | awk -F'\t' 'NF { print $1 }')
  for rid in $ids; do
    if ! td_version_endpoints "$rid"; then
      td_missing || td_fail_once "endpoints:$rid" "could not list the endpoints of the runtime $rid, so none of its v<N> endpoints is deleted: ${AWS_ERR:-no answer}"
      continue
    fi
    for name in $TD_EP_LIST; do
      if td_try bedrock-agentcore-control delete-agent-runtime-endpoint --agent-runtime-id "$rid" --endpoint-name "$name"; then
        say_log "Deleting endpoint $name of runtime $rid ... Deploys create this endpoint. The runtime cannot be deleted while the endpoint exists."
      elif ! td_missing; then
        td_fail "could not delete the endpoint $name of the runtime $rid: ${AWS_ERR:-no answer}"
      fi
    done
  done
  # the delete is asynchronous and takes minutes: a line shows what the program waits for every few minutes, like the wait for a stack
  [ -z "$ids" ] || setup_ticker_start "AgentCore to delete the versioned runtime endpoints"
  td_wait_endpoints_gone "$ids" || rc=1
  setup_ticker_stop
  [ "$rc" = 0 ] || warn "endpoints named v<N> are still being deleted; the runtimes stack may refuse to go until they are"
}

# td_wait_endpoints_gone <runtime ids>: look until no v<N> endpoint is left (HAMH_SETUP_ENDPOINT_LOOKS looks, 60 by default, setup_sleep apart). Returns 0 when none is left or a listing
# failed (said by td_fail_once), 1 when the looks ran out.
td_wait_endpoints_gone() {
  local rid looks=0 left listed
  while [ "$looks" -lt "${HAMH_SETUP_ENDPOINT_LOOKS:-60}" ]; do
    left=""
    listed=1
    for rid in $1; do
      if td_version_endpoints "$rid"; then
        left="$left$TD_EP_LIST"
      elif ! td_missing; then
        listed=0
        td_fail_once "endpoints:$rid" "could not check whether the v<N> endpoints of the runtime $rid are gone: ${AWS_ERR:-no answer}"
      fi
    done
    [ "$listed" = 1 ] || return 0
    [ -n "$left" ] || return 0
    looks=$((looks + 1))
    setup_sleep
  done
  return 1
}

td_quiesce() {
  step "Stopping what is running"
  td_disable_rules
  td_stop_executions
  td_stop_sessions
  td_delete_version_endpoints
}

# ---- emptying what CloudFormation cannot delete ---------------------------------------------------------------------------------

# td_empty_bucket <bucket>: scripts/setup/empty-bucket.sh with this run's profile and Region. On a failure AWS_ERR holds the reason.
td_empty_bucket() {
  rn_empty_bucket "$1" || return 1
  log_line "emptied $1"
}

# td_empty_repo <repository>: every image, by digest, a hundred at a time, until the listing is empty. A round in which ECR refuses some images (an image index and
# its children listed together: the children fail until the index is gone) is followed by another; when a round deletes none, each remaining image is tried on its own
# up to 3 times and each one that stays is named.
td_empty_repo() {
  local repo=$1 _args d rounds=0 sent failed nfail one tries bad_n=0 reason
  while [ "$rounds" -lt 500 ]; do
    rounds=$((rounds + 1))
    if ! td_try ecr list-images --repository-name "$repo" --max-items 100 --query 'imageIds[].imageDigest' --output text; then
      td_missing && return 0
      return 1
    fi
    _args=""
    sent=0
    for d in $(td_words "$AWS_OUT" | sort -u | grep -E '^sha256:[0-9a-f]{64}$'); do
      _args="$_args imageDigest=$d"
      sent=$((sent + 1))
    done
    if [ -z "$_args" ]; then
      if [ -n "$(td_words "$AWS_OUT")" ]; then
        AWS_ERR="the listing of $repo holds entries that are not image digests; nothing was deleted"
        return 1
      fi
      return 0
    fi
    # $_args is one shorthand item per digest and is split on purpose (a name that starts with an underscore is the form the shell rules accept for that)
    if ! td_try ecr batch-delete-image --repository-name "$repo" --image-ids $_args --query 'failures[].[imageId.imageDigest,failureReason]' --output text; then
      return 1
    fi
    failed=$(printf '%s\n' "$AWS_OUT" | awk -F'\t' 'NF && $1 != "None"')
    nfail=$(td_count "$failed")
    [ "$nfail" -lt "$sent" ] && continue
    # nothing in this round was deleted: try each image alone
    bad_n=0
    for d in $(printf '%s\n' "$failed" | cut -f1 | sort -u); do
      tries=0
      while :; do
        tries=$((tries + 1))
        one=""
        if td_try ecr batch-delete-image --repository-name "$repo" --image-ids "imageDigest=$d" --query 'failures[].[imageId.imageDigest,failureReason]' --output text; then
          one=$(printf '%s\n' "$AWS_OUT" | awk -F'\t' 'NF && $1 != "None"')
        else
          one="$d$TAB${AWS_ERR:-no answer}"
        fi
        [ -n "$one" ] || break
        [ "$tries" -lt 3 ] || break
        td_pause "$tries"
      done
      if [ -n "$one" ]; then
        reason=$(printf '%s' "$one" | cut -f2)
        say "      image $d in $repo was not deleted: $reason"
        bad_n=$((bad_n + 1))
      fi
    done
    if [ "$bad_n" -gt 0 ]; then
      AWS_ERR="ECR refused to delete $bad_n image(s) of $repo"
      return 1
    fi
  done
  AWS_ERR="the repository $repo still holds images after $rounds rounds"
  return 1
}

# td_research_secret_name: the fixed name of the stage's research secret (infra/lib/config.ts).
td_research_secret_name() { printf '%s-%s-research-tavily-api-key' "$GEN_PREFIX" "$STAGE"; }

# td_prepare_stack <stack>: empties the buckets and the repositories of the stack, remembers the research secret (td_force_secrets deletes it after the stack), and remembers
# its keys and the other secrets for the report, and remembers the VPC the stack owns (TD_PREPARED_VPC, empty for the others). Returns 1 when something could not be emptied: CloudFormation would refuse the delete, so the stack is not tried.
td_prepare_stack() {
  local line type id ok=0 name
  TD_PREPARED_VPC=""
  if printf '%s' "$TD_RES_FAILED" | grep -Fqx -- "$1"; then
    td_fail_once "resources:$1" "could not list the resources of the stack $1, so it is not deleted"
    return 1
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    type=$(printf '%s' "$line" | cut -f2)
    id=$(printf '%s' "$line" | cut -f3)
    case $type in
      AWS::S3::Bucket)
        say_log "Emptying bucket $id as $CALLER_ARN ... This removes every object version and delete marker."
        if ! td_empty_bucket "$id"; then
          td_fail_once "bucket:$id" "could not empty the bucket $id: ${AWS_ERR:-no answer}"
          case $id in
            "$GEN_PREFIX"-*-gomod-*) say "      The Go module store permits object deletion by the setup identity $TD_OPERATOR_NOTE and the runner role. You run ./setup.sh --teardown as the setup identity." ;;
          esac
          td_reason "$1" "the bucket $id could not be emptied"
          ok=1
        fi ;;
      AWS::ECR::Repository)
        say_log "Deleting the images of the repository $id ..."
        if ! td_empty_repo "$id"; then
          td_fail_once "repo:$id" "could not delete the images of the repository $id: ${AWS_ERR:-no answer}"
          td_reason "$1" "the repository $id still holds images"
          ok=1
        fi ;;
      AWS::KMS::Key) TD_KEYS="$TD_KEYS$id$NL" ;;
      AWS::EC2::VPC) TD_PREPARED_VPC=$id ;;
      AWS::SecretsManager::Secret)
        name=$(td_research_secret_name)
        case $id in
          *":secret:$name-"??????)
            # it goes right after its stack (td_force_secrets): the stack deletes the resource policy of the secret, and CloudFormation cannot do that while the secret is marked for deletion
            printf '%s' "$TD_FORCE_SECRETS" | grep -Fqx -- "$1$TAB$id" || TD_FORCE_SECRETS="$TD_FORCE_SECRETS$1$TAB$id$NL" ;;
          *) TD_SECRETS="$TD_SECRETS$id$NL" ;;
        esac ;;
    esac
  done <<<"$(td_res_rows | awk -F'\t' -v s="$1" '$1 == s')"
  return "$ok"
}

# ---- the stacks ------------------------------------------------------------------------------------------------------------------

# td_order: the stacks of the stage that exist, one per line: those this script does not know first, then the nine in the reverse of the order they are deployed in.
td_order() {
  local name known="" n present
  for n in observability delivery orchestration runtimes control packages data security network; do
    known="$known${known:+$NL}$GEN_PREFIX-$STAGE-$n"
  done
  present=$(printf '%s\n' "$TD_STACKS" | cut -f1)
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    printf '%s\n' "$known" | grep -Fqx -- "$name" || printf '%s\n' "$name"
  done <<<"$present"
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    if printf '%s\n' "$present" | grep -Fqx -- "$name"; then printf '%s\n' "$name"; fi
  done <<<"$known"
  return 0
}

# td_no_importers <export name>: 0 when the last failed call was the answer of CloudFormation for an export that no stack imports: code ValidationError and the message
# for that export, nothing else (a local failure or another message is a failed read).
td_no_importers() {
  [ "$(aws_error_code)" = ValidationError ] || return 1
  case $AWS_ERR in
    *"Export '$1' is not imported by any stack"*) return 0 ;;
  esac
  return 1
}

# td_importers <stack>: 0 and TD_IMPORTERS (names) when another stack still imports an export of this one (CloudFormation refuses to delete such a stack), 1 when nothing
# does, 2 when that could not be read: the stack is then not deleted, because "nobody imports it" is a thing the run read and did not guess. CloudFormation answers an export
# that nobody imports with an error that says so; that error is the answer, and any other error is a failed read.
td_importers() {
  local e u found="" bad=0
  TD_IMPORTERS=""
  td_read stacks "the exports of the stack $1" cloudformation describe-stacks --stack-name "$1" --query 'Stacks[0].Outputs[].ExportName' --output text
  case $? in
    1) return 1 ;;
    2) return 2 ;;
  esac
  for e in $(td_words "$AWS_OUT"); do
    if td_try cloudformation list-imports --export-name "$e" --query Imports --output text; then
      for u in $(td_words "$AWS_OUT"); do
        [ "$u" = "$1" ] || found="$found $u"
      done
    elif ! td_no_importers "$e"; then
      td_unread stacks "the stacks that import the export $e"
      bad=1
    fi
  done
  TD_IMPORTERS=${found# }
  [ -z "$TD_IMPORTERS" ] || return 0
  [ "$bad" = 0 ] || return 2
  return 1
}

# td_show_failed <stack>: the resources a stack could not delete, with the reason CloudFormation gives.
td_show_failed() {
  local line
  td_try cloudformation list-stack-resources --stack-name "$1" --query "StackResourceSummaries[?ResourceStatus=='DELETE_FAILED'].[LogicalResourceId,ResourceType,PhysicalResourceId]" --output text || return 0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    say "      did not delete: $(printf '%s' "$line" | tr '\t' ' ')"
  done <<<"$AWS_OUT"
}

# td_wait_deleted <stack>: waits for a stack to be gone and says so every few minutes (HAMH_SETUP_PROGRESS_SECONDS, 180 by default). Returns the waiter's status.
td_wait_deleted() {
  local rc
  setup_ticker_start "CloudFormation to delete the stack $1"
  td_try cloudformation wait stack-delete-complete --stack-name "$1"
  rc=$?
  setup_ticker_stop
  return "$rc"
}

# td_force_secrets <stack>: the research secret of a stack that is gone is deleted without its recovery window (it is accepted also for a secret that CloudFormation has scheduled
# for deletion), so that its fixed name is free for a new deploy. The result waits for the secret to be gone (td_await_secret).
td_force_secrets() {
  local line id name
  name=$(td_research_secret_name)
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    [ "${line%%"$TAB"*}" = "$1" ] || continue
    id=${line#*"$TAB"}
    printf '%s%s' "$TD_SECRETS_FORCED" "$TD_SECRETS" | grep -Fqx -- "$id" && continue
    say_log "The secret $name is being deleted without its recovery window. With that window CloudFormation keeps the name for 30 days, and a new deploy could not create it."
    if td_try secretsmanager delete-secret --secret-id "$id" --force-delete-without-recovery || td_missing; then
      TD_SECRETS_FORCED="$TD_SECRETS_FORCED$id$NL"
    else
      td_fail_once "secret:$id" "The secret $name cannot be deleted without its recovery window: ${AWS_ERR:-no answer}. CloudFormation deleted it with a 30-day recovery window. A new deploy cannot create it until that window ends. You can release the name with: aws secretsmanager delete-secret --secret-id $name --force-delete-without-recovery"
      TD_SECRETS="$TD_SECRETS$id$NL"
    fi
  done <<<"$TD_FORCE_SECRETS"
}

# td_force_gone_secrets: a stack can go without td_delete_one seeing it (a retain call finished it, or it went between two rounds): the secrets of every queued stack that the account
# no longer lists are deleted now. A stack that is still listed keeps its secret. The listing is the one of the generation in use, so only the stacks of that generation are judged: the
# queue holds the entries of both generations, and a stack of the other one is missing from this listing without being gone.
td_force_gone_secrets() {
  local line stack seen=""
  [ -n "$TD_FORCE_SECRETS" ] || return 0
  td_read_stacks || return 0
  [ "$TD_STACKS_LISTED" = 1 ] || return 0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    stack=${line%%"$TAB"*}
    td_prefixed "$stack" || continue
    td_has "$seen" "$stack" && continue
    seen="$seen $stack"
    printf '%s\n' "$TD_STACKS" | cut -f1 | grep -Fqx -- "$stack" && continue
    td_force_secrets "$stack"
  done <<<"$TD_FORCE_SECRETS"
}

# td_secrets_pending: 0 when the queue holds a secret of a stack of the generation in use that no delete call has dealt with yet.
td_secrets_pending() {
  local line id
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    td_prefixed "${line%%"$TAB"*}" || continue
    id=${line#*"$TAB"}
    printf '%s%s' "$TD_SECRETS_FORCED" "$TD_SECRETS" | grep -Fqx -- "$id" && continue
    return 0
  done <<<"$TD_FORCE_SECRETS"
  return 1
}

# td_force_gone_secrets_all: after the delete passes, once for each generation that was worked on. A stack that was still going when its own pass ended (it was being deleted, or it had been
# left for the next round) can be gone by now, and its secret goes with it; the reads are made only for a generation that has a secret waiting.
td_force_gone_secrets_all() {
  local g
  for g in $TD_FOUND_GENS; do
    td_use_generation "$g"
    td_plan_thaw
    td_secrets_pending || continue
    td_force_gone_secrets
  done
}

# td_stack_gone <stack> [note]: the stack is deleted; what waited for that goes now.
td_stack_gone() {
  ok "stack $1 deleted${2:+ $2}"
  td_force_secrets "$1"
}

# td_network_type <resource type>: 0 for the resource types that AgentCore's interfaces hold up, which a delete may retain.
td_network_type() {
  case $1 in
    AWS::EC2::Subnet|AWS::EC2::SecurityGroup|AWS::EC2::VPC|AWS::EC2::NetworkInterface|AWS::EC2::VPCEndpoint|AWS::EC2::RouteTable|AWS::EC2::SubnetRouteTableAssociation|AWS::EC2::SecurityGroupIngress|AWS::EC2::SecurityGroupEgress) return 0 ;;
  esac
  return 1
}

# td_announce_retained <stack> <logical id> <type> <physical id>: one retained resource, said and remembered (for the receipt, the by-hand commands and the sweep that must not take it).
td_announce_retained() {
  td_note_retained "$4 ($3 $2 in $1)"
  say "RETAINED $4 ($3 $2 in stack $1): AgentCore still holds network interfaces. AWS releases them up to 8 hours after the runtime is deleted."
  log_line "RETAINED $4 $3 $2 $1"
  TD_RETAINED_RES="$TD_RETAINED_RES$3$TAB$4$NL"
  case $3 in
    AWS::EC2::VPC) TD_RETAINED_VPCS="$TD_RETAINED_VPCS $4" ;;
  esac
}

# td_say_commands <subnet ids> <group ids> <vpc ids> [endpoint ids] [route table ids]: the commands that remove network resources by hand, in the order that works.
td_say_commands() {
  local id _endpoints=${4:-} _tables=${5:-}
  [ -n "$1$2$3$_endpoints$_tables" ] || return 0
  say "      After AWS releases the interface, you can remove these yourself in this order (account $ACCOUNT, Region $REGION):"
  for id in $_endpoints; do say "        aws ec2 delete-vpc-endpoints --vpc-endpoint-ids $id"; done
  for id in $1; do say "        aws ec2 delete-subnet --subnet-id $id"; done
  for id in $2; do say "        aws ec2 delete-security-group --group-id $id"; done
  for id in $_tables; do say "        aws ec2 delete-route-table --route-table-id $id"; done
  for id in $3; do say "        aws ec2 delete-vpc --vpc-id $id"; done
}

# td_retained_commands: the by-hand commands for the subnets, groups and VPCs that this run retained.
td_retained_commands() {
  local line subnets="" groups="" vpcs="" endpoints="" tables=""
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    case ${line%%"$TAB"*} in
      AWS::EC2::Subnet) subnets="$subnets ${line#*"$TAB"}" ;;
      AWS::EC2::SecurityGroup) groups="$groups ${line#*"$TAB"}" ;;
      AWS::EC2::VPC) vpcs="$vpcs ${line#*"$TAB"}" ;;
      AWS::EC2::VPCEndpoint) endpoints="$endpoints ${line#*"$TAB"}" ;;
      AWS::EC2::RouteTable) tables="$tables ${line#*"$TAB"}" ;;
    esac
  done <<<"$TD_RETAINED_RES"
  td_say_commands "$subnets" "$groups" "$vpcs" "$endpoints" "$tables"
}

# td_say_vpc_commands <vpc>: the by-hand commands for the subnets and groups of a VPC that carry the stage tags, and the VPC (read now, so a resource that is gone is not listed).
td_say_vpc_commands() {
  local v=$1 subnets="" groups=""
  if td_try ec2 describe-subnets --filters "Name=vpc-id,Values=$v" "Name=tag:project,Values=$SETUP_PROJECT_TAG" "Name=tag:stage,Values=$STAGE" --query 'Subnets[].SubnetId' --output text; then
    subnets=$(td_words "$AWS_OUT" | tr '\n' ' ')
  fi
  if td_try ec2 describe-security-groups --filters "Name=vpc-id,Values=$v" "Name=tag:project,Values=$SETUP_PROJECT_TAG" "Name=tag:stage,Values=$STAGE" --query "SecurityGroups[?GroupName!='default'].GroupId" --output text; then
    groups=$(td_words "$AWS_OUT" | tr '\n' ' ')
  fi
  td_say_commands "$subnets" "$groups" "$v"
}

# td_retain_failed <stack>: the network resources a stack failed to delete are retained and the stack is deleted again. Returns 0 when a retain call was made, 1 when nothing could be
# retained. A resource of another kind that failed is not retained: the stack stays and is reported.
td_retain_failed() {
  local name=$1 rows row logical rtype physical _ids="" found=0 kept=""
  if ! td_try cloudformation list-stack-resources --stack-name "$name" \
      --query "StackResourceSummaries[?ResourceStatus=='DELETE_FAILED'].[LogicalResourceId,ResourceType,PhysicalResourceId]" --output text; then
    td_warn_once "retain-read:$name" "could not read what the stack $name failed to delete: ${AWS_ERR:-no answer}"
    return 1
  fi
  rows=$AWS_OUT
  while IFS= read -r row; do
    [ -n "$row" ] || continue
    logical=$(printf '%s' "$row" | cut -f1)
    rtype=$(printf '%s' "$row" | cut -f2)
    physical=$(printf '%s' "$row" | cut -f3)
    if td_network_type "$rtype"; then
      _ids="$_ids $logical"
      found=1
      kept="$kept$logical$TAB$rtype$TAB$physical$NL"
    else
      say "NOT RETAINED $physical ($rtype $logical in stack $name) failed to delete and is not a network resource, so it stays for you to look at"
    fi
  done <<<"$rows"
  [ "$found" = 1 ] || return 1
  # $_ids is the list of logical ids and is split on purpose
  # shellcheck disable=SC2086
  if td_try cloudformation delete-stack --stack-name "$name" --retain-resources $_ids; then
    # CloudFormation accepted the call: these resources are retained from now on
    while IFS= read -r row; do
      [ -n "$row" ] || continue
      td_announce_retained "$name" "$(printf '%s' "$row" | cut -f1)" "$(printf '%s' "$row" | cut -f2)" "$(printf '%s' "$row" | cut -f3)"
    done <<<"$kept"
    td_wait_deleted "$name" || true
  else
    say "could not delete the stack $name again: ${AWS_ERR:-no answer}"
    return 1
  fi
  return 0
}

# td_hold_retain <stack>: a stack that has just ended in DELETE_FAILED because AgentCore holds a network interface in its VPC is deleted again at once, with the failed network
# resources and the VPC they hold up retained, instead of after rounds of waiting that cannot end before AWS lets the interface go (each round was 18 minutes). It is the hold when
# every resource that failed is a network resource, the stack has a VPC and an interface is held in it. Returns 0 when the stack is gone, 1 when it is not the hold or the stack stays
# (the rounds and td_retain_stuck then go on as before).
td_hold_retain() {
  local name=$1 rows row logical rtype physical failed_ids="" _all="" vpc="" held_rows="" named="" tries=0
  td_try cloudformation list-stack-resources --stack-name "$name" \
    --query "StackResourceSummaries[?ResourceStatus=='DELETE_FAILED'].[LogicalResourceId,ResourceType,PhysicalResourceId]" --output text || return 1
  rows=$AWS_OUT
  while IFS= read -r row; do
    [ -n "$row" ] && [ "$row" != None ] || continue
    logical=$(printf '%s' "$row" | cut -f1)
    rtype=$(printf '%s' "$row" | cut -f2)
    physical=$(printf '%s' "$row" | cut -f3)
    td_network_type "$rtype" || return 1
    failed_ids="$failed_ids $logical"
    _all="$_all $logical"
    held_rows="$held_rows$logical$TAB$rtype$TAB$physical$NL"
  done <<<"$rows"
  [ -n "$failed_ids" ] || return 1
  td_try cloudformation list-stack-resources --stack-name "$name" \
    --query "StackResourceSummaries[?ResourceType=='AWS::EC2::VPC'].[LogicalResourceId,PhysicalResourceId,ResourceStatus]" --output text || return 1
  while IFS= read -r row; do
    [ -n "$row" ] && [ "$row" != None ] || continue
    [ "$(printf '%s' "$row" | cut -f3)" != DELETE_COMPLETE ] || continue
    # a stack with more than one VPC is not the stage's network stack as this code knows it: the hold is not decided for it
    if [ -n "$vpc" ]; then
      log_line "the stack $name has more than one VPC; the hold of its network resources is not decided for it"
      return 1
    fi
    logical=$(printf '%s' "$row" | cut -f1)
    physical=$(printf '%s' "$row" | cut -f2)
    vpc=$physical
    if ! td_has "$_all" "$logical"; then
      _all="$_all $logical"
      held_rows="$held_rows$logical${TAB}AWS::EC2::VPC$TAB$physical$NL"
    fi
  done <<<"$AWS_OUT"
  [ -n "$vpc" ] || return 1
  td_enis "$vpc" || return 1
  [ "$TD_ENI_HELD" -gt 0 ] || return 1
  named=""
  while IFS= read -r row; do
    [ -n "$row" ] || continue
    named="$named${named:+, }$(printf '%s' "$row" | cut -f3)"
  done <<<"$held_rows"
  say "$TD_ENI_HELD network interface(s) in $vpc are still in use. AgentCore keeps its interface for up to 8 hours after its runtime is deleted, and CloudFormation cannot delete the network resources while an interface exists. The stack $name is deleted again with these resources retained: $named."
  # $_all is the list of logical ids and is split on purpose
  # shellcheck disable=SC2086
  if td_try cloudformation delete-stack --stack-name "$name" --retain-resources $_all; then
    # CloudFormation accepted the call: these resources are retained from now on, whether or not the rest of the stack goes at once
    while IFS= read -r row; do
      [ -n "$row" ] || continue
      td_announce_retained "$name" "$(printf '%s' "$row" | cut -f1)" "$(printf '%s' "$row" | cut -f2)" "$(printf '%s' "$row" | cut -f3)"
    done <<<"$held_rows"
    td_wait_deleted "$name" || true
    if ! rn_stack_status "$name" || [ -n "$RN_STATUS" ]; then
      # the stack is still there: what else failed is retained or reported by the rounds of td_retain_stuck
      return 1
    fi
    return 0
  else
    case $(aws_error_code) in
      ValidationError)
        # a CloudFormation that takes only the resources that failed: retain those, then what they held up
        td_retain_failed "$name" || return 1
        while [ "$tries" -lt 3 ]; do
          tries=$((tries + 1))
          if rn_stack_status "$name" && [ -z "$RN_STATUS" ]; then return 0; fi
          td_retain_failed "$name" || break
        done
        rn_stack_status "$name" && [ -z "$RN_STATUS" ] && return 0
        return 1 ;;
      *)
        say "could not delete the stack $name again: ${AWS_ERR:-no answer}"
        return 1 ;;
    esac
  fi
  return 1
}

# td_delete_one <stack>: prepare, delete, wait. Returns 0 when the stack is gone.
td_delete_one() {
  local name=$1 hint
  if ! td_prepare_stack "$name"; then
    return 1
  fi
  [ -z "$TD_PREPARED_VPC" ] || td_note_held_interfaces "$TD_PREPARED_VPC"
  say_log "Deleting stack $name ... CloudFormation uses the role stored with the stack."
  if ! td_try cloudformation delete-stack --stack-name "$name"; then
    hint=""
    case $AWS_ERR in
      *"invalid or cannot be assumed"*)
        # the stack stores the execution role of the CDK bootstrap; with that role gone CloudFormation refuses the delete, and a role you name is the way out
        hint=" The role that CloudFormation stored with the stack no longer exists or cannot be assumed. If you removed the CDK bootstrap stack, you can delete the stack with a role that CloudFormation can assume: aws cloudformation delete-stack --stack-name $name --role-arn <ARN of that role>" ;;
    esac
    td_fail_once "delete:$name" "could not delete the stack $name: ${AWS_ERR:-no answer}.$hint"
    td_reason "$name" "the delete was refused: $(printf '%s' "${AWS_ERR:-no answer}" | tail -n 1 | cut -c1-200)"
    return 1
  fi
  if td_wait_deleted "$name"; then
    td_stack_gone "$name"
    return 0
  fi
  # the waiter ended without saying the stack is gone: the stack's own status says what happened. A status that cannot be read is not "gone" and is not "failed" either:
  # the account is read again at the end, and a stack that is still there then is listed with its reason.
  if ! rn_stack_status "$name"; then
    td_warn_once "status:$name" "the stack $name did not finish deleting in the time the AWS CLI waits, and its status could not be read (${AWS_ERR:-no answer}); the result below reads the account again"
    td_reason "$name" "its status could not be read; run ./setup.sh --teardown again"
    return 1
  fi
  case $RN_STATUS in
    '') td_stack_gone "$name"; return 0 ;;
    DELETE_FAILED)
      TD_TRANSIENT=1
      td_warn_once "delete:$name" "the stack $name could not be deleted (DELETE_FAILED)"
      td_show_failed "$name"
      if td_hold_retain "$name"; then
        td_stack_gone "$name" "(the network resources AgentCore holds are retained)"
        return 0
      fi
      td_reason "$name" "DELETE_FAILED" ;;
    *)
      TD_TRANSIENT=1
      td_warn_once "delete:$name" "the stack $name is $RN_STATUS and did not finish deleting in the time the AWS CLI waits"
      td_reason "$name" "$RN_STATUS; it is still being deleted, run ./setup.sh --teardown again" ;;
  esac
  return 1
}

td_delete_stacks() {
  local name round=0 progress attempted extra=0 remain
  remain=$(td_order)
  while [ -n "$remain" ] && [ "$round" -lt 8 ]; do
    round=$((round + 1))
    progress=0
    attempted=0
    TD_TRANSIENT=0
    for name in $remain; do
      td_importers "$name"
      case $? in
        0)
          td_reason "$name" "still imported by $TD_IMPORTERS, which has to go first"
          continue ;;
        2)
          td_reason "$name" "the stacks that import its exports could not be read; run ./setup.sh --teardown again"
          continue ;;
      esac
      attempted=1
      if td_delete_one "$name"; then
        progress=1
        remain=$(printf '%s\n' "$remain" | grep -Fvx -- "$name" || true)
      fi
    done
    if [ "$progress" = 0 ]; then
      # a stack that ended in DELETE_FAILED may only be waiting for something asynchronous (a capacity provider while runtime versions detach): look again twice
      if [ "$attempted" = 1 ] && [ "$TD_TRANSIENT" = 1 ] && [ "$extra" -lt 2 ]; then
        extra=$((extra + 1))
        td_pause "$extra"
        continue
      fi
      break
    fi
  done
}

# td_retain_stuck: for each stage stack in DELETE_FAILED, retain the network resources that failed (subnets, security groups, the VPC and the
# interfaces and endpoints around them) and delete the stack again, up to 5 rounds because a retained subnet can free the next resource. Anything
# else that failed is not retained: the stack stays and is reported.
td_retain_stuck() {
  local round=0 line name status stuck
  while [ "$round" -lt 5 ]; do
    round=$((round + 1))
    td_read_stacks
    stuck=0
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      name=${line%%"$TAB"*}
      status=${line#*"$TAB"}
      [ "$status" = DELETE_FAILED ] || continue
      if ! td_has "$TD_PLAN_STACKS" "$name"; then
        td_warn_once "late-stack:$name" "the stack $name appeared after the plan was confirmed and is not touched; run ./setup.sh --teardown again to see it in a plan"
        continue
      fi
      stuck=1
      td_retain_failed "$name" || true
    done <<<"$TD_STACKS"
    [ "$stuck" = 1 ] || return 0
  done
  return 0
}

# ---- what the stacks did not own -------------------------------------------------------------------------------------------------

# td_leftovers: with no stack of the stage left, the log groups and the parameters in the inventory, and the stage buckets that no stack listed.
td_leftovers() {
  local g n b _batch count got log_n=0 par_n=0
  step "Removing what the stacks did not own"
  while IFS= read -r g; do
    [ -n "$g" ] || continue
    if ! printf '%s\n' "$TD_PLAN_LOGS" | grep -Fqx -- "$g"; then
      td_warn_once "late-log:$g" "the log group $g appeared after the plan was confirmed and is not deleted; run ./setup.sh --teardown again to see it in a plan"
      continue
    fi
    if td_try logs delete-log-group --log-group-name "$g"; then
      log_line "deleted the log group $g"
      log_n=$((log_n + 1))
    elif ! td_missing; then
      td_fail "could not delete the log group $g: ${AWS_ERR:-no answer}"
    fi
  done <<<"$TD_LOGS"
  _batch=""
  count=0
  for n in $(printf '%s' "$TD_PARAMS"); do
    if [ "$(td_slot_get log-keep)" = 1 ] && [ "$n" = "$(td_logging_param)" ]; then
      log_line "kept the parameter $n: the Bedrock model invocation logging setting was not put back"
      continue
    fi
    if ! printf '%s\n' "$TD_PLAN_PARAMS" | grep -Fqx -- "$n"; then
      td_warn_once "late-param:$n" "the parameter $n appeared after the plan was confirmed and is not deleted; run ./setup.sh --teardown again to see it in a plan"
      continue
    fi
    _batch="$_batch $n"
    count=$((count + 1))
    if [ "$count" -ge 10 ]; then
      # $_batch is up to ten names and is split on purpose
      if td_try ssm delete-parameters --names $_batch --query 'DeletedParameters[]' --output text; then
        got=$(td_count "$(td_words "$AWS_OUT")")
        par_n=$((par_n + got))
      else
        td_fail "could not delete parameters under $(td_param_root): ${AWS_ERR:-no answer}"
      fi
      _batch=""
      count=0
    fi
  done
  if [ -n "$_batch" ]; then
    if td_try ssm delete-parameters --names $_batch --query 'DeletedParameters[]' --output text; then
      got=$(td_count "$(td_words "$AWS_OUT")")
      par_n=$((par_n + got))
    else
      td_fail "could not delete parameters under $(td_param_root): ${AWS_ERR:-no answer}"
    fi
  fi
  for b in $TD_BUCKETS_ORPHAN; do
    if ! printf '%s\n' "$TD_PLAN_BUCKETS" | grep -Fqx -- "$b"; then
      td_warn_once "late-bucket:$b" "the bucket $b appeared after the plan was confirmed and is not touched; run ./setup.sh --teardown again to see it in a plan"
      continue
    fi
    say_log "Emptying bucket $b as $CALLER_ARN ... This removes every object version and delete marker."
    if td_empty_bucket "$b"; then
      td_try s3api delete-bucket --bucket "$b" --expected-bucket-owner "$ACCOUNT" || td_missing || td_fail "could not delete the bucket $b: ${AWS_ERR:-no answer}"
    else
      td_fail "could not empty the bucket $b: ${AWS_ERR:-no answer}"
    fi
  done
  say_log "$log_n log groups and $par_n SSM parameters with no owning stack have been deleted."
}

# td_enis <vpc>: sets TD_ENI_HELD (the number of interfaces AgentCore or another service holds in the VPC, or that are in use), TD_ENI_AGENTIC (how many of the interfaces have the type
# agentic_ai, the type of AgentCore's interfaces; the NAT gateway, endpoint and firewall interfaces of the network stack are requester-managed and have other types) and TD_ENI_FREE
# (ids of interfaces that are available and are not managed by a service). The call asks for managed resources too (--include-managed-resources); an AWS CLI that does not know the option is asked again
# without it, and the plan and the retained text say that managed interfaces may be hidden.
td_enis() {
  local v=$1 line id status managed type
  TD_ENI_HELD=0
  TD_ENI_AGENTIC=0
  TD_ENI_FREE=""
  if ! td_try ec2 describe-network-interfaces --include-managed-resources --filters "Name=vpc-id,Values=$v" --query 'NetworkInterfaces[].[NetworkInterfaceId,Status,RequesterManaged,InterfaceType]' --output text; then
    case $AWS_ERR in
      *"Unknown options"*|*"unrecognized arguments"*)
        TD_NO_MANAGED=1
        warn "this AWS CLI cannot list the network interfaces that AWS services manage (--include-managed-resources), so interfaces that AgentCore holds in $v may be hidden from this check"
        td_try ec2 describe-network-interfaces --filters "Name=vpc-id,Values=$v" --query 'NetworkInterfaces[].[NetworkInterfaceId,Status,RequesterManaged,InterfaceType]' --output text || return 1 ;;
      *) return 1 ;;
    esac
  fi
  while IFS= read -r line; do
    [ -n "$line" ] && [ "$line" != None ] || continue
    id=$(printf '%s' "$line" | cut -f1)
    status=$(printf '%s' "$line" | cut -f2)
    managed=$(printf '%s' "$line" | cut -f3)
    type=$(printf '%s' "$line" | cut -f4)
    [ "$type" != agentic_ai ] || TD_ENI_AGENTIC=$((TD_ENI_AGENTIC + 1))
    if [ "$status" = available ] && [ "$managed" != True ]; then
      TD_ENI_FREE="$TD_ENI_FREE$id$NL"
    else
      TD_ENI_HELD=$((TD_ENI_HELD + 1))
    fi
  done <<<"$AWS_OUT"
  return 0
}

# td_note_held_interfaces <vpc>: before the network stack is deleted, one line when AgentCore holds an interface (type agentic_ai) in its VPC, because CloudFormation then retries the
# subnet and the security group for about 20 minutes and ends the delete in DELETE_FAILED, and the screen would show nothing about why. It is information only: a read that fails costs
# the line, and the VPC is looked at once per run (a stack that fails is deleted again in later rounds).
td_note_held_interfaces() {
  local vpc=$1
  td_has "$TD_HELD_LOOKED" "$vpc" && return 0
  TD_HELD_LOOKED="$TD_HELD_LOOKED $vpc"
  if ! td_enis "$vpc"; then
    log_line "could not read the network interfaces of $vpc before the delete: ${AWS_ERR:-no answer}"
    return 0
  fi
  [ "$TD_ENI_AGENTIC" -gt 0 ] || return 0
  say_log "AgentCore holds $TD_ENI_AGENTIC network interface(s) in $vpc and may keep them for up to 8 hours after runtime deletion. CloudFormation can wait about 20 minutes and then report the network stack as DELETE_FAILED; teardown then retains and lists the network resources those interfaces hold."
}

# td_sweep_vpc <vpc id>: the resources of the VPC that carry the stage tags, then the VPC. The VPC is one that no stack owns and no stack of the stage is left (the caller
# checked both). Returns 1 when AgentCore still holds network interfaces in it (nothing is changed then), or when a delete failed.
td_sweep_vpc() {
  local v=$1 n bad=0 pass table assoc assocs main row looks left tagged _ids igws gw
  local tagf1="Name=tag:project,Values=$SETUP_PROJECT_TAG" tagf2="Name=tag:stage,Values=$STAGE"
  say_log "Removing the network resources of $v that carry the stage tags ..."
  if ! td_enis "$v"; then
    td_fail "could not list the network interfaces of $v: ${AWS_ERR:-no answer}"
    return 1
  fi
  if [ "$TD_ENI_HELD" -gt 0 ]; then
    td_note_retained "$v (VPC with its subnets and security groups; AgentCore holds $TD_ENI_HELD network interface(s) in it)"
    say "RETAINED $v: AgentCore holds $TD_ENI_HELD network interface(s) in it. AWS can hold them for up to 8 hours after runtime deletion. $(td_again_text)"
    [ "$TD_NO_MANAGED" != 1 ] || say "      This AWS CLI cannot show the interfaces that AWS services manage, so there may be more than this count."
    td_say_vpc_commands "$v"
    return 1
  fi
  if [ -n "$TD_ENI_FREE" ]; then
    tagged=""
    if td_try ec2 describe-network-interfaces --filters "Name=vpc-id,Values=$v" "$tagf1" "$tagf2" --query 'NetworkInterfaces[].NetworkInterfaceId' --output text; then
      tagged=$(td_words "$AWS_OUT")
    else
      td_fail "could not list the network interfaces that carry the stage tags of $v: ${AWS_ERR:-no answer}"
      return 1
    fi
    for n in $TD_ENI_FREE; do
      if printf '%s\n' "$tagged" | grep -Fqx -- "$n"; then
        td_try ec2 delete-network-interface --network-interface-id "$n" || { td_fail "could not delete the network interface $n: ${AWS_ERR:-no answer}"; bad=1; }
      else
        say "      network interface $n in $v does not carry the stage tags: not touched"
      fi
    done
  fi
  if td_try ec2 describe-vpc-endpoints --filters "Name=vpc-id,Values=$v" "$tagf1" "$tagf2" --query 'VpcEndpoints[].VpcEndpointId' --output text; then
    _ids=$(td_words "$AWS_OUT" | tr '\n' ' ')
    if [ -n "$_ids" ]; then
      # $_ids is the list of endpoint ids and is split on purpose
      if td_try ec2 delete-vpc-endpoints --vpc-endpoint-ids $_ids --query 'Unsuccessful[].[ResourceId,Error.Message]' --output text; then
        left=$(printf '%s\n' "$AWS_OUT" | awk 'NF && $0 != "None"')
        if [ -n "$left" ]; then td_fail "could not delete the VPC endpoints: $(printf '%s' "$left" | head -n 1 | tr '\t' ' ')"; bad=1; fi
      else
        td_fail "could not delete the VPC endpoints $_ids: ${AWS_ERR:-no answer}"
        bad=1
      fi
      # the delete is asynchronous: the subnets cannot go while the endpoints' interfaces are there
      looks=0
      while [ "$bad" = 0 ] && [ "$looks" -lt "${HAMH_SETUP_ENDPOINT_LOOKS:-60}" ]; do
        if ! td_try ec2 describe-vpc-endpoints --filters "Name=vpc-id,Values=$v" "$tagf1" "$tagf2" --query 'VpcEndpoints[].VpcEndpointId' --output text; then
          td_fail "could not list the VPC endpoints of $v: ${AWS_ERR:-no answer}"
          return 1
        fi
        [ -n "$(td_words "$AWS_OUT")" ] || break
        looks=$((looks + 1))
        setup_sleep
      done
    fi
  else
    td_fail "could not list the VPC endpoints of $v: ${AWS_ERR:-no answer}"
    bad=1
  fi
  if td_try ec2 describe-subnets --filters "Name=vpc-id,Values=$v" "$tagf1" "$tagf2" --query 'Subnets[].SubnetId' --output text; then
    for n in $(td_words "$AWS_OUT"); do
      td_try ec2 delete-subnet --subnet-id "$n" || { td_fail "could not delete the subnet $n: ${AWS_ERR:-no answer}"; bad=1; }
    done
  else
    td_fail "could not list the subnets of $v: ${AWS_ERR:-no answer}"
    bad=1
  fi
  # security groups can refer to each other: a second pass deletes the ones the first left
  for pass in 1 2; do
    if ! td_try ec2 describe-security-groups --filters "Name=vpc-id,Values=$v" "$tagf1" "$tagf2" --query "SecurityGroups[?GroupName!='default'].GroupId" --output text; then
      td_fail "could not list the security groups of $v: ${AWS_ERR:-no answer}"
      bad=1
      break
    fi
    for n in $(td_words "$AWS_OUT"); do
      if ! td_try ec2 delete-security-group --group-id "$n" && [ "$pass" = 2 ]; then
        td_fail "could not delete the security group $n: ${AWS_ERR:-no answer}"
        bad=1
      fi
    done
  done
  # the internet gateways of the stage are read first: a route table association with one of them is the stage's to remove
  igws=""
  if td_try ec2 describe-internet-gateways --filters "Name=attachment.vpc-id,Values=$v" "$tagf1" "$tagf2" --query 'InternetGateways[].InternetGatewayId' --output text; then
    igws=$(td_words "$AWS_OUT")
  else
    td_fail "could not list the internet gateways of $v: ${AWS_ERR:-no answer}"
    bad=1
  fi
  # route tables of the stage: the association of a subnet of the stage went with the subnet, so what is left is somebody else's and is not removed (the delete then fails and says so);
  # an association with an internet gateway of the stage is removed first
  if td_try ec2 describe-route-tables --filters "Name=vpc-id,Values=$v" "$tagf1" "$tagf2" --query 'RouteTables[].RouteTableId' --output text; then
    for table in $(td_words "$AWS_OUT"); do
      main=0
      assocs=""
      if td_try ec2 describe-route-tables --route-table-ids "$table" --query 'RouteTables[0].Associations[].[RouteTableAssociationId,Main,GatewayId]' --output text; then
        while IFS= read -r row; do
          [ -n "$row" ] && [ "$row" != None ] || continue
          gw=$(printf '%s' "$row" | cut -f3)
          if [ "$(printf '%s' "$row" | cut -f2)" = True ]; then
            main=1
          elif [ "$gw" != None ] && printf '%s\n' "$igws" | grep -Fqx -- "$gw"; then
            assocs="$assocs $(printf '%s' "$row" | cut -f1)"
          fi
        done <<<"$AWS_OUT"
      else
        td_fail "could not read the associations of the route table $table: ${AWS_ERR:-no answer}"
        bad=1
        continue
      fi
      [ "$main" = 0 ] || continue
      for assoc in $assocs; do
        td_try ec2 disassociate-route-table --association-id "$assoc" || { td_fail "could not remove the association $assoc of the route table $table: ${AWS_ERR:-no answer}"; bad=1; }
      done
      td_try ec2 delete-route-table --route-table-id "$table" || { td_fail "could not delete the route table $table: ${AWS_ERR:-no answer}. If an association of this route table does not belong to the stage, teardown did not remove it."; bad=1; }
    done
  else
    td_fail "could not list the route tables of $v: ${AWS_ERR:-no answer}"
    bad=1
  fi
  for n in $igws; do
    td_try ec2 detach-internet-gateway --internet-gateway-id "$n" --vpc-id "$v" &&
      td_try ec2 delete-internet-gateway --internet-gateway-id "$n" || { td_fail "could not delete the internet gateway $n: ${AWS_ERR:-no answer}"; bad=1; }
  done
  if [ "$bad" = 0 ] && td_try ec2 delete-vpc --vpc-id "$v"; then
    ok "VPC $v removed"
    return 0
  fi
  if [ "$bad" = 0 ]; then
    td_fail "could not delete the VPC $v: ${AWS_ERR:-no answer}. If a resource in it does not carry the stage tags, teardown treats it as outside the stage and does not touch it. You can remove that resource yourself or leave the VPC."
  fi
  td_say_vpc_commands "$v"
  return 1
}

# td_sweep_network: the VPCs of the stage that no stack owns any more (an earlier teardown retained them because AgentCore held network interfaces in them). It runs only
# when no stack of the stage is left (a stack that cannot be deleted may still own its VPC); TD_VPCS holds only VPCs that no existing stack owns.
td_sweep_network() {
  local v started=0
  td_no_stacks || return 0
  [ -n "$TD_VPCS" ] || return 0
  for v in $TD_VPCS; do
    # this run retained it a moment ago because AgentCore holds its interface: it is reported, and the next run sweeps it
    td_has "$TD_RETAINED_VPCS" "$v" && continue
    if [ "$started" = 0 ]; then
      step "Removing the network resources an earlier teardown retained"
      started=1
    fi
    if ! printf '%s\n' "$TD_PLAN_VPCS" | grep -Fqx -- "$v"; then
      td_warn_once "late-vpc:$v" "the VPC $v appeared after the plan was confirmed and is not touched; run ./setup.sh --teardown again to see it in a plan"
      continue
    fi
    td_sweep_vpc "$v" || true
  done
}

# ---- the report -------------------------------------------------------------------------------------------------------------------

# td_report_pending: the keys and secrets CloudFormation scheduled for deletion, with their dates (they are not gone yet, and are said not to be). A key or secret whose state
# cannot be read is said and counted.
# td_await_secret <secret id>: the secret that was deleted without a recovery window stays described ("marked for deletion") for about 30 seconds. The result waits until it is not
# found (HAMH_SETUP_SECRET_LOOKS looks, 20 by default, setup_sleep apart), because its fixed name is free for a new deploy only then.
td_await_secret() {
  local looks=0 unmarked=0 name max date
  # the name comes from the secret's own id (<name>-<six characters>), not from the generation in use: the keys and secrets of every pass are reported together
  name=${1#*:secret:}
  name=${name%-??????}
  max=$(setup_number "${HAMH_SETUP_SECRET_LOOKS:-20}" 20)
  [ "$max" -gt 0 ] || max=1
  while :; do
    if td_try secretsmanager describe-secret --secret-id "$1" --query '[Name,DeletedDate]' --output text; then
      looks=$((looks + 1))
      date=$(printf '%s' "$AWS_OUT" | cut -f2)
      if [ -z "$date" ] || [ "$date" = None ]; then
        # described and not marked for deletion: it may take a moment for the mark to show, so three looks in a row decide that the delete did not take (it was restored, or the call went to another secret)
        unmarked=$((unmarked + 1))
        if [ "$unmarked" -ge 3 ] || [ "$looks" -ge "$max" ]; then
          say "LEFT: secret $name still exists; the delete without a recovery window did not mark it for deletion"
          TD_LEFT_N=$((TD_LEFT_N + 1))
          return 0
        fi
        setup_sleep
        continue
      fi
      unmarked=0
      if [ "$looks" -ge "$max" ]; then
        # its fixed name is not free for a new deploy yet, so the stage is not clean
        say "LEFT: secret $name is still marked for deletion after $looks looks. A new deploy cannot create it until AWS has finished. Run ./setup.sh --teardown again later: it reads the account again."
        TD_LEFT_N=$((TD_LEFT_N + 1))
        return 0
      fi
      setup_sleep
    elif td_missing; then
      say "deleted: secret $name (without a recovery window; its name is free for a new deploy)"
      return 0
    else
      td_fail "could not read the state of the secret $1: ${AWS_ERR:-no answer}"
      return 0
    fi
  done
}

td_report_pending() {
  local k s state date name
  for k in $(printf '%s' "$TD_KEYS" | sort -u); do
    if td_try kms describe-key --key-id "$k" --query 'KeyMetadata.[KeyState,DeletionDate]' --output text; then
      state=$(printf '%s' "$AWS_OUT" | cut -f1)
      date=$(printf '%s' "$AWS_OUT" | cut -f2)
      case $state in
        PendingDeletion) say "pending: KMS key $k has deletion scheduled for $(td_date_only "$date"). AWS deletes a key after a waiting period of 7 to 30 days." ;;
        *) say "LEFT: KMS key $k is $state; the stack did not delete it and nothing has scheduled its deletion"; TD_LEFT_N=$((TD_LEFT_N + 1)) ;;
      esac
    elif ! td_missing; then
      td_fail "could not read the state of the KMS key $k: ${AWS_ERR:-no answer}"
    fi
  done
  for s in $(printf '%s' "$TD_SECRETS_FORCED" | sort -u); do
    td_await_secret "$s"
  done
  for s in $(printf '%s' "$TD_SECRETS" | sort -u); do
    if td_try secretsmanager describe-secret --secret-id "$s" --query '[Name,DeletedDate]' --output text; then
      name=$(printf '%s' "$AWS_OUT" | cut -f1)
      date=$(printf '%s' "$AWS_OUT" | cut -f2)
      if [ "$date" = None ] || [ -z "$date" ]; then
        say "LEFT: secret $name still exists; the stack did not delete it and nothing has scheduled its deletion"
        TD_LEFT_N=$((TD_LEFT_N + 1))
      else
        say "pending: secret $name has deletion scheduled for $(td_date_only "$date")"
      fi
    elif ! td_missing; then
      td_fail "could not read the state of the secret $s: ${AWS_ERR:-no answer}"
    fi
  done
}

td_tag_index() {
  local n
  td_try resourcegroupstaggingapi get-resources --tag-filters "Key=project,Values=$SETUP_PROJECT_TAG" "Key=stage,Values=$STAGE" --query 'ResourceTagMappingList[].ResourceARN' --output text || {
    warn "could not read the tag index (it is information only, and nothing above depends on it): ${AWS_ERR:-no answer}"
    return 0
  }
  n=$(td_words "$AWS_OUT" | wc -l | tr -d ' ')
  [ "$n" -gt 0 ] || return 0
  say "The tag index lists $n resource(s) of the stage for information only. It can lag behind a delete by hours. It lists a key or secret until AWS deletes it:"
  td_words "$AWS_OUT" | head -n 10 | while IFS= read -r line; do say "  $line"; done
}

td_state_begin() {
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
  log_line "setup teardown: account $ACCOUNT Region $REGION stage $STAGE identity $CALLER_ARN covering $(td_covers_text)"
  [ -z "$PROFILE_EFF" ] || st_set profile "$PROFILE_EFF" || true
  st_set state teardown-started || true
  st_save || warn "could not write the receipt $ST_FILE"
  say "Log of this run: $RUN_LOG"
}

# td_say_not_touched: what the inventory found and did not touch, as kept: lines (they never keep the result from being zero: nothing shows that they are the stage's). Stacks,
# runtimes and buckets are named; parameters and log groups are counted.
td_say_not_touched() {
  local line n
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    say "kept: stack ${line%%"$TAB"*} was not touched ($(printf '%s' "$line" | cut -f3))"
  done <<<"$TD_FOREIGN"
  [ "$TD_RUNNER_OTHER" != 1 ] || say "kept: stack $SETUP_RUNNER_STACK was not touched (it does not carry the tag project=$SETUP_PROJECT_TAG, or its SetupBucket output is not $GEN_SETUP-$ACCOUNT-$REGION)"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    say "kept: AgentCore runtime ${line##*"$TAB"} was not touched (no stack of the stage lists it and it carries no stage tags)"
  done <<<"$TD_RUNTIMES_OTHER"
  for n in $TD_BUCKETS_OTHER; do say "kept: bucket $n was not touched (no stack of the stage lists it and it carries no stage tags)"; done
  n=$(td_count "$TD_PARAMS_OTHER")
  [ "$n" = 0 ] || say "kept: $n SSM parameter(s) under $GEN_ROOT/$STAGE/ without the stage tags were not touched"
  n=$(td_count "$TD_LOGS_KEPT")
  [ "$n" = 0 ] || say "kept: $n log group(s) under the stage's prefixes were not touched because the function, project or runtime that writes to them exists"
}

# td_plan_freeze: keeps the plan of the generation in use as the limit of its pass. The lists are the ones the plan showed (TD_PLAN_STACKS and the others, set by the caller).
td_plan_freeze() {
  td_slot_set plan-stacks "$TD_PLAN_STACKS"
  td_slot_set plan-logs "$TD_PLAN_LOGS"
  td_slot_set plan-params "$TD_PLAN_PARAMS"
  td_slot_set plan-vpcs "$TD_PLAN_VPCS"
  td_slot_set plan-buckets "$TD_PLAN_BUCKETS"
  td_slot_set plan-runner "$RN_EXISTS"
}

# td_plan_thaw: the frozen plan of the generation in use is the limit again.
td_plan_thaw() {
  TD_PLAN_STACKS=$(td_slot_get plan-stacks)
  TD_PLAN_LOGS=$(td_slot_get plan-logs)
  TD_PLAN_PARAMS=$(td_slot_get plan-params)
  TD_PLAN_VPCS=$(td_slot_get plan-vpcs)
  TD_PLAN_BUCKETS=$(td_slot_get plan-buckets)
}

# td_verdict_gen: the lines of the result about the generation in use, from the last read of the account: the stacks, the runner, the VPCs, runtimes, buckets, log groups and
# parameters that are still there. It returns 1 when something is left. It reads and says; the counters of the run belong to the caller.
td_verdict_gen() {
  local left=0 stacks_left line name status why v vpcs_left note="" runner_why runner_command
  case $GEN in
    current) ;;
    *) note=" under $GEN_LABEL" ;;
  esac
  stacks_left=$(td_count "$TD_STACKS")
  if [ "$stacks_left" -gt 0 ]; then
    left=1
    say "STACKS LEFT: $stacks_left stack(s) of stage $STAGE could not be deleted$note:"
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      name=${line%%"$TAB"*}
      status=${line#*"$TAB"}
      why=$(printf '%s' "$TD_REASONS" | awk -F'\t' -v n="$name" '$1 == n { r = $2 } END { print r }')
      say "  stack $name ($status)${why:+: $why}"
    done <<<"$TD_STACKS"
  fi
  if [ "$RN_EXISTS" = 1 ]; then
    left=1
    runner_why=$(td_slot_get runner-why)
    runner_command=$(td_slot_get runner-command)
    why="LEFT: stack $SETUP_RUNNER_STACK is still there"
    if [ -n "$runner_why" ]; then why="$why: $runner_why"; fi
    if [ -n "$runner_command" ]; then why="$why. You can remove it, in account $ACCOUNT and Region $REGION, with: $runner_command"; fi
    say "$why"
  fi
  if [ -n "$TD_VPCS$TD_RUNTIMES$TD_RUNTIMES_ORPHAN$TD_BUCKETS_ORPHAN$TD_LOGS$TD_PARAMS" ]; then
    left=1
    [ -z "$TD_VPCS" ] || for v in $TD_VPCS; do say "LEFT: VPC $v with the project and stage tags is still there"; done
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      say "LEFT: AgentCore runtime ${line##*"$TAB"} (id $(printf '%s' "$line" | cut -f1)) is still there"
    done <<<"$TD_RUNTIMES$NL$TD_RUNTIMES_ORPHAN"
    for v in $TD_BUCKETS_ORPHAN; do say "LEFT: bucket $v is still there"; done
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      say "LEFT: log group $line is still there"
    done <<<"$TD_LOGS"
    for v in $(printf '%s' "$TD_PARAMS"); do say "LEFT: SSM parameter $v is still there"; done
  fi
  vpcs_left=$(td_vpcs_left)
  if [ -n "$vpcs_left" ]; then
    left=1
    td_say_vpcs_left "$vpcs_left"
  fi
  return "$left"
}

run_teardown() {
  local left=0 stacks_left=0 stacks_gen line name g verdict verdict_rc vpcs_left_all="" skipped_note=""
  FULL_ACTION=teardown
  TD_RETAINED=""
  TD_RETAINED_N=0
  TD_FAILED=0
  TD_FAILED_SEEN=""
  TD_WARNED_SEEN=""
  TD_UNREAD_RUN=""
  TD_LEFT_N=0
  TD_FORCE_SECRETS=""
  TD_SECRETS_FORCED=""
  TD_RETAINED_VPCS=""
  TD_RETAINED_RES=""
  TD_LOG_MODE=""
  TD_LOG_KEEP=0
  TD_PLAN_CUT=0
  TD_FOUND_GENS=""
  TD_ANY_STACKS=0
  TD_ANY_VPCS=0
  TD_CLI_CHECKED=0
  TD_YES_GIVEN=$YES
  if [ "$YES" = 1 ] && [ "${ACK_TEARDOWN:-0}" = 1 ] && [ "$DRY" != 1 ] && [ -z "${ACCOUNT_ID_OPT:-}" ]; then
    setup_exit 4 "--yes --acknowledge-teardown deletes without a question, so it needs --account-id <your 12-digit account id> to be sure of the account" \
      "Add the 12-digit id of the account you mean:  ./setup.sh --teardown --yes --acknowledge-teardown --account-id <account id>" "Nothing was deleted."
  fi
  pf_region
  pf_profile
  pf_aws_cli
  pf_identity
  pf_bootstrap_version
  td_cover_begin
  # the first part only reads: each covered generation is read with its own names, and a generation that holds something is planned and its plan is kept as the limit of its pass
  TD_UNREAD=""
  TD_CATS_OK=""
  TD_CATS_BAD=""
  for g in $TD_COVER; do
    td_use_generation "$g"
    td_inventory keep
    [ "$TD_STACKS_LISTED" = 1 ] || setup_exit 1 "could not list the CloudFormation stacks: ${AWS_ERR:-no answer}" "Check that your credentials may call cloudformation:ListStacks." "Nothing was deleted."
    td_slot_set kept "$(td_say_not_touched)"
    vpcs_left_all="$vpcs_left_all$(td_vpcs_left)$NL"
    td_found || continue
    if [ -z "$TD_FOUND_GENS" ]; then td_plan_head; fi
    TD_FOUND_GENS="$TD_FOUND_GENS $g"
    if [ -n "$TD_STACKS" ] && [ "$TD_CLI_CHECKED" = 0 ]; then
      td_check_cli
      TD_CLI_CHECKED=1
    fi
    td_busy
    td_check_go_store
    td_read_logging
    td_plan
    td_plan_mark
    # the confirmed plan is the boundary of what is deleted: what shows up later is reported and left
    TD_PLAN_STACKS=$(printf '%s' "$TD_STACKS" | cut -f1 | tr '\n' ' ')
    TD_PLAN_LOGS=$TD_LOGS
    TD_PLAN_PARAMS=$TD_PARAMS
    TD_PLAN_VPCS=$TD_VPCS
    TD_PLAN_BUCKETS=$TD_BUCKETS_ORPHAN
    td_plan_freeze
    td_slot_set would "$(td_would_gen)"
  done
  if [ -z "$TD_FOUND_GENS" ]; then
    if [ -n "$TD_UNREAD_RUN" ]; then
      step "Teardown could not check the stage"
      td_list_unread
      say "TEARDOWN-INVENTORY INCOMPLETE stage=$STAGE: $(td_count "$TD_UNREAD_RUN") read(s) failed, so nothing is said about what is left"
      say "checked: $(td_checked_sentence)."
      exit 8
    fi
    if [ -n "$(printf '%s' "$vpcs_left_all" | awk 'NF')" ]; then
      step "Nothing the teardown may delete"
      say "Stage $STAGE has no stack, no runner and no other resource in account $ACCOUNT, Region $REGION that the teardown may delete."
      td_say_vpcs_left "$vpcs_left_all"
      say "TEARDOWN-INVENTORY INCOMPLETE stage=$STAGE: see the lines above"
      say "checked: $(td_checked_sentence)."
      for g in $TD_COVER; do
        td_use_generation "$g"
        td_slot_get kept
      done
      exit 8
    fi
    step "Nothing to tear down"
    say "No stack, runner, retained resource or bucket of the stage exists under $(td_covers_text) in account $ACCOUNT, Region $REGION, so there is nothing to tear down."
    say "checked: $(td_checked_sentence)."
    if [ -n "$(td_skipped_gen)" ]; then
      say "The generation using $(td_gen_label "$(td_skipped_gen)") is excluded by --only and is not read."
    fi
    for g in $TD_COVER; do
      td_use_generation "$g"
      td_slot_get kept
    done
    [ -z "$BOOT_VERSION" ] || say "kept: the CDK bootstrap stack $SETUP_BOOTSTRAP_STACK (other CDK projects of the account may use it; remove it with: aws cloudformation delete-stack --stack-name $SETUP_BOOTSTRAP_STACK)"
    exit 0
  fi
  td_plan_tail
  if [ -n "$TD_UNREAD_RUN" ]; then
    warn "some reads failed, so the plan may be incomplete:"
    td_list_unread
  fi
  if [ "$DRY" = 1 ]; then
    step "Dry run"
    for g in $TD_FOUND_GENS; do
      td_use_generation "$g"
      td_slot_get would
    done
    if [ -n "$TD_UNREAD_RUN" ]; then
      say "TEARDOWN-INVENTORY INCOMPLETE stage=$STAGE: the plan above is not complete because $(td_count "$TD_UNREAD_RUN") read(s) failed"
      exit 8
    fi
    exit 0
  fi
  if [ "$YES" = 1 ]; then
    [ "${ACK_TEARDOWN:-0}" = 1 ] || setup_exit 4 "--yes does not confirm a teardown" "Add --acknowledge-teardown and --account-id to delete without a question:  ./setup.sh --teardown --yes --acknowledge-teardown --account-id <account id>" "Nothing was deleted."
  elif [ "$NO_INPUT" = 1 ]; then
    setup_exit 4 "--no-input was given, so there is no one to ask" "Add --yes --acknowledge-teardown --account-id <account id> to go ahead." "Nothing was deleted."
  elif [ -t 0 ]; then
    printf 'Delete these resources? [y/N] '
    IFS= read -r ANSWER || { printf '\n'; setup_exit 4 "the input ended before you confirmed. Nothing was deleted."; }
    case $ANSWER in
      y|Y|yes|YES) log_line "confirmed at the prompt" ;;
      *) setup_exit 4 "stopped at your request. Nothing was deleted." ;;
    esac
  else
    setup_exit 4 "there is no terminal to ask on" "Add --yes --acknowledge-teardown --account-id <account id> to go ahead without a prompt." "Nothing was deleted."
  fi
  YES=1
  td_state_begin
  # the second part deletes, one generation after the other, with the same functions: the lists of its own read are cut to the plan the generation was confirmed with
  for g in $TD_FOUND_GENS; do
    td_use_generation "$g"
    td_plan_thaw
    TD_PLAN_CUT=1
    TD_PROVIDER_READ=0
    TD_PROVIDER=""
    td_inventory
    td_check_go_store
    td_read_logging
    if [ "$RN_EXISTS" = 1 ] && [ "$(td_slot_get plan-runner)" != 1 ]; then
      td_warn_once "late-runner:$SETUP_RUNNER_STACK" "Runner stack $SETUP_RUNNER_STACK appears after plan confirmation and is not touched. Run ./setup.sh --teardown again to see it in a plan."
      RN_EXISTS=0
    fi
    if [ "$TD_FOUND_GENS" != " $g" ]; then step "The delete pass starts for the generation using $GEN_LABEL."; fi
    # the runner goes first: its authority to assume the CDK roles ends with the stack, and nothing it holds is needed to delete the stage
    RN_LEFT_WHY=""
    RN_LEFT_COMMAND=""
    if [ "$RN_EXISTS" = 1 ]; then
      step "Removing the runner"
      # the runner's key is scheduled for deletion with its stack and gets a pending: line like the keys of the stage
      if td_read runner "the keys of the runner stack" cloudformation list-stack-resources --stack-name "$SETUP_RUNNER_STACK" \
          --query "StackResourceSummaries[?ResourceType=='AWS::KMS::Key'].PhysicalResourceId" --output text; then
        TD_KEYS="$TD_KEYS$(td_words "$AWS_OUT")$NL"
      fi
      rn_remove || TD_FAILED=$((TD_FAILED + 1))
    fi
    td_slot_set runner-why "$RN_LEFT_WHY"
    td_slot_set runner-command "$RN_LEFT_COMMAND"
    # the setting names a log group that goes with the stage's stacks, so it is put back before the first stack is deleted
    td_restore_logging
    # whether the record stays is decided for the generation that was restored: the sweep at the end runs after the passes of both, and one global flag would hold the last one's answer
    td_slot_set log-keep "$TD_LOG_KEEP"
    if [ -n "$TD_STACKS" ]; then
      td_quiesce
      step "Deleting the stacks"
      say "Deleting the stacks takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface. A line shows what the program is waiting for every 3 minutes."
      td_delete_stacks
      td_retain_stuck
      td_force_gone_secrets
      td_retained_commands
    fi
  done
  td_force_gone_secrets_all
  # read the account again: what is left is what the account says, not what this script did. Network resources, log groups and parameters go only when no stack of the
  # generation is left, and then the account is read once more for its result.
  TD_PLAN_CUT=0
  TD_UNREAD=""
  TD_CATS_OK=""
  TD_CATS_BAD=""
  for g in $TD_COVER; do
    td_use_generation "$g"
    td_plan_thaw
    td_inventory keep
    if td_no_stacks && td_has "$TD_FOUND_GENS" "$g"; then
      td_sweep_network
      td_leftovers
      td_inventory keep
    fi
    verdict=$(td_verdict_gen; printf 'VERDICT-RC=%s' "$?")
    verdict_rc=${verdict##*VERDICT-RC=}
    verdict=${verdict%VERDICT-RC=*}
    td_slot_set verdict "$verdict"
    [ "$verdict_rc" = 0 ] || left=1
    stacks_gen=$(td_count "$TD_STACKS")
    stacks_left=$((stacks_left + stacks_gen))
    td_slot_set kept "$(td_say_not_touched)"
  done
  if [ -n "$(td_skipped_gen)" ]; then skipped_note=", and the generation using $(td_gen_label "$(td_skipped_gen)") (excluded by --only)"; fi
  step "Teardown result"
  # these two read the account too, and a read that fails is a failure: they come before the verdict
  td_report_pending
  for g in $TD_COVER; do
    td_use_generation "$g"
    td_tag_index
  done
  for g in $TD_COVER; do
    td_use_generation "$g"
    td_slot_get verdict
  done
  [ "$TD_FAILED" = 0 ] || left=1
  if [ -n "$TD_RETAINED" ]; then
    left=1
    st_set retained "$TD_RETAINED" || true
  fi
  if [ -n "$TD_UNREAD_RUN" ]; then
    left=1
    td_list_unread
  fi
  if [ "$TD_LEFT_N" -gt 0 ]; then
    left=1
  fi
  if [ "$left" = 0 ]; then
    st_set retained "" || true
    st_set state torn-down || true
  else
    st_set state teardown-incomplete || true
  fi
  st_save || true
  if [ -n "$TD_RETAINED" ] && [ "$TD_FAILED" = 0 ] && [ -z "$TD_UNREAD_RUN" ] && [ "$TD_LEFT_N" = 0 ] && [ "$stacks_left" = 0 ]; then
    say "TEARDOWN-INVENTORY RETAINED $TD_RETAINED_N resource(s) in stage $STAGE; they are not zero resources and stay until AWS lets them go (up to 8 hours)"
    say "$(td_again_text)"
    say "checked: $(td_checked_sentence)."
  elif [ "$left" = 1 ]; then
    say "TEARDOWN-INVENTORY INCOMPLETE stage=$STAGE: see the lines above"
    [ -z "$TD_RETAINED" ] || say "RETAINED $TD_RETAINED_N resource(s) also stay until AWS lets them go (up to 8 hours). $(td_again_text)"
    say "checked: $(td_checked_sentence)."
  else
    say "TEARDOWN-INVENTORY 0 stacks stage=$STAGE runner=none"
    say "checked: $(td_checked_sentence). Not checked: untagged resources, IAM roles outside the stacks, and the files and images in the CDK bootstrap bucket$skipped_note."
  fi
  for g in $TD_COVER; do
    td_use_generation "$g"
    td_slot_get kept
  done
  [ -z "$BOOT_VERSION" ] || say "kept: the CDK bootstrap stack $SETUP_BOOTSTRAP_STACK (updates need it, and other CDK projects of the account may use it; remove it with: aws cloudformation delete-stack --stack-name $SETUP_BOOTSTRAP_STACK)"
  [ -z "$BOOT_VERSION" ] || say "kept: the CDK bootstrap bucket and container repository, including the files and images the deploy published for the stage"
  if [ -n "$(ham_now)" ]; then
    say "kept: the client at $(ham_now) (setup.sh deletes none of your files)"
  fi
  say "kept: the receipt $ST_FILE and the logs beside it"
  [ "$left" = 0 ] || exit 8
  exit 0
}
