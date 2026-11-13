# scripts/lib

## Purpose

Helpers behind the `scripts/` wrappers: logic that is easier to test in Python than in bash.

## Ownership

Owned by `scripts/AGENTS.md`, which documents each wrapper's user-facing contract.

## Local Contracts

- `aws_target.py`: reads the root `accounts.json` (schemaVersion 1, selector
  `HAMH_AWS_PROFILE`; per entry `account`, `region`, `profile`, `role` active|history|test, `azs` and `azIds`
  per stage, `invocationLogging` enable-and-restore|read-only) and applies its rules (exactly
  one active entry (a `test` entry never counts), unique profiles and account-Region pairs, AZ names in the entry's Region paired one to
  one with AZ IDs). `select()` is the entry `HAMH_AWS_PROFILE` names, else the active one (`TargetError`
  otherwise); `module_target()` is `select()` for module-level constants (a refusal prints `aws_target:
  <reason>` and exits 2); `accounts()`, `is_known()`, `pinned_regions(home, inference)` (home first, then
  criteria order). CLI: `shell` (the four assignments bash evals), `get <field>` and `digest` (`committed`, or the SHA-256 of the file in use). The account-file
  override: `HAMH_ACCOUNTS_FILE` (`FILE_HOOK`), an absolute path read in place of the committed file by `accounts_path`; a relative path is a
  `TargetError`; `config_digest` is the deploy record's `deploymentConfigDigest`. Python 3.8+.
- `source_bundle.py`: `add-accounts-file <zip> [--expect <sha256>]` adds the file `HAMH_ACCOUNTS_FILE` names to the source zip of `codebuild-run.sh` as the member
  `.hamh-build/accounts.json` (fixed time and mode) and prints its digest. `child-build-env.sh` is sourced by the first command of every buildspec under
  `images/codebuild`: given `HAMH_ACCOUNTS_FILE_SHA256` it checks that member and exports `HAMH_ACCOUNTS_FILE` for the commands after it, else it does nothing. POSIX sh.
- `build_selection.py`: which accounts.json entry a Rust build was made for, and where a deploy
  builds. `target_dir(root, t)` is `<root>/target/deploy/<account>-<region>` (the `CARGO_TARGET_DIR` of a deploy) and
  `lambda_dir(root, t, stage)` its `lambda/<stage>`. `check_binary` (repo-gateway: the selected
  account id; any other name: `None`), `check_build_output` (the harness-protocol
  `aws_target.rs` files under a target directory name the selected entry; the newest of a build root decides) and
  `check_driver` (a live-run driver such as ham-e2e: the selected account id and no other entry's) raise `Mismatch` naming the binary
  and both selections. `deploy.py` and `agentd_build.py` check every build with the first two before any upload. Why only those binaries, and what the real builds embed, is in its docstring.
  CLI: `check-driver <binary>` (exit 0 silent, 1 with the reason on stderr, 2 on usage). Python 3.8+, standard library.
- `aws-target.sh`: sourced by wrappers after `python.sh`; exports `HAMH_ACCOUNT`, `HAMH_REGION`,
  `HAMH_PROFILE` and `AWS_PROFILE` from `aws_target.py shell`, or exits 2.
- `path-canon.sh` is sourced and never executed. It requires bash 3.2 or later and uses no `readlink -f` or `realpath`. `ham_canon_path <absolute path>` prints the physical path. It resolves `.` and `..`
  and follows every link, including the last component. A `..` after a link moves up from the link's target. A nonexistent path is resolved as far as it exists, as `os.path.realpath` does.
  It returns 1 for a relative path or more than 40 links. `ham_path_is_under <dir> <path>` returns 0 when the resolved path is below the resolved directory, 1 when it is outside, and 2 when either
  cannot be resolved. `ham_file_mount_args <checkout> <file>` sets `ham_file_canon` and the array `ham_mount_args`, which contains the docker `-v` arguments for a file the mount of the checkout does not show.
  `node.sh` uses it for `HAMH_ACCOUNTS_FILE`. A script that determines whether a path containing `..` or a link is inside or outside a directory calls it and never matches the path text.
  `<checkout>/../work/accounts.json` begins with the checkout's path and lies outside it. `../tests/test_path_canon.py` checks it against `os.path.realpath` and the paths of a failed runner build.
- `tools_lock.py`: `tools.lock` reader and installer (`install`, `check`, `gate-tools`, `get <table.key>`,
  `list`); `load()` raises `LockError`. `gate-tools` builds the gate tool set from the
  `[gate_tools]` table (contract in `scripts/AGENTS.md`). `dest_roots` lists the directories a destination may be in: the working directory and `TMPDIR`, or `TOOLS_LOCK_ROOTS` alone when it is set. Requires Python 3.11+ (`tomllib`).
- `gomod_store.py`: implementation of `scripts/gomod-store.sh`: the pure parts (Go path escaping, semver order, pseudo-versions, `latest`, `merge_list`, the module-list reader and the refusals), `GoTool` (the fetch environment and the lock step's environment), the
  consumer resolve, the harvest and checksum-database check, the staged tree, the offline check, and `Aws`/`Bucket` (the aws CLI, assumed-role credentials in the child environment only, create-only and compare-and-swap puts). Python 3.8+, standard library only.
- `engine_source.py`, `engine_fetch.py`, `engine_recipes.py`: implementation of `scripts/engine-fetch` (contract and exit codes in `scripts/AGENTS.md`; the source files in `engine/sources/AGENTS.md`).
  Python 3.8+, standard library only (no `tomllib`: the CodeBuild host of the image has Python 3.9). `engine_source.py` reads a strict TOML subset (a repeated key, an unknown escape or any other syntax is an error, so a partial parse
  never passes), validates the three kinds (a pin is a full commit or a SHA-256, never a branch or "head") and rewrites one value in place for `update`. `engine_fetch.py` holds the adapters (`Fetcher.git_tag`, `git_path_bundle`,
  `release_asset`), `Git` (an isolated environment), `download` (https only, retried on a transport error including a cut or short answer and a bad status line, never on a hash mismatch), `member_bytes` (one archive member by name), `staged` (a destination
  filled beside itself, with the mode of a new directory), `verify_fetched` and `verify_checkout` (a git checkout against the series tree, the base commit and a clean tree), `run` (a program that cannot be started is a
  FetchError of the class of the caller), `resolve_ref` and the command line, whose last handler turns an unexpected error into one `ENGINE-FETCH FAIL internal` line and exit 11. `engine_recipes.py` holds the `esbuild-bundle` recipe (`build_goal_plugin`, `verify_goal_plugin`, the rendered NOTICE, manifest and PROVENANCE), the plugin lock reader and writer,
  `check_all` and `update_goal_plugin`. It deletes only the scratch directories it made itself (`tempfile.mkdtemp`). Tests: `scripts/tests/test_engine_source.py`, `test_engine_fetch.py`, `test_engine_recipes.py`.
- `third_party_scan.py`: the check that no third-party engine or plugin source is tracked: `scan` over `git ls-files -s` and the work tree, with the rules `forbidden-path`, `forbidden-hash`, `licence-file` (upper-case names with any suffix; in any case license, licence, copying, copyright, notice, provenance, unlicense and third-party-notices with no suffix, a text suffix or a licence name), `gitlink`, `symlink`, `vendor-dir` (in any case), `engine-layout`, `engine-size`, `bundle-comment`, `long-line`,
  `binary-file` and `rules-copy`, and the diagnostic `header_signals`. `ENGINE_ALLOWED` lists what `engine/` may hold; a new path is added there on purpose. CLI: `third_party_scan.py [ROOT]` prints `THIRD-PARTY-SCAN PASS|FAIL` (exit 0, 1, or 2 when ROOT is not a git work tree). Python 3.8+, standard library only.
- `image_key_guard.py`: implementation of `scripts/image-key-guard.sh` (contract in `scripts/AGENTS.md`);
  reads `engine/policy/image-key.pin` (one 64-hex line after `#` comments) and `digests.worker_image_key`.
  Python 3.8+, standard library only.
- `digests.py`: `source_tree`, `image_key` (every hashing function takes a tree-ish `rev`, default
  `HEAD`; `WORKER_IMAGE_PACKAGES` and `worker_image_key` give the worker image's key over the crate closure
  of agentd, the value `deploy.py` tags the image with; with `local_crate_closure` over `cargo metadata`;
  `IMAGE_EXCLUDED` paths `engine/catalog` and `engine/policy` stay out of the key; `accounts.json` is in
  both the deployable paths and every image key, since each binary carries the selection it was built with),
  `image_digest`, `template_hash` and `deployment_digest`, plus `STACK_NAMES` (the 9 stacks of
  stage `direct`). The one definition of `sourceTree`: `deploy.py` writes it.
- `deploy_preflight.py`: immutable composition and execution for `scripts/deploy-preflight.sh`. `register` validates all common and future engine reads and checks before launch, then `AwsReader.read` rechecks the independent exact-operation policy immediately before its one AWS subprocess boundary. Evaluators receive only immutable context and declared read results. JSON parsing rejects named non-finite constants and finite-syntax numbers that overflow. The current command passes an empty engine sequence. The account check also applies the deploy's operator rule (`aws_target.operator_may_deploy`) to the ARN of the same answer, and halts later reads on any non-pass. VPC rows and CIDRs are validated before the owned-stage exemption. Output uses stable reason codes and masks account IDs. Python 3.11+, standard library only.
- `deploy.py`: implementation of `scripts/deploy.sh`; flow and test hooks `DEPLOY_*` in its
  docstring. Right after the runtime checks, before the SSM digests, it looks up the control-stack endpoint (30 seconds a read, three reads when the stack gives no answer); its last step before the pass line is the read-only Go module store report. Every AWS JSON response rejects named non-finite constants and finite-syntax numbers that overflow. The AZ and VPC checks use the evaluators of `deploy_preflight.py`; an answer they cannot read is the refusal `azs unavailable` or `vpc-cidr unavailable`, whose line names the `aws ec2` call and what to do, and a VPC refusal names only VPCs that are not the stage's own. The endpoint must be the single `V1ApiUrl` output and the selected Region's lowercase HTTPS execute-api host at `/direct`, with no credentials, port, whitespace, query or fragment. The value is never printed. The deploy and settings records succeed before the safe readiness line. It deploys stage `direct` only and targets the selected entry of the deployed tree's `accounts.json` (`HAMH_ACCOUNTS_FILE`
  names another file; the record then carries `deploymentConfigDigest` beside `sourceTree`, SSM `/hamh/<stage>/deployment-config-digest` keeps it, and a deploy whose digest differs from that record says so before it builds anything). Before any build it checks the caller's account, the AZ name against its AZ ID and the stage VPC CIDR 10.50.0.0/16 against the
  account's other VPCs; it checks the CDK bootstrap version after the synth and creates a missing bootstrap only after every check that can refuse. It builds the Lambdas, the Cline fork binary (present in the stage bucket, copied from the
  account that holds it, or built on CodeBuild ARM and promoted only on the lock's SHA-256), the worker image and the delivery image, runs one
  `cdk deploy` pass over every stack, checks the `live` and `v<N>` runtime endpoints, records invocation logging with its prior state, and writes
  digests to SSM and a deploy record. The worker image is the `BinaryImage` value `WORKER_IMAGE`, built from the zigbuild-cross-built `agentd`;
  `image_tag` and `build_binary_image` serve it. `resolve_tools_mode` (`TOOLS_MODES`, `DEFAULT_TOOLS_MODE`, `ToolsModeError`) resolves the stage setting `HAMH_TOOLS_MODE` by the same table as `infra/lib/config.ts`
  (`infra/test/fixtures/tools-mode.json`); the `Deployer` resolves it in its constructor (a bad word is a usage error from `main`), hands the resolved word to every cdk call, compares it with `AGENTD_TOOLS_MODE` of the worker runtime
  version the live endpoint serves (`current_tools_mode`, from the same `GetAgentRuntime` read as `current_image`) and asks the deploy guard once, before the Lambda build, when they differ. That live read happens before the build, so a `GetAgentRuntime` answer without a container image ends the deploy as `DEPLOY-FAIL live-runtime` and never as a traceback.
  The Instances compute type has the same shape: `resolve_compute_instances` (`COMPUTE_INSTANCES_MODES`, `ComputeInstancesError`, one table with `infra/lib/config.ts`: `infra/test/fixtures/compute-instances.json`), `compute_instances_state` (the runtimes stack has the output `InstancesCapacityProviderArn` or `WorkerPythonInstancesRuntimeId`), `refuse_instances_off` (a stage that has the part and a setting that resolves to off, an explicit `off` or `default` or a record that does not keep it, is refused with `instances-off` before the guard and before any cdk call; a deploy never removes the part, the stage teardown does) and the guard when the part is added; `provider_replacement` asks it again after the synth when the provider's name (a hash of its configuration) differs from the live one. `RUNTIME_WORDS` is the table of isolation words each worker runtime must serve (`AGENTD_ENGINE_SANDBOX`: `uid`, and `landlock` with `AGENTD_ENGINE_USER` set to the empty string on the Instances runtime; `SANDBOX_WORDS` is its sandbox column); `isolation_state` compares them with the live runtimes before the build (a lacking or different word is a runtime change that asks the guard, `inputs.isolationWords` in the record) and `check_runtimes` requires them after the deploy. Requires Python 3.11+ (`tomllib`).
  The stage settings: `load_settings` reads
  `stage-settings.tsv`; the constructor checks each explicit value against its pattern (`SettingError`, exit 2; an empty value is unset, `default` returns to the
  code default); `settle_settings` resolves each setting as explicit, else the `set.` line of SSM `/hamh/<stage>/settings`, else (a stage without a record) the
  live value (`live_setting`, which names the source of each row), else the default (`SETTING_DEFAULTS`; the utility model's from the catalog and the Region;
  the keep tag's and the stage kind's from the role of the account entry, `ROLE_DEFAULTS`, the table of `infra/lib/config.ts`, which `tools_mode_state` follows),
  prints one `DEPLOY-SETTING` line per setting and hands the values to every cdk call. The live value decides a setting only on a stage without a record, where
  a read that fails or is ambiguous refuses (`settings-read`); on a stage with a record the record decides, and such a read is said on a `deploy:` line and
  refuses nothing. `stage_log_group` is the rule of the retention reader: it reads the log groups whose literal name carries the stage's name and never a group
  that a CDK construct makes for itself (the provider function of AwsCustomResource, 731 days). A new code default of class replace, decrease or remove that a
  stage with a record would take only with consent is kept: the live value becomes `kept`, is recorded and gets the `new-default=` suffix and the
  `held_sentence` on its `DEPLOY-SETTING` line. `require_consent` and `require_provider_consent` refuse a change of class replace, decrease or remove that no
  explicit value asks for (the earlier intent of a deploy that did not finish counts: `earlier_intent`, the record's `explicit=` line, or each `set.` line of such a record that a release before that line wrote; for a setting that record has no line for, `legacy_deploying` keeps nothing by itself and the deploy asks); `consent_exports` words the action: each export with what it gives (the record's value, the
  live value, `default` and the value it comes to). `begin_changes` writes the record with `status=deploying` right before the first stack
  change (the bootstrap of an account that has none, else the first `cdk deploy`) and the deploy writes `status=deployed` before its pass line. The record
  always holds `set.HAMH_KEEP_TAG=` and `set.HAMH_STAGE_KIND=` with the resolved words (every other `set.` line is a user's choice, never a code default), so
  a deploy with an account entry of another role keeps them. The record's
  `commit=` is the commit the user asked to deploy (`source_commit`: `HAMH_SOURCE_COMMIT` when it holds 40 hex digits, which the setup runner exports because
  it deploys from a source archive with a commit of its own; else HEAD); the deploy record file keeps `codeSha`, the HEAD of the tree that was built. `take_lock` and
  `release_lock` hold SSM `/hamh/<stage>/deploy-lock` (create-only, `route=manual`) unless `HAMH_DEPLOY_LOCK_TOKEN` names the caller's lock; while the deploy
  runs, `keep_lock` (a daemon thread, `renew_lock` every `LOCK_RENEW_S` seconds) writes a new `started` into the lock as long as it holds this deploy's token,
  and `release_lock` deletes it only then, so a lock that another deploy replaced is left alone. A Fail of a step
  in `REFUSALS` before the first stack change prints `DEPLOY-REFUSED reason=<step> wait=<yes|no> stage=unchanged :: <action>` and exits 3; any other failure
  before it prints `DEPLOY-STAGE unchanged` on the line before its `DEPLOY-FAIL`. `--print-settings` runs the resolution alone and writes nothing.
  Tests: `scripts/tests/test_setup_deploy_settings.py`.
- `stage-settings.tsv`: the registry of the stage settings an update keeps, one `NAME|KEPT|CHANGE|PATTERN|NOTE` row each (the header says how to read a
  pattern that holds `|`). Plain data, never sourced; `deploy.py` and setup read it. A pattern is an anchored POSIX extended regular expression with no
  backslash class, no look-around and no repetition count above 255, so Python and bash 3.2 answer alike; `scripts/tests/test_setup_deploy_settings.py` runs each
  through both and holds it to what `infra/lib/config.ts`, `infra/lib/stacks/network.ts` and the utility-model catalog accept. A new stage setting of the CDK
  app gets its row, its default in `stage-defaults.tsv` (or `ROLE_DEFAULTS` of `deploy.py` when the role of the account entry gives it, as for `HAMH_KEEP_TAG` and
  `HAMH_STAGE_KIND`) and its live reader in the same change.
- `stage-defaults.tsv`: the code defaults of the stage settings, one `NAME|DEFAULT` row each (empty for no value); `deploy.py` loads it into
  `SETTING_DEFAULTS` and setup reads it for the plan of an update. The utility model, its purposes, the keep tag and the stage kind have no row (their default
  comes from the catalog, the Region or the role). A changed default changes this file and `infra/lib/config.ts` together (the tests pin them).
- `bootstrap-min-version.txt`: one positive integer line, the CDK bootstrap version a deploy needs when the synthesized assembly names none.
  `deploy.py` reads it (`MIN_BOOTSTRAP`, `read_min_bootstrap`), and setup's preflight compares the account's bootstrap with it.
- `teardown.py`: implementation of `scripts/teardown.sh` (the selected entry of the tree's `accounts.json`;
  it restores the invocation logging deploy.sh recorded before enabling it); flow, ownership rules, the
  record (`write_record`) and test hooks `TEARDOWN_*` in its
  docstring. A change that may need time takes `act(..., retry_if=...)`: it repeats the call while the error text names one of the words, for up to `TEARDOWN_SETTLE_S` and every `TEARDOWN_SETTLE_POLL_S`; the AgentCore step (`agentcore`) uses it to delete a leftover Instances capacity provider only after the stage's runtimes are gone (`wait_runtimes_gone`) and reads the provider until it is gone (`wait_provider_gone`). Python 3.9+.
- `agentd_build.py`: the one recipe that builds the worker binaries (`cargo zigbuild --release -p agentd --target
  aarch64-unknown-linux-gnu.2.34`, the selection environment, a remap of `CARGO_HOME`, the zig, cargo-zigbuild and rustc pins).
  `deploy.py` builds through `zigbuild_args` and `build_env` and writes `recipe_digest` into the image-build record; `prove` builds
  root A in place and root B from a copy of the working tree with a fresh `CARGO_HOME`, compares the hashes and scans both binaries
  for host paths. The file is a worker image input (`digests.IMAGE_PATHS`). Python 3.11+.
- `arm64_sysroot.py`: runs the production aarch64 `agentd` on an x86_64 host: the glibc loader and libraries copied out of a
  created container of the pinned arm64 Amazon Linux 2023 image (`tools.lock` `containers.al2023_arm64`), each file checked
  against its SHA-256, and a QEMU binfmt registration decided by running the loader. Python 3.11+.
- `wait_scale.py` (stdlib, Python 3.8+): the rule of `HAMH_TEST_WAIT_SCALE`, the factor that the setup tests multiply their wait limits by: unset, empty or blank is no choice (1 for a test file run directly), a
  number from 1 to 8 is the factor, anything else is refused with a message (`parse`, `from_environment`); `choose(jobs)` says what the runner exports and why.
- `python.sh`: sourced, not executed; sets `$py` and defines `ham_pick_python [minor]`. COMPAT-CAH: `hamh_host_env_compat` runs when the file is sourced and exports the old-spelled host-level names (`CAH_PYTHON`, `CAH_TOOLS`, `CAH_TMP`, `CAH_DEVDOCS`)
  under `HAMH_` when the new name is unset or empty; the contract is in `../AGENTS.md`.
- Standard library only; no third-party imports.

## Work Guidance

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest scripts/tests/test_tools_lock.py
```

## Child DOX Index

None.
