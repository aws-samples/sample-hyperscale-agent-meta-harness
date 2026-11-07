# scripts/setup

## Purpose

The helper files of `setup.sh`, the single entry point of the system. `setup.sh` itself keeps the options, the
questions and the client steps. The AWS lifecycle of the full setup, `--status`, `--doctor`, `--update` and `--teardown` lives here, in small files that
`setup.sh` sources, so the script stays readable and each part can be tested on its own with a stand-in AWS CLI. The other half of this folder runs inside
the user's account: the build of the account-local runner that `setup.sh` installs with the AWS CLI alone.

The user of the full setup is a stranger on a Mac or a Linux desktop who has only the AWS CLI, version 2, and a fresh AWS account. Nothing in this folder
may need the authors' accounts, buckets, profiles or hosts, a GitHub token or a persistent password.

## Ownership

Owned by `scripts/AGENTS.md`. Four groups of files:

- Sourced by `setup.sh` on the user's machine, never run on their own: `lib.sh`, `json.sh`, `accounts.sh`, `state.sh`, `client.sh`, `runner.sh`, `preflight.sh`, `full.sh`,
  `status.sh` and `teardown.sh`.
- `names.sh`, the one definition of the client's names, sourced by `setup.sh` and by every script here.
- Run inside the runner's CodeBuild project (Linux): `runner-build.sh` (the build entry that `images/codebuild/setup-runner.yml` calls), `runner_state.py`,
  `runner-toolchain.sh`, `build-client.sh` and `client_check.py`.
- Run on the user's machine by `setup.sh`: `empty-bucket.sh`.

The runner stack is `infra/setup-runner.yaml` with its policy file `infra/setup-runner-policy.json` (`infra/AGENTS.md`); its buildspec is
`images/codebuild/setup-runner.yml` (`images/AGENTS.md`). `setup.sh` at the repository root is the entry point and is documented in `docs/`.

## Local Contracts

### The contract between the wizard and the runner

Held by `scripts/tests/test_setup_contract.py`, which runs the real files of each side against the other.

- Stack `hamh-setup-runner`, template `infra/setup-runner.yaml`, made with `aws cloudformation deploy --capabilities CAPABILITY_NAMED_IAM`. Outputs `SetupBucket`, `BuildProject`,
  `LogGroup`, `RunnerRoleArn`; `setup.sh` reads these four and no other. Parameters, all passed at every install (`rn_install`): `InstallerPrincipal` and `InstallerPrincipalWithPath`
  (the identity behind the credentials as the two `aws:PrincipalArn` patterns that match it, from `setup_installer_patterns` in `lib.sh`: exact for a user, the root user and a federated user;
  for a role session the role without and with a path) and `AuthorityExpires` (a UTC time, `HAMH_SETUP_AUTHORITY_HOURS` hours ahead, 12 by default, 5 to 72). A stack that an earlier run left in
  `ROLLBACK_COMPLETE` or `CREATE_FAILED` is deleted and made again, one that is still changing is waited for (`HAMH_SETUP_STACK_LOOKS` looks, 120 by default), one in `DELETE_FAILED` or a failed
  rollback stops the run with exit 5 and the state named (`rn_prepare_stack`, for this stack and for `CDKToolkit`). The CDK bootstrap stack `CDKToolkit` is made when the SSM parameter
  `/cdk-bootstrap/hnb659fds/version` is absent and no `CDKToolkit` stack exists.
  `pf_bootstrap_stack` in `preflight.sh` ends the run with exit 7 before anything is created when the parameter is absent and a settled `CDKToolkit` stack exists (a bootstrap made with another qualifier, or one whose parameter was removed): `full_run` calls it for the runner route and for `--build-on local`, so `--dry-run` and `--preflight-only` report it too.
  The message names the stack, its status, the missing parameter and the default qualifier `hnb659fds` that the stage needs, and gives two ways out: another account or Region, or `cdk bootstrap --qualifier hnb659fds --toolkit-stack-name <name>`.
  `rn_prepare_stack` deletes and recreates a stack in `ROLLBACK_COMPLETE` or `CREATE_FAILED`. `rn_bootstrap` waits for a stack that is changing and looks again: a parameter that has appeared is used as it is, and a stack that settled without it ends the run with the same message and the note that the runner stays installed.
  `scripts/tests/test_setup_bootstrap_stack.py` holds the cases. The runner role cannot make a bootstrap and the desktop has no Node, so the runner's first build
  (`SETUP_ACTION=bootstrap-template`, no lease) prints the template with the pinned CDK CLI and puts it in the bucket, and `setup.sh` deploys it with the user's own credentials
  (`CloudFormationExecutionPolicies` set to `AdministratorAccess`, the CDK's own default) only when its SHA-256 is the one in `engine/policy/cdk-bootstrap-template.pin` of the clone (the
  result of the build and the bytes downloaded from the bucket must agree with it too; a mismatch is exit 7) and it is no larger than 51,200 bytes, which the AWS CLI deploys without a
  staging bucket. The template is not committed (the scanners report 31 findings on it and a third-party artifact is fetched, not copied); the pin moves with `tools.lock`
  (`scripts/tests/test_cdk_bootstrap_pin.py`). The `inputs` phase of a deploy fails with a clear message when the parameter is missing.
- Bucket `hamh-setup-<account>-<region>` (made by the stack), keys `source/<commit>.zip` (`git archive` of the clean clone), `config/accounts-<first 12 digits of the SHA-256>.json` (the
  generated account file, a separate object), `results/<build id>/result.json`, `results/<build id>/phases.jsonl`, `results/<build id>/deploy-records/`,
  `bootstrap/cdk-bootstrap-<first 12 digits of the SHA-256>.yaml`, `client/<build id>/<platform>/<binary>` and `client/<build id>/<platform>/<binary>-admin` (the operator tool),
  `client/<build id>/SHA256SUMS` and `lease/<stage>.json` (written by `setup.sh` alone).
- The bucket is not where a decision is made. Its policy refuses writes and deletes to every principal but the runner role and the installer, and the wizard still believes nothing that
  lies there. The source archive (`git archive` of the clean clone, written to the receipt folder) and the generated account file are uploaded with `aws s3api put-object --checksum-algorithm SHA256
  --expected-bucket-owner <account>` on every run: an object found under the key is replaced, never trusted. The project's source is an S3 object named by a placeholder in the template, so
  `setup.sh` starts every build with `--source-location-override <bucket>/<source key>` and `--source-version <the version id the upload answered with>` (taken from the answer of
  `put-object`, never from a later look at the key; a bucket that gives no version id stops the run with exit 5). The runner script reads the archive by that version
  (`CODEBUILD_SOURCE_VERSION`), so the bytes it hashes and runs are the ones that were uploaded.
- Build environment: `SETUP_SOURCE_KEY` (`source/<40 hex commit>.zip`), `SETUP_CONFIG_KEY` (`config/accounts-<12 hex>.json`), `SETUP_STAGE` (`direct`), `SETUP_AZS` (`1`),
  `SETUP_BINARY_NAME` (`$HAMH_CLIENT_BIN`), `SETUP_SOURCE_SHA256` (64 hex, the SHA-256 of the archive) and `SETUP_CONFIG_SHA256` (64 hex, the SHA-256 of the account file, of which the 12 digits in the
  key are the start), and for the bootstrap template `SETUP_ACTION=bootstrap-template`. The build id is the CodeBuild build id.
- The result is anchored outside the bucket. After a build that passed, the runner writes `<work>/anchor.env` (`export SETUP_RESULT_SHA256=<SHA-256 of the result.json it uploaded>`), the buildspec
  sources it and lists the variable as an exported variable, and CodeBuild keeps it with the build. `setup.sh` downloads `result.json` whole, hashes the bytes and compares them with
  `builds[0].exportedEnvironmentVariables` of `codebuild batch-get-builds` (`rn_anchor`, called by `rn_verify`) before it reads a single value of the result:
  the commit, the endpoint and the SHA-256 of the client it installs and runs all come from that file. No value recorded, or another one, is exit 7 with the lease released and nothing installed;
  a look that fails is exit 6 and keeps the lease. The endpoint is compared with the `V1ApiUrl` output of the control stack of the account and must be the https URL of an API Gateway stage
  (`valid_stage_endpoint` in `setup.sh`).
- `result.json`, schema "1": `state` (`SETUP-PASS` or `SETUP-FAIL`), `sourceCommit`, `configDigest`, `buildId`, `stage`, `action` (`deploy` or `bootstrap-template`), `region`, `account`, `endpoint`,
  `stackOutputs` (`<stack>.<OutputKey>` to value), `client` (platform to `key`, `sha256`, `bytes` and `admin`, which holds the same three members for the operator tool), `phases` (`name`, `state` ok|skipped|failed, `seconds`), `failedPhase`, `detail`,
  `deploymentDigest`, and for the bootstrap template `bootstrapTemplate` (`key`, `sha256`, `bytes`). The wizard reads seventeen of these paths and no others (listed in
  `test_setup_contract.py`); it compares `sourceCommit` and `configDigest` on their first 12 characters. The last line of the build log is `SETUP-PASS stage=<s> digest=<12> endpoint=<url>`
  (the template: `SETUP-PASS stage=<s> action=bootstrap-template template=<12 hex>`) or `SETUP-FAIL <phase> <detail>`.
- `failedPhase` `guard` is the deploy guard (runs are active and pin the worker runtime): the wizard exits 7 on it. Any other failed deploy is exit 5.
- An update that fails in `guard`, `inputs`, `fetch`, `toolchain` or `account-file` has not touched the stage (`rn_before_deploy` in `runner.sh`): `rn_fail_result` writes the receipt, releases the lease and removes
  the runner it installed (`rn_remove`), and the last lines say "The stage is unchanged. The runner is removed." (or that the runner could not be removed and stays installed, with the RETAINED
  lines above). A failure in `deploy` or a later phase, and a build that left no result, keep the runner for the retry and say "The stage may contain partial deployment changes." A first
  setup (`--full`) keeps its runner after every failed build. The failure report names the CloudWatch log group only when the runner stays, because the group goes with the runner stack.
- Platforms: `darwin-arm64`, `linux-amd64`, `linux-arm64`. The generated account file names the profile `hamh-setup` in the runner route (the runner writes a profile of that name for the
  container credentials of CodeBuild) and the user's own profile in the local route; its `invocationLogging` is `read-only` in the runner route, which the runner role can apply
  (`--invocation-logging enable-and-restore` needs `--build-on local`, and `setup.sh` refuses it before anything is made).

### The generations of names and the earlier names

- `lib.sh` defines the current and the legacy generation of names; a stage belongs to one of them. `setup_gen_use <current|legacy>` sets `GEN`, `GEN_PREFIX` (the first word of the stage's names), `GEN_ROOT` (the root of the parameter, role-path and log-group names), `GEN_PROJECT` (the value of the project tag), `GEN_SETUP` (the first words of the runner's names), `GEN_E2E` (the prefix of the CodeCommit repositories) and `GEN_LABEL`, and points `SETUP_RUNNER_STACK` and `SETUP_PROJECT_TAG` at that generation; `lib.sh` selects the current generation at its end.
- `pf_legacy` in `preflight.sh` (marker `LEGACY-TOMBSTONE`: the detector has no end, so a stage of the previous names is never reported as none) runs at the end of `pf_stage`. `LEGACY_STATE` is `none`; `legacy` (stacks named `<previous prefix>-<stage>-*` that carry the previous project tag and the stage tag; `LEGACY_STACKS` lists them and `LEGACY_ENDPOINT` holds the `V1ApiUrl` of their control stack when it can be read); or `legacy-retained` (no such stack, but a VPC, a stage bucket or a runner stack of that generation is left; `LEGACY_LEFT` names what). A read that fails is a warning and never the answer `none`.
- `--status` prints `STATUS stage=<stage> state=legacy` (exit 7) or `state=legacy-retained` (exit 8) when the current generation has no stack, and a `previous:` line when both generations exist. `--doctor` fails (exit 3) with `FAIL stage <stage>: ` and the sentence of `pf_legacy_head`, the same sentence as `--status`.
- `full_run` calls `pf_legacy_refuse` right after `pf_stage`: `./setup.sh --full` and `--update` (also with `--dry-run`) end with exit 7 before the lease is read, a runner is installed or anything is created. The message says that the previous stage keeps running and that the client still talks to it (the wire format is unchanged, so `ham pull <runId>` works against the old endpoint), and lists four steps: install the new client against the old endpoint with a command that works as printed (`./setup.sh --client-only [--profile <profile>] --region <Region> --endpoint <old endpoint URL> --from <directory with ham and SHA256SUMS>`: the profile and the Region of the refused run, and a source for the client; `--build` compiles it from the checkout instead; the step creates nothing), with a sentence that step 1 replaces the unedited rules pack of the earlier client and that runs on the old stage then show 'No test command was found' on the delivery card until the stage is torn down and redeployed (the code and the tests are unaffected), pull each run to keep, `./setup.sh --teardown`, `./setup.sh --full`. It adds what teardown removes (run records, delivered zips, engine settings, the Tavily key and the rows of the tables) and that the CodeCommit repositories of the previous prefix stay. For `legacy-retained` nothing of the previous stage runs any more, so the message lists two steps: `./setup.sh --teardown` (again after AWS has released the network interface when it reports `RETAINED`), then `./setup.sh --full`. `scripts/tests/test_setup_legacy_stage.py` holds the cases.
- On an account that holds a stage under the previous names `status.sh` does not advise `--update` for a missing operator tool (it is refused there): it says that a new stage brings the tool (`status_admin_advice`). It compares the client's endpoint with the endpoint of that stage when the account has no stage under the current names (`STATUS_COMPARE_ENDPOINT`, named "the stage under the previous names"), and keeps the sentence "the stage has no endpoint to compare with" for the case that no endpoint is known. The menu that `fail_no_client` prints says where `--full` and `--update` stop on such an account. The `installed:` line of the client-only setup shows the version from the client's stdout; its notices on stderr are not part of it.
- `pf_source` refuses `--revision` of a commit whose `scripts/setup/names.sh` does not define `HAMH_CLIENT_BIN` (a commit from before the rename) with exit 2, before the stage is read.
- `setup.sh` keeps a relative link with the earlier command name (`LEGACY_CLIENT_BIN`) beside the installed client in `PREFIX/bin` (marker `COMPAT-CAH`); only `setup.sh` makes it. `alias_decide` sets `ALIAS_ACTION`: `make` (nothing there), `replace`, `ok` (already a link to the client), `keep` or `skip` (the client is not installed in the install folder). A file at that path is replaced only when it is a link to the client or an earlier client of this system recognised without being run (`alias_recognised`): its SHA-256 is a line of a `download/verified/SHA256SUMS` of a receipt under the settings folder or the earlier settings folder, or it holds the name of the header the earlier client reads the backend version from. Any other file, a directory and a link to another program are kept with a warning. The link is made under a temporary name (`ln -sfn`) and renamed over the old file (`mv -f`). The Plan prints a `link:` line and a dry run prints `would link ...`. `scripts/tests/test_setup_alias.py` holds the cases.
- A client on the `PATH` counts only when its file holds `x-hamh-backend-version`, the header name the client reads the backend version from (`client_on_path` searches the file and never runs it); `fail_no_client` names a program of the client's name that fails this test. `--from DIR` with the earlier command name and no client exits 1 before anything is installed. A client given with `--from` (a file or a folder) is installed only when its file holds that header name too (`is_current_client`, checked after the checksum): the earlier client under any name exits 1 with a sentence that names the missing header, and nothing is installed. `valid_default_repo` also accepts the repository prefix of the previous names (marker `COMPAT-CAH-DATA`).
- `names.sh` defines `LEGACY_CLIENT_BIN` and `HAMH_LEGACY_CONFIG_DIR_NAME` (defaults: the previous names), `ENV_COMPAT_NAMES` (the suffixes setup reads or hands on) and `ENV_COMPAT_SELECTORS` (`AWS_PROFILE`, `ACCOUNTS_FILE`, `OWNER`). `hamh_env_compat [SUFFIX ...]` runs first when `names.sh` is sourced. For each exported variable of the earlier prefix whose suffix is in the list (or among its arguments) it copies the value to the exported `HAMH_<suffix>` when that is unset or empty, unsets the earlier variable so that nothing setup starts sees two spellings of one setting, and prints one line per name on stderr unless `HAMH_NO_DEPRECATION_NOTICES=1`. When both are set and differ the current one wins; for a selector it is an error: the function returns 2, `names.sh` fails and `setup.sh` exits 2 with a message that names both variables.
- In its Config step, before any `config set`, `setup.sh` runs `<client> config migrate` when the earlier settings folder holds settings (`config_has_content`, the rule of the client: `config.toml`, `continue.md`, `review.md`, or an `engines/`, `rules/` or `skills/` folder with something in it besides a file browser's `.DS_Store`) and the current folder holds none (`config_migration_due`; a current folder that has the marker `.<name>-migrating` of a copy that stopped half way counts as holding none). It says so, a dry run prints `would run: ...`, and a failure ends the setup with the earlier folder in place. The client reads the earlier folder read-only and refuses to write while only that folder holds settings.
- `runner-build.sh` blanks and unsets both spellings of the profile and account-file selectors where it blanks or unsets one, and its probe for the account-file override looks for the text `ACCOUNTS_FILE` in `infra/lib/config.ts` and `crates/harness-protocol/build.rs`, under either prefix. The generated account file carries the selector word `HAMH_AWS_PROFILE`; `scripts/lib/aws_target.py` reads both words in stored files.

### Teardown by generation

- `td_use_generation <current|legacy>` in `teardown.sh` calls `setup_gen_use` and sets `GEN`, `GEN_PREFIX`, `GEN_ROOT`, `GEN_SETUP`, `GEN_E2E`, `SETUP_PROJECT_TAG` and `SETUP_RUNNER_STACK`; every teardown read and delete uses these names, and literal previous names occur only in one marked comment.
- `setup.sh` sets `TEARDOWN_ONLY` from `--only`, which is valid only with `--teardown`; `TEARDOWN_ONLY` accepts empty, `current` or `legacy`, and any other value exits 2; `TD_COVER` records covered generations and `TD_FOUND_GENS` records generations that hold something.
- `run_teardown` makes one combined plan and asks for one confirmation across the covered generations; missing confirmation exits 4, and `--only` makes both the plan and result state that the other generation is not read.
- `td_inventory keep` accumulates `TD_CATS_OK`, `TD_CATS_BAD` and `TD_UNREAD_RUN` across covered generations; a category counts as checked only when every covered generation reads it successfully.
- Before confirmation, each generation that holds something receives `td_busy`, the Go module store check and `td_read_logging`; `td_plan` prints under `The plan covers the generation using <label>:` and remains the limit of that generation's deletion pass.
- When nothing is found across covered generations, teardown prints `No stack, runner, retained resource or bucket of the stage exists under <covers> ... nothing to tear down`; a failed read or remaining VPC instead produces `INCOMPLETE` and exit 8.
- `td_plan_tail` precedes the single confirmation and reports duration, what stays, what is lost with a removed stage, the CodeCommit repositories under both prefixes that stay, and a note about `--only`.
- Deletion proceeds generation by generation with reads cut to the saved plan, in the order runner, logging restore, quiesce, stacks, retained resources and secrets; afterward teardown reads every covered generation again, sweeps networks and leftovers per generation, prints pending keys and secrets and each generation's tag index, then prints `td_verdict_gen` for each generation and the final line.
- `td_slot_set` and `td_slot_get` store per-generation text without `eval` or indirect expansion and support Bash 3.2; `td_plan_freeze` and `td_plan_thaw` save and restore `TD_PLAN_*`; `TD_PLAN_CUT=1` skips and warns about stacks that appear after the plan, including runner stacks.
- `td_read_stacks` lists only the prefix and project-tag pair of the generation in use; a stack with the other generation's project tag or no project tag is foreign, appears on a `kept:` line with the reason from `td_foreign_reason`, and is never touched.
- `TEARDOWN-INVENTORY 0 stacks stage=<s> runner=none` requires every covered generation to read zero; a failed or denied read makes the run `INCOMPLETE` with exit 8, and a failed category is omitted from the `checked:` sentence, which names each covered generation's prefix.
- A running build in either generation's runner stack exits 7 without deleting anything; a stack list that cannot be read exits 1; a retained VPC or tagged bucket in either generation counts as present, and a failed runtime listing never authorises log-group deletion.
- `td_again_text` ends with `Without --yes, teardown asks before it deletes; answer n to keep that stage.` when `--yes` is absent; when `--yes --acknowledge-teardown` sets `TD_YES_GIVEN`, it ends with the sentence stating that teardown deletes without asking.
- `empty-bucket.sh` accepts `<word>-setup-<12 digits>-<Region>` and `<word>-direct-(harness|gomod)-<12 digits>` for the current and legacy words in `FIRST_WORDS`; unmatched names exit 2 before any call, governance bypass applies only to either generation's Go module store, and a bucket-name pattern is a guard rather than permission to delete.
- `scripts/tests/setup_fake_aws.py` models the legacy generation through `legacy_stage_deployed`, `legacy_unfinished_deploy`, `legacy_runner_present`, `legacy_endpoint`, `legacy_logging_record` and its `_tagged` variant, plus `foreign_stacks`, `World.deploy_stage(gen=)`, `make_runner(gen)` and `stage_model(.., gen)`; generation cases live in `test_setup_teardown_legacy.py` and `test_setup_fake_aws_generations.py`.

### The maintainer tools and the previous names

- `scripts/lib/aws_target.py` resolves each `HAMH_` variable first and the same variable under the previous prefix second, and treats an empty value as unset. A notice about a previous name goes to stderr only, once per variable name and process; `HAMH_NO_DEPRECATION_NOTICES=1` silences it. Stdout stays clean for callers that evaluate the output of `shell`.
- Different nonempty values for `HAMH_AWS_PROFILE`, `HAMH_ACCOUNTS_FILE` or `HAMH_OWNER` and the same variable under the previous prefix stop the resolution with exit 2 and name both variables. For any other variable the `HAMH_` value wins.
- `aws_target.py shell` prints `HAMH_ACCOUNT`, `HAMH_REGION`, `HAMH_PROFILE` and `AWS_PROFILE`, then the first three under the previous prefix; `scripts/lib/aws-target.sh` exports only the new names. An account file that names the selector word of the previous prefix loads without a notice; the files that setup generates carry `HAMH_AWS_PROFILE`.
- `fold_previous(env)` and `with_selector(env, name, value)` give a child process one spelling of a variable. `node.sh` forwards every exported variable of either prefix to the CDK container; when only the previous account-file variable is set it moves the value to `HAMH_ACCOUNTS_FILE` and unsets the previous one, so the container never sees two spellings of one selector.
- `deploy.py` sets `HAMH_LAMBDA_DIR`, `HAMH_WORKER_IMAGE` and `HAMH_DELIVERY_IMAGE` under the new names only. It finds the engine pin's artifact bucket of the retired stages and of the current stage. A VPC overlap with a stage of the previous names stops the deploy with "this account still has the stage under the previous names" and points to `./setup.sh --teardown`.
- `scripts/teardown.sh` (`scripts/lib/teardown.py`) takes `--names new|old|both`, default `both`. A generation owns a stack only when the name prefix and the project tag are both its own; a mixed pair is printed as `kept:` and never touched. The previous generation is removed through CloudFormation, with its invocation-logging record and role path read under the previous names. The pass for the previous names writes its own evidence file only when it finds something. The zero-resource line prints once, and only when every selected generation reads zero.
- The tests that need accounts take them from the shared fixture `scripts/tests/fixtures/accounts-three.json` through `scripts/tests/accounts_fixture.py` (`accounts_three`, `write_accounts`; placeholder accounts and profiles only) and never read the committed `accounts.json`.

### The wizard side (sourced by `setup.sh`)

- Sourced, never run. Each helper starts with a line that says `setup.sh` sources it, holds definitions only and prints nothing when sourced. Compatible with bash 3.2 (the bash of
  macOS): no `declare -A`, `mapfile`, `[[ =~ ]]`, `${!var}` or `${var,,}`. They use only what ships with macOS and Linux (awk, sed, tr, cut, sort, grep, date, sleep, uname, git and the AWS
  CLI): there is no jq and no python on a stock Mac. `lib.sh` defines `say`, `ok`, `warn`, `err`, `detail` and `fail` only when `setup.sh` has not, so a helper can be tested alone.
- No delete (`rm`, `rmdir`, `find -delete`), no `sudo`, no `curl` or `wget`, no `eval`, no credential value read or printed, no account id, endpoint or checksum written into the text,
  nothing that names a tool, host, account or profile of the authors. `scripts/tests/test_setup_full_static.py` holds each helper to this and to `internal_names.scan_text`.
- State: AWS is authoritative. A lease object (`lease/<stage>.json`: owner, build id, source commit, config digest, action) written with `put-object --if-none-match '*'` stops a second
  setup. A run that finds a lease with the same source and account file attaches to its build (running) or reads its result (succeeded) and never starts another; another source with a
  running build is exit 7; a lease whose build ended is replaced. A lease that was taken but never recorded a build (a setup died between the two calls) is replaced by a run with the
  same inputs; two setups with identical inputs that start inside that one-call gap can both start a build. The local receipt `~/.config/<config dir>/setup/<account>/<region>/<stage>.json`
  (mode 0600) caches identifiers and holds no credential; a fresh machine recovers from the lease alone. A per-run log sits beside it in a logs folder. Interrupting setup (Ctrl-C) or
  losing the connection (`HAMH_SETUP_POLL_RETRIES` failed looks in a row, 20 by default) exits 6 and never cancels the build. `HAMH_SETUP_POLL_SECONDS` (15) sets the pause between looks.
- `rn_report_failure` prints the failed build's phase, reason, build log location, and at most 15 log lines. `rn_failure_excerpt` selects the lines; `rn_failure_kind` names the excerpt kind.
  The excerpt normally shows the lines before the build's first `SETUP-FAIL` or `DEPLOY-FAIL` line, which contain the cause. If none of the last 15 lines looks like an error and one of the preceding
  45 does, the window starts at that line. CodeBuild's closing lines follow the failure line and are excluded. A build with no failure line or no output before it shows its last log lines.
  A log with no build line shows its own last lines. Each line is truncated at 240 characters, and control characters other than the tab are removed.
  `scripts/tests/test_setup_failure_excerpt.py` checks this behavior with `fixtures/setup/runner-failed-build.txt`.
- The account file is generated by `accounts.sh` from `sts get-caller-identity` and `ec2 describe-availability-zones` in the schema of `accounts.json` (one entry, role `active`). The entry's `operator` is the
  identity behind the credentials (`acct_operator`, the seventh argument of `acct_generate`, which `pf_zones` fills with `CALLER_ARN`): a role session `arn:aws:sts::<account>:assumed-role/<role>/<session>` becomes
  `{"kind": "role", "name": "<role>"}` (an IAM Identity Center session gives the `AWSReservedSSO_<permission set>_<hash>` role), an IAM user `arn:aws:iam::<account>:user/[<path>/]<name>` becomes
  `{"kind": "user", "name": "<name>"}`. The stage trusts exactly that identity (and the runner's role where the runner fills the Go module store); the root user, a federated user, an ARN of another account or of a
  shape setup does not know, and a name the account file does not accept are refused with the reason and the way out, exit 1, before anything is created. A person who runs setup again as another identity makes another
  account file (another digest), and the update changes the stage's trust. The rows of `infra/test/fixtures/operator-identity.json` (`generate`) are the cases. The zones are
  the AgentCore ones of the Region (use1-az1, use1-az2 and use1-az4 in us-east-1; usw2-az1, usw2-az2 and usw2-az3 in us-west-2), taken in the order of their ids. Placeholders and any other
  Region are refused. `HAMH_ACCOUNTS_FILE` (absolute path) makes the deploy scripts, the CDK app and the Rust build scripts read it; the committed `accounts.json` is never edited.
- Exit codes of `setup.sh` (0 to 3 keep their meaning from before the full setup):
  - 0 done
  - 1 a prerequisite or step failed
  - 2 bad usage
  - 3 installed, but a doctor found a problem
  - 4 cancelled, or a required confirmation is missing
  - 5 bootstrap, deploy or initialization failed
  - 6 an operation is still running and safe to resume (run setup.sh again to attach)
  - 7 a conflicting operation, or a source or configuration mismatch: a stage deployed under the previous names (`--full`, `--update`, and `--status` as `state=legacy`), a dirty clone, another account than `--account-id`, a running build of other inputs, a running run when an
    update starts (`pf_active_runs_check`, before the runner is installed; `--allow-active-runs`, valid with `--update` only, skips it) or the deploy guard that refuses an update that changes the worker runtime while runs are active,
    a `CDKToolkit` stack that exists without the bootstrap parameter
  - 8 cleanup is incomplete: a stack of the stage that stays, retained resources, a runner that could not be removed, a teardown step that failed, something of the stage that the final
    read still finds, or a read that failed (the zero line then is not printed and the read is named)
- The state of the stage decides what a run does (`pf_stage`): `none`; `partial` (no control stack, or a stack that rolled back on its first create, `ROLLBACK_COMPLETE`, which holds
  nothing: the CDK deploy deletes it and makes it again (its `isCreationFailure` handling), and the plan says so; or every stack complete but no deploy record, `STAGE_UNFINISHED=1`: the
  first pass of the two-pass deploy leaves all nine stacks, and the SSM parameter `/hamh/<stage>/deployment-digest`, which `scripts/lib/deploy.py` writes after its last stack, the runtime checks and the
  endpoints, is the deploy's own record of its end; `--full` and `--update` run the deploy again, `--status` says `state=partial` and `--doctor` fails the stage check, each saying the deploy did not
  finish); `complete` (the control stack and the others are complete and the deploy record exists; `UPDATE_ROLLBACK_COMPLETE` counts. CloudFormation leaves this status unchanged when the deploy has no stack changes); `deploying` (a stack is `*_IN_PROGRESS`: exit 7 with the stack named and no mention of a teardown, unless a build of this checkout explains it); `broken` (`DELETE_FAILED`, a failed
  rollback and the like: exit 7, look in the console or `--teardown`). A lease that records a running or a finished build of the same inputs (`attach` or `finish`) is attached to or read first,
  whatever the stacks say; only without one does the state refuse a run. `--preflight-only` on a deployed stage prints what it found and exits 0 before anything is installed or configured. A
  retry says the command that retries (`./setup.sh --full` or `--update`).
- For a run that deploys, `pf_stage_owner` reads the stage's `owner` tag from the network stack, else the control stack, into `STAGE_OWNER`, skipping a stack in `ROLLBACK_COMPLETE`. When a nonempty `HAMH_OWNER` differs from `STAGE_OWNER`, `pf_owner_check` ends the run with exit 7 before anything is created with `--build-on local` and warns on the runner route.
- `--repo-name NAME` (repeatable, or comma-separated, 1 to 8 entries of the form the account file's `repoNames` takes: a repository name, a prefix ending in `*`, or `*`) is an option of the full setup and of `--update`; `setup.sh` checks every entry (`check_repo_names`, `valid_repo_entry`) before any AWS call, refuses it with `--client-only`, `--status`, `--doctor` and `--teardown`, and checks `--default-repo` against the rule in force (`repo_name_covered`; the default rule `hamh-e2e-*` keeps the old message). `acct_generate` takes the rule as its eighth argument and writes `repoNames` only when it is given (`ACCT_REPO_NAMES`). `pf_zones` passes `--repo-name`; on an `--update` without it, `pf_stage_repo_names` reads the output `RepoNames` of the control stack into `STAGE_REPO_NAMES` so that the update keeps the stage's rule, and a rule equal to the default is not written. `full_plan` names a rule that is not the default. `scripts/tests/test_setup_repo_names.py` covers the flag, the generated file, `--default-repo` and the update.
- The runner goes after the client is in place. `full_runner_flow` only marks it (`FULL_REMOVE_RUNNER`); `full_installed`, which `setup.sh` calls once `ham models` has run, removes it unless
  `--keep-runner` is given. The checked client also stays in `~/.config/<config dir>/setup/<account>/<region>/download/verified`; a run on a deployed stage that finds no client installed installs that
  copy again when it still hashes to its `SHA256SUMS` line (`full_reuse_client`; a copy of the operator tool that no longer matches its line stops the reuse), and a runner that an earlier run left behind is removed by the run that finds the stage deployed. A client that was
  never downloaded (a second machine) is built again with `--update`, which installs the runner again for the client phase.
- A runner that is left behind says so on every run (`rn_note_installed`, `--status`, `--doctor`): anyone who may start its builds acts with the authority of the CDK roles until the
  expiry it carries (`RN_EXPIRES`, read from the stack), after which its role assumes nothing. `--teardown` removes it with the caller's own credentials and never installs it again: the
  bucket lets only the identity that installed it (and the runner role) delete, so a caller who is another identity cannot empty it, and the run says so (`RETAINED`, exit 8).
- The local route's account file names the profile your credentials really come from (`full_local_profile`): `--profile` or the ham config, else `AWS_PROFILE`, else `hamh-setup` for credentials that
  are only in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (the deploy then runs with `AWS_CONFIG_FILE` set to a file of the receipt folder that has that profile and a Region and no credential),
  else `default`.
- The screen of the local route is quiet about the CDK notices that repeat for every stack (`full_quiet_cdk` in `full.sh`): the `aws-cdk-lib` deprecation warning with its two indented lines, the
  Docker kernel warning of `cdk synth` and the annotation that run names are off in this Region with its two indented lines. The log of the run keeps every line (`deploy: <line>`) and `--verbose`
  leaves none out; any other line stays on the screen. The plan prints one `run names:` line (`full_run_names`) from `crates/ham-core/assets/utility-models.json` of the clone: on, or off in a Region
  where the default row is not available; a clone without the catalog prints nothing. The CDK app itself prints no deprecation warning (`infra/test/app.test.ts`).
- On a terminal `setup.sh` asks one question at a time for the values that no option gave. `question_title` numbers the questions in the order the run asks them ("Question 1: ...", "Question 2: ..."); a
  question whose option was given is not asked and takes no number, and no total is announced. The questions are what to set up (`question_mode`, skipped by `--full`, `--client-only` and the options
  that imply a mode), the stage endpoint (`question_backend`, client-only setup), where the client program comes from (`question_source`, client-only setup, asked when no client is installed and `--from`, `--sha256` and `--build` are absent), the AWS profile, the Region, an optional default repository (client-only setup, asked when an endpoint is known) and where
  to build (`question_build`, full setup, asked when the machine has cargo, cargo-zigbuild, zig, node and docker). At most six questions are asked (five after a typed `--client-only`, four in the full setup). A typed `--client-only` promises that nothing is
  created in the account, so its endpoint question offers two choices (the URL, not now; the default is 2) and not the answer "deploy one for me"; client only chosen at the first question, or implied by
  `--from`, keeps three choices. `QuestionNumbersCase` and `TypedClientOnlyCase` in `scripts/tests/test_setup_messages.py` hold the numbering and the menus.
- `aws_run` in `lib.sh` keeps the answer (`AWS_OUT`), the error text (`AWS_ERR`) and the exit code (`AWS_RC`, also the return value) of a call apart and cuts trailing newlines. It makes no file: a file needs
  a name in a folder that other users can write to and a clean-up that no signal can be trusted to run, and the helper files delete nothing. An inner command substitution captures the answer, the error
  stream goes into the outer one through file descriptor 3, and the exit code and the answer follow the error text after a separator drawn for the call; the error text comes first because cutting the
  pieces apart takes time in proportion to the length of the text before the first separator. `scripts/tests/test_setup_no_leftovers.py` compares it with the earlier file-based version and checks that
  whole runs leave their `TMPDIR` empty, also after Ctrl-C and SIGTERM.
- The one confirmation: `--yes --acknowledge-costs` for the full setup, `--yes` for an update, `--yes --acknowledge-teardown --account-id <id>` for a teardown (a teardown that deletes without a
  question names the account it means; without `--account-id` it stops with exit 4 before any AWS call, and another account than the credentials' is exit 7); `--yes` alone never chooses the full setup, and
  after the confirmation nothing is asked. Read-only AWS calls happen before it, nothing else (`--dry-run` and `--preflight-only` stop there).
- The options of the client-only setup (`--from`, `--sha256` and `--build` for a client of the user's own, `--endpoint` for a stage that exists) do not go with the full setup, which builds the
  client in the user's account and makes its own stage. With `--full`, with an option that only the full setup takes (`--keep-runner`, `--acknowledge-costs`, `--preflight-only`, `--build-on`,
  `--invocation-logging`, each of which turns the full setup on by itself) and with a command (`--status`, `--doctor`, `--update`, `--teardown`) they are a usage error, exit 2, where `setup.sh`
  reads its options. On a terminal the answer "deploy one for me" to the endpoint question (`question_backend`) turns a client-only run into the full setup after the options were read, and the check that
  follows it in `setup.sh` ends the run with exit 2 before the profile and Region questions. The usage error for an option of the full setup together with a client of your own names the option that was typed:
  `--full`, or else the first option that only the full setup takes (`FULL_VIA` in `setup.sh`); `--full` wins when it was typed as well. Each refusal comes before any AWS call and before anything is written. The converse holds:
  `--client-only` does not take an option of the full setup. `FullWithAClientOfYourOwnCase` and `OwnClientVersusFullCase` hold every route against the stand-in AWS CLI.
- `--teardown` (`teardown.sh`) runs on the user's machine with the user's own credentials. The authority to delete the stage is the caller's already, so nothing is delegated: it installs no runner,
  uploads no source, starts no build, assumes no role and opens no authority window, and it needs no git clone and no clean tree (a zip download holds the helper files it sources). CloudFormation
  deletes each stack with the execution role it stored with the stack (the caller needs no `iam:PassRole`). The runner role is not given the permissions of a teardown and the runner script has no
  teardown action. `scripts/teardown.sh` and `scripts/lib/teardown.py` stay the repository's own program (Python, Node, an evidence record) and the wizard never calls them.
- What belongs to the stage is decided by its tags and its stacks, never by a name prefix alone (`td_read_stacks`, `td_ours`). A stack is the stage's when its name starts with `hamh-<stage>-` (the names of the generation in use; see "Teardown by generation") and it
  carries `project=hyperscale-agent-meta-harness` and `stage=<stage>` (the CDK app puts both on every stack); the same name without the tags is not touched and is named in a `kept:` line at the end, never in
  the plan. Buckets, image repositories, state machines, rules, the sessions table, the research secret and the runtimes come from the resource list of those stacks (`TD_RES`), not from a prefix. A
  runtime, a bucket or a VPC that no stack lists is the stage's only with the stage tags (a runtime is reported with the command that deletes it, never deleted); SSM parameters under `/hamh/<stage>/` are
  the stage's only with the tags (`describe-parameters` with `Path` and `tag:` filters); a log group under `/aws/bedrock-agentcore/runtimes/hamh_<stage>_`, `/aws/lambda/hamh-<stage>-` or
  `/aws/codebuild/hamh-<stage>-` (prefixes that end in the delimiter) is deleted only when the runtime, function or project that writes to it no longer exists or goes with a stack of the stage. The runner
  stack needs the project tag.
- `td_inventory` is the one function that reads the account, and it runs before the plan, after the stacks and after the leftovers; the result is made from the last run. Every read goes through
  `td_read <category> <what> <aws arguments>`: a throttled or transient read is repeated up to 5 times with a growing pause (`HAMH_SETUP_RETRY_SECONDS`, 2 by default; the tests set 0), a missing thing
  is "not there" (`aws_missing` in `lib.sh` reads the error code, `An error occurred (<Code>)`: a code with `NotFound` or `NoSuch` in it, `404`, or a `ValidationError` that ends in "does not exist";
  a word of the message decides nothing, because a refusal that names a resource called `NoSuchBucket` is a refusal, and a text with no code, such as an unreachable endpoint or an expired SSO token, never says that a thing is gone), and any other failure is named (`could not read ...`), puts its category out of the `checked:` sentence and makes the run `INCOMPLETE` (exit 8). "Nothing to tear down" and the zero
  line need every read to have passed and found nothing; the `checked:` sentence is built from the categories that were read and passed (`td_checked_sentence`). A list of stacks that cannot be read is
  exit 1 with nothing deleted.
- The confirmed plan is the boundary of what is deleted. At the confirmation the lists of the first inventory are frozen (`TD_PLAN_STACKS`, `TD_PLAN_LOGS`, `TD_PLAN_PARAMS`, `TD_PLAN_VPCS`, `TD_PLAN_BUCKETS`); a stack, log group, parameter, VPC or bucket that shows up afterwards is reported (`appeared after the plan`) and not touched, and still counts as left. A read that failed anywhere in the run
  stays a failure of the run (`TD_UNREAD_RUN`; the inventory that follows clears only `TD_UNREAD`); the lease read of the runner bucket fails closed (exit 6, nothing deleted); a KMS key or a secret that a stack left alive is `LEFT:`; a resource that a stack lists as `DELETE_COMPLETE` is history and is ignored; a session is stopped only when its whole runtime ARN is the ARN of a runtime a stage stack
  lists, and a capacity provider id is used only when its parameter carries the stage tags; a route table association of a subnet is never removed by hand (the stage's own subnets take theirs with them), only an association with an internet gateway of the stage is; the runner stack is the stage's only when its `SetupBucket` output is `hamh-setup-<account>-<Region>`; a VPC that carries the stage tags
  and is not swept (no CloudFormation owner tag, or a stack outside the stage owns it) is `LEFT:`, exit 8, never a zero and never "nothing to tear down" (`td_vpcs_left`); the children of a planned VPC that carry the stage tags are read when the VPC is swept, so the plan fixes the VPCs and names the kinds of children, not their ids. A stack whose imports cannot be read (`td_importers` returns 2) is not
  deleted, and a read that decides the next step of the VPC sweep (the tagged interfaces, the endpoint poll) is a failure that stops the sweep. A log group is the stage's only when nothing that exists writes to it: the writer lists hold each Lambda function name and the `LoggingConfig.LogGroup` of every version (`list-functions --function-version ALL`), and each CodeBuild project name and, when a
  group is not accounted for by the name of its own writer, the `logsConfig.cloudWatchLogs.groupName` of every project (`batch-get-projects`, 100 names a call). A group that its name calls gone is kept while a function or a project of either service is set to write to it; a group that a stage stack lists is the stage's. A failed read keeps the groups and makes the run incomplete. `rn_empty_bucket`
  counts `EMPTY-BUCKET PASS` or `EMPTY-BUCKET GONE` as success only with the script's exit code 0. Residuals, stated and accepted: the APIs address a stack by name, so a stack replaced by a stranger in the seconds between the read and the delete is a race (one operator is assumed); a log group under a delimiter-terminated prefix whose Lambda function, CodeBuild project or AgentCore runtime is gone is
  the stage's (CloudWatch Logs groups made by those services carry no tags); `empty-bucket.sh` runs as long as a writer keeps adding versions; each CloudFormation wait is the AWS CLI's own (up to an hour a stack), so a run with many stuck stacks is long but bounded by the eight rounds; the lease is read before the question and not after it; a Retain-policy resource of a stage stack that no category
  reads back is not found (the CDK app has none).
- The steps of `--teardown`, after the plan (account, Region, stage, the caller's identity, every stack with its status, every bucket, image repository, rule, state machine, runtime, log group and
  parameter that will be touched by name, the research secret, the runner, what stays and the Go module store's trust of the caller; the sentence `Teardown takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface` only when the plan holds a stack, and a plan that holds only a VPC that an earlier teardown retained says that the run only retries removing the retained network resources) and the one confirmation (exit 4 without it): the lease of the
  stage names no running build and no stack is `*_IN_PROGRESS` (`DELETE_IN_PROGRESS` is waited for), else exit 7 with nothing deleted; the runner stack is removed first (its bucket emptied with
  `empty-bucket.sh`, which prints `emptied <bucket>: <n> versions and delete markers`; when CloudFormation fails on a refusal of the caller's own session, `rn_remove` reads the failed resource and its reason
  from `describe-stack-events` (only the events of the latest delete, up to the event that started it), deletes the stack again, only when the whole reason of the latest delete says that the caller (`CALLER_ARN`) `is not authorized to perform: iam:...` (another principal, a bucket policy or an SCP is not worked around; the plan names the role), with `--role-arn` of the CDK CloudFormation execution role `cdk-<qualifier>-cfn-exec-role-<account>-<Region>`, and when that fails too prints `RETAINED stack ...
  (DELETE_FAILED)` with the cause and the command, a `LEFT: stack hamh-setup-runner` line in the result and exit 8; its KMS key gets a `pending:` line), so that its authority ends before the long deletes begin; then the Bedrock model invocation logging setting is put back (`td_read_logging` in the plan, `td_restore_logging` before the first stack goes: the record `/hamh/<stage>/invocation-logging-prior`, read only when it carries the stage tags, says `{"loggingConfig": null}` or holds the configuration a deploy in the mode `enable-and-restore` found; the Region's configuration is deleted or replaced only while the setting names the stage's log group `/hamh/<stage>/bedrock-invocations`; with no record, an untagged record, another destination or a setting that is already the recorded state the output says `Left Bedrock model invocation logging as it is: <why>` and nothing is written; a restore that fails, or a record or a setting that cannot be read, is a failed step, exit 8, and `td_leftovers` keeps the record for a second run; the decision is stored for the generation that was restored, in its slot `log-keep`, so that the generation worked on last does not decide for the other); then the EventBridge rules that the stage's stacks list are disabled, the executions of the state machines they list and the
  AgentCore sessions of the harness table (on a runtime a stack lists) that are not stopped are stopped (a session on the Instances runtime also deletes its machine), and the `v<N>` endpoints of those
  runtimes are deleted and looked for until gone (`HAMH_SETUP_ENDPOINT_LOOKS`, 60 looks); then the stacks go one at a time: those the script does not know first, then `observability, delivery,
  orchestration, runtimes, control, packages, data, security, network`, each deferred while another stack imports one of its exports (`describe-stacks` outputs with `ExportName`, `list-imports`; an export that nobody imports is CloudFormation's `ValidationError` for that export, `td_no_importers`, and any other failure is a failed read), in at
  most eight rounds (a stack that ends in `DELETE_FAILED` gets two more looks after a pause, because a dependency may detach in the background). Before a stack the buckets and repositories that its own resource
  list names are emptied (`empty-bucket.sh`, which sends the governance bypass for the Go module store alone; images by digest, a hundred at a time, in rounds while ECR deletes some, then each remaining image
  on its own three times, each one that stays named) and the research secret is remembered (`td_prepare_stack`, only the secret whose ARN ends in `:secret:hamh-<stage>-research-tavily-api-key-` and six characters). The stack is not tried when a bucket or
  repository cannot be emptied (CloudFormation would refuse it). The research secret is deleted right after its stack (`td_stack_gone` calls `td_force_secrets`: `delete-secret --force-delete-without-recovery`, accepted
  also for a secret that CloudFormation scheduled for deletion), never before it: CloudFormation deletes the resource policy of the secret as a resource of the stack and cannot while the secret is marked for deletion,
  which lasts about 30 seconds after a force delete, and the fixed name would otherwise stay reserved for 30 days. The result waits for the secret to be not found (`td_await_secret`, `HAMH_SETUP_SECRET_LOOKS` looks,
  20 by default, `setup_sleep` apart; `deleted: secret ...`; `LEFT: secret ...` and exit 8 when AWS still describes it as marked for deletion after the looks, because its fixed name is not free for a new deploy, or when it is described and not marked three looks in a row). A stack that stays keeps its secret untouched, and a stack that went without `td_delete_one` seeing it (a retain call finished it)
  has its secret deleted by `td_force_gone_secrets` after the retain rounds. That step judges only the stacks of the generation in use (`td_prefixed`), because the queue holds the entries of both generations and a stack of the other one is missing from this generation's listing without being gone; `td_force_gone_secrets_all` repeats it for each generation after the delete passes, reading the account only for a generation that has a secret waiting. A stack in `DELETE_FAILED` because AgentCore still holds a network interface
  is seen as that after its first failed round (`td_hold_retain`: every resource that failed is a network resource, the stack has a VPC, and `td_enis` finds an interface held in it) and is deleted again at once with
  `--retain-resources` for the failed resources and the VPC in one call (a resource that did not fail is accepted by CloudFormation); a `ValidationError` falls back to the failed resources first and what they held
  up after them (`td_retain_failed`). When the interfaces cannot be read or none is held, the rounds apply as before and `td_retain_stuck` retains what still fails, in up to five rounds. Retained resources are
  reported as `RETAINED` with the by-hand commands in the order that works (`td_retained_commands` and `td_say_commands`: `delete-vpc-endpoints`, `delete-subnet`, `delete-security-group`, `delete-route-table`, `delete-vpc`; a command appears only for a resource that is retained), never as zero resources; the inventory line is
  `TEARDOWN-INVENTORY RETAINED ...`. A VPC that this run retained is not swept in the same run (`TD_RETAINED_VPCS`) and is not called a late arrival. Before the delete of a stack that owns a VPC, `td_note_held_interfaces` reads the interfaces of that VPC once with `td_enis` (which also counts the interfaces of type `agentic_ai`, `TD_ENI_AGENTIC`; the NAT gateway, VPC endpoint and firewall interfaces of the stack are requester-managed and of other types) and prints one line, once per VPC, when AgentCore holds one: CloudFormation then retries the subnet and the security group for about 20 minutes before the stack ends in `DELETE_FAILED`. A read that fails costs only the line. The wait for the `v<N>` endpoints (`td_delete_version_endpoints`, `td_wait_endpoints_gone`) runs under the same ticker (`still waiting for AgentCore to delete the versioned runtime endpoints`). Every wait for a stack prints `still waiting for ...` every
  `HAMH_SETUP_PROGRESS_SECONDS` seconds (180 by default; 0 turns it off): `setup_ticker_start` and `setup_ticker_stop` in `lib.sh` run a background job whose sleep runs in its own background without standard output (stopping the ticker ends it, so no sleep holds a pipe or stays
  behind) and which looks at its parent shell every `HAMH_SETUP_TICKER_CHECK_SECONDS` (5) and ends silently when it is gone (a background job ignores Ctrl-C); `setup_number` reads `HAMH_SETUP_PROGRESS_SECONDS` and `HAMH_SETUP_SECRET_LOOKS` as decimal numbers (`08` is 8) and turns a
  non-number into the default. `td_hold_retain` decides the hold only for a stack with exactly one VPC and records the retained resources as soon as CloudFormation accepts the call.
- Only when no stack of the stage is left (`td_no_stacks`: the stacks and their resources were read, and none is listed) the leftovers go: the VPCs that carry the stage tags and that no existing stack
  owns (the stack that made it is gone: the tags `aws:cloudformation:stack-name` and `aws:cloudformation:stack-id` of the VPC are compared with the `StackId` of the stack that has that name now, so a VPC that an earlier
  network stack retained is found also when the stage was deployed again and a new stack has the same name; a tag with a name and no id decides by the name), the log groups and the parameters of the inventory, and a stage bucket that no stack listed and that carries the
  stage tags. A VPC is swept only for such a VPC and only its resources that carry the stage tags go: interfaces that are available, VPC endpoints (the answer's `Unsuccessful` list is read and the endpoints
  are looked for until gone), subnets, security groups, route tables (their associations first; never the main one) and internet gateways; then the VPC. What does not carry the tags stays and is named, and the
  VPC then stays. `describe-network-interfaces` asks for managed resources too (`--include-managed-resources`; an AWS CLI that does not know the option is asked again without it and the output says that
  interfaces AWS services manage may be hidden); while any interface is in use or managed the run changes nothing and says `RETAINED`. The account is read back (the inventory), the KMS keys and secrets that
  the stacks owned are listed as `pending:` with their deletion date (`td_date_only`: year, month and day, no time of day), the tag index is listed as information only (it lags, cannot see untagged resources, and nothing depends on it), and the output ends with
  `TEARDOWN-INVENTORY 0 stacks stage=<s> runner=none` and what was checked and what was not, or with `RETAINED` or `INCOMPLETE` and what was checked. `kept:` lines name the CDK bootstrap stack, its bucket and
  repository (they still hold the files and images the deploy published), the client, the receipt and what was found and not touched. The zero line is never printed while a stack, a retained resource,
  something of the stage or the runner is left, a step failed or a read failed (exit 8). Interrupting it leaves the account in a state the next run reads; every step is repeatable.
- The Go module store lets only the stage operator (and the runner role) delete its objects. `td_check_go_store` reads its bucket policy and warns in the plan when it does not name the caller's
  role or user; the delete is tried anyway, because the policy decides, and a refusal names the identity the policy does name and the command that ends a dead end
  (`aws s3api delete-bucket-policy`, run by the bucket's owner, when that identity no longer exists). A `delete-stack` refused with "is invalid or cannot be assumed" (the execution role of the CDK
  bootstrap, which the stack stored, is gone) prints `aws cloudformation delete-stack --stack-name <stack> --role-arn <ARN of that role>` as the way out; for a stage stack the teardown never picks a role itself. Only the runner stack is deleted again with the CDK execution role (`rn_remove`).
- `empty-bucket.sh <bucket>` removes every object version and delete marker so CloudFormation can delete the versioned bucket (the templates have no custom resource). It accepts the setup bucket
  `hamh-setup-<account>-<Region>` and the two buckets of the stage, `hamh-direct-harness-<account>` and `hamh-direct-gomod-<account>`, and the same three names under the legacy word (their Region comes from `AWS_REGION`, which `rn_empty_bucket` sets
  to the Region of the run; the name has none), sends `--bypass-governance-retention` for the Go module store alone, passes the account of the name as the expected bucket owner on every call, and
  uses the AWS CLI alone (`AWS_CLI_PROG`, default `aws`; it removes only the temporary files it makes). It lists ONE page with `list-object-versions --no-paginate --max-keys 1000` (S3 returns at most 1,000
  entries per call, versions and delete markers together; `--max-items` counts the versions only and adds every delete marker of the pages it read, which can exceed the 1,000 keys `delete-objects`
  accepts), deletes exactly that page, lists again, and stops with a failure when a delete answers an `Errors` entry, when the same page comes back after its delete, or when a page holds more than 1,000
  entries; only the answer `{"Objects": []}` is an empty bucket (a null, an object in place of the array, or keys without version ids is a failure); a throttled call is repeated up to 5 times. It uses the profile of the run as `--profile` (`HAMH_EMPTY_BUCKET_PROFILE`, like every other call of the run, so exported access keys cannot make the step
  another identity), prints `EMPTY-BUCKET identity <arn>` first and stops when `HAMH_EXPECT_CALLER_ARN` (the plan's identity) differs, and ends with `EMPTY-BUCKET PASS bucket=<b> deleted=<n>` or
  `EMPTY-BUCKET FAIL <detail>`. `setup.sh` runs it and treats a bucket that is gone as nothing to empty.
- `setup.sh` calls the client as `"$HAMH_CLIENT_BIN"` and builds its settings directory from `$HAMH_CONFIG_DIR_NAME`; it sources `names.sh` before the helpers, and when that file is missing
  next to it (a client-only setup from `setup.sh` alone) it falls back to the defaults `names.sh` has. Those three lines are the only place where the commands and the text of `setup.sh` hold the literal client name (its comments are not scanned; names of resources stay literal, see
  the section on `names.sh`).

### The client's names and the client build (`names.sh`, `build-client.sh`, `client_check.py`)

- `names.sh` is the one definition of the client's names and platform table. It is sourced (`. "$here/names.sh"`) by `setup.sh` and by the scripts that run the build, and run with `bash` it
  prints the definitions. `HAMH_CLIENT_BIN` (default `ham`) is the command, the cargo package and the binary; `HAMH_ADMIN_BIN` (default `"$HAMH_CLIENT_BIN"-admin`, a different name from the client's) is the operator tool, built and installed beside the client; `HAMH_PRODUCT_NAME` (default `Hyperscale Agent Meta Harness`) is the product's name for text that names the product, and no message of `setup.sh` prints it: they call the product "the system" and name the client with `$HAMH_CLIENT_BIN`;
  `HAMH_CONFIG_DIR_NAME` (default `ham`) is the directory below `~/.config` that holds the client's files; `HAMH_CLIENT_PLATFORM_TABLE` has one row per client platform,
  `<platform> <Rust target> <binary kind>`: `darwin-arm64` `aarch64-apple-darwin` `macho-arm64`, `linux-amd64` `x86_64-unknown-linux-musl` `elf-x86_64`, `linux-arm64`
  `aarch64-unknown-linux-musl` `elf-aarch64`. The environment overrides each value. Functions: `ham_client_platforms`, `ham_client_target <platform>`, `ham_client_kind <platform>` and
  `ham_host_platform [os] [arch]` (an Intel Mac and any other machine without a row are refused with the reason). Bash 3.2; nothing is started when the file is sourced.
- A script of this feature runs the client as `"$HAMH_CLIENT_BIN"`, builds config paths with `"$HAMH_CONFIG_DIR_NAME"` and writes the client's name in messages, prompts and plans as `$HAMH_CLIENT_BIN`. The help
  text writes `@CLIENT@`, which `show_help` replaces; it reads its here-document line by line, because bash 3.2 fails to parse a here-document with an apostrophe inside `$( )`.
  `scripts/tests/test_setup_names.py` scans every non-comment word of `setup.sh` and of each script here except `names.sh` (commands, assignments, paths, printed text, prompts, the arguments of `fail`,
  `warn` and `die`, and the bodies of here-documents) and fails on the literal client name; the only allowed uses are the three default lines of `setup.sh`. `scripts/tests/test_setup_messages.py` runs
  `setup.sh` with `HAMH_CLIENT_BIN=ham` and `HAMH_CONFIG_DIR_NAME=ham` through the help, a dry run, an install, failed checks and the questions and wants no `ham` left in the text outside resource names and the
  `/hamh/<stage>/` namespace, and holds golden transcripts of the default names in `fixtures/setup/golden/`.
  What stays literal and is not scanned: names of resources and parameters (`hamh-direct-*`, `hamh-setup-<account>-<Region>`, the repository prefix `cc:hamh-e2e-`, the namespace `/hamh/<stage>/`), comments and the
  documents. The rename to `ham` is therefore one change in `names.sh` plus the infrastructure names, the Rust crate and binary names, and the documents. `names.sh` is the only file that assigns the names.
- `build-client.sh <out-dir> <bucket> <build-id>` cross-builds the client and the operator tool for every row of the table (`cargo zigbuild --release --locked --target <target> -p "$HAMH_CLIENT_BIN" --bin
  "$HAMH_CLIENT_BIN" -p "$HAMH_ADMIN_BIN" --bin "$HAMH_ADMIN_BIN"` through `cargo-slot.sh`: one run builds both), checks each file with `client_check.py` (the same kind for both), writes `<out-dir>/<platform>/<client>`,
  `<out-dir>/<platform>/<admin>` and `<out-dir>/SHA256SUMS` (a line `<sha256>  <platform>/<name>` for each file), and only when every file of every platform passed uploads them with `aws s3 cp` to
  `s3://<bucket>/client/<build-id>/<platform>/<name>` and `.../SHA256SUMS` (last). stdout is one JSON object
  `{"<platform>": {"key", "sha256", "bytes", "admin": {"key", "sha256", "bytes"}}}` and nothing on a failure; stderr ends with `BUILD-CLIENT PASS ...` (exit 0) or `BUILD-CLIENT FAIL <platform> <reason>` (exit 1; a
  reason about the operator tool starts with its name); usage errors exit 2.
  A platform that cannot be built is a row removed from the table with its reason; the script never skips one. The environment, `HAMH_ACCOUNTS_FILE` among it, reaches cargo unchanged. The
  cargo slots live in `CARGO_SLOT_DIR`, else `$HAMH_TOOLS/locks`, else `<out-dir>/.cargo-slots`, so nothing is made below the user's home folder. Test hook: `BUILD_CLIENT_CARGO_SLOT`.
  The runner names the operator tool after the build's binary name (`$SETUP_BINARY_NAME-admin`), so the build environment carries one name. `runner_state.py client-check [--part admin] FILE` checks the section
  (a platform without the operator tool, or an entry with a member other than these, is refused) and lists the client's rows, or the operator tool's with `--part admin`; the `client` phase reads every one of
  those files back from the bucket and compares its size and SHA-256.
- The wizard downloads both files in `cl_fetch` (`cl_get_checked` for each): the file must hash to the value `result.json` names and to its line of `SHA256SUMS`, and only when both pass do they enter the
  folder `~/.config/<config dir>/setup/<account>/<region>/download/verified` with a `SHA256SUMS` that lists both. Either failing leaves the folder as it was and installs nothing; a result that lists no operator tool is such a failure (exit 5).
  `setup.sh` installs the operator tool beside the client, by the same steps (a temporary file in the same folder, then a rename), when the folder given as `FROM` holds it: its checksum comes from the
  `SHA256SUMS` of that folder alone, a file without a line or with another digest stops the setup before anything is installed, and a client-only setup with a folder that lacks it installs the client
  alone. The local route (`--build-on local`) compiles both in one cargo run (`BUILD_ADMIN=1`, set by `full_local_flow`; `--build` of the client-only setup builds the client alone). `--status` prints
  `operator tool:` with its path or that it is missing; `--doctor` prints `PASS operator-tool` when `--version` of the file under the client's folder runs, `FAIL operator-tool` when it does not, and
  `WARN operator-tool` (not a failure) when there is none.
- `client_check.py <macho-arm64|elf-x86_64|elf-aarch64> <file>` prints `OK <kind> <facts>` (exit 0) or `FAIL <code>: <reason>` (exit 1). For `macho-arm64` it requires a thin arm64 executable
  with an `LC_CODE_SIGNATURE` whose SHA-256 page hashes match the file (Apple Silicon kills an unsigned or altered arm64 binary at launch; the zig linker signs ad hoc). For the ELF kinds it
  requires a 64-bit little-endian executable of the right machine with no program interpreter, so the musl builds start on any Linux. Standard library, Python 3.7 or later.

### The runner side (`runner-build.sh` and its helpers)

- `runner-build.sh` (no argument) is the whole build of the runner. `names.sh` must be in the source archive (the run stops in `inputs` without it). A deploy runs nine phases (`inputs`, `fetch`,
  `toolchain`, `account-file`, `deploy`, `gomod`, `scanner-rules`, `client`, `outputs`). Inputs are the environment variables of the contract, `CODEBUILD_BUILD_ID` and `AWS_REGION`; any
  `SETUP_ACTION` other than `deploy` or `bootstrap-template` (a teardown included) stops the build in `inputs`. It reads the objects from the bucket `hamh-setup-<account>-<Region>`, checks the digest of the file and the commit the
  archive names (the zip comment `git archive` writes), makes a git repository of the extracted archive, installs the toolchain, lays the generated account file in `<work>/account/accounts.json` and
  exports it as `HAMH_ACCOUNTS_FILE` (an archive of a commit that does not read the variable gets the file as `accounts.json`, committed on top), writes an AWS config with a
  profile of the file's name for the container credentials, runs `deploy.sh --stage S --azs N --fork-source build`, `gomod-store.sh fill --reuse-current` (and `check` unless the fill ended in `GOMOD-FILL CURRENT`), `scanner_kit_rules.py build` and
  `publish --apply`, and `build-client.sh <out dir> <bucket> <build id>`, then reads the stack outputs. A deploy that `deploy.sh` refuses with `DEPLOY-FAIL guard ...` ends as the phase `guard`.
- A bootstrap-template build runs `inputs` (no bootstrap check), `fetch`, `toolchain` (`RUNNER_TOOLCHAIN_ONLY=template`: the pinned AWS CLI and a Docker daemon, no Rust and no Go),
  `account-file` (no CDK smoke run) and `template`: `scripts/node.sh cdk bootstrap --show-template`, refused when it fails, prints nothing, prints more than 51,200 bytes or a template whose SHA-256 is
  not the one in `engine/policy/cdk-bootstrap-template.pin`; otherwise the template goes to `bootstrap/cdk-bootstrap-<12 hex>.yaml` and `result.json` names it as `bootstrapTemplate`. Never skipped.
- Results: `results/<build id>/phases.jsonl` (one JSON line per phase: schema, buildId, stage, sourceCommit, configDigest, name, state, seconds, detail, at, and `from`, `deploymentDigest` or `binary`
  where they apply) and `results/<build id>/result.json`; a failure in any phase still writes both. Exit 0 or 1. `runner_state.py` builds every JSON document; the shell never writes JSON by hand.
- Re-runs: `deploy` and `gomod` are skipped when an earlier build recorded the phase as ok for the same stage, source commit and configuration digest and the account shows the work is still
  there: the account's deployment digest is the recorded one, the account's `/hamh/<stage>/deployed-source-tree` is the source tree of this archive (`scripts/lib/digests.py`) and its
  `/hamh/<stage>/deployment-config-digest` is `SETUP_CONFIG_SHA256`, and all nine stacks are in `CREATE_COMPLETE`, `UPDATE_COMPLETE` or `IMPORT_COMPLETE`, with `UPDATE_ROLLBACK_COMPLETE` excluded because it signals a rolled-back update after the recorded deploy; the full `gomod-store.sh check` passes. A record in the bucket
  never skips a phase on its own, because other principals of the account may have written it. A skipped phase names the build in `from`. `scanner-rules` always runs: the publish step
  leaves a version that is already there alone. `client` always runs: it is cheap next to the deploy, and it builds the one file that is installed and run on the user's machine.
  The `gomod` phase of a build that is not skipped asks the store first (`fill --reuse-current`): when the newest manifest of the store was written by a fill with the same roots, tool roots,
  Go version, upstream and checksum database, the fill ends in `GOMOD-FILL CURRENT` and the phase ends there, without resolving a module and without the full check; an update that changes none of them
  saves the 9 to 12 minutes of the fill and the check. A fresh store, other modules, a manifest that cannot be read and an offline check that did not pass fill and check in full.
  `RUNNER_BUILD_NO_SKIP=1` turns skipping off. `RUNNER_BUILD_WORK` sets the work folder (default beside the source folder). The script resolves the work folder to a physical path (absolute, no `..`, no link) before deriving any path from it.
  The buildspec names it `$CODEBUILD_SRC_DIR/../hamh-setup-work`, and `node.sh` compares the account file in it with the checkout.
- The `outputs` phase runs only after the `deploy` phase of the same build passed and accepts `CREATE_COMPLETE`, `UPDATE_COMPLETE`, `IMPORT_COMPLETE` and `UPDATE_ROLLBACK_COMPLETE` for each of the nine stacks (`stacks_complete after-deploy`). CloudFormation leaves `UPDATE_ROLLBACK_COMPLETE` unchanged on a deploy with no stack changes, and the stack holds the state from before the failed update. Every other state, `UPDATE_ROLLBACK_FAILED`, `ROLLBACK_COMPLETE`, every `*_FAILED` and `*_IN_PROGRESS` state and `DELETE_*` included, is refused with the stack and its state named. `StackStateContractCase` in `scripts/tests/test_setup_contract.py` runs the wizard's `pf_stage` and this phase over every CloudFormation stack state and fails when they disagree about which states are complete.
- Before anything of the archive or the account file is used, `fetch` hashes the bytes it read and compares them with `SETUP_SOURCE_SHA256` and `SETUP_CONFIG_SHA256`; a difference stops the build
  there. After a result that says `SETUP-PASS` the runner writes `<work>/anchor.env` (see the wizard side).
- The runner role cannot write the Bedrock invocation-logging setting, so the `account-file` phase refuses an accounts file whose `invocationLogging` is `enable-and-restore`.
- `runner-toolchain.sh <tools folder>` installs, with SHA-256 checks, the `tools.lock` tools the deploy builds with (rustup-init, cargo-lambda, cargo-zigbuild, zig and the pinned AWS CLI, put first on
  PATH), the Rust toolchain (a re-run installs the pinned toolchain, components and targets again, so an interrupted install is completed), the Go toolchain of `images/tools/lang/lang.lock` and a
  running Docker daemon, and writes `<tools folder>/env.sh`. Last line `TOOLCHAIN PASS ...` or `TOOLCHAIN FAIL <step> <detail>`. With `RUNNER_TOOLCHAIN_ONLY=template` it installs the AWS CLI and
  starts the daemon and nothing else (last line `TOOLCHAIN PASS awscli=<v>`).
- Every AWS CLI call of the scripts that run with the runner role's credentials is listed in `infra/setup-runner-policy.json` with the IAM actions it needs;
  `scripts/tests/test_setup_runner_policy.py` scans the scripts and fails on a call the file lacks. A new call is added there first. `scripts/teardown.sh` and `scripts/lib/teardown.py` are not in that
  list because the runner never runs them: the teardown runs with the caller's credentials (see the wizard side), and the runner role holds no delete, stop or IAM permission.

## Work Guidance

- Test first. The suites are `scripts/tests/test_setup_full_units.py` (the JSON reader, the account file, the receipt, the platform), `scripts/tests/test_setup_full_flow.py` (the full setup, its
  refusals, failures, source and bucket handling and the local route), `scripts/tests/test_setup_full_state.py` (attach, lease, interruption, `--status`, `--doctor`, `--update`, the invariants of `--teardown`), `scripts/tests/test_setup_update_guard.py` (the look at the running runs before an update installs anything, and the clean-up of the runner after an update that stopped before it changed the stage), `scripts/tests/test_setup_teardown_local.py` (the teardown: authority, order, quiescing, leftovers, failures, the retained network resources, the runner), `scripts/tests/test_setup_teardown_logging.py` (the Bedrock model invocation logging setting: restore, left alone, failures, plan),
  `scripts/tests/test_setup_full_wizard.py` (the questions on a pseudo-terminal) and `scripts/tests/test_setup_full_static.py` (the rules above, the help text, the option conflicts and the rename), `scripts/tests/test_setup_messages.py` (golden transcripts of the default names, the renamed
  client, the numbering of the questions and the typed `--client-only` menu), `scripts/tests/test_setup_no_leftovers.py` (no temporary file; `aws_run`) and `scripts/tests/test_setup_bootstrap_stack.py` (an existing `CDKToolkit`).
  They run against `scripts/tests/setup_fake_aws.py`, a stand-in AWS CLI with a world of its own that plays the runner with `runner_state.py`, so the files it writes are the real ones, and against
  the fixtures of `fixtures/setup/`. A new AWS call needs its handler in the stand-in and a test that pins the arguments. A change to a name or a format that both sides use is made in both
  places and in `scripts/tests/test_setup_contract.py` in one change.
- Keep the AWS calls to the AWS CLI with `--query` and `--output text`; the only JSON that is parsed is what the runner and setup.sh write (`json_flat`).
- A new default ships with its way to change it (a flag of `setup.sh`), as the project tenet says.
- A new script here sources `names.sh`, takes the client name from it, keeps stdout for the result the caller reads and writes progress to stderr.
- A new client platform is a row in `names.sh` and a case in `client_check.py` before it is a build.
- The runner scripts run on Linux (CodeBuild Amazon Linux 2023); `empty-bucket.sh` also runs on macOS and stays compatible with bash 3.2 and BSD tools. Tests run on a Linux machine with
  `$HAMH_PYTHON` naming a Python 3.11 or later interpreter; they use a fake `aws` and stand-ins for the scripts the build calls.

## Verification

```bash
export HAMH_PYTHON=python3   # Python 3.11 or later
$HAMH_PYTHON -m unittest discover -s scripts/tests -p 'test_setup_*.py'
$HAMH_PYTHON -m unittest discover -s scripts/tests -p 'test_client_check.py'
$HAMH_PYTHON -m unittest discover -s scripts/tests -p 'test_build_client.py'
```

`test_setup_runner_build.py`, `test_setup_runner_toolchain.py`, `test_setup_runner_empty_bucket.py`, `test_setup_runner_policy.py`, `test_setup_runner_template.py`,
`test_setup_runner_buildspec.py` and `test_setup_runner_cfn_yaml.py` match the pattern above; the full list is in `scripts/tests/AGENTS.md`.

A run under the real bash 3.2 (the bash of macOS) needs only Docker. The BusyBox tools of that image stand in for the BSD tools of a Mac and found two defects that the
GNU tools of a build host hide (an awk regex literal with a slash inside brackets, which the macOS awk reads as the end of the pattern, and a `tr` character class):

```bash
docker run --rm -v "$PWD":/w:ro bash:3.2 sh -c 'apk add --no-cache python3 git zip >/dev/null && cp -r /w /work && cd /work && python3 -m unittest discover -s scripts/tests -p "test_setup_full_*.py"'
```

Component tests here do not replace the run from a fresh clone in a fresh account with only the AWS CLI; that run is the acceptance.

## Child DOX Index

None.
