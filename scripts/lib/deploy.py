"""scripts/deploy.sh: deploy the committed HEAD to stage `direct`, stack by stack.

Target: the entry of the deployed tree's accounts.json that HAMH_AWS_PROFILE names by
profile, else the active one (scripts/lib/aws_target.py); an unknown profile is a usage error (exit 2).
Its account, home Region and profile reach every AWS call (AWS_PROFILE is always its profile), bucket,
ECR URI, ARN and record; the deploy record's inputs.target names it.

The account file: HAMH_ACCOUNTS_FILE, an absolute path, replaces the tree's accounts.json for the whole deploy: this
program, every cargo build (harness-protocol's build script), every cdk call (scripts/node.sh mounts the file into the container) and every
CodeBuild image build (scripts/codebuild-run.sh puts it into the source bundle as a member of its own) read it. Unset or empty, the tree's
committed accounts.json is read and nothing below changes. The file in use is a separate input from the source: `sourceTree` is still the hash
of the committed deployable paths, and the deploy record and every image-build record carry `deploymentConfigDigest` beside it (`committed`, or
the SHA-256 of the file in use), also kept in SSM /hamh/<stage>/deployment-config-digest. A deploy whose digest differs from that earlier record
says so before it builds anything (`deploy: deployment config <new> differs from the earlier record <old> ...`); it reports, it does not stop.

Flow (every step prints `deploy: ...`; failures end with `DEPLOY-FAIL <step> <detail>`, exit 1; a refusal before the first stack change ends with
`DEPLOY-REFUSED reason=<id> wait=<yes|no> stage=unchanged :: <action>`, exit 3, see "Refusals" below):
1. Refuse bad flags, a stage setting whose value its registry pattern does not take, and uncommitted tracked changes (exit 2) before any AWS
   call; the caller must be the
   target account (`account`) and, when the entry names an `operator`, that identity or the setup
   runner's role (`caller`: the refusal reads `<arn> is not <kind>/<name>...` and names both; an entry without the field is checked
   by account alone, as it always was); the stage AZ name must map to its accounts.json AZ ID in this account
   (`azs`, DescribeAvailabilityZones); the stage's VPC CIDR (10.50.0.0/16) must overlap no other VPC of the account
   (`vpc-cidr`). Each of the four is a refusal: `DEPLOY-REFUSED reason=account|caller|azs|vpc-cidr ...`, exit 3 ("Refusals" below). Nothing is built
   before these pass. Right after these checks and the owner (1a) the deploy takes the deploy lock or checks the
   caller's, before it reads the stage's records and settings.
1a. The owner of the stage (`settle_owner`, right after the AZ and VPC checks, before the first record of the stage is read): the `owner` tag of the network stack, else of the control stack (a stack that rolled back on its
   first create is ignored) is the owner of this deploy unless HAMH_OWNER or the entry's `owner` names one. A named owner that differs, or a tag that is no owner value, is refused
   (`DEPLOY-REFUSED reason=owner ...`, exit 3) before anything is built or changed, because a changed `owner` tag replaces the network stack's DNS query logging configuration and the
   update rolls back (AWS::Route53Resolver::ResolverQueryLoggingConfig, Tags: "Update requires: Replacement"). Without a named owner the stage's own is kept and
   `deploy: owner <value> kept from the stage` is printed; a stage that does not exist gets the derived or named value. The result reaches every cdk call as HAMH_OWNER, and the
   resources this deploy tags by hand.
1b. The stage settings: each setting of scripts/lib/stage-settings.tsv is resolved, printed on a
   `DEPLOY-SETTING` line and handed to every cdk call, before anything is built.
2. Build the Lambdas (LAMBDAS: worker-api, control-api, run-session, research-api, research-job, plus the delivery package, one cargo lambda arm64 call)
   for the selection, in its own directories: CARGO_TARGET_DIR is
   <repo>/target/deploy/<account>-<region> and the bootstraps go to <that>/lambda/<stage> (`--lambda-dir`);
   HAMH_LAMBDA_DIR points there. The builds also get HAMH_AWS_PROFILE=<the selected profile>, which the Rust build
   compiles in (harness-protocol's build script), whatever the caller exported. Before anything is uploaded or handed to
   CDK, every build is checked (`deploy: build selection ok <sel>: lambda bytes=<binaries> outputs=<n>`): repo-gateway
   must embed the selected account id, agentd the selected Region, and the harness-protocol build script output in the
   target directory must name the selected entry; else `DEPLOY-FAIL build-selection <binary> built for <sel>, target is
   <sel>: ...` (exit 1) with nothing published. Install infra/ dependencies from the lockfile (`pnpm install
   --frozen-lockfile`), so a fresh checkout deploys.
3. Stack order: `cdk synth`, then cdk.out/manifest.json dependencies, ties broken by DEPLOY_PRIORITY.
3a. CDK bootstrap (`bootstrap`): SSM /cdk-bootstrap/hnb659fds/version at or above the assembly's highest
   requiresBootstrapStackVersion (MIN_BOOTSTRAP when none) is used unchanged; a lower one is refused, since an
   existing CDKToolkit is never modified; none: `cdk bootstrap aws://<account>/<region>` with the tags
   project, stage=shared, owner and managed-by=deploy.sh, then the version is read again. The version is read after the synth; the
   bootstrap is created only after every check that can refuse (the image guard and the fork pointer below) has passed.
4. Worker image, tagged `in-<image key>` (the key covers the crate closure of agentd and the IMAGE_PATHS of lib/digests.py):
   reused when ECR has the tag. Otherwise: a fresh stage (no ECR repository) first gets a phase-1 pass of every stack without
   an image (the runtimes stack holds the image-build project). Then agentd is cross-built in one zigbuild through
   scripts/lib/agentd_build.py (its argv, the CARGO_HOME remap and the selection environment; the image-build record carries
   the recipe digest; CARGO_TARGET_DIR as in step 2: the binary is read from <target dir>/aarch64-unknown-linux-gnu/release),
   checked as in step 2 (agentd must embed the selected Region; `deploy: build selection ok <sel>: worker...`), uploaded to
   builds/out/agentd/<sha>/agentd, and scripts/codebuild-run.sh builds the image with its SHA-256 (AGENTD_SHA256) and
   LANG_FAMILY=direct.
   Before the first image that COPYs the Cline fork (CLINE_SHA256), the fork binary is settled by
   `--fork-source auto|build|copy` (default auto), and the image is built with the SHA-256 of the fork that was settled:
   - the lock's object: s3://<bucket>/engine/cline/<lock binarySha256>/cline with that SHA-256 (its sha256 metadata, or the hash of
     its bytes when it has none), used in auto and copy mode when present;
   - the pointer of a source build of this stage, s3://<bucket>/engine/cline/by-tree/<series tree>-<toolchain digest>.json, which names
     the fork a source build recorded for the same series tree and toolchain pins (auto and build mode); a pointer that names a missing
     or other object is fatal, and a deploy of a changed image reuses the recorded fork and never builds it again;
   - the engine/lock.toml `artifact`, copied with the profile of the accounts entry whose stage bucket holds it, in auto mode only when
     that profile is configured on this host (`aws configure list-profiles`) and in copy mode always; a copy that fails is fatal, never
     a build, and a copy with another SHA-256 is fatal;
   - a build on CodeBuild ARM from the pinned upstream source (images/codebuild/cline-fork-arm64.yml), in build mode, and in auto
     mode when nothing above answered. Builds are not bit-reproducible, so it is accepted on the evidence of the build itself, not on
     equality with the lock's SHA-256: the metadata and the record the build computed must show the locked commit, series tree and
     toolchain pins, an aarch64 ELF, telemetry off, leaks 0, a frozen lockfile and the suites run; the smoke test names the locked
     version; the CodeBuild build id equals the id this deploy started; and the SHA-256 the build computed equals the SHA-256 of the
     bytes downloaded. The binary is promoted to engine/cline/<its SHA-256>/cline with a create-only put and the pointer is written
     create-only; a lost pointer race uses the winner's fork. The deploy record names the lock's SHA-256 as a reference
     (`forkBinary.lockBinarySha256`, `matchesLock`) next to the SHA-256 that was built.
   The delivery image (images/delivery/Dockerfile: the three delivery binaries, git, gitleaks), tagged
   `dl-<delivery key>`: reused when ECR has the tag. Otherwise a stage without hamh-<stage>-delivery first
   deploys the delivery stack's phase 1 (repository and roles), then the bootstraps are uploaded to
   builds/out/delivery/<sha>/<bin> and scripts/codebuild-run.sh builds images/codebuild/delivery.yml.
5. One pass over every stack (`cdk deploy <stack> --exclusively`) with HAMH_WORKER_IMAGE and HAMH_DELIVERY_IMAGE,
   so each producer publishes the outputs its consumers read. A pass never omits a running worker image: until the new
   one exists, a pass carries the image the runtime serves.
6. The runtime's `live` endpoint version (runtimes stack outputs `<Name>RuntimeId`) must be READY,
   VPC mode, platform V1, requireMMDSV2 true and run the deployed image. Next, before anything records the deployment, the
   control stack's `V1ApiUrl` output must be the stage's execute-api URL in its Region, ending in `/direct` (`endpoint_ready`;
   30 s a lookup, three lookups when the stack gives no answer; `DEPLOY-FAIL endpoint missing|invalid|unavailable`; the URL is never printed).
6a. Version endpoints. AgentCore's qualifier is an endpoint name,
   never a bare version, so a run pins `v<N>`, the endpoint on the worker version N its first dispatch
   found behind `live`. The worker runtime gets `v<N>` for the version `live` serves, tagged, and READY
   serving N before the digests (`deploy: endpoint v<N> on <runtime id> serves version <N>
   (created|present)`; otherwise `DEPLOY-FAIL version-endpoint <runtime id> v<N> <status>`). Older
   `v<M>` are deleted unless M is the highest version below N (the rollback target) or a RUNNING execution of hamh-<stage>-run
   is pinned to it (its Run item's `runtimeEndpoint`).
6b. Invocation logging: set is read and never overwritten; unset,
   a `read-only` entry (ham-acct-b02) records `unset-read-only` and writes nothing, and an
   `enable-and-restore` entry (acct-a01) first writes the prior state to SSM
   /hamh/<stage>/invocation-logging-prior (tagged, never overwritten), then enables it to
   /hamh/<stage>/bedrock-invocations; teardown.sh restores the recorded state.
3b. The first stack change (the `cdk bootstrap` of an account that has none, else the first `cdk deploy`: `begin_changes`) is preceded by the
   settings record with `status=deploying`. Before it, the worker image that must be built has its fork binary looked up read-only
   (`lookup_fork`), so a pointer to a missing fork is refused before any stack changes.
7. Digests (scripts/lib/digests.py) to SSM /hamh/<stage>/{deployment-digest,deployed-source-tree,
   image-digest}, plus /hamh/<stage>/worker-revision and /hamh/<stage>/deployment-config-digest, tagged; the CDK asset objects to s3://<harness bucket>/deploy/<stage>/cdk-assets.json
   for teardown; an 11.6 evidence record to DEPLOY_RECORD_DIR.
7a. The Go module store. The packages stack creates the bucket hamh-<stage>-gomod-<account> empty and nothing in the deploy fills it, so a stage in a fresh
   account fails the events sample on 404s for every Go module. One `s3api list-objects-v2 --max-items 1` says whether the bucket holds an object and prints one line,
   always before the pass line: `GOMOD-STORE OK ...`, `GOMOD-STORE EMPTY ...` (with the fill command and the check command of scripts/gomod-store.sh) or
   `GOMOD-STORE UNKNOWN ...` (the bucket cannot be read). It is a report, never a failure, and it writes nothing; the deploy record carries it as
   `measurements.gomodStore` {bucket, state: filled|empty|unknown}. The release gate stops on an empty store before the events sample.
7b. The control stack must expose exactly one valid `V1ApiUrl`. Its lookup is bounded to 30 seconds and runs after the Go module store report. The value is never retained or printed.
8. The deploy and settings records succeed, then the safe readiness line names only the stack and Region, followed by `DEPLOY-PASS stage=<s> digest=<12> sourceTree=<12>`.

HAMH_WORKER_REVISION (SSM /hamh/<stage>/worker-revision, default 0) reaches every cdk call and the
runtime's environment; --bump-worker-version adds 1, a CloudFormation environment change that
makes a new runtime version from the same source tree. A plain deploy keeps it.

HAMH_TOOLS_MODE is the stage setting of the tool layer. It becomes the worker runtime's AGENTD_TOOLS_MODE, always as a concrete off or on
(infra/lib/config.ts `resolveToolsMode`, infra/lib/stacks/runtimes.ts). Any other word is a usage error (exit 2) before any AWS call. Every cdk call gets the
resolved word. The deploy says the mode on a `deploy: tools mode ...` line, names the live runtime's AGENTD_TOOLS_MODE (before any build) and records
`inputs.toolsMode` {setting, mode, previous, liveRuntime, change, stageKind}. A mode that differs from the live runtime's (a missing variable differs) is a
runtime environment change, so a new runtime version: it meets the deploy guard like a bump does, before the Lambda build.

HAMH_COMPUTE_INSTANCES is the stage setting of the
Instances compute type. Off, the stage keeps the one microVM worker runtime. On, the runtimes stack also holds an Instances capacity provider and a
second worker runtime (`WorkerPythonInstances`) on the same image, and the security stack its operator role (infra/lib/config.ts
`resolveComputeInstances`, one table: infra/test/fixtures/compute-instances.json). Any other word is a usage error (exit 2) before any AWS call.
Every cdk call gets the resolved word. The deploy says the setting on a `deploy: compute instances ...` line, compares it with the live stage (the
runtimes stack has the capacity provider output or the Instances runtime's id output) and records `inputs.computeInstances`. Adding the part (the stage has
none and the setting is on) meets the deploy guard (below) before the Lambda build, like a bump. A deploy never removes the part: a stage that has it and a
setting that resolves to off (an explicit off or `default`, or a record that does not keep it) is refused (`instances-off`) before any cdk call, build or run listing, because removing it ends every
machine and session on the provider and cannot finish in CloudFormation (the security stack deletes the operator role first, then the runtimes update is
refused); removal is a separate, explicit step, for now the stage teardown. Each runtime must serve its own
isolation words (RUNTIME_WORDS: AGENTD_ENGINE_SANDBOX `uid` on the microVM runtime, `landlock` and an empty AGENTD_ENGINE_USER on the Instances runtime) and the
Instances runtime must run on the capacity provider, else the deploy fails (`runtime-flags`).

Deploy guard: a deploy
that makes a new worker runtime version (a bump, a worker image other than the one the live
endpoint serves, a tools mode other than the live runtime's, an isolation word the live runtime lacks or carries another way, adding the Instances part, or a
changed Instances machine configuration, which replaces the capacity provider: its name ends in a hash of the configuration, compared after the synth) first lists the RUNNING executions of hamh-<stage>-run; any is
a refusal, `DEPLOY-REFUSED reason=guard wait=yes stage=unchanged:: <n> running run(s) on hamh-<stage>-run pin the worker runtime: <ids>. Wait until
they end, then deploy again.` (exit 3), before a build or a deploy (a bump is checked before the Lambda build).
The mode `--check-guard` checks the account and the guard only and keeps its own two answers: `DEPLOY-GUARD PASS stage=<s> running=0` (exit 0), or `DEPLOY-FAIL guard <the same sentence>` (exit 1).

Previous names: the previous spellings of the variables this program reads (the earlier prefix and the same suffix) work
after the current ones, with one notice on stderr (lib/aws_target.py `env_get`), and the children get the selectors under the new names only (`fold_previous`). A VPC of the
account that carries the previous project tag and this stage is the old stage's: an overlap with it stops the deploy with the reason (`./setup.sh --teardown` removes the old
stage). The artifact of the engine pin may sit in a bucket of the retired live stage of the previous names (`artifact_owner`).

Stage settings. scripts/lib/stage-settings.tsv (beside this file; `load_settings`) lists them: NAME|KEPT|CHANGE|PATTERN|NOTE. A value is trimmed
of the six ASCII whitespace characters; empty is unset; `default` returns to the code default; any other value must match PATTERN (`SettingError`, exit 2,
before any AWS call). Each setting resolves, in this order: the explicit value of the environment (the previous spelling counts); the `set.` line of the
stage's settings record (KEPT rows); on a stage that exists and has no record yet (an earlier release deployed it), the live value read from the stage
(KEPT rows; a live value equal to today's default counts as the default); the code default (SETTING_DEFAULTS; the utility model's from the catalog
crates/ham-core/assets/utility-models.json and the stage Region; the keep tag's and the stage kind's from the role of the account entry, ROLE_DEFAULTS, as
infra/lib/config.ts KEEP_TAG_OF_ROLE and STAGE_KIND_OF_ROLE give them). Each setting prints one line before anything is built:
`DEPLOY-SETTING HAMH_X=<value or default> source=<explicit|kept|live|default>[ change=<old> -> <new> (<CHANGE>)]` (old is the live value of a stage that
exists; `none` is no value), and the resolved values reach every cdk call (a default leaves the variable out; HAMH_TOOLS_MODE and HAMH_COMPUTE_INSTANCES are
always a concrete off or on). The deploy record carries them as `inputs.stageSettings`. The live value decides a setting only on a stage without a record,
and there a read that fails or is ambiguous refuses with `settings-read` (a KEPT row, or a row whose change needs consent). On a stage with a record the
record decides: the live value is read only to show a change and to ask for consent, and a read that fails or is ambiguous is said on a line
`deploy: the live value of HAMH_X is not known (<why>); the settings record decides it` and refuses nothing. The live readers (`live_setting`):
- HAMH_COMPUTE_INSTANCES: the outputs of hamh-<stage>-runtimes (InstancesCapacityProviderArn or WorkerPythonInstancesRuntimeId), DescribeStacks;
- HAMH_INSTANCES_TYPES, _ROOT_FREE_GIB, _WORKSPACE_GIB, _LIFETIME_S, _IDLE_S: the AWS::BedrockAgentCore::CapacityProvider of the runtimes template
  (GetTemplate): AllowedInstanceTypes, RootVolume.FreeSpaceGiB, Volumes[0].EbsConfiguration.SizeGiB, LifecycleConfiguration.MaxLifetime and .IdleInstanceTimeout;
- HAMH_TOOLS_MODE: AGENTD_TOOLS_MODE of the worker runtime version the live endpoint serves (GetAgentRuntime, the read `current_tools_mode` makes);
- HAMH_UTILITY_MODEL, HAMH_UTILITY_PURPOSES, HAMH_RESEARCH_PROVIDER, HAMH_PROVIDER_SILENCE_S, HAMH_SLICE_DEADLINE_S: the environment of the function
  hamh-<stage>-run-session in the orchestration template (UTILITY_MODEL, UTILITY_PURPOSES, RESEARCH_PROVIDER, HAMH_PROVIDER_SILENCE_S, HAMH_SLICE_DEADLINE_S);
- HAMH_RESEARCH_ALARM_TOPIC_ARN, HAMH_TAVILY_ALARMS, HAMH_TAVILY_KEY_MAX_AGE_DAYS: the alarms hamh-<stage>-research-tavily-* of the control template (their
  AlarmActions; off when none exists while the function hamh-<stage>-research-api is there and the provider can call Tavily; the Threshold of key-age);
- HAMH_LOG_RETENTION_DAYS: RetentionInDays of the AWS::Logs::LogGroup resources of the nine templates whose retention the stage sets (`stage_log_group`: the
  CDK app gives each of them a literal LogGroupName that carries the stage's name, /hamh/<stage>/... or /aws/<service>/hamh-<stage>-...; a group that a CDK
  construct makes for itself, such as the one of the AwsCustomResource provider function, has a name made by an intrinsic function or none and keeps the
  construct's own days, so it is not read); two values among the stage's groups are ambiguous;
- HAMH_PR_CHECK_MINUTES, HAMH_PR_CHECK_COMPUTE: the CodeBuild project hamh-<stage>-prcheck of the delivery template (TimeoutInMinutes, Environment.ComputeType);
- HAMH_AGENTCORE_STORAGE_ACCOUNTS: the statement AgentCoreSessionStorage of the S3 gateway endpoint policy of the network template (aws:ResourceAccount);
- HAMH_KEEP_TAG: the stack tags of every stack of the stage but hamh-<stage>-network, which never carries it (KEEP_TAG_EXCEPTION_STACKS; DescribeStacks): on when
  they carry auto-delete=no, off when they do not (stacks that differ are ambiguous);
- HAMH_STAGE_KIND: HAMH_STAGE_KIND of the functions hamh-<stage>-research-api and hamh-<stage>-research-job in the control template (no research function: none,
  so the role's word; a function without the variable, or two values, is ambiguous). tools_mode_state's stageKind is the resolved kind.
Consent (C12): a change of class replace, decrease (a lower value) or remove (to no value or off) whose source is not explicit (a kept value that differs from
the live stage) is refused (`consent`); so is a synthesized capacity provider whose
name differs from the live one while no Instances machine setting (INSTANCES_SETTINGS) is consented to. A new code default that would need consent is not taken:
on a stage that has a record with no `set.` line for a KEPT row and a known live value, the deploy keeps the live value as the stage's own (`source=kept`,
written to the record like a value the user chose) and prints `DEPLOY-SETTING HAMH_X=<kept> source=kept new-default=<default> (<CHANGE>):: <held_sentence>`,
which names `HAMH_X=default`, the export that takes the default; a default that needs no consent is taken, and a stage without a live value (a new stage, a
stage without the Instances part) gets the default. A record whose deploy did not finish (`status=deploying`) names on its `explicit=` line the settings that
deploy got from its environment (`earlier_intent`): for those, what the record holds (a `set.` line, or none after `default`) is the earlier explicit intent,
needs no new consent and keeps nothing; a kept value of another setting is no consent. A record of a deploy that did not finish written by a release
before that line (`legacy_deploying`) keeps the rule of that release: each `set.` line it holds is the earlier explicit intent, and for a setting it has no
line for, which that deploy may have been given as `default`, the deploy keeps no value by itself, says so on a `deploy:` line and asks for consent. The refusal names what each export gives (Addendum 4, A4.12; `consent_exports`): `export HAMH_X=<record value> to keep what you chose,
HAMH_X=<live value> to keep what the stage has now, or HAMH_X=default for <the default>`, without the record's value when the record holds none, and without an
export that gives the same value as one named before it.

The settings record: SSM /hamh/<stage>/settings, written through `put_param` (tagged) with `status=deploying` right before the first stack change
and `status=deployed` at the end: schema=1, status, commit (the commit the user asked to deploy, `source_commit`: HAMH_SOURCE_COMMIT when it holds 40 hex
digits, else HEAD. The setup runner deploys from a source archive, in a repository with one commit of its own that no clone holds, and exports the user's
commit in that variable, so the next update can compare the record with its clone; a manual deploy and the local route deploy a clone and leave it unset.
The deploy record file is unchanged: its `codeSha`, like the `sha=` of the first `deploy:` line and the keys of the uploaded binaries, stays the HEAD of the
tree that was built), tree (the sourceTree of /hamh/<stage>/deployed-source-tree), version (VERSION),
accounts (the value of /hamh/<stage>/deployment-config-digest), operator (the principal ARN the entry's `operator` names, empty when none), azs (the
stage's AZ IDs), updated (UTC), explicit,
and `set.HAMH_X=<value>` for each KEPT setting whose source is explicit, kept or live (never a code default; `default`
writes no line). The keep tag and the stage kind are the exception (Addendum 4, A4.7): `set.HAMH_KEEP_TAG=` and `set.HAMH_STAGE_KIND=` are always
written, with the resolved word, also when the role of the account entry gave it. Their default depends on the entry in use, and a later deploy may come
with an entry of another role (setup always generates role active), so only a recorded word lets the stage keep them. A `set.` line of a setting this
release does not know is kept as it is. A record whose schema is newer than 1 is refused (`record-downgrade`).

Deploy lock: SSM /hamh/<stage>/deploy-lock (`token`, `holder`, `identity`, `route`, `started`, `commit`, `inputs`, `build` lines; `commit` is the
commit of the settings record). With
HAMH_DEPLOY_LOCK_TOKEN set the deploy reads the lock and is refused (`lock`, wait=yes) unless its `token` line matches; it neither takes nor deletes the
lock. Without it the deploy takes the lock itself with a create-only put (`route=manual`, tagged; a lock that exists is refused, naming its holder),
renews it while it runs and deletes it when it ends, after a failure or a refusal too.
Renewal and release (Addendum 4, A4.3): setup and teardown take a lock that records no build for abandoned once its `started` is 30 minutes old, and a
deploy that builds the worker image or the Cline fork runs longer. So a daemon thread (`keep_lock`) writes a new `started` into the deploy's own lock
every LOCK_RENEW_S seconds (300: at least every 10 minutes, with room for a slow call), through the long waits in cdk, cargo and CodeBuild too. It writes
only while the lock still holds this deploy's token: a lock that another deploy replaced is left alone and the renewals stop; a lock that is gone is taken
again, create-only. `release_lock` stops the thread, then deletes the lock only when its `token` is the one this deploy wrote; a lock that another deploy
holds by then is left alone, and a `deploy:` line says so.

Refusals: a Fail of a step in REFUSALS raised before the first stack change prints exactly one line, `DEPLOY-REFUSED reason=<step> wait=<yes|no>
stage=unchanged :: <detail and action>`, and exits 3: guard and lock (wait=yes), instances-off, owner, caller, account, azs, vpc-cidr, bootstrap,
fork-pointer, settings-read, record-downgrade, consent (wait=no). Every other failure, and any failure after the first stack change, keeps
`DEPLOY-FAIL <step> <detail>` and exit 1; a failure before the first stack change prints `DEPLOY-STAGE unchanged` on the line before its DEPLOY-FAIL line.

`--print-settings` runs the settings resolution alone and writes nothing (`print_settings` names its read calls): the DEPLOY-SETTING lines and exit 0,
or a refusal line and exit 3. It needs no clean tree and checks the caller's account, not the operator. MIN_BOOTSTRAP is the one integer line of
scripts/lib/bootstrap-min-version.txt.

Test hooks: DEPLOY_ROOT, DEPLOY_AWS_BIN, DEPLOY_CARGO_BIN, DEPLOY_CARGO_SLOT (empty: no slot),
DEPLOY_NODE_BIN, DEPLOY_CODEBUILD_BIN, HAMH_ACCOUNTS_FILE, DEPLOY_RECORD_DIR, DEPLOY_CRITERIA, DEPLOY_ENDPOINT_POLL_S
(default 5) and DEPLOY_ENDPOINT_TIMEOUT_S (default 600), the wait for a new version endpoint. DEPLOY_LOCK_RENEW_S shortens the interval of the
lock renewal (seconds; a value above LOCK_RENEW_S or no number is ignored). DEPLOY_AWS_CALL_LOG names a file that gets one
JSON line {argv, profile} for every call this program makes to the aws CLI (not the calls of codebuild-run.sh or cdk); the fresh-account
dry run reads it to show that no call named a bucket of another account.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import hashlib
import json
import os
import re
import secrets
import shlex
import shutil
import socket
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
import tempfile
import threading
import time
import tomllib
import urllib.parse
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agentd_build  # noqa: E402
import aws_target  # noqa: E402
import build_selection  # noqa: E402
import deploy_preflight  # noqa: E402
import digests  # noqa: E402
import gomod_store  # noqa: E402

# The generations of names a stage can have been deployed under: the current project tag, then the frozen value of the
# previous names. This deploy only ever writes the current one; it reads the previous one to tell the old stage's VPC from a stranger's.
PROJECT = "hyperscale-agent-meta-harness"
LEGACY_PROJECT = "cline-agentcore-harness"  # COMPAT-CAH-STAGE: the project tag of a stage deployed under the previous names
AZS = {"direct": 1}
# The VPC CIDR of the stage (infra/lib/config.ts stageConfig `vpcCidr`; scripts/tests/test_deploy_direct.py pins the two together).
# A deploy refuses a CIDR that overlaps any other VPC of the account.
VPC_CIDRS = {"direct": "10.50.0.0/16"}
# The stage deploys five Lambdas: worker-api, control-api and run-session, and the research route's research-api and research-job
# (infra/lib/config.ts DIRECT_LAMBDAS); the delivery functions come from the delivery image. The cargo packages are four: the package
# `research-api` builds two binaries, so one `-p research-api` yields the bootstraps of both functions.
LAMBDA_PACKAGES = ("worker-api", "control-api", "run-session", "research-api")
LAMBDAS = ("worker-api", "control-api", "run-session", "research-api", "research-job")
# The endpoint a run pins on worker version N (run-session).
VERSION_ENDPOINT = re.compile(r"v([1-9][0-9]*)")
# The result line of scripts/codebuild-run.sh on success: `CODEBUILD PASS id=<project>:<uuid> sha=<sha7> seconds=<n>`.
CODEBUILD_RESULT = re.compile(r"^CODEBUILD PASS id=(\S+)")
DEPLOY_PRIORITY = ("network", "security", "data", "packages", "control", "runtimes", "orchestration", "delivery", "observability")
# The stacks whose `owner` tag is the owner of a stage that exists, in the order they are read: the network stack first (it holds the DNS query logging configuration,
# which CloudFormation replaces when its tags change, and the replacement fails), then the control stack. A stack in one of the states below holds nothing: its first create
# rolled back and the deploy makes it again.
OWNER_STACKS = ("network", "control")
EMPTY_STACK_STATUSES = ("ROLLBACK_COMPLETE", "CREATE_FAILED", "DELETE_COMPLETE", "REVIEW_IN_PROGRESS")
AGENTD_TARGET = agentd_build.TRIPLE
# The Rust binaries the worker image carries, with the build argument that pins each one.
WORKER_BINARIES = (("agentd", "AGENTD_SHA256"),)
# The delivery container-image functions (infra/lib/config.ts DELIVERY_FUNCTIONS): package `delivery`,
# one binary each, shipped in the hamh-<stage>-delivery image rather than as zip Lambdas.
DELIVERY_PACKAGE = "delivery"
DELIVERY_BINS = ("repo-gateway", "bundle-validator", "packager")
# Delivery image inputs besides the delivery crate closure.
DELIVERY_IMAGE_PATHS = ("Cargo.toml", "Cargo.lock", "accounts.json", "images/delivery", "images/codebuild/delivery.yml")


@dataclass(frozen=True)
class BinaryImage:
    """A runtime image built on CodeBuild ARM from workspace binaries cross-built in one zigbuild."""

    name: str
    """Record and log name: the record carries `<name>ImageTag` and `<name>Image`."""
    repo: str
    """ECR repository `hamh-<stage>-<repo>`."""
    tag_prefix: str
    binaries: tuple[tuple[str, str], ...]
    """(package and binary, the build argument carrying its SHA-256)."""
    paths: tuple[str, ...]
    """Tag inputs besides the binaries' crate closures."""
    buildspec: str
    runtime: str
    """The runtimes stack names the runtime's id in output `<runtime>RuntimeId`."""
    env: str | None
    """The CDK entry variable that carries the image (None: the worker's explicit argument)."""
    steps: tuple[str, str, str]
    """Failure step names: cross-build, upload, image build."""
    cline: bool = False
    """The build takes the promoted Cline fork (CLINE_SHA256): a worker image."""
    worker: bool = False
    """A worker runtime: runs pin its versions, so a new image is guarded."""


WORKER_IMAGE = BinaryImage(
    name="worker", repo="worker-python", tag_prefix="in", binaries=WORKER_BINARIES, paths=digests.IMAGE_PATHS,
    buildspec="images/codebuild/worker-python.yml", runtime="WorkerPython", env=None,
    steps=("agentd-build", "agentd-upload", "image-build"))
# Every worker runtime a run pins, with the family whose Run item entry holds the pin.
WORKER_RUNTIMES = {WORKER_IMAGE.runtime: "python"}
IMAGE_ENV = ("HAMH_WORKER_IMAGE", "HAMH_DELIVERY_IMAGE")
# The tool layer's stage setting: infra/lib/config.ts TOOLS_MODES and DEFAULT_TOOLS_MODE, pinned together with the words of
# infra/test/fixtures/tools-mode.json by scripts/tests/test_deploy_direct.py. TOOLS_MODE_VAR is the worker runtime's environment variable.
TOOLS_MODES = ("off", "on")
DEFAULT_TOOLS_MODE = "off"
# The only characters HAMH_TOOLS_MODE is trimmed of (infra/lib/config.ts `TOOLS_SPACE`): Python's strip() and JavaScript's trim() differ on
# U+001C to U+001F, U+0085 and U+FEFF, so neither resolver uses them and both accept the same words.
TOOLS_SPACE = " \t\n\r\f\v"
TOOLS_MODE_VAR = "AGENTD_TOOLS_MODE"
# The Instances compute type's stage setting: infra/lib/config.ts COMPUTE_INSTANCES_MODES and DEFAULT_COMPUTE_INSTANCES, pinned together with the words of
# infra/test/fixtures/compute-instances.json by scripts/tests/test_deploy_direct.py. It trims the same ASCII whitespace as the tool layer's setting.
COMPUTE_INSTANCES_MODES = ("off", "on")
DEFAULT_COMPUTE_INSTANCES = "off"
# The second worker runtime (runtimes stack output `<name>RuntimeId`) and the stack output that says the capacity provider exists.
INSTANCES_RUNTIME = "WorkerPythonInstances"
INSTANCES_PROVIDER_OUTPUT = "InstancesCapacityProviderArn"
# `capacity-provider/<name>-<id>` (a name has no hyphen) and the type of the provider in a synthesized template.
PROVIDER_ARN_NAME = re.compile(r"capacity-provider/([^-/]+)-")
PROVIDER_TYPE = "AWS::BedrockAgentCore::CapacityProvider"
# The runtime environment words of the engine's isolation each worker runtime must serve: the microVM has a second user (`uid`), an Instances container
# a Landlock sandbox (`landlock`). In landlock mode agentd runs the engine as its own user and refuses to start it while AGENTD_ENGINE_USER names another one, so the Instances
# runtime sets the variable to the empty string (the worker image sets it to `agent`, which the microVM runtime keeps). infra/lib/stacks/runtimes.ts sets these words and
# scripts/tests/test_deploy_direct.py pins the two together. A live runtime that lacks a word or carries another is a runtime change (the deploy guard) before the deploy and an
# error after it (`check_runtimes`).
SANDBOX_VAR = "AGENTD_ENGINE_SANDBOX"
ENGINE_USER_VAR = "AGENTD_ENGINE_USER"
RUNTIME_WORDS = {WORKER_IMAGE.runtime: {SANDBOX_VAR: "uid"}, INSTANCES_RUNTIME: {SANDBOX_VAR: "landlock", ENGINE_USER_VAR: ""}}
SANDBOX_WORDS = {name: words[SANDBOX_VAR] for name, words in RUNTIME_WORDS.items()}
RECORDS = os.path.join(os.environ.get("XDG_STATE_HOME") or os.path.join(os.environ.get("HOME", "/nonexistent"), ".local", "state"), "ham")
# A fresh account.
BOOTSTRAP_PARAM = "/cdk-bootstrap/hnb659fds/version"
# The bootstrap version `cdk deploy` needs when the synthesized assembly names none: the one integer line of scripts/lib/bootstrap-min-version.txt, which
# setup's preflight compares with the account's bootstrap as well.
BOOTSTRAP_MIN_FILE = Path(__file__).resolve().parent / "bootstrap-min-version.txt"


def read_min_bootstrap(path: Path = BOOTSTRAP_MIN_FILE) -> int:
    """The one positive integer line of the file; anything else is a ValueError."""
    text = path.read_text(encoding="utf-8")
    if not re.fullmatch(r"[1-9][0-9]*\n?", text):
        raise ValueError(f"{path.name} must hold one positive integer line, got {text[:40]!r}")
    return int(text)


MIN_BOOTSTRAP = read_min_bootstrap()
def bootstrap_tags(owner: str) -> tuple[str, ...]:
    """Tags of a bootstrap deploy.sh creates: shared by both stages, so no stage's teardown inventory holds it. `owner` is the owner of the entry (aws_target.owner_of)."""
    return (f"project={PROJECT}", "stage=shared", f"owner={owner}", "managed-by=deploy.sh")


def project_tags(owner: str, stage: str) -> dict[str, str]:
    """The four project tags of a resource deploy.sh creates in a stage. `owner` is the owner of the entry (aws_target.owner_of)."""
    return {"project": PROJECT, "stage": stage, "owner": owner, "managed-by": "deploy.sh"}


# The refusals of settle_owner: one line each, read by a person who runs setup.sh or scripts/deploy.sh.
OWNER_CHANGE_REFUSED = (
    "Stage {stage} already has the owner tag {have}; this run would set {want} through {named}. Changing the owner tag of a deployed stage replaces the network "
    "stack's DNS query logging configuration, and CloudFormation rolls the update back; nothing was changed. You can keep HAMH_OWNER={have} (or unset it), or remove "
    "the stage with ./setup.sh --teardown and deploy it again.")
OWNER_TAG_REFUSED = (
    'Stage {stage} has the owner tag "{have}", which is not a valid owner value (1 to 64 letters, digits or + = . @ _ -). A deploy cannot keep it; changing the tag '
    "of a deployed stage replaces the network stack's DNS query logging configuration, so nothing was changed. You can remove the stage with ./setup.sh --teardown "
    "and set it up again.")
FORK_SPEC = "images/codebuild/cline-fork-arm64.yml"
# --fork-source: where the Cline fork binary of a stage comes from.
FORK_SOURCES = ("auto", "build", "copy")
FORK_OUT = "builds/out/cline-fork"
FORK_POINTER_DIR = "engine/cline/by-tree"
S3_MISSING = ("(404)", "Not Found", "NoSuchKey", "does not exist")
S3_EXISTS = ("PreconditionFailed", "(412)")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
# Where deploy.sh records the invocation-logging state it found before it enabled logging (teardown restores it).
PRIOR_LOGGING_PARAM = "/hamh/{stage}/invocation-logging-prior"
ENDPOINT_LOOKUP_TIMEOUT_S = 30
# A lookup of the control stack that gets no answer (the time limit, a failed call) is tried ENDPOINT_LOOKUP_ATTEMPTS times, ENDPOINT_LOOKUP_WAIT_S
# seconds after the first try and twice that after the second. An answer that names no endpoint, or a malformed one, is final.
ENDPOINT_LOOKUP_ATTEMPTS = 3
ENDPOINT_LOOKUP_WAIT_S = 5


# The registry of the stage settings (beside this file, so the deploy reads the rows of its own release), the stage record and the deploy lock.
SETTINGS_FILE = Path(__file__).resolve().parent / "stage-settings.tsv"
# The code defaults of the stage settings, one NAME|DEFAULT row each, read by this program (SETTING_DEFAULTS) and by setup.sh for the plan of an update.
DEFAULTS_FILE = Path(__file__).resolve().parent / "stage-defaults.tsv"
SETTINGS_PARAM = "/hamh/{stage}/settings"
SETTINGS_SCHEMA = "1"
LOCK_PARAM = "/hamh/{stage}/deploy-lock"
LOCK_CALLER_VARIABLE = "HAMH_DEPLOY_LOCK_TOKEN"
LOCK_FIELDS = ("token", "holder", "identity", "route", "started", "commit", "inputs", "build")
# Addendum 4 (A4.3): how often the deploy writes a new `started` into the lock it took. Setup and teardown take a lock that records no build for abandoned when
# its `started` is older than 30 minutes (scripts/setup/runner.sh RN_LOCK_STALE_SECONDS), so this stays at or below 10 minutes; 5 leaves room for a slow call.
LOCK_RENEW_S = 300
LOCK_RENEW_VARIABLE = "DEPLOY_LOCK_RENEW_S"
# How long `release_lock` waits for a renewal that is under way before it reads the lock.
LOCK_JOIN_S = 120
# Addendum 4 (A4.1): the commit the user asked to deploy, for a deploy whose tree is no clone of it. The setup runner sets it; 40 hex digits, else it is ignored.
SOURCE_COMMIT_VARIABLE = "HAMH_SOURCE_COMMIT"
HEX40 = re.compile(r"[0-9a-fA-F]{40}")
CHANGE_CLASSES = ("free", "replace", "decrease", "remove", "refuse-off")
CONSENT_CLASSES = ("replace", "decrease", "remove")


def load_defaults(path: Path = DEFAULTS_FILE) -> dict[str, str]:
    """The rows of scripts/lib/stage-defaults.tsv: NAME|DEFAULT, one per setting, an empty DEFAULT for no value; a comment line starts with #."""
    out: dict[str, str] = {}
    for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        name, sep, value = line.partition("|")
        if not sep or not re.fullmatch(r"HAMH_[A-Z0-9_]+", name) or "|" in value or name in out:
            raise ValueError(f"{path.name}:{n}: not a NAME|DEFAULT row")
        out[name] = value
    return out


# The code defaults of the CDK app (infra/lib/config.ts; scripts/tests/test_setup_deploy_settings.py pins them to it), from scripts/lib/stage-defaults.tsv. "" is
# no value. The utility model and its purposes are left out: their default comes from the catalog and the stage Region (`utility_default`); so are the keep tag
# and the stage kind (ROLE_DEFAULTS).
SETTING_DEFAULTS = load_defaults()
# What taking a new default of a consent class does, in the sentence of a value the deploy keeps instead (`held_sentence`; setup's plan prints the same
# words, scripts/setup/full.sh full_held_cost). Every setting of the class replace is an Instances machine setting (a test holds that).
HELD_COST = {"replace": "replaces the machines of the Instances part", "decrease": "deletes what is older than the new value",
             "remove": "removes what the setting holds now"}
INSTANCES_SETTINGS = ("HAMH_INSTANCES_TYPES", "HAMH_INSTANCES_ROOT_FREE_GIB", "HAMH_INSTANCES_WORKSPACE_GIB", "HAMH_INSTANCES_LIFETIME_S", "HAMH_INSTANCES_IDLE_S")
NUMBER_SETTINGS = (*INSTANCES_SETTINGS[1:], "HAMH_TAVILY_KEY_MAX_AGE_DAYS", "HAMH_LOG_RETENTION_DAYS", "HAMH_PR_CHECK_MINUTES", "HAMH_PROVIDER_SILENCE_S",
                   "HAMH_SLICE_DEADLINE_S")
LOWER_SETTINGS = ("HAMH_COMPUTE_INSTANCES", "HAMH_TOOLS_MODE", "HAMH_TAVILY_ALARMS", "HAMH_PR_CHECK_COMPUTE", "HAMH_UTILITY_MODEL", "HAMH_UTILITY_PURPOSES",
                  "HAMH_KEEP_TAG", "HAMH_STAGE_KIND")
# Addendum 3 (A3.1): the keep tag and the stage kind default to what the role of the selected account entry gives (infra/lib/config.ts KEEP_TAG_OF_ROLE and
# STAGE_KIND_OF_ROLE; scripts/tests/test_setup_deploy_settings.py pins both tables to it), so a stage keeps the words it was deployed with when the account file of
# an update gives its entry another role. The keep tag is infra/lib/keep-tag.ts KEEP_TAG; the network stack never carries it (config.ts KEEP_TAG_EXCEPTIONS).
ROLE_DEFAULTS = {
    "HAMH_KEEP_TAG": {"active": "off", "history": "off", "test": "on"},
    "HAMH_STAGE_KIND": {"active": "official", "history": "test", "test": "test"},
}
KEEP_TAG_KEY, KEEP_TAG_VALUE = "auto-delete", "no"
KEEP_TAG_EXCEPTION_STACKS = ("network",)
RESEARCH_FUNCTIONS = ("research-api", "research-job")
LIST_SETTINGS = ("HAMH_INSTANCES_TYPES", "HAMH_UTILITY_PURPOSES", "HAMH_AGENTCORE_STORAGE_ACCOUNTS")
# The settings the deploy hands to the CDK app as a concrete word, never left out (the tool layer and the Instances part).
CONCRETE_SETTINGS = ("HAMH_TOOLS_MODE", "HAMH_COMPUTE_INSTANCES")
UTILITY_CATALOG = Path("crates") / "ham-core" / "assets" / "utility-models.json"
UTILITY_PURPOSE_WORDS = ("run-name", "run-tags")
TAVILY_PROVIDERS = ("tavily", "auto")
# The steps whose failure before the first stack change is a refusal (DEPLOY-REFUSED, exit 3), with whether waiting cures it.
# Waiting cures the first two (wait=yes); the others need an action (wait=no).
REFUSAL_STEPS = ("guard", "lock", "instances-off", "owner", "caller", "account", "azs", "vpc-cidr", "bootstrap", "fork-pointer", "settings-read",
                 "record-downgrade", "consent")
WAIT_CURES = ("guard", "lock")
REFUSALS = {step: "yes" if step in WAIT_CURES else "no" for step in REFUSAL_STEPS}


@dataclass(frozen=True)
class Setting:
    """One row of scripts/lib/stage-settings.tsv."""

    name: str
    kept: bool
    change: str
    pattern: str
    note: str


def load_settings(path: Path = SETTINGS_FILE) -> tuple[Setting, ...]:
    """The rows of the registry: NAME, KEPT and CHANGE are the first three fields, NOTE the last, PATTERN everything between (it may hold `|`)."""
    rows = []
    for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        parts = line.split("|")
        if len(parts) < 5 or not re.fullmatch(r"HAMH_[A-Z0-9_]+", parts[0]) or parts[1] not in ("yes", "no") or parts[2] not in CHANGE_CLASSES:
            raise ValueError(f"{path.name}:{n}: not a NAME|KEPT|CHANGE|PATTERN|NOTE row")
        pattern = "|".join(parts[3:-1])
        re.compile(pattern)
        rows.append(Setting(parts[0], parts[1] == "yes", parts[2], pattern, parts[-1]))
    return tuple(rows)


def canonical(name: str, value: str) -> str:
    """The one spelling of a value the setting's pattern took: whole numbers without leading zeros, case-free words in lower case, lists without the
    spaces around their commas (and without the empty items the storage accounts reader drops). The CDK app reads every one the same way."""
    if name in NUMBER_SETTINGS:
        return str(int(value))
    if name in LOWER_SETTINGS:
        value = value.lower()
    if name in LIST_SETTINGS:
        value = ",".join(p for p in (part.strip(TOOLS_SPACE) for part in value.split(",")) if p)
    return value


def shown(value: str) -> str:
    return value if value else "none"


def held_sentence(name: str, kept: str, default: str, change: str) -> str:
    """The sentence of a setting whose new default this deploy does not take because taking it needs consent (the end of its DEPLOY-SETTING line, and
    of the line of setup's plan): the value kept, the default and the export that takes it."""
    return f"the stage keeps {kept}; this release's default is {shown(default)}, and taking it {HELD_COST[change]}: export {name}=default to take it"


class SettingError(ValueError):
    """A stage setting's value that its pattern does not take: a usage error, raised before any AWS call."""


class LiveUnknown(Exception):
    """A live value the stage does not answer with one value (two log retentions, two alarm topics): the deploy cannot keep it by itself."""


class Fail(Exception):
    """A step that failed: `DEPLOY-FAIL <step> <detail>`. A refusal (REFUSALS) adds what to do (`action`); `refusal` replaces the detail on the
    DEPLOY-REFUSED line when the detail alone is no sentence (the caller's account)."""

    def __init__(self, step: str, detail: str = "", action: str = "", refusal: str = "") -> None:
        super().__init__(f"{step} {detail}".strip())
        self.step, self.detail, self.action, self.refusal = step, detail, action, refusal


def refusal_line(e: Fail) -> str:
    """The one line of a refusal before the first stack change: what was found, a full stop, what to do."""
    text = " ".join((e.refusal or e.detail).split())
    action = " ".join(e.action.split())
    if action:
        if text and not text.endswith((".", "!", "?")):
            text += "."
        text = f"{text} {action}".strip()
    return f"DEPLOY-REFUSED reason={e.step} wait={REFUSALS[e.step]} stage=unchanged :: {text}"


def unreadable(step: str, operation: str) -> Fail:
    """The refusal of the AZ or the VPC check when its EC2 answer has a shape the check cannot read (deploy_preflight's MALFORMED_RESPONSE):
    the detail is `unavailable`, and the refusal line says what was read and what to do."""
    return Fail(step, "unavailable", refusal=f"the answer of `aws ec2 {operation}` has a shape this deploy cannot check",
                action=f"Check that the AWS CLI of this machine answers `aws ec2 {operation}` as the EC2 API documents it, then deploy again.")


class ToolsModeError(ValueError):
    """HAMH_TOOLS_MODE is neither off nor on: a usage error, raised before any AWS call."""


def resolve_tools_mode(setting: str | None) -> str:
    """The concrete mode HAMH_TOOLS_MODE names: unset, empty or blank is the default; `off` or `on` in any letter case, surrounding ASCII whitespace ignored.
    Any other word raises ToolsModeError. infra/lib/config.ts `resolveToolsMode` resolves the same words (one table: infra/test/fixtures/tools-mode.json)."""
    word = (setting or "").strip(TOOLS_SPACE).lower()
    if word == "":
        return DEFAULT_TOOLS_MODE
    if word in TOOLS_MODES:
        return word
    raise ToolsModeError(f'HAMH_TOOLS_MODE="{setting}" is not one of {", ".join(TOOLS_MODES)} (unset or empty means {DEFAULT_TOOLS_MODE})')


class ComputeInstancesError(ValueError):
    """HAMH_COMPUTE_INSTANCES is neither off nor on: a usage error, raised before any AWS call."""


def resolve_compute_instances(setting: str | None) -> str:
    """The concrete word HAMH_COMPUTE_INSTANCES names: unset, empty or blank is the default; `off` or `on` in any letter case, surrounding ASCII whitespace ignored.
    Any other word raises ComputeInstancesError. infra/lib/config.ts `resolveComputeInstances` resolves the same words (one table: infra/test/fixtures/compute-instances.json)."""
    word = (setting or "").strip(TOOLS_SPACE).lower()
    if word == "":
        return DEFAULT_COMPUTE_INSTANCES
    if word in COMPUTE_INSTANCES_MODES:
        return word
    raise ComputeInstancesError(
        f'HAMH_COMPUTE_INSTANCES="{setting}" is not one of {", ".join(COMPUTE_INSTANCES_MODES)} (unset or empty means {DEFAULT_COMPUTE_INSTANCES})')


def say(msg: str) -> None:
    print(f"deploy: {msg}", flush=True)


def utc_now() -> dt.datetime:
    """The time of the deploy's clock (`Deployer.clock`), for the times it writes into its lock."""
    return dt.datetime.now(dt.timezone.utc)


class Deployer:
    def __init__(self, stage: str, bump: bool = False, fork_source: str = "auto") -> None:
        self.stack_deps: dict[str, set[str]] = {}
        here = Path(__file__).resolve().parent
        self.root = Path(os.environ.get("DEPLOY_ROOT") or here.parents[1])
        # The accounts.json entry of the tree being deployed that HAMH_AWS_PROFILE selects
        # (else the active one); aws_target.TargetError is a usage error.
        # COMPAT-CAH: the previous spellings of the selectors are folded into the new ones, so the children (cargo, the CDK container) get one spelling of each.
        environ = aws_target.fold_previous(os.environ)
        env = dict(environ)
        env.setdefault(aws_target.FILE_HOOK, str(self.root / "accounts.json"))
        self.target = aws_target.select(env)
        self.account, self.region, self.profile = self.target.account, self.target.region, self.target.profile
        # The value of the `owner` tag of every resource this deploy creates by hand: the entry's, as the CDK app tags the stacks with it (infra/lib/owner.ts).
        self.owner = aws_target.owner_of(self.target)
        self.entries = aws_target.accounts(env)
        # `committed`, or the SHA-256 of the account file HAMH_ACCOUNTS_FILE names. Read from the caller's environment, not from `env`
        # above, which holds the tree's own accounts.json by default.
        self.config_digest = aws_target.config_digest(environ)
        self.accounts_file = environ.get(aws_target.FILE_HOOK, "")
        # The stage settings of the registry. An explicit value is checked here, before any AWS call (a bad one is a usage error from `main`); the
        # record and the live stage are read in `settle_settings`, which resolves every setting and hands the result to the cdk calls.
        self.settings = load_settings()
        self.explicit: dict[str, str] = {}
        for st in self.settings:
            raw = aws_target.env_get(st.name)
            word = (raw or "").strip(TOOLS_SPACE)
            if not word:
                continue
            if word != "default":
                # The two words the deploy compares with the live stage keep their own messages (tools-mode.json, compute-instances.json).
                if st.name == "HAMH_TOOLS_MODE":
                    resolve_tools_mode(word)
                if st.name == "HAMH_COMPUTE_INSTANCES":
                    resolve_compute_instances(word)
                if not re.fullmatch(st.pattern, word):
                    raise SettingError(f'{st.name}="{raw}" is not a value of this setting. {st.note} Unset keeps the stage\'s value, and {st.name}=default '
                                       "returns to the default.")
                word = canonical(st.name, word)
            self.explicit[st.name] = word
        self.setting_by_name = {st.name: st for st in self.settings}
        self.settings_state: dict[str, dict] = {}
        self.record: dict | None = None
        self.changed = False
        self.lock_owned = False
        self.lock_fields: dict[str, str] = {}
        self.lock_stop = threading.Event()
        self.lock_thread: threading.Thread | None = None
        # The environment of the keeper's aws calls: a copy made when the thread starts, so the thread never reads `self.env` while the deploy changes it.
        self.lock_env: dict | None = None
        self.lock_renew_s = self.renew_interval()
        # The deploy's clock, for the times of the lock; a test moves it by hand.
        self.clock = utc_now
        self.caller_arn = ""
        self.sha = self.tree = self.commit = ""
        # The tool layer's stage setting; a bad word is a usage error before any AWS call. The resolved word, not the caller's spelling, reaches every
        # cdk call, so the word compared with the live runtime is the word the CDK app writes into it. `settle_settings` replaces the explicit-or-default
        # word below with the resolved one (explicit, kept, live or default).
        raw = aws_target.env_get("HAMH_TOOLS_MODE") or None
        self.tools_setting = (raw or "").strip(TOOLS_SPACE) or None
        self.tools_mode = resolve_tools_mode(None if self.tools_setting in (None, "default") else self.tools_setting)
        self.tools_state: dict = {}
        # The Instances compute type's stage setting, resolved in the same way; the resolved word reaches every cdk call.
        raw_instances = aws_target.env_get("HAMH_COMPUTE_INSTANCES") or None
        self.instances_setting = (raw_instances or "").strip(TOOLS_SPACE) or None
        self.compute_instances = resolve_compute_instances(None if self.instances_setting in (None, "default") else self.instances_setting) == "on"
        self.instances_state: dict = {}
        self.isolation: dict = {}
        self.stage = stage
        self.prefix = f"hamh-{stage}"
        self.bucket = f"{self.prefix}-harness-{self.account}"
        self.repo_name = f"{self.prefix}-{WORKER_IMAGE.repo}"
        self.delivery_repo = f"{self.prefix}-delivery"
        self.delivery_uri = f"{self.account}.dkr.ecr.{self.region}.amazonaws.com/{self.delivery_repo}"
        self.delivery_image: str | None = None
        self.aws_bin = os.environ.get("DEPLOY_AWS_BIN", "aws")
        self.cargo = shlex.split(os.environ.get("DEPLOY_CARGO_BIN", "cargo"))
        slot = os.environ.get("DEPLOY_CARGO_SLOT", str(self.root / "scripts" / "cargo-slot.sh"))
        self.slot = [slot] if slot else []
        self.node = os.environ.get("DEPLOY_NODE_BIN", str(self.root / "scripts" / "node.sh"))
        self.codebuild = os.environ.get("DEPLOY_CODEBUILD_BIN", str(self.root / "scripts" / "codebuild-run.sh"))
        # Builds go to this selection's own directories: the cargo
        # target directory keyed by account and Region, the Lambda bootstraps CDK zips keyed by stage as well.
        self.target_dir = build_selection.target_dir(self.root, self.target)
        self.lambda_dir = build_selection.lambda_dir(self.root, self.target, stage)
        # The variables this deploy sets itself for the CDK app (the image references, the Lambda folder, the worker revision, the owner, the two settings) are given
        # under their current names only; a previous spelling in the caller's environment is not handed on next to them.
        own = (*IMAGE_ENV, "HAMH_LAMBDA_DIR", "HAMH_WORKER_REVISION", *(st.name for st in self.settings))
        self.env = {k: v for k, v in environ.items() if k not in (*IMAGE_ENV, "HAMH_LAMBDA_DIR") and k not in map(aws_target.previous_name, own)}
        # The deploy's cargo builds go ahead of helper builds waiting for a slot (cargo-slot.sh).
        self.env["CARGO_SLOT_PRIORITY"] = "high"
        self.env["HAMH_TOOLS_MODE"] = self.tools_mode
        self.env["HAMH_COMPUTE_INSTANCES"] = "on" if self.compute_instances else "off"
        # The selected entry's profile always: an inherited AWS_PROFILE never picks the account.
        self.env["AWS_PROFILE"] = self.profile
        # A cargo build compiles the selection in (harness-protocol's build script reads HAMH_AWS_PROFILE): the
        # selected profile and the selection's target directory, whatever the caller exported.
        self.build_env = dict(self.env, CARGO_TARGET_DIR=str(self.target_dir), HAMH_AWS_PROFILE=self.profile)
        # The fork binary's outcome, once per deploy, before the first image that COPYs it.
        self.fork: dict | None = None
        self.fork_source = fork_source
        self.assets: dict[str, set] = {"files": set(), "images": set()}
        self.bump = bump
        self.revision = 0
        # Binaries this deploy cross-built and uploaded: the package sets built, and each binary's
        # SHA-256 as last uploaded to builds/out/<binary>/<sha>/.
        self.built_sets: set[tuple[str, ...]] = set()
        self.uploaded: dict[str, str] = {}

    # ---- process helpers --------------------------------------------------------------------

    def run(self, cmd: list[str], cwd: Path | None = None, env: dict | None = None, stream: bool = False,
            timeout: float | None = None):
        log = os.environ.get("DEPLOY_AWS_CALL_LOG")
        if log and cmd and cmd[0] == self.aws_bin:
            with open(log, "a", encoding="utf-8") as f:
                f.write(json.dumps({"argv": cmd[1:], "profile": (env or self.env).get("AWS_PROFILE")}) + "\n")
        if stream:
            return subprocess.run(cmd, cwd=cwd or self.root, env=env or self.env, text=True, encoding="utf-8", errors="strict", timeout=timeout)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
        return subprocess.run(cmd, cwd=cwd or self.root, env=env or self.env, capture_output=True, text=True, encoding="utf-8", errors="strict", timeout=timeout)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell

    def run_codebuild(self, cmd: list[str]) -> tuple[int, str | None]:
        """Runs scripts/codebuild-run.sh with its output on this console. Returns its exit code and the id of the CodeBuild build its
        success line names (`CODEBUILD PASS id=<project>:<uuid> ...`; the last such line), None when it printed none. The build id goes
        into the image-build record (R21 reads the build back). A byte of the build log that is no UTF-8 is replaced, not an error."""
        build_id = None
        with subprocess.Popen(cmd, cwd=self.root, env=self.env, text=True, encoding="utf-8", errors="replace",  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit, python36-compatibility-Popen1, python36-compatibility-Popen2 -- argument list built here from its own options and paths, no shell; scripts run on Python 3.8 or newer as scripts/lib/python.sh picks it, so the errors and encoding arguments of Popen exist
                              stdout=subprocess.PIPE) as proc:
            for line in proc.stdout:  # type: ignore[union-attr]
                sys.stdout.write(line)
                sys.stdout.flush()
                m = CODEBUILD_RESULT.match(line)
                if m:
                    build_id = m.group(1)
        return proc.returncode, build_id

    def aws(self, *args: str, ok_missing: tuple[str, ...] = (), env: dict | None = None,
            timeout: float | None = None) -> dict | None:
        try:
            proc = self.run(
                [self.aws_bin, *args, "--region", self.region, "--output", "json"],
                env=env,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            raise Fail("aws", "timeout") from None
        if proc.returncode != 0:
            if any(m in proc.stderr for m in ok_missing):
                return None
            raise Fail("aws", f"{' '.join(args[:2])}: {proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() else proc.returncode}")
        return json.loads(
            proc.stdout,
            parse_constant=deploy_preflight._reject_json_constant,
            parse_float=deploy_preflight._finite_json_float,
        ) if proc.stdout.strip() else {}

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.root), *args], capture_output=True, text=True).stdout.strip()  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool

    def cdk(self, *args: str, image: str | None) -> int:
        env = dict(self.env, HAMH_LAMBDA_DIR=str(self.lambda_dir), HAMH_WORKER_REVISION=str(self.revision))
        if image:
            env["HAMH_WORKER_IMAGE"] = image
        if self.delivery_image:
            env["HAMH_DELIVERY_IMAGE"] = self.delivery_image
        rc = self.run([self.node, "cdk", *args, "-c", f"stage={self.stage}"], cwd=self.root / "infra", env=env, stream=True).returncode
        self.collect_assets()
        return rc

    # ---- steps ------------------------------------------------------------------------------

    def check_account(self, operator: bool = True) -> None:
        """The caller must be the target account and, for a deploy (`operator`), the entry's operator or the setup runner role."""
        doc = self.aws("sts", "get-caller-identity") or {}
        result = deploy_preflight.evaluate_account(doc, self.account)
        got = doc.get("Account", "") if isinstance(doc, dict) else ""
        self.caller_arn = doc.get("Arn", "") if isinstance(doc, dict) else ""
        if result.state != "PASS":
            raise Fail("account", got or "none", action="Sign in to that account, then deploy again.",
                       refusal=f"the caller is in account {got or 'none'}, not in account {self.account} of the selected entry ({self.profile})")
        op = self.target.operator
        if operator and op is not None:
            # The entry names the identity that operates its stage; the stage trusts that identity, so a deploy made as another one would leave a stage that
            # refuses its operator. The setup runner of the account deploys with its own role and passes (aws_target.operator_may_deploy).
            arn = doc.get("Arn", "")
            if not aws_target.operator_may_deploy(self.account, op, arn):
                where = "the committed accounts.json" if self.config_digest == "committed" else f"HAMH_ACCOUNTS_FILE={self.accounts_file}"
                raise Fail("caller", f"{arn or 'no identity'} is not {op.label}, the operator that {where} names for account {self.account}. "
                                     f"Sign in as {op.label} (the account's setup runner role {aws_target.SETUP_RUNNER_ROLE} may deploy as well), or correct \"operator\" in that file")

    def stage_owner(self) -> str | None:
        """The value of the `owner` tag of the stage that exists in the account: the tag of the network stack, else of the control stack. None when the stage has no
        such stack (a stage that is new, or whose first create rolled back) or its stacks carry no `owner` tag."""
        for short in OWNER_STACKS:
            doc = self.aws("cloudformation", "describe-stacks", "--stack-name", f"{self.prefix}-{short}", ok_missing=("does not exist",))
            stack = ((doc or {}).get("Stacks") or [{}])[0]
            if not stack or stack.get("StackStatus") in EMPTY_STACK_STATUSES:
                continue
            for tag in stack.get("Tags", []):
                if tag.get("Key") == "owner":
                    return tag.get("Value", "")
        return None

    def settle_owner(self) -> None:
        """The owner of this deploy, before anything is built or changed. A change of the `owner` tag is a change of the tag of every resource of the stage, and
        CloudFormation replaces the network stack's DNS query logging configuration when its tags change (AWS::Route53Resolver::ResolverQueryLoggingConfig, Tags:
        "Update requires: Replacement"); the replacement fails and the stack rolls back. So a stage that exists keeps its owner: without a named owner (HAMH_OWNER or
        the entry's `owner`) the owner is the stage's own, and a named owner that differs is refused (`DEPLOY-REFUSED reason=owner`). A stage that does not exist gets the
        derived or named owner. The value reaches the CDK app and the resources this deploy tags by hand as HAMH_OWNER, so the two cannot disagree."""
        have = self.stage_owner()
        if have is not None:
            try:
                aws_target.parse_owner(have, "owner tag")
            except aws_target.TargetError:
                raise Fail("owner", OWNER_TAG_REFUSED.format(stage=self.prefix, have=have)) from None
            if aws_target.owner_is_named(self.target):
                if self.owner != have:
                    named = "HAMH_OWNER" if aws_target.env_get(aws_target.OWNER_ENV) else "the owner of the account file"
                    raise Fail("owner", OWNER_CHANGE_REFUSED.format(stage=self.prefix, have=have, want=self.owner, named=named))
            else:
                self.owner = have
                say(f"owner {have} kept from the stage")
        self.env[aws_target.OWNER_ENV] = self.owner
        self.build_env[aws_target.OWNER_ENV] = self.owner

    def check_azs(self) -> dict[str, str]:
        """Each configured AZ name must map to the configured ID in this account."""
        expected = tuple(zip(self.target.azs[self.stage], self.target.az_ids[self.stage]))
        document = self.aws("ec2", "describe-availability-zones", "--zone-names", *(name for name, _ in expected)) or {}
        result = deploy_preflight.evaluate_az_mapping(document, expected)
        if result.state != "PASS":
            rows = document.get("AvailabilityZones", []) if isinstance(document, dict) else []
            found = {
                row.get("ZoneName"): row.get("ZoneId")
                for row in rows
                if isinstance(row, dict) and isinstance(row.get("ZoneName"), str)
            } if isinstance(rows, list) else {}
            bad = [(name, zone_id) for name, zone_id in expected if found.get(name) != zone_id]
            if not bad:
                # Every zone has its ID and the answer is still not one row per zone: nothing to name, so no guess either.
                raise unreadable("azs", "describe-availability-zones")
            raise Fail(
                "azs",
                " ".join(f"{name}={found.get(name) or 'missing'}" for name, _ in bad)
                + f" (accounts.json: {' '.join(zone_id for _, zone_id in bad)})",
                action="Correct the zones of the stage (azs and azIds) in the account entry so the name maps to its zone ID in this account, then deploy again.",
            )
        mapping = dict(result.facts["mapping"])
        say("azs " + " ".join(f"{name}={zone_id}" for name, zone_id in expected))
        return mapping

    def check_vpc_cidr(self) -> dict:
        """Validate every VPC row, then refuse overlaps except the current stage's owned VPC."""
        document = self.aws("ec2", "describe-vpcs") or {}
        owned = {"project": PROJECT, "stage": self.stage}
        result = deploy_preflight.evaluate_vpc_overlap(document, VPC_CIDRS[self.stage], owned)
        if result.state == "UNKNOWN":
            raise unreadable("vpc-cidr", "describe-vpcs")
        if result.state == "FAIL":
            details = deploy_preflight.vpc_overlap_details(document, VPC_CIDRS[self.stage])
            if details is None:
                raise unreadable("vpc-cidr", "describe-vpcs")
            wanted, overlaps = details
            # The stage's own VPC is never a clash (the exemption evaluate_vpc_overlap applies), so the refusal names the other VPCs only.
            clashes = [(vpc_id, cidr, tags) for vpc_id, cidr, tags in overlaps
                       if not all(dict(tags).get(key) == value for key, value in owned.items())]
            previous = [
                vpc_id
                for vpc_id, _cidr, tags in clashes
                if dict(tags).get("project") == LEGACY_PROJECT and dict(tags).get("stage") == self.stage
            ]
            overlap = " ".join(f"{vpc_id}={cidr}" for vpc_id, cidr, _tags in clashes)
            if previous:
                raise Fail(
                    "vpc-cidr",
                    f"{wanted} overlaps {overlap}: this account still has the stage under the previous names "
                    f"({' '.join(dict.fromkeys(previous))} carries the tag of the previous project). "
                    "This version does not deploy over it. `./setup.sh --teardown` removes it before you deploy again.",
                )
            raise Fail(
                "vpc-cidr",
                f"{wanted} overlaps {overlap}",
                action=f"The stage needs {wanted} for its own VPC. Free that range in this account, or deploy to an account where it is free, then deploy again.",
            )
        say(f"vpc cidr {result.facts['cidr']} overlaps no other VPC of the account")
        return {"cidr": result.facts["cidr"], "overlaps": []}

    def required_bootstrap(self) -> int:
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        found = [(a.get("properties") or {}).get("requiresBootstrapStackVersion") for a in manifest.get("artifacts", {}).values()]
        return max((v for v in found if isinstance(v, int)), default=MIN_BOOTSTRAP)

    def bootstrap_version(self) -> int | None:
        doc = self.aws("ssm", "get-parameter", "--name", BOOTSTRAP_PARAM, ok_missing=("ParameterNotFound",))
        return int(doc["Parameter"]["Value"]) if doc else None

    def check_bootstrap(self) -> dict:
        """An existing CDKToolkit is used unchanged when it meets the synthesized assembly's version and never
        modified, so a lower version is refused here, before any stack change. `outcome` is `existing`, or `missing` for an account and Region with none,
        which `create_bootstrap` gets after every other check."""
        need, have = self.required_bootstrap(), self.bootstrap_version()
        if have is None:
            return {"outcome": "missing", "version": None, "required": need}
        if have < need:
            raise Fail("bootstrap", f"version {have} is below the required {need}; the existing CDKToolkit is never modified",
                       action=f"Upgrade the CDK bootstrap of aws://{self.account}/{self.region} to version {need} or later (cdk bootstrap), then deploy again.")
        say(f"bootstrap version {have} used unchanged (required {need})")
        return {"outcome": "existing", "version": have, "required": need}

    def ensure_bootstrap(self) -> dict:
        boot = self.check_bootstrap()
        return self.create_bootstrap(boot) if boot["outcome"] == "missing" else boot

    def create_bootstrap(self, boot: dict) -> dict:
        """An account and Region with no CDKToolkit get one, with the project tags: the first change of the deploy, so the settings record comes first."""
        need = boot["required"]
        say(f"bootstrap: none in {self.account} {self.region}; creating it with the project tags")
        self.begin_changes()
        tags = [a for tag in bootstrap_tags(self.owner) for a in ("--tags", tag)]
        if self.cdk("bootstrap", f"aws://{self.account}/{self.region}", *tags, image=None) != 0:
            raise Fail("bootstrap", "cdk bootstrap failed")
        have = self.bootstrap_version()
        if have is None or have < need:
            raise Fail("bootstrap", f"version {have} after cdk bootstrap (required {need})")
        say(f"bootstrap version {have} created")
        return {"outcome": "created", "version": have, "required": need}

    def artifact_owner(self, uri: str):
        """The accounts entry whose stage bucket holds `uri`: another account's, or this account's retired stage
        (the lock's artifact may sit in a `cah-live-harness-<account>` or `cah-drill-harness-<account>` bucket of the project's
        history). This stage's own bucket has nothing to copy. The stage buckets of the previous names are the first alternative of the pattern
        and the current direct bucket the second: the current names never had a live or drill stage (COMPAT-CAH-STAGE)."""
        m = re.fullmatch(r"s3://((?:cah-(?:live|drill|direct)|hamh-direct)-harness-([0-9]{12}))/.+", uri or "")  # COMPAT-CAH-STAGE: the alternative of the previous names
        if not m or m.group(1) == self.bucket:
            return None
        return next((t for t in self.entries if t.account == m.group(2)), None)

    # ---- the Cline fork binary -----------------------------------------------

    def cline_series(self) -> dict:
        """The series pin of engine/cline/lock.toml: what a source build must show, and what a recorded fork is keyed by (the series tree and
        the first 12 hex characters of the SHA-256 of the [toolchain] entries as `key=value` lines (no quotes, no spaces), sorted by key and joined
        by newlines, so a new toolchain pin is a new key)."""
        try:
            lock = tomllib.loads((self.root / "engine" / "cline" / "lock.toml").read_text())
            tree, commit, tag, tc = lock["series"]["tree"], lock["upstream"]["commit"], lock["upstream"]["tag"], dict(lock["toolchain"])
        except (OSError, KeyError, tomllib.TOMLDecodeError) as e:
            raise Fail("fork-binary", f"engine/cline/lock.toml cannot be read: {e}") from None
        digest = hashlib.sha256("\n".join(f"{k}={v}" for k, v in sorted(tc.items())).encode()).hexdigest()[:12]
        return {"tree": tree, "commit": commit, "tag": tag, "toolchain": tc, "toolchainDigest": digest, "pointerKey": f"{FORK_POINTER_DIR}/{tree}-{digest}.json"}

    def s3_head(self, key: str) -> dict | None:
        return self.aws("s3api", "head-object", "--bucket", self.bucket, "--key", key, ok_missing=S3_MISSING)

    def s3_text(self, uri: str) -> str | None:
        """The text of a small object of the stage bucket, None when it is not there."""
        proc = self.run([self.aws_bin, "s3", "cp", uri, "-", "--region", self.region, "--only-show-errors"])
        if proc.returncode != 0:
            if any(m in proc.stderr for m in S3_MISSING):
                return None
            raise Fail("fork-binary", f"{uri}: {proc.stderr.strip()[-200:]}")
        return proc.stdout

    def s3_download(self, uri: str, dest: Path) -> None:
        proc = self.run([self.aws_bin, "s3", "cp", uri, str(dest), "--region", self.region, "--only-show-errors"])
        if proc.returncode != 0 or not dest.is_file():
            raise Fail("fork-binary", f"{uri}: {proc.stderr.strip()[-200:]}")

    def s3_put_once(self, key: str, body: Path, sha256: str | None = None) -> bool:
        """A create-only put into the stage bucket: True when created, False when the key exists (the 412)."""
        cmd = [self.aws_bin, "s3api", "put-object", "--bucket", self.bucket, "--key", key, "--body", str(body), "--if-none-match", "*"]
        if sha256:
            cmd += ["--metadata", f"sha256={sha256}"]
        proc = self.run([*cmd, "--region", self.region, "--output", "json"])
        if proc.returncode == 0:
            return True
        if any(m in proc.stderr for m in S3_EXISTS):
            return False
        raise Fail("fork-binary", f"s3://{self.bucket}/{key}: {proc.stderr.strip()[-200:]}")

    def profile_configured(self, profile: str) -> bool:
        """Whether the AWS CLI of this host has the profile (`aws configure list-profiles`: local, no network)."""
        proc = self.run([self.aws_bin, "configure", "list-profiles"])
        return proc.returncode == 0 and profile in proc.stdout.split()

    def fork_present(self, want: str, work: Path) -> dict | None:
        """The lock's object in the stage bucket, verified by its sha256 metadata or else by its bytes (a promotion without metadata)."""
        key = f"engine/cline/{want}/cline"
        dest = f"s3://{self.bucket}/{key}"
        head = self.s3_head(key)
        if head is None:
            return None
        got = (head.get("Metadata") or {}).get("sha256")
        how = "sha256 metadata"
        if got is None:
            # A promotion without metadata (ham-acct-b02's c07af1c8 object): its bytes decide.
            local = work / "present"
            self.s3_download(dest, local)
            got, how = hashlib.sha256(local.read_bytes()).hexdigest(), "sha256 of its bytes"
        if got != want:
            raise Fail("fork-binary", f"{dest} has sha256 {got[:12]}, the lock pins {want[:12]}")
        say(f"fork binary {want[:12]} present at {dest} ({how})")
        return {"outcome": "present", "uri": dest, "sha256": want, "verifiedBy": how}

    def fork_from_doc(self, doc: dict, series: dict, uri: str, lock_sha: str, pointer_outcome: str) -> dict:
        """The fork a pointer document names, after the document is held to this series tree and toolchain and the object is found with its sha256."""
        want = doc.get("sha256") if isinstance(doc, dict) else None
        fix = f"Delete the pointer {uri}, then deploy again: the deploy then builds the Cline binary from the pinned source and records it anew."
        if (not isinstance(want, str) or not HEX64.match(want) or doc.get("schema") != 1 or doc.get("seriesTree") != series["tree"]
                or doc.get("toolchainDigest") != series["toolchainDigest"] or doc.get("upstreamCommit") != series["commit"]):
            raise Fail("fork-pointer", f"pointer {uri} does not describe this series tree and toolchain", action=fix)
        key = f"engine/cline/{want}/cline"
        head = self.s3_head(key)
        if head is None or (head.get("Metadata") or {}).get("sha256") != want:
            raise Fail("fork-pointer", f"pointer {uri} names a missing or other object (s3://{self.bucket}/{key})", action=fix)
        return {"outcome": doc.get("outcome") or "built-from-source", "uri": f"s3://{self.bucket}/{key}", "sha256": want, "lockBinarySha256": lock_sha,
                "matchesLock": want == lock_sha, "seriesTree": series["tree"], "upstreamCommit": series["commit"], "toolchainDigest": series["toolchainDigest"],
                "codebuildBuildId": doc.get("codebuildBuildId"), "pointer": uri, "pointerOutcome": pointer_outcome,
                "verifiedBy": "pointer of this series tree and toolchain, sha256 metadata of the object"}

    def fork_from_pointer(self, series: dict, lock_sha: str) -> dict | None:
        uri = f"s3://{self.bucket}/{series['pointerKey']}"
        text = self.s3_text(uri)
        if text is None:
            return None
        try:
            doc = json.loads(text)
        except ValueError:
            raise Fail("fork-pointer", f"pointer {uri} is not JSON", action=f"Delete the pointer {uri}, then deploy again.") from None
        found = self.fork_from_doc(doc, series, uri, lock_sha, "read")
        say(f"fork binary {found['sha256'][:12]} recorded by an earlier source build of this stage (series tree {series['tree'][:12]}, toolchain {series['toolchainDigest']})")
        return found

    def fork_copy(self, lock: dict, want: str, owner, work: Path, hint: str) -> dict:
        """The lock's artifact, copied with the profile of the account that owns it and checked against the lock's SHA-256."""
        src = lock.get("artifact") or ""
        if not owner:
            raise Fail("fork-binary", f"no copy source in reach for s3://{self.bucket}/engine/cline/{want}/cline: the object is not there and the lock's artifact "
                                      f"({src or 'none'}) is held by no account of accounts.json; {hint}")
        local = work / "cline"
        proc = self.run([self.aws_bin, "s3", "cp", src, str(local), "--profile", owner.profile, "--region", owner.region, "--only-show-errors"])
        if proc.returncode != 0 or not local.is_file():
            raise Fail("fork-binary", f"{src} unreadable with profile {owner.profile}: {proc.stderr.strip()[-200:]}; {hint}")
        got = hashlib.sha256(local.read_bytes()).hexdigest()
        if got != want:
            raise Fail("fork-binary", f"{src} has sha256 {got[:12]}, the lock pins {want[:12]}")
        say(f"fork binary {want[:12]} copied from {src} (profile {owner.profile})")
        dest = f"s3://{self.bucket}/engine/cline/{want}/cline"
        up = self.run([self.aws_bin, "s3", "cp", str(local), dest, "--metadata", f"sha256={want}", "--region", self.region, "--only-show-errors"])
        if up.returncode != 0:
            raise Fail("fork-binary", up.stderr.strip()[-200:])
        return {"outcome": "copied", "source": src, "sourceProfile": owner.profile, "verifiedBy": "sha256 of the copied bytes", "uri": dest, "sha256": want}

    def check_fork_build(self, series: dict, build_id: str, listed_id: str, docs: dict, got: str) -> None:
        """The evidence the build itself computed (build-cline-fork.sh's metadata and record, the smoke test of the build spec) against the lock and
        the bytes downloaded. A log line is not read. Every difference is fatal and nothing is promoted."""
        meta, rec, smoke = docs["build-metadata.json"], docs["build-record.json"], docs["smoke-acp-initialize.json"]
        tc = series["toolchain"]
        sect = lambda d, k: d.get(k) if isinstance(d.get(k), dict) else {}  # noqa: E731
        binary, up, ser, tool, gate = sect(meta, "binary"), sect(meta, "upstream"), sect(meta, "series"), sect(meta, "toolchain"), sect(meta, "gate")
        mrec = sect(rec, "measurements")
        version = str(series["tag"]).removeprefix("cli-v")
        checks = [
            (listed_id == build_id, f"build id {listed_id or 'missing'} in the build output is not the build id this deploy started ({build_id}): stale or other output"),
            (meta.get("mode") == "build" and meta.get("target") == "linux-arm64", "build metadata is not a linux-arm64 build (mode, target)"),
            (up.get("commit") == series["commit"], f"build metadata upstream.commit {str(up.get('commit'))[:12]} is not the lock's {series['commit'][:12]}"),
            (ser.get("tree") == series["tree"], f"build metadata series.tree {str(ser.get('tree'))[:12]} is not the lock's {series['tree'][:12]}"),
            (tool.get("bun_zip_sha256") == tc.get("bun_linux_aarch64_zip_sha256"), "build metadata toolchain.bun_zip_sha256 is not the lock's bun_linux_aarch64_zip_sha256"),
            (tool.get("bun_version") == tc.get("bun_version") and tool.get("build_image") == tc.get("build_image"), "build metadata toolchain bun_version or build_image is not the lock's"),
            ("--frozen-lockfile" in str(tool.get("build_command")), "build metadata toolchain.build_command lacks --frozen-lockfile (frozen lockfile)"),
            (gate.get("ran") is True, "build metadata gate.ran is not true: the suites did not run (gate)"),
            (meta.get("telemetry") == "off", f"build metadata telemetry is {meta.get('telemetry')!r}, not 'off' (telemetry)"),
            (binary.get("elf_machine") == "aarch64", f"build metadata binary.elf_machine is {binary.get('elf_machine')!r}, not 'aarch64' (ELF machine)"),
            (binary.get("sha256") == got, f"build metadata binary.sha256 {str(binary.get('sha256'))[:12]} is not the SHA-256 of the downloaded bytes {got[:12]}"),
            (rec.get("result") == "pass" and mrec.get("binarySha256") == got and mrec.get("seriesTree") == series["tree"],
             "build record is not a pass for these bytes and this series tree"),
            (mrec.get("telemetry") == "off" and mrec.get("leaks") == 0, f"build record measurements.leaks is {mrec.get('leaks')!r} and telemetry {mrec.get('telemetry')!r}, not 0 and 'off' (leaks)"),
            (str(mrec.get("lastLine", "")).startswith("FORK-BUILD PASS "), "build record has no FORK-BUILD PASS line"),
            (((smoke.get("result") or {}).get("agentInfo") or {}).get("version") == version,
             f"smoke initialize of the built binary does not report version {version} (smoke)"),
        ]
        for ok, message in checks:
            if not ok:
                raise Fail("fork-binary", message)

    def fork_build(self, sha: str, series: dict, lock_sha: str, work: Path) -> dict:
        """Builds the fork from the pinned upstream source on CodeBuild ARM and accepts it on the evidence of the build (`check_fork_build`): builds
        are not bit-reproducible, so equality with the lock's SHA-256 is no test. The binary is promoted under its own SHA-256 and recorded by a
        pointer, both create-only."""
        say(f"fork binary: building {series['tag']} with its series from the pinned source on CodeBuild ARM ({FORK_SPEC})")
        rc, build_id = self.run_codebuild([self.codebuild, "--buildspec", FORK_SPEC, "--stage", self.stage, "--env", f"HARNESS_BUCKET={self.bucket}"])
        if rc != 0:
            raise Fail("fork-binary", f"codebuild-run {FORK_SPEC} exit {rc}")
        if not build_id:
            raise Fail("fork-binary", "codebuild-run printed no build id")
        prefix = f"s3://{self.bucket}/{FORK_OUT}/{sha}/"
        docs: dict = {}
        for name in ("build-metadata.json", "build-record.json", "smoke-acp-initialize.json"):
            text = self.s3_text(prefix + name)
            if text is None:
                raise Fail("fork-binary", f"{prefix}{name}: the build left no such file")
            try:
                docs[name] = json.loads(text)
            except ValueError:
                raise Fail("fork-binary", f"{prefix}{name} is not JSON") from None
        listed = (self.s3_text(prefix + "build-id.txt") or "").strip()
        local = work / "cline"
        self.s3_download(prefix + "cline", local)
        got = hashlib.sha256(local.read_bytes()).hexdigest()
        self.check_fork_build(series, build_id, listed, docs, got)
        key = f"engine/cline/{got}/cline"
        dest = f"s3://{self.bucket}/{key}"
        if not self.s3_put_once(key, local, got):
            head = self.s3_head(key)
            if head is None or (head.get("Metadata") or {}).get("sha256") != got:
                raise Fail("fork-binary", f"{dest} exists and is not this build ({got[:12]})")
        pointer_uri = f"s3://{self.bucket}/{series['pointerKey']}"
        body = {"schema": 1, "sha256": got, "seriesTree": series["tree"], "upstreamCommit": series["commit"], "toolchainDigest": series["toolchainDigest"],
                "outcome": "built-from-source", "codebuildBuildId": build_id, "sourceSha": sha}
        pfile = work / "pointer.json"
        pfile.write_text(json.dumps(body, sort_keys=True, indent=1) + "\n")
        if self.s3_put_once(series["pointerKey"], pfile):
            found = self.fork_from_doc(body, series, pointer_uri, lock_sha, "created")
            found["verifiedBy"] = "evidence of the build (metadata, record, smoke test, build id) and the SHA-256 of the downloaded bytes"
            say(f"fork binary {got[:12]} built from source (series tree {series['tree'][:12]}, toolchain {series['toolchainDigest']}); "
                f"the lock's {lock_sha[:12]} is kept as a reference and {'equals' if got == lock_sha else 'differs from'} it -> {dest}")
            return found
        text = self.s3_text(pointer_uri)
        try:
            doc = json.loads(text or "")
        except ValueError:
            raise Fail("fork-pointer", f"pointer {pointer_uri} is not JSON", action=f"Delete the pointer {pointer_uri}, then deploy again.") from None
        found = self.fork_from_doc(doc, series, pointer_uri, lock_sha, "lost-race" if doc.get("sha256") != got else "read")
        say(f"another deploy recorded the fork first; using {found['sha256'][:12]} (this build's {got[:12]} stays as an unreferenced object)")
        return found

    def ensure_fork(self, sha: str) -> dict:
        """The fork binary every worker image COPYs, settled once per deploy by `--fork-source` (module docstring, step 4).
        The result names the SHA-256 the image is built with (`sha256`); for a fork built from source that is the SHA-256 of that build, with the
        lock's kept as a reference."""
        if self.fork is not None:
            return self.fork
        found = self.lookup_fork()
        if found is not None:
            return found
        lock = tomllib.loads((self.root / "engine" / "lock.toml").read_text())["cline"]
        want = lock["binarySha256"]
        mode = self.fork_source
        work = Path(tempfile.mkdtemp(prefix="deploy-fork-"))
        try:
            owner = self.artifact_owner(lock.get("artifact") or "")
            hint = "rerun with --fork-source build to build from the pinned source"
            if mode == "build":
                use_copy = False
            elif mode == "copy":
                use_copy = True
            elif owner and self.profile_configured(owner.profile):
                say(f"fork-source auto -> copy (profile {owner.profile} of the account that holds the lock's artifact is configured here)")
                use_copy = True
            else:
                why = (f"profile {owner.profile} of the artifact account is not configured here" if owner
                       else "no account of accounts.json holds the lock's artifact")
                say(f"fork-source auto -> build ({why})")
                use_copy = False
            found = self.fork_copy(lock, want, owner, work, hint) if use_copy else self.fork_build(sha, self.cline_series(), want, work)
        finally:
            # This deploy's own mkdtemp directory.
            shutil.rmtree(work, ignore_errors=True)
        self.fork = found
        return self.fork

    def lookup_fork(self) -> dict | None:
        """The read-only half of `ensure_fork`, once per deploy: the lock's object in the stage bucket (auto, copy) or the pointer of an earlier source build
        (auto, build). A pointer that names a missing or other object is refused here (`fork-pointer`), which the deploy asks before its first stack change.
        None: nothing found, so `ensure_fork` copies or builds the binary."""
        if getattr(self, "_fork_looked", False):
            return self.fork
        self._fork_looked = True
        want = tomllib.loads((self.root / "engine" / "lock.toml").read_text())["cline"]["binarySha256"]
        mode = self.fork_source
        if mode == "build":
            # said before the pointer lookup: a rerun that finds the pointer of an earlier source build makes no build and must still show the choice
            say("fork-source build (the lock's object and the copy source are not used)")
        work = Path(tempfile.mkdtemp(prefix="deploy-fork-"))
        try:
            found = self.fork_present(want, work) if mode in ("auto", "copy") else None
            if found is None and mode in ("auto", "build"):
                found = self.fork_from_pointer(self.cline_series(), want)
        finally:
            # This deploy's own mkdtemp directory.
            shutil.rmtree(work, ignore_errors=True)
        self.fork = found
        return found

    def build_lambda(self) -> None:
        packages = [a for name in (*LAMBDA_PACKAGES, DELIVERY_PACKAGE) for a in ("-p", name)]
        # The target directory goes by CARGO_TARGET_DIR (self.build_env), never --target-dir: cargo lambda 1.9.2 passes
        # its arguments on to `cargo metadata`, which rejects it.
        proc = self.run([*self.slot, *self.cargo, "lambda", "build", "--release", "--arm64", "--lambda-dir", str(self.lambda_dir),
                         *packages], env=self.build_env, stream=True)
        if proc.returncode != 0:
            raise Fail("lambda-build", f"cargo lambda exit {proc.returncode}")
        for name in (*LAMBDAS, *DELIVERY_BINS):
            boot = self.lambda_dir / name / "bootstrap"
            if not boot.is_file():
                raise Fail("lambda-build", f"{name} has no bootstrap")
            say(f"lambda {name} bootstrap sha256={hashlib.sha256(boot.read_bytes()).hexdigest()[:12]}")
        self.verify_build("lambda", {name: self.lambda_dir / name / "bootstrap" for name in (*LAMBDAS, *DELIVERY_BINS)})

    def verify_build(self, what: str, files: dict[str, Path]) -> None:
        """Before anything built is uploaded or handed to CDK: each file must have been built for this deploy's
        selection (`DEPLOY-FAIL build-selection <binary> built for <sel>, target is <sel>: ...`). The binaries that
        embed their selection are read; the harness-protocol build output under the target directory, where every
        binary's selection comes from, is checked for all (lib/build_selection.py)."""
        checked = []
        try:
            for name, path in files.items():
                if build_selection.check_binary(path, name, self.target, self.entries):
                    checked.append(name)
            outputs = build_selection.check_build_output(self.target_dir, self.target, self.entries)
        except build_selection.Mismatch as e:
            raise Fail("build-selection", f"{e.binary} {e.detail}") from None
        say(f"build selection ok {build_selection.label(self.target)}: {what} bytes={','.join(checked) or 'none'} "
            f"outputs={len(outputs)}")

    def install_infra(self) -> None:
        # A fresh checkout has no infra/node_modules; the lockfile pins every package.
        rc = self.run([self.node, "pnpm", "install", "--frozen-lockfile"], cwd=self.root / "infra", stream=True).returncode
        if rc != 0:
            raise Fail("infra-install", f"pnpm install exit {rc}")

    def stack_order(self) -> list[str]:
        if self.cdk("synth", "--quiet", image=self.current_image) != 0:
            raise Fail("synth", "cdk synth failed")
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        arts = manifest.get("artifacts", {})
        stacks = {n: a for n, a in arts.items() if a.get("type") == "aws:cloudformation:stack" and n.startswith(self.prefix + "-")}
        deps = {n: {d for d in a.get("dependencies", []) if d in stacks} for n, a in stacks.items()}
        self.stack_deps = {n: set(d) for n, d in deps.items()}
        rank = {f"{self.prefix}-{n}": i for i, n in enumerate(DEPLOY_PRIORITY)}
        order: list[str] = []
        while deps:
            ready = sorted((n for n, d in deps.items() if not d), key=lambda n: (rank.get(n, len(rank)), n))
            if not ready:
                raise Fail("synth", "stack dependency cycle")
            order.append(ready[0])
            deps.pop(ready[0])
            for d in deps.values():
                d.discard(ready[0])
        return order

    def deploy_stack(self, name: str, image: str | None) -> None:
        self.begin_changes()
        delivery = f" delivery={self.delivery_image.rsplit(':', 1)[-1][:12]}" if self.delivery_image else ""
        say(f"stack {name}{' image=' + image.rsplit(':', 1)[-1][:12] if image else ' (no worker image)'}{delivery}")
        if self.cdk("deploy", name, "--exclusively", "--require-approval", "never", "--progress", "events", image=image) != 0:
            raise Fail("stack", name)

    def collect_assets(self) -> None:
        out = self.root / "infra" / "cdk.out"
        for f in out.glob(f"{self.prefix}-*.assets.json") if out.is_dir() else []:
            doc = json.loads(f.read_text())
            for asset in doc.get("files", {}).values():
                for d in asset.get("destinations", {}).values():
                    self.assets["files"].add((d["bucketName"], d["objectKey"]))
            for asset in doc.get("dockerImages", {}).values():
                for d in asset.get("destinations", {}).values():
                    self.assets["images"].add((d["repositoryName"], d["imageTag"]))

    def outputs(self, stack: str, *, timeout: float | None = None) -> dict[str, str]:
        doc = self.aws(
            "cloudformation", "describe-stacks", "--stack-name", stack,
            ok_missing=("does not exist",), timeout=timeout,
        )
        if not doc:
            return {}
        stacks = doc.get("Stacks") if isinstance(doc, dict) else None
        if not isinstance(stacks, list) or len(stacks) != 1 or not isinstance(stacks[0], dict):
            raise ValueError("invalid stack response")
        rows = stacks[0].get("Outputs", [])
        if not isinstance(rows, list):
            raise ValueError("invalid stack outputs")
        outputs: dict[str, str] = {}
        for row in rows:
            if not isinstance(row, dict):
                raise ValueError("invalid stack output")
            key, value = row.get("OutputKey"), row.get("OutputValue")
            if not isinstance(key, str) or not isinstance(value, str) or key in outputs:
                raise ValueError("invalid stack output")
            outputs[key] = value
        return outputs

    def endpoint_ready(self) -> bool:
        """Require a bounded, valid control endpoint without retaining or printing its value. A lookup that gets no answer is tried again:
        it follows the stack changes, so one slow or throttled describe-stacks must not fail a deploy whose stacks are in place."""
        for attempt in range(ENDPOINT_LOOKUP_ATTEMPTS):
            try:
                return self._endpoint_ready_once()
            except Fail as e:
                if e.detail != "unavailable" or attempt + 1 >= ENDPOINT_LOOKUP_ATTEMPTS:
                    raise
            time.sleep(ENDPOINT_LOOKUP_WAIT_S * 2 ** attempt)
        raise Fail("endpoint", "unavailable")

    def _endpoint_ready_once(self) -> bool:
        try:
            outputs = self.outputs(f"{self.prefix}-control", timeout=ENDPOINT_LOOKUP_TIMEOUT_S)
        except (Fail, OSError, UnicodeDecodeError):
            raise Fail("endpoint", "unavailable") from None
        except Exception:
            raise Fail("endpoint", "invalid") from None
        if "V1ApiUrl" not in outputs:
            raise Fail("endpoint", "missing")
        value = outputs["V1ApiUrl"]
        if any(ord(char) <= 32 or ord(char) == 127 or char.isspace() for char in value):
            raise Fail("endpoint", "invalid")
        try:
            parsed = urllib.parse.urlsplit(value)
            host = parsed.hostname
            port = parsed.port
            user = parsed.username
            password = parsed.password
        except (ValueError, UnicodeError):
            raise Fail("endpoint", "invalid") from None
        host_pattern = rf"[a-z0-9]+\.execute-api\.{re.escape(self.region)}\.amazonaws\.com"
        if (
            parsed.scheme != "https"
            or not host
            or not re.fullmatch(host_pattern, host)
            or parsed.netloc != host
            or user is not None
            or password is not None
            or port is not None
            or parsed.path != "/direct"
            or parsed.query
            or parsed.fragment
        ):
            raise Fail("endpoint", "invalid")
        return True

    def live_versions(self) -> list[dict]:
        """Every runtime's live endpoint version, as GetAgentRuntime returns it, with the output name
        (`<runtime>RuntimeId`) that identifies it."""
        out = []
        for key, rid in sorted(self.outputs(f"{self.prefix}-runtimes").items()):
            m = re.fullmatch(r"(\w+)RuntimeId", key)
            if not m:
                continue
            ep = self.aws("bedrock-agentcore-control", "get-agent-runtime-endpoint", "--agent-runtime-id", rid,
                          "--endpoint-name", "live", ok_missing=("ResourceNotFoundException",))
            if not ep or not ep.get("liveVersion"):
                out.append({"name": m.group(1), "runtimeId": rid, "endpoint": None})
                continue
            rt = self.aws("bedrock-agentcore-control", "get-agent-runtime", "--agent-runtime-id", rid,
                          "--agent-runtime-version", ep["liveVersion"]) or {}
            out.append({"name": m.group(1), "runtimeId": rid, "endpoint": ep, "runtime": rt})
        return out

    @property
    def current_runtimes(self) -> dict[str, dict]:
        """Each runtime's live version as GetAgentRuntime returns it, by runtime output name, before this deploy (a runtime with no live endpoint is absent)."""
        if not hasattr(self, "_current_runtimes"):
            self._current_runtimes = {v["name"]: v["runtime"] for v in self.live_versions() if v.get("endpoint")}
        return self._current_runtimes

    @property
    def current_images(self) -> dict[str, str]:
        """The image each runtime's live endpoint serves, by runtime output name, before this deploy."""
        out = {}
        for name, rt in self.current_runtimes.items():
            try:
                out[name] = rt["agentRuntimeArtifact"]["containerConfiguration"]["containerUri"]
            except (KeyError, TypeError):
                # The live read now happens before the Lambda build: an answer without an image ends the deploy by name, never in a traceback.
                raise Fail("live-runtime", f"{name}: GetAgentRuntime of the live version names no container image") from None
        return out

    @property
    def current_image(self) -> str | None:
        return self.current_images.get(WORKER_IMAGE.runtime)

    @property
    def current_tools_mode(self) -> str | None:
        """AGENTD_TOOLS_MODE of the worker runtime version the live endpoint serves (the version `current_image` reads); None when that runtime has
        no such variable, or when there is no runtime yet."""
        env = (self.current_runtimes.get(WORKER_IMAGE.runtime) or {}).get("environmentVariables") or {}
        return env.get(TOOLS_MODE_VAR)

    def tools_mode_state(self) -> dict:
        """The tool layer's stage setting against the live worker runtime, before anything is built. `change`: the mode differs from the live
        runtime's (a runtime without the variable differs), so this deploy makes a new runtime version. The deploy record keeps it as `inputs.toolsMode`."""
        live = self.current_image is not None
        previous = self.current_tools_mode
        return {"setting": self.tools_setting, "mode": self.tools_mode, "previous": previous, "liveRuntime": live,
                "change": live and previous != self.tools_mode,
                "stageKind": self.stage_kind()}

    def stage_kind(self) -> str:
        """The kind of the stage as the CDK app gets it: the resolved HAMH_STAGE_KIND (explicit, kept, live or the role's word)."""
        return (self.settings_state.get("HAMH_STAGE_KIND") or {}).get("new") or self.default_of("HAMH_STAGE_KIND")

    def say_tools_mode(self, st: dict) -> None:
        how = self.setting_how("HAMH_TOOLS_MODE", st["setting"])
        if not st["liveRuntime"]:
            after = "the stage has no live runtime yet"
        else:
            had = f"{TOOLS_MODE_VAR}={st['previous']}" if st["previous"] else f"no {TOOLS_MODE_VAR}"
            after = f"the live runtime has {had}"
            if st["change"]:
                after += ", so this deploy CHANGES it and makes a new runtime version"
                if st["setting"] is None and st["previous"] == "on":
                    # A deploy that only forgot the setting says what it does to a stage that runs the layer.
                    after += "; HAMH_TOOLS_MODE is not set, so the layer goes from on to off (export HAMH_TOOLS_MODE=on to keep it)"
        say(f"tools mode {st['mode']} ({how}); {after}")
        if st["mode"] == "on" and st["stageKind"] == "official":
            say("tools mode is on at this stage; keep it on only when the sample projects pass with the layer on")

    def compute_instances_state(self) -> dict:
        """The Instances compute type's stage setting against the live stage, before anything is built. `live`: the runtimes stack already holds the capacity
        provider. `change`: the setting differs from the live stage, so this deploy adds or removes the provider and the second runtime. The deploy record
        keeps it as `inputs.computeInstances`."""
        outputs = self.outputs(f"{self.prefix}-runtimes")
        # The provider or the second runtime: a half-removed part still counts, and a deploy does not finish the removal either.
        live = INSTANCES_PROVIDER_OUTPUT in outputs or f"{INSTANCES_RUNTIME}RuntimeId" in outputs
        return {"setting": self.instances_setting, "mode": "on" if self.compute_instances else "off", "live": live,
                "change": live != self.compute_instances}

    def setting_how(self, name: str, setting: str | None) -> str:
        """How a setting got its word, for the `deploy:` line of the tool layer and of the Instances part."""
        source = (self.settings_state.get(name) or {}).get("source", "explicit" if setting else "default")
        if source == "explicit":
            return f"{name}={setting}"
        return {"kept": f"{name} not set, kept from the stage's settings record", "live": f"{name} not set, kept from the live stage"}.get(
            source, f"{name} not set, the default")

    def say_compute_instances(self, st: dict) -> None:
        how = self.setting_how("HAMH_COMPUTE_INSTANCES", st["setting"])
        after = "the stage has the Instances part" if st["live"] else "the stage has no Instances capacity provider"
        if st["change"]:
            if st["mode"] == "on":
                after += ", so this deploy CREATES the capacity provider, its operator role and the second worker runtime"
            else:
                after += ", which a deploy does not remove"
        say(f"compute instances {st['mode']} ({how}); {after}")

    def isolation_state(self) -> dict:
        """The isolation words (RUNTIME_WORDS) against the live worker runtimes, before anything is built. `differs`: per runtime, each word the live version lacks (None)
        or carries another way, with the word it wants. A runtime with no live version has nothing to differ from. `change`: any differs, so this deploy makes a new
        runtime version (the environment is part of it) and meets the deploy guard. The deploy record keeps it as `inputs.isolationWords`."""
        differs: dict[str, dict] = {}
        for name, words in RUNTIME_WORDS.items():
            live = self.current_runtimes.get(name)
            if live is None:
                continue
            env = live.get("environmentVariables") or {}
            bad = {var: {"live": env.get(var), "want": want} for var, want in words.items() if env.get(var) != want}
            if bad:
                differs[name] = bad
        return {"change": bool(differs), "differs": differs}

    def say_isolation(self, st: dict) -> None:
        shown = lambda v: "''" if v == "" else str(v)  # noqa: E731
        for name, bad in sorted(st["differs"].items()):
            parts = [(f"no {var}" if d["live"] is None else f"{var}={shown(d['live'])}") + f" (wants {shown(d['want'])})" for var, d in sorted(bad.items())]
            say(f"isolation words: the live {name} runtime has {', '.join(parts)}, so this deploy changes its environment and makes a new runtime version")

    def provider_replacement(self) -> tuple[str, str] | None:
        """(live name, synthesized name) of the Instances capacity provider when they differ, else None. The provider's name ends in a hash of its configuration
        (infra/lib/config.ts `instancesProviderShape`), and AgentCore edits nothing of a provider but its description, so a different name is a replacement:
        CloudFormation creates the new provider, moves the runtime to it (a new runtime version) and deletes the old one with the machines on it. The synthesized
        name is read from the template of the runtimes stack that `stack_order` wrote, so this comes after the synth. A stage without the provider, or a synth without it, has nothing to compare."""
        m = PROVIDER_ARN_NAME.search(self.outputs(f"{self.prefix}-runtimes").get(INSTANCES_PROVIDER_OUTPUT, ""))
        manifest = json.loads((self.root / "infra" / "cdk.out" / "manifest.json").read_text())
        template_file = (manifest.get("artifacts", {}).get(f"{self.prefix}-runtimes", {}).get("properties") or {}).get("templateFile")
        if not m or not template_file:
            return None
        template = json.loads((self.root / "infra" / "cdk.out" / template_file).read_text())
        names = [r.get("Properties", {}).get("Name") for r in (template.get("Resources") or {}).values() if r.get("Type") == PROVIDER_TYPE]
        return (m.group(1), names[0]) if names and names[0] and names[0] != m.group(1) else None

    def refuse_instances_off(self, st: dict) -> None:
        """A stage that has the Instances part and a setting that resolves to off: removing the part ends every machine and session on the provider, and CloudFormation
        cannot finish it (the security stack deletes the operator role first, then the runtimes update fails). The removal is its own step, so the deploy stops here."""
        if not (st["live"] and not self.compute_instances):
            return
        source = (self.settings_state.get("HAMH_COMPUTE_INSTANCES") or {}).get("source")
        word = f"is {st['setting']}" if source == "explicit" else "is not set and the stage's settings record does not keep it on"
        raise Fail("instances-off", f"the stage has the Instances part (the capacity provider and the second worker runtime) and HAMH_COMPUTE_INSTANCES {word}, which is off. "
                                    "Removing the part ends every machine and session on the provider, so a deploy does not do it; removal is a separate, explicit step, "
                                    'and for now that step is the stage teardown. '
                                    "To deploy this stage as it is, export HAMH_COMPUTE_INSTANCES=on.")

    def find_image(self, tag: str, repo: str | None = None) -> str | None:
        repo = repo or self.repo_name
        doc = self.aws("ecr", "describe-images", "--repository-name", repo, "--image-ids", f"imageTag={tag}",
                       ok_missing=("ImageNotFoundException", "RepositoryNotFoundException"))
        if not doc or not doc.get("imageDetails"):
            return None
        return f"{self.account}.dkr.ecr.{self.region}.amazonaws.com/{repo}@{doc['imageDetails'][0]['imageDigest']}"

    def repo_exists(self, repo: str | None = None) -> bool:
        return self.aws("ecr", "describe-repositories", "--repository-names", repo or self.repo_name,
                        ok_missing=("RepositoryNotFoundException",)) is not None

    def image_tag(self, spec: BinaryImage, metadata: dict) -> str:
        """`<prefix>-<key>`: git tree hashes of the binaries' crate closures and the spec's paths."""
        crates = sorted({d for name, _ in spec.binaries for d in digests.local_crate_closure(metadata, name)})
        # Paths an image never reads (digests.IMAGE_EXCLUDED: engine/catalog, engine/policy) stay out
        # of its key, as in digests.image_key.
        paths = digests._without(self.root, spec.paths, digests.IMAGE_EXCLUDED)
        return f"{spec.tag_prefix}-" + digests._hash_paths(self.root, [*crates, *paths])

    def build_binary_image(self, spec: BinaryImage, sha: str, tag: str, extra: dict[str, str] | None = None) -> str:
        """Cross-builds the spec's binaries in one zigbuild, uploads each to builds/out/<binary>/<sha>/, and
        builds hamh-<stage>-<repo>:<tag> on CodeBuild ARM with their SHA-256s."""
        build, upload, image_step = spec.steps
        if extra and "CLINE_SHA256" in extra:
            # The image is built with the SHA-256 of the fork that was settled: the lock's for a present or copied one, the build's own for a source build.
            extra = {**extra, "CLINE_SHA256": self.ensure_fork(sha)["sha256"]}
        repo = f"{self.prefix}-{spec.repo}"
        envs = {"HARNESS_BUCKET": self.bucket, "REPO_URI": f"{self.account}.dkr.ecr.{self.region}.amazonaws.com/{repo}",
                **(extra or {}), "IMAGE_TAG": tag}
        names = tuple(name for name, _ in spec.binaries)
        if names in self.built_sets:
            # Another image of this deploy cross-built and uploaded these binaries already.
            say(f"{spec.name}: reusing {', '.join(names)} from this deploy's cross-build")
        else:
            # The recipe module owns the argv, the remap of CARGO_HOME and the variables that change the compiler's
            # output, so the fake-acp CI step builds the binary this deploy ships the same way.
            cross_env = agentd_build.build_env(self.build_env, target_dir=self.target_dir, profile=self.profile)
            proc = self.run([*self.slot, *self.cargo, *agentd_build.zigbuild_args(names)], env=cross_env, stream=True)
            release = self.target_dir / AGENTD_TARGET / "release"
            missing = [name for name in names if not (release / name).is_file()]
            if proc.returncode != 0 or missing:
                raise Fail(build, f"cargo zigbuild exit {proc.returncode} missing {','.join(missing) or 'none'}")
            self.verify_build(spec.name, {name: release / name for name in names})
            for name in names:
                binary = release / name
                digest = hashlib.sha256(binary.read_bytes()).hexdigest()
                say(f"{name} sha256={digest[:12]}")
                up = self.run([self.aws_bin, "s3", "cp", str(binary), f"s3://{self.bucket}/builds/out/{name}/{sha}/{name}",
                               "--region", self.region, "--only-show-errors"])
                if up.returncode != 0:
                    raise Fail(upload, up.stderr.strip()[-200:])
                self.uploaded[name] = digest
            self.built_sets.add(names)
        for name, env in spec.binaries:
            envs[env] = self.uploaded[name]
        cmd = [self.codebuild, "--buildspec", spec.buildspec, "--stage", self.stage]
        for k, v in envs.items():
            cmd += ["--env", f"{k}={v}"]
        rc, build_id = self.run_codebuild(cmd)
        if rc != 0:
            raise Fail(image_step, f"codebuild-run exit {rc}")
        if not build_id:
            raise Fail(image_step, "codebuild-run printed no build id")
        image = self.find_image(tag, repo)
        if not image:
            raise Fail(image_step, f"no image tagged {tag} after the build")
        self.write_image_record(spec.name, spec.buildspec, repo, tag, image, envs, build_id=build_id,
                                recipe=agentd_build.recipe_digest(self.root, self.profile))
        return image

    def cline_sha256(self) -> str:
        return tomllib.loads((self.root / "engine" / "lock.toml").read_text())["cline"]["binarySha256"]

    def build_image(self, sha: str, tag: str) -> str:
        # The stage's one worker runs any language a prompt names, so its image also carries Go, the JDK and Maven.
        extra = {"CLINE_SHA256": self.cline_sha256(), "LANG_FAMILY": "direct"}
        return self.build_binary_image(WORKER_IMAGE, sha, tag, extra)

    def build_delivery_image(self, sha: str, tag: str) -> str:
        """Uploads the three delivery bootstraps and builds hamh-<stage>-delivery:<tag> on CodeBuild ARM."""
        envs = {"HARNESS_BUCKET": self.bucket, "REPO_URI": self.delivery_uri, "IMAGE_TAG": tag}
        for b in DELIVERY_BINS:
            boot = self.lambda_dir / b / "bootstrap"
            envs[f"{b.upper().replace('-', '_')}_SHA256"] = hashlib.sha256(boot.read_bytes()).hexdigest()
            up = self.run([self.aws_bin, "s3", "cp", str(boot), f"s3://{self.bucket}/builds/out/delivery/{sha}/{b}",
                           "--region", self.region, "--only-show-errors"])
            if up.returncode != 0:
                raise Fail("delivery-upload", up.stderr.strip()[-200:])
        cmd = [self.codebuild, "--buildspec", "images/codebuild/delivery.yml", "--stage", self.stage]
        for k, v in envs.items():
            cmd += ["--env", f"{k}={v}"]
        rc, build_id = self.run_codebuild(cmd)
        if rc != 0:
            raise Fail("delivery-image-build", f"codebuild-run exit {rc}")
        if not build_id:
            raise Fail("delivery-image-build", "codebuild-run printed no build id")
        image = self.find_image(tag, self.delivery_repo)
        if not image:
            raise Fail("delivery-image-build", f"no image tagged {tag} after the build")
        self.write_image_record("delivery", "images/codebuild/delivery.yml", self.delivery_repo, tag, image, envs, build_id=build_id)
        return image

    def resolve_delivery_image(self, sha: str, tag: str, order: list[str], worker_image: str | None) -> str:
        found = self.find_image(tag, self.delivery_repo) if self.repo_exists(self.delivery_repo) else None
        if found:
            say(f"delivery image {tag[:15]} found {found.rsplit(':', 1)[-1][:12]}")
            return found
        stack = f"{self.prefix}-delivery"
        if stack in order and not self.repo_exists(self.delivery_repo):
            say("delivery phase-1: the stage has no delivery repository yet")
            self.deploy_stack(stack, worker_image)
        say(f"delivery image {tag[:15]} missing: building")
        return self.build_delivery_image(sha, tag)

    def expected_images(self, image: str | None) -> dict[str, str | None]:
        """The runtimes this deploy must leave serving `image`: the microVM worker runtime, and with HAMH_COMPUTE_INSTANCES=on the Instances one."""
        out: dict[str, str | None] = {WORKER_IMAGE.runtime: image}
        if self.compute_instances:
            out[INSTANCES_RUNTIME] = image
        return out

    def check_runtimes(self, images: dict[str, str | None]) -> list[dict]:
        """Every runtime's live version against its own deployed image; `images` maps each runtime
        output name (`WorkerPython`) to the image it must serve."""
        checks, bad = [], []
        versions = self.live_versions()
        seen = {v["name"] for v in versions}
        bad += [f"{name}:no-runtime" for name, image in sorted(images.items()) if image and name not in seen]
        for v in versions:
            rid = v["runtimeId"]
            if v["name"] not in images:
                bad.append(f"{rid}:no-deployed-image")
                continue
            if not v.get("endpoint"):
                bad.append(f"{rid}:no-live-endpoint")
                continue
            rt, ep = v["runtime"], v["endpoint"]
            on_instances = v["name"] == INSTANCES_RUNTIME
            c = {"runtime": v["name"], "runtimeId": rid, "liveVersion": ep["liveVersion"], "endpointStatus": ep.get("status"),
                 "status": rt.get("status"), "networkMode": rt.get("networkConfiguration", {}).get("networkMode"),
                 "platformVersion": rt.get("platformVersion"),
                 "requireMMDSV2": rt.get("metadataConfiguration", {}).get("requireMMDSV2"),
                 "image": rt.get("agentRuntimeArtifact", {}).get("containerConfiguration", {}).get("containerUri"),
                 # Where the runtime runs and which isolation word its environment carries (agentd refuses to start the engine without the right one).
                 "capacityProvider": (rt.get("capacityProviderConfiguration") or {}).get("capacityProviderArn"),
                 "sandbox": (rt.get("environmentVariables") or {}).get(SANDBOX_VAR),
                 "engineUser": (rt.get("environmentVariables") or {}).get(ENGINE_USER_VAR)}
            checks.append(c)
            # A microVM runtime is in VPC mode with a network of its own; an Instances runtime has no network configuration (its network is the provider's) and names the provider.
            want = {"endpointStatus": "READY", "status": "READY", "networkMode": None if on_instances else "VPC", "platformVersion": "V1",
                    "requireMMDSV2": True, "image": images[v["name"]], "sandbox": SANDBOX_WORDS.get(v["name"])}
            if ENGINE_USER_VAR in RUNTIME_WORDS.get(v["name"], {}):
                want["engineUser"] = RUNTIME_WORDS[v["name"]][ENGINE_USER_VAR]
            bad += [f"{rid}:{k}={c[k]}" for k, w in want.items() if c[k] != w]
            if on_instances and not c["capacityProvider"]:
                bad.append(f"{rid}:capacityProvider=None")
            if not on_instances and c["capacityProvider"]:
                bad.append(f"{rid}:capacityProvider={c['capacityProvider']}")
        if not checks and not bad:
            bad.append("no-runtimes")
        if bad:
            raise Fail("runtime-flags", " ".join(bad))
        for c in checks:
            where = "capacity-provider" if c["capacityProvider"] else "vpc"
            user = f" {ENGINE_USER_VAR}={c['engineUser']!r}" if ENGINE_USER_VAR in RUNTIME_WORDS.get(c["runtime"], {}) else ""
            say(f"runtime {c['runtimeId']} live={c['liveVersion']} {where}=ok platform=V1 requireMMDSV2=true {SANDBOX_VAR}={c['sandbox']}{user}")
        return checks

    def endpoint_state(self, rid: str, name: str) -> dict | None:
        return self.aws("bedrock-agentcore-control", "get-agent-runtime-endpoint", "--agent-runtime-id", rid,
                        "--endpoint-name", name, ok_missing=("ResourceNotFoundException",))

    def wait_endpoint(self, rid: str, name: str, version: str) -> None:
        """Until `name` is READY and serves `version`; a FAILED status or the timeout fails the deploy."""
        poll = float(os.environ.get("DEPLOY_ENDPOINT_POLL_S", "5"))
        deadline = time.monotonic() + float(os.environ.get("DEPLOY_ENDPOINT_TIMEOUT_S", "600"))
        while True:
            ep = self.endpoint_state(rid, name) or {}
            status = ep.get("status") or "absent"
            if status == "READY":
                if ep.get("liveVersion") != version:
                    raise Fail("version-endpoint", f"{rid} {name} serves {ep.get('liveVersion')}")
                return
            if status.endswith("FAILED") or status == "absent" or time.monotonic() >= deadline:
                raise Fail("version-endpoint", f"{rid} {name} {status}")
            time.sleep(poll)

    def pinned_endpoints(self) -> set[str]:
        """The endpoints the RUNNING runs of hamh-<stage>-run are pinned to on the worker runtime (their Run items' runtimeEndpoint)."""
        pins = set()
        for run in self.running_runs():
            key = json.dumps({"PK": {"S": f"RUN#{run}"}, "SK": {"S": "META"}})
            doc = self.aws("dynamodb", "get-item", "--table-name", f"{self.prefix}-harness", "--key", key,
                           "--consistent-read", "--projection-expression", "runtimeEndpoint") or {}
            pinned = doc.get("Item", {}).get("runtimeEndpoint", {}).get("S")
            if pinned:
                pins.add(pinned)
        return pins

    def version_endpoints(self, checks: list[dict]) -> list[dict]:
        """Step 6a: `v<N>` on the worker runtime for the version `live` serves; older `v<M>` pruned."""
        out = []
        for c in (c for c in checks if c["runtime"] in (*WORKER_RUNTIMES, INSTANCES_RUNTIME)):
            rid, n = c["runtimeId"], c["liveVersion"]
            name = f"v{n}"
            listed = self.aws("bedrock-agentcore-control", "list-agent-runtime-endpoints", "--agent-runtime-id", rid) or {}
            names = [e["name"] for e in listed.get("runtimeEndpoints", [])]
            created = name not in names
            if created:
                tags = project_tags(self.owner, self.stage)
                self.aws("bedrock-agentcore-control", "create-agent-runtime-endpoint", "--agent-runtime-id", rid,
                         "--name", name, "--agent-runtime-version", n, "--description",
                         f"{self.prefix} worker version {n}: runs pinned to it stay on it", "--tags", json.dumps(tags))
            self.wait_endpoint(rid, name, n)
            say(f"endpoint {name} on {rid} serves version {n} ({'created' if created else 'present'})")
            others = sorted(int(m.group(1)) for e in names if e != name and (m := VERSION_ENDPOINT.fullmatch(e)))
            below = [v for v in others if v < int(n)]
            keep = [name] + ([f"v{max(below)}"] if below else [])
            candidates = [f"v{v}" for v in others if f"v{v}" not in keep]
            pinned = self.pinned_endpoints() if candidates else set()
            kept = keep + sorted((e for e in candidates if e in pinned), key=lambda e: -int(e[1:]))
            deleted = [e for e in candidates if e not in pinned]
            for e in deleted:
                self.aws("bedrock-agentcore-control", "delete-agent-runtime-endpoint", "--agent-runtime-id", rid,
                         "--endpoint-name", e)
                say(f"endpoint {e} deleted (no running run pins it)")
            if kept[1:]:
                say("endpoints kept " + " ".join(kept))
            out.append({"runtime": c["runtime"], "runtimeId": rid, "endpoint": name, "version": n, "created": created,
                        "kept": kept, "deleted": deleted})
        return out

    def template_hashes(self) -> dict[str, str | None]:
        out = {}
        for n in digests.STACK_NAMES:
            doc = self.aws("cloudformation", "get-template", "--stack-name", f"{self.prefix}-{n}", "--template-stage", "Original",
                           ok_missing=("does not exist",))
            out[n] = digests.template_hash(doc["TemplateBody"]) if doc else None
        return out

    def asl_versions(self) -> dict[str, str]:
        return {k: v for k, v in self.outputs(f"{self.prefix}-orchestration").items() if k.endswith("StateMachineVersionArn")}

    def report_gomod_store(self) -> dict:
        """Say whether the stage's Go module store holds anything (see step 7a of the module docstring). A report, never a failure."""
        bucket = f"{self.prefix}-gomod-{self.account}"
        check = gomod_store.check_command(self.stage)
        try:
            doc = self.aws("s3api", "list-objects-v2", "--bucket", bucket, "--expected-bucket-owner", self.account, "--max-items", "1")
        except Fail as exc:
            print(f"GOMOD-STORE UNKNOWN stage={self.stage} bucket={bucket}: the store could not be read ({exc}); look at it with: {check}", flush=True)
            return {"bucket": bucket, "state": "unknown"}
        if (doc or {}).get("Contents"):
            print(f"GOMOD-STORE OK stage={self.stage} bucket={bucket} holds objects; audit it with: {check}", flush=True)
            return {"bucket": bucket, "state": "filled"}
        print(f"GOMOD-STORE EMPTY stage={self.stage} bucket={bucket}: The deploy leaves this S3 bucket for Go modules (libraries) empty. "
              f"While it is empty, every Go module download in a run fails with HTTP 404; runs in other languages do not use it. "
              f"On a machine with Go, you fill it once with {gomod_store.fill_command(self.stage)} and check it with {check}", flush=True)
        return {"bucket": bucket, "state": "empty"}

    def invocation_logging(self) -> dict:
        """Home-Region model invocation logging is enabled only for
        an `enable-and-restore` entry (acct-a01), only when it is unset, and only after the prior state is
        written to SSM /hamh/<stage>/invocation-logging-prior (never overwritten; teardown.sh restores it).
        A `read-only` entry (ham-acct-b02) is read and recorded, never written; a destination that is set is
        read and never overwritten. The outcome goes into the deploy record."""
        group = f"/hamh/{self.stage}/bedrock-invocations"
        cur = (self.aws("bedrock", "get-model-invocation-logging-configuration") or {}).get("loggingConfig")
        if cur:
            existing = (cur.get("cloudWatchConfig") or {}).get("logGroupName") or (cur.get("s3Config") or {}).get("bucketName")
            if existing == group:
                say(f"invocation logging already ours: {group}")
                return {"outcome": "ours", "logGroup": group}
            say(f"invocation logging points at {existing}; not overwritten")
            return {"outcome": "foreign", "logGroup": existing}
        if self.target.invocation_logging == "read-only":
            say(f"invocation logging unset; {self.profile} is read-only (never written)")
            return {"outcome": "unset-read-only", "logGroup": None}
        prior = PRIOR_LOGGING_PARAM.format(stage=self.stage)
        if self.aws("ssm", "get-parameter", "--name", prior, ok_missing=("ParameterNotFound",)):
            say(f"invocation logging prior state already recorded at {prior}")
        else:
            self.put_param(prior, json.dumps({"loggingConfig": None}), overwrite=False)
            say(f"invocation logging prior state (unset) recorded at {prior}")
        config = {
            "cloudWatchConfig": {"logGroupName": group, "roleArn": f"arn:aws:iam::{self.account}:role/{self.prefix}-bedrock-logging"},
            "textDataDeliveryEnabled": True, "imageDataDeliveryEnabled": False,
            "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False,
        }
        self.aws("bedrock", "put-model-invocation-logging-configuration", "--logging-config", json.dumps(config))
        got = (self.aws("bedrock", "get-model-invocation-logging-configuration") or {}).get("loggingConfig") or {}
        if (got.get("cloudWatchConfig") or {}).get("logGroupName") != group:
            raise Fail("invocation-logging", "the configuration did not take")
        say(f"invocation logging enabled: {group}")
        return {"outcome": "enabled", "logGroup": group, "prior": None}

    def running_runs(self) -> list[str]:
        """Names of the RUNNING executions of hamh-<stage>-run (one per run, named by the run id)."""
        arn = f"arn:aws:states:{self.region}:{self.account}:stateMachine:{self.prefix}-run"
        doc = self.aws("stepfunctions", "list-executions", "--state-machine-arn", arn, "--status-filter", "RUNNING",
                       ok_missing=("StateMachineDoesNotExist",))
        return sorted(e["name"] for e in (doc or {}).get("executions", []))

    def guard(self) -> None:
        """Every run pins the worker runtime version it started on; a new version waits."""
        runs = self.running_runs()
        if runs:
            shown = " ".join(runs[:20]) + (f" (+{len(runs) - 20} more)" if len(runs) > 20 else "")
            raise Fail("guard", f"{len(runs)} running run(s) on {self.prefix}-run pin the worker runtime: {shown}",
                       action="Wait until they end, then deploy again.")

    def report_config_record(self) -> None:
        """Says when the account file in use is not the one the stage's earlier deploy recorded (SSM /hamh/<stage>/deployment-config-digest,
        `committed` or a SHA-256). A report, never a failure: a changed file can be intended, and nothing below depends on the earlier value."""
        name = f"/hamh/{self.stage}/deployment-config-digest"
        doc = self.aws("ssm", "get-parameter", "--name", name, ok_missing=("ParameterNotFound",))
        earlier = ((doc or {}).get("Parameter") or {}).get("Value") or None
        where = "the committed accounts.json" if self.config_digest == "committed" else f"HAMH_ACCOUNTS_FILE={aws_target.env_get(aws_target.FILE_HOOK)}"
        say(f"deployment config {self.config_digest[:12]} ({where})")
        if earlier and earlier != self.config_digest:
            say(f"deployment config {self.config_digest[:12]} differs from the earlier record {earlier[:12]} of stage {self.stage}: "
                f"the account file in use is not the one the last deploy recorded in {name}")

    def print_settings(self) -> None:
        """--print-settings: the resolution of the stage settings alone, read-only. It checks the caller's account, then resolves every setting from
        the record, or from the live stage when it has no record, and prints the DEPLOY-SETTING lines; a reader that fails, the Instances refusal and a change
        that needs consent raise their refusal as a deploy would. It writes nothing: no record, no lock, no stack call, no build, no deploy record. Its AWS calls
        are reads: sts get-caller-identity, ssm get-parameter (the record), cloudformation get-template (each stack) and describe-stacks (the runtimes
        stack, and every stack but the network stack for the keep tag), and bedrock-agentcore-control get-agent-runtime-endpoint and get-agent-runtime (the live worker runtime, for the tool layer)."""
        self.check_account(operator=False)
        consent = self.settle_settings()
        self.refuse_instances_off(self.compute_instances_state())
        self.require_consent(consent)

    def check_guard(self) -> str:
        self.check_account()
        self.guard()
        return f"DEPLOY-GUARD PASS stage={self.stage} running=0"

    def put_param(self, name: str, value: str, overwrite: bool = True) -> None:
        self.aws("ssm", "put-parameter", "--name", name, "--value", value, "--type", "String",
                 *(["--overwrite"] if overwrite else []))
        self.tag_param(name)

    def tag_param(self, name: str, env: dict | None = None) -> None:
        self.aws("ssm", "add-tags-to-resource", "--resource-type", "Parameter", "--resource-id", name, "--tags",
                 f"Key=project,Value={PROJECT}", f"Key=stage,Value={self.stage}",
                 f"Key=owner,Value={self.owner}", "Key=managed-by,Value=deploy.sh", env=env)

    # ----: the stage settings, the settings record and the deploy lock -----------------------------------------------

    def stage_templates(self) -> dict:
        """The deployed template of every stack of the stage (GetTemplate, Original), read once per deploy: a dict, None for a stack that does not exist,
        or the Fail of a read that failed (a live value it would hold is then unknown)."""
        if getattr(self, "_templates", None) is not None:
            return self._templates
        out: dict = {}
        for short in digests.STACK_NAMES:
            try:
                doc = self.aws("cloudformation", "get-template", "--stack-name", f"{self.prefix}-{short}", "--template-stage", "Original",
                               ok_missing=("does not exist",))
            except Fail as e:
                out[short] = e
                continue
            body = doc.get("TemplateBody") if doc else None
            if isinstance(body, str):
                try:
                    body = json.loads(body)
                except ValueError:
                    out[short] = Fail("aws", f"the template of {self.prefix}-{short} is not JSON")
                    continue
            out[short] = None if doc is None else (body if isinstance(body, dict) else {})
        self._templates = out
        return out

    def stage_exists(self) -> bool:
        return any(t is not None for t in self.stage_templates().values())

    def resources(self, short: str, kind: str) -> list[dict]:
        """The properties of every resource of type `kind` in the live template of stack `short` (none for a stack that does not exist)."""
        t = self.stage_templates()[short]
        if isinstance(t, Fail):
            raise t
        return [r.get("Properties") or {} for r in ((t or {}).get("Resources") or {}).values() if isinstance(r, dict) and r.get("Type") == kind]

    @staticmethod
    def scalar(value) -> str:
        """A plain value of a template as text; anything else (an intrinsic function, a list) is no value the deploy can keep."""
        if isinstance(value, bool) or not isinstance(value, (str, int, float)) or (isinstance(value, float) and not value.is_integer()):
            raise LiveUnknown(f"the template holds {json.dumps(value)[:80]} where a plain value belongs")
        return str(int(value)) if isinstance(value, float) else str(value)

    def function_env(self, short: str, function: str) -> dict | None:
        """The environment of the Lambda function named `function` in the live template of stack `short`; None when there is no such function."""
        found = [f for f in self.resources(short, "AWS::Lambda::Function") if f.get("FunctionName") == function]
        if len(found) > 1:
            raise LiveUnknown(f"the {short} template holds {len(found)} functions named {function}")
        return None if not found else ((found[0].get("Environment") or {}).get("Variables") or {})

    def stack_tags(self, short: str) -> dict[str, str] | None:
        """The tags CloudFormation holds for stack `short` (DescribeStacks); None when the stack does not exist or holds nothing (a create that rolled back)."""
        doc = self.aws("cloudformation", "describe-stacks", "--stack-name", f"{self.prefix}-{short}", ok_missing=("does not exist",))
        stack = ((doc or {}).get("Stacks") or [{}])[0]
        if not stack or stack.get("StackStatus") in EMPTY_STACK_STATUSES:
            return None
        return {str(t.get("Key", "")): str(t.get("Value", "")) for t in stack.get("Tags") or [] if isinstance(t, dict)}

    def stage_log_group(self, props: dict) -> bool:
        """Whether a log group of a live template is one whose retention the stage sets (Addendum 4, A4.2). The CDK app creates each of those as a CfnLogGroup
        with cfg.logRetentionDays and a literal name that carries the stage's name: `/hamh/<stage>/...` (the network, invocation and image-build logs) or
        `/aws/<service>/hamh-<stage>-<name>` (the Lambda functions and the pull request check). A group that a CDK construct makes for itself has no such
        name: the provider function of AwsCustomResource in the network and runtimes stacks gets a group named by `Fn::Join` over the function's reference,
        with the construct's own 731 days, which no stage setting reaches. Reading it made every stage answer with two values (30 and 731).
        scripts/tests/test_setup_deploy_settings.py holds this rule to the log groups of infra/lib/stacks."""
        name = props.get("LogGroupName")
        return isinstance(name, str) and (name.startswith(f"/hamh/{self.stage}/") or f"/{self.prefix}-" in name)

    def tavily_alarms(self) -> list[dict]:
        return [a for a in self.resources("control", "AWS::CloudWatch::Alarm") if str(a.get("AlarmName", "")).startswith(f"{self.prefix}-research-tavily-")]

    def live_setting(self, name: str) -> str | None:
        """The value the live stage runs for setting `name`, or None when the stage holds none (the module docstring names the source of each setting).
        Raises Fail when a read failed and LiveUnknown when the stage answers with more than one value."""
        p = self.prefix
        if name == "HAMH_COMPUTE_INSTANCES":
            outputs = self.outputs(f"{p}-runtimes")
            if not outputs and self.stage_templates()["runtimes"] is None:
                return None
            return "on" if INSTANCES_PROVIDER_OUTPUT in outputs or f"{INSTANCES_RUNTIME}RuntimeId" in outputs else "off"
        if name == "HAMH_TOOLS_MODE":
            return self.current_tools_mode
        if name in INSTANCES_SETTINGS:
            providers = self.resources("runtimes", PROVIDER_TYPE)
            if len(providers) > 1:
                raise LiveUnknown(f"the runtimes template holds {len(providers)} capacity providers")
            if not providers:
                return None
            ec2 = (providers[0].get("ComputeConfiguration") or {}).get("Ec2Configuration") or {}
            if name == "HAMH_INSTANCES_TYPES":
                types = (((ec2.get("LaunchTemplateSource") or {}).get("LaunchParameters") or {}).get("InstanceRequirements") or {}).get("AllowedInstanceTypes")
                return ",".join(self.scalar(t) for t in types) if isinstance(types, list) and types else None
            volumes = ec2.get("Volumes") or [{}]
            value = {"HAMH_INSTANCES_ROOT_FREE_GIB": (ec2.get("RootVolume") or {}).get("FreeSpaceGiB"),
                     "HAMH_INSTANCES_WORKSPACE_GIB": ((volumes[0] if isinstance(volumes[0], dict) else {}).get("EbsConfiguration") or {}).get("SizeGiB"),
                     "HAMH_INSTANCES_LIFETIME_S": (ec2.get("LifecycleConfiguration") or {}).get("MaxLifetime"),
                     "HAMH_INSTANCES_IDLE_S": (ec2.get("LifecycleConfiguration") or {}).get("IdleInstanceTimeout")}[name]
            return None if value is None else self.scalar(value)
        run_session = {"HAMH_UTILITY_MODEL": "UTILITY_MODEL", "HAMH_UTILITY_PURPOSES": "UTILITY_PURPOSES", "HAMH_RESEARCH_PROVIDER": "RESEARCH_PROVIDER",
                       "HAMH_PROVIDER_SILENCE_S": "HAMH_PROVIDER_SILENCE_S", "HAMH_SLICE_DEADLINE_S": "HAMH_SLICE_DEADLINE_S"}
        if name in run_session:
            env = self.function_env("orchestration", f"{p}-run-session") or {}
            value = env.get(run_session[name])
            return None if value in (None, "") else self.scalar(value)
        if name == "HAMH_RESEARCH_ALARM_TOPIC_ARN":
            actions = {tuple(self.scalar(a) for a in (alarm.get("AlarmActions") or [])) for alarm in self.tavily_alarms()}
            if len(actions) > 1 or any(len(a) > 1 for a in actions):
                raise LiveUnknown("the alarms on the stored Tavily key send to more than one topic")
            got = next(iter(actions), ())
            return got[0] if got else None
        if name == "HAMH_TAVILY_ALARMS":
            if self.tavily_alarms():
                return "on"
            research = self.function_env("control", f"{p}-research-api")
            provider = (research or {}).get("RESEARCH_PROVIDER") or SETTING_DEFAULTS["HAMH_RESEARCH_PROVIDER"]
            return "off" if research is not None and provider in TAVILY_PROVIDERS else None
        if name == "HAMH_TAVILY_KEY_MAX_AGE_DAYS":
            ages = [a for a in self.tavily_alarms() if str(a.get("AlarmName", "")).endswith("-key-age")]
            if len(ages) > 1:
                raise LiveUnknown("the control template holds more than one key-age alarm")
            return self.scalar(ages[0]["Threshold"]) if ages and ages[0].get("Threshold") is not None else None
        if name == "HAMH_LOG_RETENTION_DAYS":
            days = {self.scalar(g["RetentionInDays"]) for short in digests.STACK_NAMES for g in self.resources(short, "AWS::Logs::LogGroup")
                    if self.stage_log_group(g) and g.get("RetentionInDays") is not None}
            if len(days) > 1:
                raise LiveUnknown(f"the log groups of the stage keep their events for different numbers of days ({', '.join(sorted(days, key=int))})")
            return next(iter(days), None)
        if name in ("HAMH_PR_CHECK_MINUTES", "HAMH_PR_CHECK_COMPUTE"):
            projects = [b for b in self.resources("delivery", "AWS::CodeBuild::Project") if b.get("Name") == f"{p}-prcheck"]
            if len(projects) > 1:
                raise LiveUnknown(f"the delivery template holds {len(projects)} projects named {p}-prcheck")
            if not projects:
                return None
            if name == "HAMH_PR_CHECK_MINUTES":
                minutes = projects[0].get("TimeoutInMinutes")
                return None if minutes is None else self.scalar(minutes)
            compute = self.scalar((projects[0].get("Environment") or {}).get("ComputeType") or "")
            return compute.removeprefix("BUILD_GENERAL1_").lower() or None
        if name == "HAMH_AGENTCORE_STORAGE_ACCOUNTS":
            conditions = [(stmt.get("Condition") or {}).get("StringEquals") or {} for ep in self.resources("network", "AWS::EC2::VPCEndpoint")
                          for stmt in ((ep.get("PolicyDocument") or {}).get("Statement") or []) if isinstance(stmt, dict) and stmt.get("Sid") == "AgentCoreSessionStorage"]
            if len(conditions) > 1:
                raise LiveUnknown("the network template holds more than one session storage statement")
            owners = conditions[0].get("aws:ResourceAccount") if conditions else None
            if not owners:
                return None
            return f"{self.region}={'+'.join(self.scalar(a) for a in ([owners] if isinstance(owners, str) else owners))}"
        if name == "HAMH_KEEP_TAG":
            words: dict[str, str] = {}
            for short in digests.STACK_NAMES:
                tags = None if short in KEEP_TAG_EXCEPTION_STACKS else self.stack_tags(short)
                if tags is not None:
                    words[f"{p}-{short}"] = "on" if tags.get(KEEP_TAG_KEY) == KEEP_TAG_VALUE else "off"
            if len(set(words.values())) > 1:
                carry = ", ".join(s for s, w in words.items() if w == "on")
                lack = ", ".join(s for s, w in words.items() if w == "off")
                raise LiveUnknown(f"the stacks of the stage do not agree on the tag {KEEP_TAG_KEY}={KEEP_TAG_VALUE}: {carry} carry it, {lack} do not")
            return next(iter(words.values()), None)
        if name == "HAMH_STAGE_KIND":
            kinds: dict[str, str] = {}
            for function in (f"{p}-{f}" for f in RESEARCH_FUNCTIONS):
                env = self.function_env("control", function)
                if env is None:
                    continue
                if env.get("HAMH_STAGE_KIND") in (None, ""):
                    raise LiveUnknown(f"the function {function} of the control template has no HAMH_STAGE_KIND")
                kinds[function] = self.scalar(env["HAMH_STAGE_KIND"])
            if len(set(kinds.values())) > 1:
                raise LiveUnknown("the research functions of the stage have different kinds (" + ", ".join(f"{f} {k}" for f, k in kinds.items()) + ")")
            return next(iter(kinds.values()), None)
        raise ValueError(f"no live reader for {name}")

    def utility_catalog(self) -> dict:
        """crates/ham-core/assets/utility-models.json of the tree being deployed (the file the CDK app reads), else of this program's checkout."""
        if not hasattr(self, "_catalog"):
            self._catalog = {}
            for path in (self.root / UTILITY_CATALOG, Path(__file__).resolve().parents[2] / UTILITY_CATALOG):
                try:
                    self._catalog = json.loads(path.read_text(encoding="utf-8"))
                    break
                except (OSError, ValueError):
                    continue
        return self._catalog

    def utility_row(self, name: str) -> dict | None:
        rows = [m for m in self.utility_catalog().get("models") or [] if isinstance(m, dict)]
        return next((m for m in rows if m.get("id") == name), None) or next((m for m in rows if m.get("alias") == name.lower()), None)

    def utility_default(self) -> str:
        """What an unset HAMH_UTILITY_MODEL comes to on this stage (config.ts resolveUtilityModel): the catalog's default row when it is smoked in the
        stage Region, else off."""
        row = self.utility_row(str(self.utility_catalog().get("default") or ""))
        return row["id"] if row and row.get("region") == self.region else "off"

    def utility_purposes_of(self, model: str) -> str:
        """Every purpose the utility model's row holds a smoke record for, in catalog order (config.ts resolveUtilityPurposes), or off."""
        if model == "off":
            return "off"
        row = self.utility_row(model) or {}
        held = row.get("purposes") if isinstance(row.get("purposes"), dict) else {}
        return ",".join(w for w in UTILITY_PURPOSE_WORDS if w == "run-name" or str((held.get(w) or {}).get("smokeRecordId") or "").strip())

    def effective(self, name: str, value: str | None, model: str | None = None) -> str:
        """What a value comes to on this stage, so two can be compared: `default` (or None) is the code default; the utility model's alias is its id; the
        purposes are in catalog order, off with the model (`model`: the utility model they belong to); the storage accounts are the entry of the stage Region."""
        if name == "HAMH_UTILITY_MODEL":
            if value in (None, "default"):
                return self.utility_default()
            if value.lower() == "off":
                return "off"
            row = self.utility_row(value)
            return row["id"] if row else value
        if name == "HAMH_UTILITY_PURPOSES":
            model = self.utility_default() if model is None else model
            if model == "off" or value == "off":
                return "off"
            if value in (None, "default"):
                return self.utility_purposes_of(model)
            given = {w.strip(TOOLS_SPACE).lower() for w in value.split(",")}
            return ",".join(w for w in UTILITY_PURPOSE_WORDS if w in given)
        value = self.default_of(name) if value in (None, "default") else value
        if not value:
            return ""
        value = canonical(name, value)
        if name == "HAMH_AGENTCORE_STORAGE_ACCOUNTS":
            return next((e for e in value.split(",") if e.split("=", 1)[0] == self.region), "")
        return value

    def default_of(self, name: str) -> str:
        """The code default of a setting on this stage: the keep tag's and the stage kind's from the role of the account entry (ROLE_DEFAULTS), the others'
        from SETTING_DEFAULTS (the utility model's: `effective`)."""
        return ROLE_DEFAULTS[name][self.target.role] if name in ROLE_DEFAULTS else SETTING_DEFAULTS[name]

    @staticmethod
    def needs_consent(change: str, old: str, new: str) -> bool:
        if change == "replace":
            return True
        if change == "decrease":
            return int(new) < int(old)
        return change == "remove" and new in ("", "off")

    def record_deploying(self) -> bool:
        return ((self.record or {}).get("fields") or {}).get("status") == "deploying"

    def earlier_intent(self, name: str) -> bool:
        """Whether the record of a deploy that did not finish (`status=deploying`) holds the user's explicit choice of that deploy for setting `name`, so
        the retry needs no new consent for it. Such a record names on its `explicit=` line the settings that deploy got from its environment; for those, what
        the record holds (its `set.` line, or no line after `default`) is that choice. A record that a release before the line wrote (`legacy_deploying`) keeps
        the rule of that release, which kept no value by itself: each `set.` line it holds is the choice."""
        fields = (self.record or {}).get("fields") or {}
        if fields.get("status") != "deploying":
            return False
        if "explicit" not in fields:
            return name in ((self.record or {}).get("sets") or {})
        return name in fields["explicit"].split(",")

    def legacy_deploying(self) -> bool:
        """A record of a deploy that did not finish, written by a release before the `explicit=` line. For a setting it has no `set.` line for it cannot
        say whether that deploy was given `default`, so the deploy keeps no value of such a setting by itself and asks for consent, as that release did."""
        fields = (self.record or {}).get("fields") or {}
        return fields.get("status") == "deploying" and "explicit" not in fields

    def consented(self, name: str) -> bool:
        """An explicit value is the consent; so is the earlier explicit intent of a deploy that did not finish (`earlier_intent`)."""
        source = (self.settings_state.get(name) or {}).get("source")
        return source == "explicit" or self.earlier_intent(name)

    def settle_settings(self) -> list[str]:
        """Resolves every stage setting (explicit, else the record's `set.` line, else the live value of a stage without a record, else the code
        default), prints its DEPLOY-SETTING line and hands the values to every cdk call. Returns the settings whose change needs consent (C12); the deploy
        refuses them after the Instances check. A live value it needs and cannot read refuses here (`settings-read`): that is a stage without a record only.
        On a stage with a record the record decides (A4.2), and a live value that cannot be read or is ambiguous is said and compared with nothing."""
        self.record = self.read_settings_record()
        exists = self.stage_exists()
        sets = (self.record or {}).get("sets") or {}
        state: dict[str, dict] = {}
        unknown: list[tuple[str, str]] = []
        notes: list[str] = []
        live_model = new_model = None
        for st in self.settings:
            live, why = None, None
            if exists:
                try:
                    live = self.live_setting(st.name)
                    if live is not None:
                        live = canonical(st.name, live)
                        if not (re.fullmatch(st.pattern, live) or (st.name == "HAMH_UTILITY_PURPOSES" and live == "off")):
                            raise LiveUnknown(f"the live value {live} is not one this release takes")
                except Fail as e:
                    why = e.detail or str(e)
                except (LiveUnknown, ValueError) as e:
                    why = str(e)
            context = live_model if st.name == "HAMH_UTILITY_PURPOSES" else None
            explicit = self.explicit.get(st.name)
            if explicit is not None:
                source, value = "explicit", explicit
            elif st.kept and st.name in sets:
                source, value = "kept", sets[st.name]
                if value != "default" and not re.fullmatch(st.pattern, value):
                    unknown.append((st.name, f"the settings record holds set.{st.name}={value}, which is not a value this release takes"))
                    continue
            elif st.kept and self.record is None and exists:
                if why is not None:
                    unknown.append((st.name, why))
                    continue
                keep = live is not None and self.effective(st.name, live, context) != self.effective(st.name, "default", context)
                source, value = ("live", live) if keep else ("default", "default")
            else:
                source, value = "default", "default"
            if exists and why is not None and source != "explicit" and st.change in CONSENT_CLASSES:
                if self.record is None:
                    unknown.append((st.name, why))
                    continue
                # A4.2: the record decides the value of a stage that has one; the live value would only have been compared with it.
                notes.append(f"the live value of {st.name} is not known ({why}); the settings record decides it")
            new = self.effective(st.name, value, new_model if st.name == "HAMH_UTILITY_PURPOSES" else None)
            old = None
            if exists and why is None:
                old = self.effective(st.name, live if live is not None else sets.get(st.name), context)
            if st.name == "HAMH_UTILITY_MODEL":
                live_model, new_model = (old if old is not None else self.effective(st.name, sets.get(st.name))), new
            held = None
            if (source == "default" and st.kept and st.change in CONSENT_CLASSES and self.record is not None and live is not None and old is not None
                    and old != new and not self.earlier_intent(st.name) and self.needs_consent(st.change, old, new)):
                if self.legacy_deploying():
                    # The record of a deploy that did not finish cannot say whether that deploy was given `default` here, so nothing is kept by itself and
                    # the consent check below asks, as the release that wrote the record did.
                    notes.append(f"{st.name}: the settings record is of a deploy that did not finish, and the release that wrote it does not say whether that "
                                 f"deploy was given {st.name}=default, so this deploy does not keep {shown(live)} by itself and asks for your consent")
                else:
                    # The record follows the default and this release's default differs from what the stage runs; taking it would replace resources,
                    # delete data or remove something that nothing exported asks for. The stage keeps its value, as its own, and the line names the export.
                    held, source, value, new = new, "kept", live, old
            state[st.name] = {"value": value, "source": source, "old": old, "new": new, "live": live, "held": held}
        if unknown:
            names = [n for n, _ in unknown]
            raise Fail("settings-read", "the deploy cannot tell the stage's value of " + "; ".join(f"{n} ({why})" for n, why in unknown) + ".",
                       action="; ".join(f"export {n}=<value>, or {n}=default" for n in names) + ".")
        for note in notes:
            say(note)
        consent = []
        for st in self.settings:
            got = state[st.name]
            line = f"DEPLOY-SETTING {st.name}={got['value']} source={got['source']}"
            if got["held"] is not None:
                line += f" new-default={shown(got['held'])} ({st.change}) :: {held_sentence(st.name, got['value'], got['held'], st.change)}"
            elif got["old"] is not None and got["old"] != got["new"]:
                line += f" change={shown(got['old'])} -> {shown(got['new'])} ({st.change})"
                if (st.change in CONSENT_CLASSES and got["live"] is not None and got["source"] != "explicit"
                        and not self.earlier_intent(st.name) and self.needs_consent(st.change, got["old"], got["new"])):
                    consent.append(st.name)
            print(line, flush=True)
            if st.name in CONCRETE_SETTINGS:
                continue
            if got["value"] == "default":
                self.env.pop(st.name, None)
            else:
                self.env[st.name] = got["value"]
        self.settings_state = state
        self.tools_mode = state["HAMH_TOOLS_MODE"]["new"]
        self.compute_instances = state["HAMH_COMPUTE_INSTANCES"]["new"] == "on"
        self.env["HAMH_TOOLS_MODE"] = self.tools_mode
        self.env["HAMH_COMPUTE_INSTANCES"] = "on" if self.compute_instances else "off"
        return consent

    def require_consent(self, names: list[str]) -> None:
        """C12: a change that replaces resources, deletes data or removes something, which no explicit value asks for, is refused before any stack change."""
        if not names:
            return
        changes = "; ".join(f"{n} goes from {shown(self.settings_state[n]['old'])} to {shown(self.settings_state[n]['new'])} ({self.setting_by_name[n].change})"
                            for n in names)
        raise Fail("consent", f"{changes}, and nothing you set asks for it.", action="; ".join(self.consent_exports(n) for n in names))

    def consent_exports(self, name: str) -> str:
        """A4.12: the exports that answer a consent refusal of setting `name`, each with what it gives: the record's value (when the record holds one: what
        the user chose), the value the live stage has now, and `default` with the value it comes to. An export that gives the same value as one named
        before it is left out, so no two of them lead to the same stage."""
        got = self.settings_state[name]
        exports, given = [], []
        if got["source"] == "kept":
            exports.append(f"{name}={got['value']} to keep what you chose")
            given.append(got["new"])
        if got["old"] not in given:
            exports.append(f"{name}={got['old']} to keep what the stage has now")
            given.append(got["old"])
        default = self.effective(name, "default")
        if default not in given:
            exports.append(f"{name}=default for {shown(default)}")
        return "export " + (", ".join(exports[:-1]) + ", or " + exports[-1] if len(exports) > 1 else exports[0])

    def require_provider_consent(self, replaced: tuple[str, str]) -> None:
        """C12 after the synth: a capacity provider whose synthesized name differs from the live one (a new release changed its shape) replaces the provider
        and deletes the machines on it; an explicit Instances machine setting (INSTANCES_SETTINGS) is the consent."""
        if any(self.consented(n) for n in INSTANCES_SETTINGS):
            return
        types = (self.settings_state.get("HAMH_INSTANCES_TYPES") or {}).get("new") or SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"]
        raise Fail("consent", f"this deploy builds the Instances capacity provider as {replaced[1]}, which replaces the live {replaced[0]} and deletes the machines "
                              "on it, and nothing you set asks for it.",
                   action=f"Export one of the Instances machine settings ({', '.join(INSTANCES_SETTINGS)}) to accept the replacement, for example "
                          f"HAMH_INSTANCES_TYPES={types}, then deploy again.")

    def read_settings_record(self) -> dict | None:
        """The stage's settings record (C2) as {fields, sets, other}; None when the stage has none. `other` holds the `set.` lines of settings this release
        does not keep, written back as they are."""
        name = SETTINGS_PARAM.format(stage=self.stage)
        fix = f"Correct or delete the parameter {name}, then deploy again."
        try:
            doc = self.aws("ssm", "get-parameter", "--name", name, ok_missing=("ParameterNotFound",))
        except Fail as e:
            raise Fail("settings-read", f"the settings record {name} cannot be read ({e.detail}).", action="Let the caller read it, then deploy again.") from None
        if not doc:
            return None
        fields: dict[str, str] = {}
        sets: dict[str, str] = {}
        other: list[str] = []
        for line in ((doc.get("Parameter") or {}).get("Value") or "").splitlines():
            if not line.strip():
                continue
            key, sep, value = line.partition("=")
            if not sep:
                raise Fail("settings-read", f"the settings record {name} holds a line that is not key=value.", action=fix)
            st = self.setting_by_name.get(key[len("set."):]) if key.startswith("set.") else None
            if key.startswith("set."):
                if st is not None and st.kept:
                    sets[st.name] = value
                else:
                    other.append(line)
            else:
                fields[key] = value
        schema = fields.get("schema", "")
        if schema != SETTINGS_SCHEMA:
            if schema.isdigit() and int(schema) > int(SETTINGS_SCHEMA):
                raise Fail("record-downgrade", f"the settings record {name} has schema {schema}: a newer release than this one deployed the stage "
                                               f"(commit {fields.get('commit') or 'unknown'}, version {fields.get('version') or 'unknown'}).",
                           action="Deploy that release or a newer one (git pull), then deploy again.")
            raise Fail("settings-read", f"the settings record {name} has no schema={SETTINGS_SCHEMA} line.", action=fix)
        return {"fields": fields, "sets": sets, "other": other}

    def operator_arn(self) -> str:
        op = self.target.operator
        return f"arn:aws:iam::{self.account}:{op.kind}/{op.name}" if op is not None else ""

    def version_text(self) -> str:
        try:
            return (self.root / "VERSION").read_text(encoding="utf-8").strip()
        except OSError:
            return ""

    @staticmethod
    def source_commit(head: str) -> str:
        """The commit the settings record and the deploy lock name (Addendum 4, A4.1): HAMH_SOURCE_COMMIT when it holds 40 hex digits (written in lower
        case, as git prints a commit), else `head`. The setup runner builds from an archive of the user's commit and commits that archive in a repository
        of its own, so its HEAD is a commit no clone holds; a record that named it stopped every later update at the commit comparison."""
        given = os.environ.get(SOURCE_COMMIT_VARIABLE, "").strip(TOOLS_SPACE)
        return given.lower() if HEX40.fullmatch(given) else head

    def write_settings_record(self, status: str) -> None:
        """C2: the record of what the stage runs and of the settings a user chose (never a code default), through the tagged put of the digests. A4.7: the
        keep tag and the stage kind are always recorded, with the word the stage runs, because their default is the word of the entry's role and the entry
        of the next deploy may have another role."""
        lines = [f"schema={SETTINGS_SCHEMA}", f"status={status}", f"commit={self.commit}", f"tree={self.tree}", f"version={self.version_text()}",
                 f"accounts={self.config_digest}", f"operator={self.operator_arn()}", f"azs={','.join(self.target.az_ids[self.stage])}",
                 f"updated={dt.datetime.now(dt.timezone.utc):%Y-%m-%dT%H:%M:%SZ}"]
        # What this deploy got from its environment, with the earlier intent it carries: the consent of a retry if this deploy does not finish.
        lines.append("explicit=" + ",".join(st.name for st in self.settings
                                            if (self.settings_state.get(st.name) or {}).get("source") == "explicit" or self.earlier_intent(st.name)))
        for st in self.settings:
            got = self.settings_state.get(st.name) or {}
            if st.name in ROLE_DEFAULTS and got.get("new"):
                lines.append(f"set.{st.name}={got['new']}")
            elif st.kept and got.get("source") in ("explicit", "kept", "live") and got.get("value") not in (None, "default"):
                lines.append(f"set.{st.name}={got['value']}")
        lines += (self.record or {}).get("other") or []
        self.put_param(SETTINGS_PARAM.format(stage=self.stage), "\n".join(lines))
        say(f"settings record {SETTINGS_PARAM.format(stage=self.stage)} status={status}")

    def begin_changes(self) -> None:
        """Right before the first stack change (the bootstrap of an account that has none, else the first cdk deploy): the record says `deploying`. After
        this point no refusal is left; a failure is DEPLOY-FAIL."""
        if self.changed:
            return
        self.write_settings_record("deploying")
        self.changed = True

    def lock_name(self) -> str:
        return LOCK_PARAM.format(stage=self.stage)

    def read_lock(self, env: dict | None = None) -> dict | None:
        doc = self.aws("ssm", "get-parameter", "--name", self.lock_name(), ok_missing=("ParameterNotFound",), env=env)
        if not doc:
            return None
        return dict(line.split("=", 1) for line in ((doc.get("Parameter") or {}).get("Value") or "").splitlines() if "=" in line)

    @staticmethod
    def describe_lock(held: dict) -> str:
        return ", ".join(f"{k} {held.get(k) or 'unknown'}" for k in ("holder", "started", "route", "commit"))

    @staticmethod
    def renew_interval() -> float:
        """LOCK_RENEW_S, or the shorter interval DEPLOY_LOCK_RENEW_S names (a test hook: it can shorten the interval, never lengthen it)."""
        try:
            given = float(os.environ.get(LOCK_RENEW_VARIABLE) or LOCK_RENEW_S)
        except ValueError:
            return LOCK_RENEW_S
        return given if 0 < given <= LOCK_RENEW_S else LOCK_RENEW_S

    def lock_time(self) -> str:
        return self.clock().strftime("%Y-%m-%dT%H:%M:%SZ")

    def lock_value(self) -> str:
        return "\n".join(f"{k}={self.lock_fields[k]}" for k in LOCK_FIELDS)

    def start_lock_keeper(self) -> None:
        """A4.3: a daemon thread renews the lock this deploy took while the deploy runs, the long waits in cdk, cargo and CodeBuild included.
        `release_lock` stops it; a deploy that is killed takes it along, and its lock then ages like any lock nobody renews."""
        self.lock_stop = threading.Event()
        self.lock_env = dict(self.env)
        self.lock_thread = threading.Thread(target=self.keep_lock, args=(self.lock_stop.wait,), name="deploy-lock-keeper", daemon=True)
        self.lock_thread.start()

    def keep_lock(self, wait) -> None:
        """Renews the lock every time `wait(<seconds>)` ends without the stop signal (`wait` returns True when the deploy ends), until a renewal finds
        that the lock is no longer this deploy's."""
        while not wait(self.lock_renew_s):
            if not self.renew_lock():
                return

    def renew_lock(self) -> bool:
        """One renewal of the lock this deploy took: a new `started`, written over the lock only while the lock still holds this deploy's token. A lock
        that is gone is taken again (create-only, tagged). False when the lock belongs to another deploy by now, or the deploy has ended: nothing is
        written and the renewals stop. A call that fails is said and tried again at the next interval; it never ends the deploy."""
        name, env = self.lock_name(), self.lock_env
        try:
            held = self.read_lock(env=env)
            if held is not None and held.get("token") != self.lock_fields["token"]:
                say(f"the deploy lock {name} now belongs to another deploy ({self.describe_lock(held)}); it is left alone and this deploy stops renewing it")
                return False
            if self.lock_stop.is_set():
                return False
            self.lock_fields["started"] = self.lock_time()
            put = ("ssm", "put-parameter", "--name", name, "--value", self.lock_value(), "--type", "String")
            if held is not None:
                self.aws(*put, "--overwrite", env=env)
            elif self.aws(*put, ok_missing=("ParameterAlreadyExists",), env=env) is not None:
                # Create-only, as in `take_lock`: when another deploy took the name this moment, the next renewal reads its token and stops.
                self.tag_param(name, env=env)
                say(f"the deploy lock {name} was gone while this deploy runs and is taken again")
        except (Fail, OSError, ValueError, RuntimeError) as e:
            say(f"the deploy lock {name} could not be renewed ({getattr(e, 'detail', '') or e}); the next try is in {self.lock_renew_s:g} s")
        return True

    def take_lock(self) -> None:
        """C6, the deploy side. With HAMH_DEPLOY_LOCK_TOKEN the caller (setup) holds the lock: the deploy only checks the token. Without it the deploy takes
        the lock itself, create-only (a lock that exists is someone else's), tagged, with route=manual, and keeps its `started` fresh (`start_lock_keeper`);
        `release_lock` deletes it when the deploy ends."""
        name = self.lock_name()
        token = os.environ.get(LOCK_CALLER_VARIABLE, "").strip()
        if token:
            held = self.read_lock()
            if held is None:
                raise Fail("lock", f"{LOCK_CALLER_VARIABLE} is set, but the stage has no deploy lock {name}: the deploy that took it has ended, or its lock was replaced.",
                           action="Run the update again; it takes the lock anew.")
            if held.get("token") != token:
                raise Fail("lock", f"another deploy holds the deploy lock {name} ({self.describe_lock(held)}).",
                           action="It may be your own earlier setup. Wait until it ends, then deploy again.")
            say(f"deploy lock {name} held by the caller ({self.describe_lock(held)})")
            return
        self.lock_fields = dict(zip(LOCK_FIELDS, (secrets.token_hex(16), f"{socket.gethostname()}:{os.getpid()}", self.caller_arn, "manual", self.lock_time(),
                                                  self.commit, "", "")))
        # Create-only (no --overwrite): a name that exists is another deploy's lock.
        if self.aws("ssm", "put-parameter", "--name", name, "--value", self.lock_value(), "--type", "String", ok_missing=("ParameterAlreadyExists",)) is None:
            held = self.read_lock() or {}
            raise Fail("lock", f"another deploy holds the deploy lock {name} ({self.describe_lock(held)}).",
                       action=f"It may be your own earlier setup. Wait until it ends, then deploy again; when no deploy of this stage is running, delete the "
                              f"parameter {name} and deploy again.")
        self.lock_owned = True
        self.tag_param(name)
        say(f"deploy lock {name} taken (route manual)")
        self.start_lock_keeper()

    @contextlib.contextmanager
    def deploy_lock(self):
        """`take_lock` on entry, `release_lock` on exit, however the block ends. `take_lock` runs inside the try block: a call that fails after its create-only
        put (the tag call) releases the lock this deploy created, instead of leaving it to block the next deploy for 30 minutes."""
        try:
            self.take_lock()
            yield
        finally:
            self.release_lock()

    def release_lock(self) -> None:
        """Ends the lock this deploy took (`take_lock` without a token); a lock the caller holds is the caller's to delete. The keeper stops first. Then
        the lock is deleted only while it still holds this deploy's token (A4.3): a lock that another deploy replaced, as setup does with a lock it takes
        for abandoned, is left alone, and a line says so. SSM has no conditional delete, so the token is read right before the delete."""
        if not self.lock_owned:
            return
        self.lock_owned = False
        self.lock_stop.set()
        if self.lock_thread is not None:
            self.lock_thread.join(timeout=LOCK_JOIN_S)
            self.lock_thread = None
        name = self.lock_name()
        try:
            held = self.read_lock()
        except Fail as e:
            say(f"the deploy lock {name} could not be read ({e.detail or e}) and is left as it is; when it still names holder {self.lock_fields.get('holder')}, "
                "delete it before the next deploy of this stage")
            return
        if held is None:
            return
        if held.get("token") != self.lock_fields.get("token"):
            say(f"the deploy lock {name} now belongs to another deploy ({self.describe_lock(held)}); it is left alone")
            return
        proc = self.run([self.aws_bin, "ssm", "delete-parameter", "--name", name, "--region", self.region, "--output", "json"])
        if proc.returncode != 0 and "ParameterNotFound" not in proc.stderr:
            say(f"the deploy lock {name} could not be deleted ({proc.stderr.strip()[-200:]}); delete it before the next deploy of this stage")

    def record_assets(self) -> None:
        key = f"s3://{self.bucket}/deploy/{self.stage}/cdk-assets.json"
        prev = self.run([self.aws_bin, "s3", "cp", key, "-", "--region", self.region])
        files, images = set(self.assets["files"]), set(self.assets["images"])
        if prev.returncode == 0 and prev.stdout.strip():
            old = json.loads(prev.stdout)
            files |= {(f["bucket"], f["key"]) for f in old.get("files", [])}
            images |= {(i["repository"], i["tag"]) for i in old.get("images", [])}
        doc = {"files": [{"bucket": b, "key": k} for b, k in sorted(files)],
               "images": [{"repository": r, "tag": t} for r, t in sorted(images)]}
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(doc, f, indent=1)
        up = self.run([self.aws_bin, "s3", "cp", f.name, key, "--region", self.region, "--only-show-errors"])
        os.unlink(f.name)
        if up.returncode != 0:
            raise Fail("assets-record", up.stderr.strip()[-200:])
        say(f"recorded {len(doc['files'])} asset objects and {len(doc['images'])} asset images at {key}")

    def write_image_record(self, name: str, buildspec: str, repo: str, tag: str, image: str, envs: dict[str, str], *, build_id: str,
                           recipe: str | None = None) -> None:
        """The record of one image this deploy built on CodeBuild ARM. `build_id` is the id of the CodeBuild
        build (scripts/codebuild-run.sh prints it); R21 reads the build back to see it ran on an ARM environment. `recipe` is the
        digest of the binaries' build recipe (scripts/lib/agentd_build.py); a delivery image has none."""
        doc = self.aws("ecr", "describe-images", "--repository-name", repo, "--image-ids", f"imageTag={tag}",
                       ok_missing=("ImageNotFoundException",))
        size = ((doc or {}).get("imageDetails") or [{}])[0].get("imageSizeInBytes")
        crit = Path(os.environ.get("DEPLOY_CRITERIA", ""))
        now = dt.datetime.now(dt.timezone.utc)
        rec = {
            "schemaVersion": 1, "kind": "image-build", "createdAt": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "host": socket.getfqdn(), "account": self.account, "region": self.region, "stage": self.stage,
            "codeSha": self.git("rev-parse", "HEAD"), "sourceTree": digests.source_tree(self.root),
            "deploymentConfigDigest": self.config_digest,
            # Known once every stack is up; the deploy record that follows names it.
            "deploymentDigest": None, "imageDigest": image.split("@", 1)[1],
            "criteriaSha256": hashlib.sha256(crit.read_bytes()).hexdigest() if crit.is_file() else None,
            "result": "pass",
            "inputs": {"image": name, "buildspec": buildspec, "repository": repo, "imageTag": tag,
                       "binaries": {k: v for k, v in sorted(envs.items()) if k.endswith("_SHA256")},
                       **({"recipeDigest": recipe} if recipe else {}),
                       **({"langFamily": envs["LANG_FAMILY"]} if "LANG_FAMILY" in envs else {})},
            "measurements": {"buildHost": "codebuild", "codebuildBuildId": build_id, "image": image, "sizeBytes": size},
            "runIds": [], "notes": "built by scripts/codebuild-run.sh on the stage's CodeBuild ARM project",
        }
        if "CLINE_SHA256" in envs:
            # R24: "the value every worker image asserted at image build". The build COPYs the promoted fork after
            # `sha256sum -c` of build/cline against CLINE_SHA256, and ensure_fork verified that value, the lock's binarySha256, against
            # the S3 object the build downloads. The images that carry no fork (verifier, evaluator, delivery) state none.
            fork = self.fork or {}
            if fork.get("sha256") != envs["CLINE_SHA256"]:
                raise Fail("image-record", f"{name}: the build asserted CLINE_SHA256 {envs['CLINE_SHA256'][:12]}, the verified fork binary "
                                           f"is {str(fork.get('sha256'))[:12]}")
            rec["measurements"]["clineSha256"] = fork["sha256"]
            rec["measurements"]["forkBinary"] = {k: v for k, v in fork.items() if k != "sha256"}
        d = Path(os.environ.get("DEPLOY_RECORD_DIR", f"{RECORDS}/deploy"))
        d.mkdir(parents=True, exist_ok=True)
        path = d / f"{now:%Y-%m-%d}-image-build-{self.stage}-{name}-{tag[-12:]}-{self.account}.json"
        path.write_text(json.dumps(rec, indent=1) + "\n")
        say(f"image record {path}")

    def write_record(self, rec: dict) -> None:
        d = Path(os.environ.get("DEPLOY_RECORD_DIR", f"{RECORDS}/deploy"))
        d.mkdir(parents=True, exist_ok=True)
        path = d / f"{dt.datetime.now(dt.timezone.utc):%Y-%m-%d}-deploy-{self.stage}-{rec['codeSha'][:7]}-{self.account}.json"
        path.write_text(json.dumps(rec, indent=1) + "\n")
        say(f"record {path}")

    # ---- main -------------------------------------------------------------------------------

    def deploy(self) -> str:
        start = dt.datetime.now(dt.timezone.utc)
        sha = self.git("rev-parse", "HEAD")
        tree = digests.source_tree(self.root)
        self.sha, self.tree = sha, tree
        self.commit = self.source_commit(sha)
        say(f"stage={self.stage} sha={sha[:7]} sourceTree={tree[:12]} profile={self.env['AWS_PROFILE']}")
        if self.commit != sha:
            say(f"source commit {self.commit} ({SOURCE_COMMIT_VARIABLE}); the tree deployed is at {sha[:7]}")
        self.check_account()
        azs = self.check_azs()
        vpc = self.check_vpc_cidr()
        self.settle_owner()
        # The lock comes after the owner (it is tagged with the stage's owner, as every parameter the deploy tags by hand) and before the
        # stage's records and settings are read; it is released when the deploy ends, after a failure or a refusal too.
        with self.deploy_lock():
            self.report_config_record()
            doc = self.aws("ssm", "get-parameter", "--name", f"/hamh/{self.stage}/worker-revision", ok_missing=("ParameterNotFound",))
            current = int(doc["Parameter"]["Value"]) if doc else 0
            self.revision = current + 1 if self.bump else current
            say(f"worker revision {self.revision}{' (bumped from %d)' % current if self.bump else ''}")
            # Every stage setting resolved (explicit, kept, live, default) and handed to the cdk calls, before anything is built.
            consent = self.settle_settings()
            # The Instances part is never removed by a deploy. Said and refused first, before the guard and before anything is built: waiting for the runs to end would not help.
            self.instances_state = self.compute_instances_state()
            self.say_compute_instances(self.instances_state)
            self.refuse_instances_off(self.instances_state)
            self.require_consent(consent)
            guarded = self.bump
            if self.bump:
                self.guard()
            # A tools mode other than the live runtime's is a runtime environment change, so a new runtime version. It is said and guarded
            # before the Lambda build, so a refusal costs seconds; a deploy that is also a bump or a new image asks the guard once.
            self.tools_state = self.tools_mode_state()
            self.say_tools_mode(self.tools_state)
            if self.tools_state["change"] and not guarded:
                self.guard()
                guarded = True
            # The isolation words are part of each runtime's environment. A live runtime that lacks one or carries another gets a new runtime version from this deploy
            # (the stage was deployed before the word existed, or by another tree), so it meets the same guard, before the Lambda build.
            self.isolation = self.isolation_state()
            self.say_isolation(self.isolation)
            if self.isolation["change"] and not guarded:
                self.guard()
                guarded = True
            # Adding the Instances part changes the runtimes, so it meets the same guard, before the Lambda build.
            if self.instances_state["change"] and not guarded:
                self.guard()
                guarded = True
            self.build_lambda()
            self.install_infra()
            meta = self.run([*self.cargo, "metadata", "--format-version", "1", "--locked"])
            if meta.returncode != 0:
                raise Fail("cargo-metadata", meta.stderr.strip()[-200:])
            doc = json.loads(meta.stdout)
            tag = self.image_tag(WORKER_IMAGE, doc)
            # Same hashing as the worker key (git tree hashes of the inputs), over the delivery inputs.
            delivery_tag = "dl-" + digests._hash_paths(
                self.root, [*digests.local_crate_closure(doc, DELIVERY_PACKAGE), *DELIVERY_IMAGE_PATHS])
            order = self.stack_order()
            say("stacks " + " ".join(order))
            # A changed provider configuration is a different provider name, which CloudFormation replaces (the machines on the old provider go): the same guard as a new runtime version.
            replaced = self.provider_replacement()
            if replaced:
                say(f"capacity provider: the live provider is {replaced[0]} and this deploy's configuration gives {replaced[1]}, so CloudFormation REPLACES it and deletes the old one with the machines on it")
                self.require_provider_consent(replaced)
                if not guarded:
                    self.guard()
                    guarded = True
            boot = self.check_bootstrap()
            image = self.find_image(tag) if self.repo_exists() else None
            if not guarded and self.current_image and image != self.current_image:
                # A new worker image is a new runtime version.
                self.guard()
            if image is None:
                # The fork binary of the image to build, looked up read-only: a pointer to a missing binary is refused before any stack change.
                self.lookup_fork()
            # Every check that can refuse has passed. The first stack change (create_bootstrap, else the first deploy_stack) writes the settings record with
            # status=deploying first (begin_changes); a failure before it, the image build included, leaves every stack as it was.
            if boot["outcome"] == "missing":
                boot = self.create_bootstrap(boot)
            if image:
                say(f"image {tag[:15]} found {image.rsplit(':', 1)[-1][:12]}")
            else:
                if not self.repo_exists():
                    say("phase-1 pass: the stage has no worker repository yet")
                    for name in order:
                        self.deploy_stack(name, None)
                say(f"image {tag[:15]} missing: building")
                image = self.build_image(sha, tag)
            self.delivery_image = self.resolve_delivery_image(sha, delivery_tag, order, image)
            for name in order:
                self.deploy_stack(name, image)
            checks = self.check_runtimes(self.expected_images(image))
            # The control endpoint is checked with the runtimes, before the SSM digests and the deploy record name the new deployment: a deploy
            # that cannot confirm it has not finished, and the stage's parameters still name the last deploy that did.
            self.endpoint_ready()
            endpoints = self.version_endpoints(checks)
            logging = self.invocation_logging()
            lock = hashlib.sha256((self.root / "engine" / "lock.toml").read_bytes()).hexdigest()
            served = sorted({c["image"].split("@", 1)[1] for c in checks})
            hashes = self.template_hashes()
            asl = self.asl_versions()
            digest = digests.deployment_digest(tree, served, lock, asl, hashes)
            img = digests.image_digest(served, lock)
            self.record_assets()
            for name, value in (("deployment-digest", digest), ("deployed-source-tree", tree), ("image-digest", img),
                                ("worker-revision", str(self.revision)), ("deployment-config-digest", self.config_digest)):
                self.put_param(f"/hamh/{self.stage}/{name}", value)
            gomod = self.report_gomod_store()
            crit = Path(os.environ.get("DEPLOY_CRITERIA", ""))
            self.write_record({
                "schemaVersion": 1, "kind": "deploy", "createdAt": start.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "host": socket.getfqdn(), "account": self.account, "region": self.region, "stage": self.stage, "codeSha": sha,
                "sourceTree": tree, "deploymentConfigDigest": self.config_digest, "deploymentDigest": digest, "imageDigest": img,
                "criteriaSha256": hashlib.sha256(crit.read_bytes()).hexdigest() if crit.is_file() else None,
                "result": "pass", "inputs": {"target": self.target.core(), "stacks": order, "imageTag": tag, "image": image,
                                             "workerRevision": self.revision, "toolsMode": self.tools_state, "computeInstances": self.instances_state, "isolationWords": self.isolation,
                                             "stageSettings": {n: {k: v for k, v in st.items() if k != "live"} for n, st in self.settings_state.items()},
                                             "deliveryImageTag": delivery_tag, "deliveryImage": self.delivery_image},
                "measurements": {"runtimes": checks, "versionEndpoints": endpoints, "templateHashes": hashes, "aslVersions": asl,
                                 "invocationLogging": logging, "gomodStore": gomod, "azs": azs, "vpcCidr": vpc, "bootstrap": boot,
                                 "forkBinary": self.fork or {"outcome": "not-needed"},
                                 "seconds": int((dt.datetime.now(dt.timezone.utc) - start).total_seconds())},
                "runIds": [], "notes": "",
            })
            self.write_settings_record("deployed")
            say(f"endpoint V1ApiUrl ready stage={self.stage} region={self.region}. Read it from stack {self.prefix}-control.")
            return f"DEPLOY-PASS stage={self.stage} digest={digest[:12]} sourceTree={tree[:12]}"


def usage(msg: str) -> int:
    print(f"deploy.sh: {msg}\nusage: scripts/deploy.sh --stage direct [--azs 1] [--bump-worker-version] [--check-guard] [--print-settings] "
          "[--fork-source auto|build|copy]", file=sys.stderr)
    return 2


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--stage", choices=("direct",))
    p.add_argument("--azs", type=int, choices=(1,))
    p.add_argument("--bump-worker-version", action="store_true")
    p.add_argument("--check-guard", action="store_true")
    p.add_argument("--print-settings", action="store_true")
    p.add_argument("--fork-source", choices=FORK_SOURCES, default="auto")
    try:
        args, extra = p.parse_known_args(argv)
    except SystemExit:
        return usage("bad arguments")
    if extra or not args.stage:
        return usage("bad arguments" if extra else "--stage is required")
    if args.azs is not None and args.azs != AZS[args.stage]:
        return usage(f"stage {args.stage} has {AZS[args.stage]} AZs (infra/lib/config.ts)")
    try:
        d = Deployer(args.stage, bump=args.bump_worker_version, fork_source=args.fork_source)
    except (aws_target.TargetError, ToolsModeError, ComputeInstancesError, SettingError) as e:
        return usage(str(e))
    if args.print_settings:
        try:
            d.print_settings()
        except Fail as e:
            if e.step in REFUSALS:
                print(refusal_line(e), flush=True)
                return 3
            print(f"DEPLOY-FAIL {e}", flush=True)
            return 1
        return 0
    if args.check_guard:
        try:
            line = d.check_guard()
        except Fail as e:
            print(f"DEPLOY-FAIL {e}", flush=True)
            return 1
        print(line, flush=True)
        return 0
    if d.git("status", "--porcelain", "--untracked-files=no"):
        print("deploy.sh: uncommitted changes to tracked files; commit first so the deploy matches a commit", file=sys.stderr)
        return 2
    try:
        line = d.deploy()
    except Fail as e:
        if not d.changed and e.step in REFUSALS:
            # Nothing on the stage changed; the one line says why and what to do.
            print(refusal_line(e), flush=True)
            return 3
        if not d.changed:
            # A failure before the settings record says `deploying` left every stack as it was.
            print("DEPLOY-STAGE unchanged", flush=True)
        print(f"DEPLOY-FAIL {e}", flush=True)
        return 1
    print(line, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
