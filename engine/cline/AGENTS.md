# engine/cline

## Purpose

Our Cline fork: an upstream release tag plus a patch series, applied with `git am` and built
with telemetry compiled off.

## Ownership

Owned by `engine/AGENTS.md`. This directory owns `lock.toml`, `rebase.sh` and `patches/`.
`scripts/build-cline-fork.sh` owns the verify and build steps. Owns the patch plan.

## Local Contracts

- Upstream: `https://github.com/cline/cline.git`, tag `cli-v3.0.71`, commit `1a5c3f84e0933268a9dba14a55a6383a6578414d` (tag object `7418804b1460929f3ff701bcccc8184fb69c8e26`). After patches 0001-0015 the tree is `f8990b6745402aec9bf0458611c763a1a7e710f8` (0012 and 0013 are
  the Bedrock additions for GLM 5.3; 0014 keeps blank keys out of the tool inputs replayed to Bedrock; 0015 lets one `run_commands` command run 10 minutes instead of 30 seconds). History includes series-13 build `491020e731f2`, patches 0001-0013, and series-14 build
  `6c12d3927676`, patches 0001-0014. `engine/lock.toml` pins the promoted linux-arm64 build of exactly the 15-patch tree. Its first 12 SHA-256 characters are `248ea4c01c87`. CodeBuild ARM build `7cb7a9bf` (the `builtBy` line of the lock) at code `cfe8cc3b` printed `FORK-BUILD
  PASS series=15 tree=f8990b674540`. It was promoted to the harness bucket at the `artifact` URL of `engine/lock.toml`. Native probe build `84d55154` ran it on arm64 first. The development linux-x64 build `12cdc2cb9414` is a record only. It is not pinned. Builds are not
  bit-reproducible. A change to the series needs a new promotion ("Promoting a series" below). Until then a worker image ships a binary that does not match the lock tree.
- `lock.toml` holds `schema`; `[upstream]` url, tag, commit; `[series]` tree; one `[[patches]]`
  table per patch in apply order (file, sha256); one `[[tdd]]` table per test-only patch (a file under
  `patches/tdd/`, sha256, commit, parent, tree); `[toolchain]` bun version, the bun
  release-zip SHA-256 per host architecture, and the build image pinned by digest; `[reference]`
  binary SHA-256 values for comparison only (empty until a build of the current tree records
  one). The build script reads a strict subset: `[table]`, `[[patches]]`, `[[tdd]]`,
  `key = "string"`, and full-line comments. Unknown keys fail.
- `patches/` holds exactly the series files the lock lists, byte for byte as `git format-patch`
  wrote them. The numbering has no gaps.
- `patches/tdd/` holds exactly the test-only patches the lock lists (0003 to 0015): the failing-test commit of
  each built patch, exported from the fork checkout, with an
  `X-Harness-Test-Only: commit=<sha> parent=<sha> tree=<sha>` header after `Date:`. Applied with
  `git am` onto the tag plus the series patches before its number, it gives the recorded tree;
  `scripts/build-cline-fork.sh --verify-only` checks that for every entry.
- The fork working checkout is scratch under `CLINE_FORK_WORK_DIR`; the files exported here are the record. `rebase.sh` leaves a clone of the
  tag with the series applied at `<work>/rebase/<tag>-<run id>/src`.
- Where the series meets upstream code (check these first on a rebase):
  - Patch 0005 (data dir, local backend) sits next to upstream's `reasoning` option in `AcpAgent` (`--thinking` applies under ACP since
    upstream #14894). Both options stay, and `acpAgent.local-backend.test.ts` pins the combination.
  - Patch 0009 (provider failure structure) sits next to upstream's `lastFinishReason` in `agent-runtime.ts` (both are set) and the
    simplified `finishReason` in `ai-sdk.ts` (the fork keeps `rawFinishReason` for `model_context_window_exceeded`). The package
    `@ai-sdk/amazon-bedrock` 5.0.98 carries `type` (lower camel), `statusCode` and `isRetryable` on its error part natively, so
    `describeProviderError` names the exception from `type` (`throttlingException` becomes `ThrottlingException`) and the series needs no
    bun patch of that package. A stream-frame error reports the provider's status (429, 503, 500, 424, 400) next to the name.
  - Patch 0010 (compaction) replaces upstream's simplified `tryGetModelInfo` with the fork's `resolveModelInfoFromConfig`.
  - Patch 0011 (Mantle reasoning floor): upstream #14991 encodes a portable disable of a toggle model natively, so the Mantle body of the
    `zai.glm-4.7-flash` model carries no `reasoning_effort` at all. The test asserts it is never floored to `low`.
  - Patch 0015 (command limit) edits `createBuiltinTools` (`index.ts` of the tools folder), `buildRunCommandsDescription` and `createShellTool` (`definitions.ts`), `withTimeout`
    (`helpers.ts`) and adds `shell-timeout.ts`. Upstream's runtime builder (`createBuiltinToolsList` in `runtime-builder.ts`) and `spawn-tool.ts` reach the run_commands tool and the
    shell executor only through `createBuiltinTools` and pass no `bashTimeoutMs` and no executor `timeoutMs`, so that function is the lever. Check after a rebase that they still pass
    none, that the executor still kills the process group at its limit, and that the stand-alone defaults of `createShellTool` and `createShellExecutor` (30,000 ms; the VS Code host
    calls both) are untouched. The tool description carries a sentence about the limit that the tests pin.
  - `toolchain.bun_version` is 1.4.2, upstream's `packageManager` in 3.0.71; the zips are pinned from the release's `SHASUMS256.txt`.
- Command limit (patch 0015): one `run_commands` command is ended at 600,000 ms (10 minutes); until series 14 it was 30,000 ms ("Command timed out after 30000ms"). `createBuiltinTools`
  resolves the limit once and gives the same value to the tool (limit, call limit and description) and to the shell executor, which kills the command's process group at the limit, so the two
  cannot disagree; a `bashTimeoutMs` or executor `timeoutMs` of the host wins, and the stand-alone defaults of `createShellTool` and `createShellExecutor` stay 30,000 ms. The environment variable
  `CLINE_RUN_COMMANDS_TIMEOUT_MS` (a whole number of milliseconds) sets another value for the engine process; the engine profile's `[env]` table (`engine/profiles/cline.toml`) is the place to set
  it, and it is not set today. The value is read from 1,000 ms (a smaller number is probably seconds) to the ceiling of 1,800,000 ms (30 minutes); above the ceiling it becomes the ceiling; anything
  else (empty means unset; units, decimals, words, below the floor) is ignored and the default applies, so no setting removes the limit. An ignored or capped value writes one line to stderr,
  once per distinct text, because stdout carries ACP. The tool result says `Command timed out after <n>ms`; agentd's recorder reads that text for any number
  (`crates/agentd/src/direct/recorder.rs`). The tool description sent to the model names the limit ("Each command is stopped after 600 seconds.") and tells it to keep finite, non-interactive
  builds, installs and test suites in the foreground and servers, watchers, commands that wait for input and longer work in the background. Other tools keep their 30,000 ms limits.
- ACP reasoning at 3.0.71: `AcpAgent.buildConfig` sets `thinking: reasoning.thinking ?? false` and `reasoningEffort`, where `reasoning` is
  `resolveCliReasoning(--thinking, --reasoning-effort)` (`low`, `medium`, `high`, `xhigh`). Without `--thinking` an ACP session runs with
  reasoning off. An engine profile that wants reasoning adds `--thinking --reasoning-effort <level>` to the ACP argv. Reasoning is a
  process-level setting, not a per-session one; the model pin (patch 0004) does not read it. `acpAgent.mantle-reasoning.test.ts` (patch
  0011's tests) carries the wire-level proof: with `--thinking --reasoning-effort high` a Mantle gpt-oss request carries
  `reasoning_effort: high` and a Converse Claude Sonnet 5 request carries
  `additionalModelRequestFields {thinking: {type: adaptive}, output_config: {effort: high}}`; without the flag Converse carries none.
  GLM 5.3 (patch 0012) is the exception: it cannot turn reasoning off, so a session without `--thinking` still sends
  `additionalModelRequestFields.reasoning_effort` `low` (the model's own least-reasoning level; its unset default is about `max`), and
  `--thinking --reasoning-effort <low|medium|high|xhigh>` sends that level (the model also accepts `max`; the CLI flag offers low to
  xhigh). The lever for a per-run GLM effort is therefore the ACP argv, not a fork change. `engine/profiles/cline.toml` carries neither
  flag (`engine/AGENTS.md`).

Patch series:

| Patch | Change | Checked by |
| --- | --- | --- |
| 0001 | Bedrock Mantle through `createBedrockMantle` with the Converse SigV4 credential chain; `/v1` or `/openai/v1` per model family. All five models of `engine/catalog/models.json` are Converse routes, so no listed route needs this patch or 0011. Patch 0004 resolves aws-mantle ids with this patch's route function, so a Converse-only list can drop 0001, 0011 and that part of 0004 together | the package suites of the build gate |
| 0002 | Telemetry defines compiled off by default; `CLINE_BUILD_TELEMETRY=on` opts in; build-time leak guard | the leak guard line in the build log (`leaks=0`) |
| 0003 | Rolling Bedrock cache point: last user or tool-result message; carries upstream cline/cline#14481 (drop when it merges; not merged at 3.0.71) | `patches/tdd/0003-test-only.patch` |
| 0004 | ACP model pin fails loud: pinned ids are kept or refused with invalidParams naming them; aws-mantle ids and ARNs accepted on Bedrock | `patches/tdd/0004-test-only.patch` |
| 0005 | `--acp --data-dir` (or `CLINE_SANDBOX=1`) moves all state into the data dir and runs the local backend, no hub daemon | `patches/tdd/0005-test-only.patch` |
| 0006 | `CLINE_API_KEY` goes only to Cline account providers; Bedrock under ACP pinned to `iam` unless providers.json chose api-key or profile | `patches/tdd/0006-test-only.patch` |
| 0007 | The ACP prompt result carries `usage` for the turn: the session's aggregate usage before and after, `inputTokens` uncached, `cachedReadTokens` and `cachedWriteTokens` apart; none when the core reports none (upstream issue #14251 and PR #14252 are open) | `patches/tdd/0007-test-only.patch` |
| 0008 | `createAgentModelFromConfig` keeps providers.json `maxTokens` (provider config `maxOutputTokens`) when no per-turn cap is set, so it reaches Converse `inferenceConfig.maxTokens` and Mantle `max_tokens` | `patches/tdd/0008-test-only.patch` |
| 0011 | On the Bedrock Mantle route a model whose catalog entry has effort levels and no off switch (`supportsOff` false: the four gpt-oss models) gets its lowest advertised effort (`low`) where the request says reasoning `none`; a model with a toggle, no effort list or no catalog entry is left alone; an explicit effort is never touched. GPT-6.1 Sol and GPT-6 Astra have the same catalog shape (efforts, no off switch) but are Converse rows for Cline, where this patch does nothing | `patches/tdd/0011-test-only.patch` (`acpAgent.mantle-reasoning.test.ts`, `bedrock-mantle-reasoning.wire.test.ts`) |
| 0012 | GLM 5.3 on Bedrock Converse (exactly `us.zai.glm-5.3`, `global.zai.glm-5.3` and `zai.glm-5.3`; profile-only, not on Mantle) gets its level as `additionalModelRequestFields.reasoning_effort` (rule `provider.bedrock.zai-glm-5.3.reasoning-effort`; low, medium, high, xhigh, max; reasoning off or unset sends `low`, reasoning on without a level `medium`, `minimal` is `low`) instead of the top-level `reasoning` object Bedrock ignores; the generic GLM rule skips these ids and still serves every other GLM route; the portable level is no longer also sent (a nested `additionalModelRequestFields.reasoning_effort` counts as a native reasoning control); the llms catalog override file replaces the generated catalog's efforts `none, low, high, max` for the two profile ids with `low, medium, high, xhigh, max` (Bedrock rejects `none`; the override survives a catalog regeneration). Additive: no other provider's behavior changes | `patches/tdd/0012-test-only.patch`; the wire test `bedrock-glm-reasoning.wire.test.ts` pins the Converse body |
| 0013 | On Bedrock, tool call ids stay unique across turns: an id that an earlier call or result of the conversation, or an id handed out earlier in the same session (`metadata.sessionId`, the 128 most recent sessions, so compaction and retries cannot repeat an id an ACP client still shows), already uses gets the smallest free numeric suffix (`call_0` becomes `call_0-2`; the result stays within Bedrock's 64 characters), one raw id mapping to one result per response; other providers keep their ids; ids that do not collide are unchanged. GLM 5.3 numbers its calls `call_0`.. in every response, which ACP clients that key tool calls by `toolCallId` (agentd, the S1 library) merged | `patches/tdd/0013-test-only.patch` (`ai-sdk.tool-call-ids.test.ts`) |
| 0014 | On Bedrock Converse (a model id that Bedrock does not serve through Mantle), the input of each replayed tool call loses its blank object keys, at every depth and inside arrays (`bedrock-tool-use-input.ts`; the option `normalizeToolCallInput` of `toAiSdkMessages` in `ai-sdk.ts`). A key is blank when it is empty or made only of Java whitespace (`Character.isWhitespace`); keys such as `" a"`, `"a b"`, U+00A0 and U+200B stay. Only the copy sent to Bedrock changes: the conversation history, inputs that are not objects (`@ai-sdk/amazon-bedrock` wraps them as `{rawInvalidInput}`) and other providers stay as they are. In two live runs GPT-6 Astra returned `run_commands` with `{"commands": [...], " ": ""}` on its fourth call; Bedrock accepted that reply and refused the replay with ValidationException at `messages.7.content.0.toolUse.input`, on all five supported models | `patches/tdd/0014-test-only.patch` (`bedrock-tool-use-input.wire.test.ts` through the real Bedrock Converse adapter, `bedrock-tool-use-input.test.ts`) |

Cline fork binary in a stage:

- A stage whose bucket holds `engine/cline/<lock binarySha256>/cline` uses it. A fresh account has no such object and needs no access to the original bucket: `auto` copies the lock's artifact only when the account that
  holds it has a profile configured on the deploy host, and otherwise builds the fork on CodeBuild ARM from the pinned upstream source (`images/codebuild/cline-fork-arm64.yml` runs `scripts/build-cline-fork.sh`, which checks the
  tag, the commit, every patch, the series tree and the toolchain pins itself). `build` skips the lock's object and the copy; `copy` never builds. A copy that fails is fatal; it never turns into a build.
- Builds are not bit-reproducible, so the lock's `binarySha256` is no test of a source build. The deploy accepts the build on the evidence the build computed: the metadata and the record it wrote (the locked commit,
  series tree, bun zip and build image; an aarch64 ELF; telemetry off; leaks 0; `bun install --frozen-lockfile`; the suites run), the smoke test of the build spec (it reports the locked version), the CodeBuild build id
  (`build-id.txt`, equal to the id the deploy started, so output of another build is never read), and the SHA-256 the build computed equal to the SHA-256 of the bytes downloaded. The binary is promoted to
  `engine/cline/<its SHA-256>/cline` (create-only) and recorded in `engine/cline/by-tree/<series tree>-<toolchain digest>.json` (create-only). The worker image is built with that SHA-256 (`CLINE_SHA256`); the lock's
  stays in the deploy record as a reference (`forkBinary.lockBinarySha256`, `matchesLock`). The next deploy of the stage that needs the fork finds the pointer and does not build again; a new toolchain pin or series tree is a new key.
  To get another binary of the same tree on purpose, delete the pointer object by hand; the next deploy records `pointerOutcome created` again.
- The older procedure below ("Promoting a series") is how the maintainers' own binary is promoted and pinned in `engine/lock.toml`; a source build does not change that pin.

Telemetry rule:

- Every build here is telemetry-off. `build-cline-fork.sh` refuses a caller environment with
  `CLINE_BUILD_TELEMETRY` set to anything but `off`, passes `off` into the build container, and
  fails unless the build log shows 0002's leak-guard line for the target.
- `CLINE_BUILD_TELEMETRY=on` exists only for upstream's release workflow; this repo never sets it.
- `telemetryOptOut: true` and the VPC egress allowlist stay as defense in depth.

## Work Guidance

Rebase onto a new upstream CLI release:

1. Resolve the tag: `git ls-remote https://github.com/cline/cline.git 'refs/tags/<tag>*'`. The
   `^{}` line is the commit.
2. In a scratch clone under `CLINE_FORK_WORK_DIR`, `git clone --branch <tag>` and
   `git am <repo>/engine/cline/patches/*.patch`. On a conflict, run
   `git am --show-current-patch=diff`, resolve, rerun that patch's package tests, then
   `git am --continue`.
3. Run the fork build gate: package suites, `bun run types`, and the telemetry-off leak
   build.
4. Replace `patches/` with `git format-patch <commit>..HEAD`. In `lock.toml`, update tag, commit, tree (`git rev-parse
   'HEAD^{tree}'`) and every patch sha256; clear `[reference]` values that no longer apply. A `bun patch` of a dependency
   does not survive a version change: bun applies the stale patch by line number to the new file without failing, so check
   `node_modules` of every patched dependency after `bun install`, or drop the patch when the dependency now carries the
   behavior.
6. Run the tests and `--verify-only`, then a full build per target.

Steps 1-2 are `engine/cline/rebase.sh <tag>`: it resolves the tag, clones it,
applies the series in lock order, prints `ok patch <file>` per patch and
`REBASE-PASS tag=<tag> commit=<sha> tree=<sha> patches=<n>`, and writes `rebase.json`; a patch
that does not apply prints `REBASE-FAIL <file>` and exits 6. It never edits the lock.

The record has no `provenPatches`: a rebase proves the series applies, not that the package tests of a patch pass. `--merged-upstream NNNN:<40 hex>[,...]`
names the patches upstream merged, each with its upstream commit, and the lock no longer lists them (four digits, none still in the lock); the record lists
them in `mergedUpstream` and maps each number to its commit in `mergedUpstreamCommits`. With `--record-dir` the commit is required, so the script refuses
before it clones. The bare `NNNN` is accepted when no record is written. The commit is the operator's statement; the fork checkout is a depth-1 clone, so an
ancestry check is not possible here. Run it a second time on the updated lock after step 4, with `--record-dir`, so the record covers the patch files the
lock names.

Keep one commit per patch, `git format-patch` the new patch into `patches/`, export the test-only commit to a numbered
`-test-only.patch` file in `patches/tdd/` with its `X-Harness-Test-Only` header, and add both to the lock with the new tree. Run
upstream's gitleaks check on the staged diff for every commit; commit with hooks off, because the upstream pre-commit hook runs a VS
Code lint-staged step that does not apply here.

Promoting a series (the procedure for every promotion):

1. Build the linux-arm64 binary of this lock in CodeBuild ARM. `images/codebuild/cline-fork-arm64.yml` greps for the series number the build must print (`FORK-BUILD PASS
   series=15`), and `images/AGENTS.md` names the same number: change both to the new patch count first. The build prints `FORK-BUILD PASS series=<n> tree=<12> bin=<12>
   telemetry=off leaks=0`. It must end `FORK-PROBE PASS`; promote only after that.
2. Promote the artifact with the conditional put (a create-only put; a 412 means the object exists: download it and compare), then in one commit replace `engine/lock.toml`
   `[cline]` `binarySha256`, `artifact`, `builtBy` and the FORK-BUILD comment line and `engine/profiles/cline.toml`
   `[binary] sha256`. Until then the two locks agree on the tree
   (`crates/harness-core/tests/real_profiles.rs` `the_engine_lock_tree_is_the_series_tree`) while the binary fields name the previous build.
3. The change moves the worker image key: `scripts/image-key-guard.sh --update` in the merge commit.

## Verification

```bash
export CLINE_FORK_WORK_DIR="$TMPDIR/<label>/work"
scripts/build-cline-fork.sh --verify-only        # last line VERIFY PASS
engine/cline/rebase.sh cli-v3.0.71               # last line REBASE-PASS ... patches=15
scripts/build-cline-fork.sh --target linux-x64
                                                 # FORK-BUILD PASS series=15 tree=<12> bin=<12> telemetry=off leaks=0
uv run --no-project --python 3.12 python -m unittest scripts/tests/test_build_cline_fork.py
                                                 # FK1 PASS lines: the command limit of patch 0015 through the real binary under ACP
```

## Child DOX Index

None.
