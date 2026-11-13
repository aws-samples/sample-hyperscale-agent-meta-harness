# engine/plugins

## Purpose

The observer is code of this repository.

## Ownership

Owned by `engine/AGENTS.md`. The pins and the recipe of the goal plugin are in `engine/sources/` (`engine/sources/AGENTS.md`); `scripts/engine-fetch`
fetches, bundles and verifies it. The code that places the plugins and supervises the loop is the `goal` module of agentd
(`crates/agentd/src/direct/goal/`, `crates/agentd/src/direct/AGENTS.md`). The image copy is `images/worker-python/Dockerfile`
(`images/worker-python/AGENTS.md`).

## Local Contracts

- The goal plugin folder is the five files `index.js`, `package.json`, `LICENSE`, `NOTICE` and `PROVENANCE`, made by
  `scripts/engine-fetch build goal-plugin --dest <dir>` (the buildspec of the worker image runs it with the `build` directory of the checkout, then `verify`, before `docker build`):
  - `index.js` is the goal plugin of the Cline project (`engine/sources/goal-plugin.toml`: the repository cline/plugins, the file plugins/goal/index.ts at a pinned
    commit) bundled by esbuild 0.25.12 (`engine/sources/esbuild.toml`, the native binary pinned by hash) into one ES module, together with `createTool`
    of the Cline SDK, taken verbatim from the pinned Cline source (the file create.ts of its shared package) and two small stand-ins we wrote for the
    imports that function never reaches here (zod and the zod converter; `engine/sources/recipes/goal-plugin/`). The bundle needs no Node packages and
    no Node on the image's path; agentd sets the plugin mode explicitly to `in_process` (`CLINE_PLUGIN_MODE`, `crates/agentd/src/engine/goal.rs`), so
    it never depends on a bootstrap file.
  - `LICENSE` is the Apache-2.0 text of cline/plugins at the pinned commit (the build checks it is byte for byte the text of cline/cline, because the
    notice says they are the same). `NOTICE` and `package.json` are rendered from the templates of the recipe; `PROVENANCE` is rendered from the source
    files and the lock, in a fixed order with no time, host, path or architecture, so any build host makes the same bytes.
  - The build fails (exit 6, nothing published) when any of the five differs from `lock.json`, when one is missing or extra, or when esbuild bundles
    other files than the five expected inputs or writes a warning.
- `harness-rounds/` is the harness's own observer: `beforeRun` and `afterRun` append one JSON line each to
  `<data dir>/plugins/harness-rounds/rounds.jsonl` (`ev` start or end, `n`, `t`, and for an end `status`, `iterations`, token counts, `error` and the last
  1,500 characters of the final text). Every write is inside try/catch and it calls nothing else. The goal plugin queues its follow-up prompts itself, so
  these runs have no `session/prompt` response and this file is the only place that shows where each begins and ends. The file steers and measures the
  loop; the done claim rests on the ACP stream, which the model cannot write.
- `lock.json` (version 1) holds the SHA-256 of every file of both folders under `files`, and under `sources.goal-plugin` the record of the bundle build:
  the bundler, the SHA-256 of the two inputs (the plugin source and `create.ts`), of esbuild's metafile and of the recipe files, and the bundle size.
  agentd reads only `version` and `files`: it checks the files in the image at startup and a mismatch switches the goal loop off for that run. Only
  `scripts/engine-fetch update goal-plugin` writes the goal entries and the record; `build/lock.py --write` rewrites the observer entries.
- `build/` is never copied into the image. `lock.py` checks the observer entries and the format of the goal entries; `plugin-unit.mjs` (the 14 checks of
  the goal plugin's state machine) and `observer-unit.mjs` drive the plugins by hand. The buildspec runs both against the shipped files inside the built
  image, with the image's own Node.
- No file here may hold a network call, a telemetry call or a child process; `node_modules` never appears.
- A pin bump of the Cline fork (`engine/cline/lock.toml`) goes through `engine/sources/cline.toml` (`engine-fetch check` holds the two together). The
  `createTool` input is pinned by git blob id at the Cline commit, so a bump that changes `create.ts` fails the fetch until the input is re-pinned; one
  that leaves it as it is needs nothing.

## Work Guidance

- Change a shipped file test first (`scripts/tests/test_goal_plugin.py`). For the observer run `build/lock.py --write`. To move the goal plugin to a new
  upstream commit run `scripts/engine-fetch update goal-plugin --to-head` (or `--to-tag`, `--to-commit`; `--dry-run` writes nothing) on a host with
  network access, review the diff of the source file and the lock, and run the 14 checks on the result. After a change of a recipe file run
  `scripts/engine-fetch update goal-plugin` (no reference) to refresh the lock.
- The observer changes only by a decision: agentd's supervisor reads exactly the fields listed above.

## Verification

```bash
scripts/engine-fetch check                                                 # offline: the plugin lock against the sources, the recipe and the tree
python3 engine/plugins/build/lock.py                                       # GOAL-PLUGIN-LOCK PASS
python3 -m unittest scripts/tests/test_goal_plugin.py                      # needs docker for the observer's Node case (scripts/node.sh)
scripts/engine-fetch build goal-plugin --dest <new dir> && HAMH_GOAL_BUNDLE_DIR=<new dir> python3 -m unittest scripts/tests/test_goal_plugin.py   # network
```

## Child DOX Index

None.
