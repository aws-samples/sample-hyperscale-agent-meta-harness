# engine

## Purpose

The pins of every third-party engine and plugin, the patch series of our Cline fork, the engine profiles, and the static model list. The harness
builds the Cline engine from an upstream commit plus patches, never from a floating release, and runs the result with telemetry off (root
invariant 3).

## Ownership

Owned by the root contract. `cline/` owns the Cline fork's lock, patches, rebase script and fork documentation. `profiles/` holds one
profile per engine (`cline`, `fake-acp`, `opencode`). `catalog/` holds the static list of the five supported models. `policy/` holds the
pinned worker image key. `rules/deps/` holds recorded dependency data. `sources/` holds the source declarations (one file per engine or plugin). `plugins/` holds the observer plugin the
worker image ships, the lock of both plugin folders and the helpers that check them; the goal plugin folder itself is fetched at build. The build entry points live in `scripts/` and
read the locks and the source files from here.

## Local Contracts

- `cline/` holds a lock file, patch files (the series and, under `cline/patches/tdd/`, the exported test-only commits) and the rebase
  script; no vendored upstream source.
- A lock pins the upstream URL, tag, commit, the SHA-256 of every patch file, and the tree hash
  after the series applies. The build fails on any mismatch.
- Patches are `git format-patch` output, numbered from `0001` with no gaps, stored byte for byte.
- `sources/` is the one place a third-party pin is declared (`sources/AGENTS.md`). The Cline tag and commit are pinned in `sources/cline.toml`, and
  `cline/lock.toml` and `lock.toml` carry the same values; `scripts/engine-fetch check` fails when they differ.
- `lock.toml` (this directory): the promoted Cline binary and the OpenCode release pins. The
  profiles in `profiles/` and `images/codebuild/worker-python.yml` carry the same values
  (`crates/harness-core/tests/real_profiles.rs`).
  Its `[cline] tree` is the series tree of `cline/lock.toml`; between a series change and the next
  promotion the binary fields name the previous build (`cline/AGENTS.md`, "Promoting a series").
- Reasoning under ACP is off in this series unless the engine profile's ACP argv carries `--thinking` (and optionally `--reasoning-effort
  low|medium|high|xhigh`). `profiles/cline.toml` carries neither, on purpose: direct run leaves Cline as it is.
  A profile gets the flags only with a measurement (`cline/AGENTS.md`, "ACP reasoning at 3.0.71"). agentd adds `--thinking <level>` to each direct run's launch itself (the user's level, else the model's catalog level); the profile stays flagless.
- Build scratch and outputs go only under the scratch directory that `TMPDIR` names.

## Work Guidance

- Change a lock and its patch files in the same commit, then run the engine's verify-only check.

## Verification

```bash
python3 scripts/dox_lint.py .
scripts/engine-fetch check                  # ENGINE-FETCH-CHECK PASS: the source files, their pins against the older locks, the plugin lock, the repository scan
scripts/build-cline-fork.sh --verify-only   # needs CLINE_FORK_WORK_DIR; see engine/cline/AGENTS.md
```

## Child DOX Index

| Path | Covers |
| --- | --- |
| `cline/AGENTS.md` | Cline fork: upstream pin, patch series 0001-0015 on cli-v3.0.71, lock, rebase procedure, promotion of a series, telemetry rule |
| `profiles/AGENTS.md` | Engine profiles: launch per transport, env, hidden paths, binary pin, K rows |
| `rules/AGENTS.md` | `rules/deps/`, the recorded dependency data (the Go approved module list and the Maven notes) |
| `policy/AGENTS.md` | `image-key.pin`, the pinned worker image key that `scripts/image-key-guard.sh` checks |
| `sources/AGENTS.md` | The pins of every third-party engine and plugin the product fetches (`cline`, `goal-plugin`, `esbuild`, `opencode`): kind, URL, commit or SHA-256, build recipe id, SPDX licence; the authored recipe files; the rules for moving a pin (`scripts/engine-fetch update`) |
| `plugins/AGENTS.md` | The goal plugin's lock and rules (the folder is fetched and bundled at image build), the harness-rounds observer plugin, the lock of both and the helpers that check them |
| `catalog/AGENTS.md` | `models.json`, the static list of the five supported models with their smoke records, and the GLM 5.3 smoke record |
