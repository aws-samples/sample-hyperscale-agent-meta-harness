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
- `deploy.py`: implementation of `scripts/deploy.sh`; flow and test hooks `DEPLOY_*` in its
  docstring. Its last step before the pass line is a read-only report on the Go module store. It deploys stage `direct` only and targets the selected entry of the deployed tree's `accounts.json` (`HAMH_ACCOUNTS_FILE`
  names another file; the record then carries `deploymentConfigDigest` beside `sourceTree`, SSM `/hamh/<stage>/deployment-config-digest` keeps it, and a deploy whose digest differs from that record says so before it builds anything). Before any build it checks the caller's account, the AZ name against its AZ ID, the stage VPC CIDR 10.50.0.0/16 against the
  account's other VPCs and the CDK bootstrap version. It builds the Lambdas, the Cline fork binary (present in the stage bucket, copied from the
  account that holds it, or built on CodeBuild ARM and promoted only on the lock's SHA-256), the worker image and the delivery image, runs one
  `cdk deploy` pass over every stack, checks the `live` and `v<N>` runtime endpoints, records invocation logging with its prior state, and writes
  digests to SSM and a deploy record. The worker image is the `BinaryImage` value `WORKER_IMAGE`, built from the zigbuild-cross-built `agentd`;
  `image_tag` and `build_binary_image` serve it. `resolve_tools_mode` (`TOOLS_MODES`, `DEFAULT_TOOLS_MODE`, `ToolsModeError`) resolves the stage setting `HAMH_TOOLS_MODE` by the same table as `infra/lib/config.ts`
  (`infra/test/fixtures/tools-mode.json`); the `Deployer` resolves it in its constructor (a bad word is a usage error from `main`), hands the resolved word to every cdk call, compares it with `AGENTD_TOOLS_MODE` of the worker runtime
  version the live endpoint serves (`current_tools_mode`, from the same `GetAgentRuntime` read as `current_image`) and asks the deploy guard once, before the Lambda build, when they differ. That live read happens before the build, so a `GetAgentRuntime` answer without a container image ends the deploy as `DEPLOY-FAIL live-runtime` and never as a traceback.
  The Instances compute type has the same shape: `resolve_compute_instances` (`COMPUTE_INSTANCES_MODES`, `ComputeInstancesError`, one table with `infra/lib/config.ts`: `infra/test/fixtures/compute-instances.json`), `compute_instances_state` (the runtimes stack has the output `InstancesCapacityProviderArn` or `WorkerPythonInstancesRuntimeId`), `refuse_instances_off` (a stage that has the part and a setting that resolves to off, unset included, ends `DEPLOY-FAIL instances-off` before the guard and before any cdk call; a deploy never removes the part, the stage teardown does) and the guard when the part is added; `provider_replacement` asks it again after the synth when the provider's name (a hash of its configuration) differs from the live one. `RUNTIME_WORDS` is the table of isolation words each worker runtime must serve (`AGENTD_ENGINE_SANDBOX`: `uid`, and `landlock` with `AGENTD_ENGINE_USER` set to the empty string on the Instances runtime; `SANDBOX_WORDS` is its sandbox column); `isolation_state` compares them with the live runtimes before the build (a lacking or different word is a runtime change that asks the guard, `inputs.isolationWords` in the record) and `check_runtimes` requires them after the deploy. Requires Python 3.11+ (`tomllib`).
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
