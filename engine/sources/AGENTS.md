# engine/sources

## Purpose

The repository holds these declarations, our patches and the fetch component; it never holds the third-party source itself
(`scripts/lib/third_party_scan.py` fails on a copy).

## Ownership

Owned by `engine/AGENTS.md`. `scripts/engine-fetch` (`scripts/lib/engine_fetch.py`) reads the files and is the only tool that changes a pin.
`scripts/lib/engine_source.py` is the reader and the validator. The plugin lock these files feed is `engine/plugins/lock.json`
(`engine/plugins/AGENTS.md`); the Cline patch series stays in `engine/cline/` (`engine/cline/AGENTS.md`).

## Local Contracts

- One TOML file per source, named after the source (`name` equals the file name): `cline`, `goal-plugin`, `esbuild`, `opencode`. A new engine adds a
  file here; one of the existing recipe ids needs no code. A new recipe id is added to `RECIPES` in `scripts/lib/engine_source.py`, to the build dispatch
  of `scripts/lib/engine_fetch.py` and with its code in `scripts/lib/engine_recipes.py`; the entry point and the adapters stay as they are.
- Every file declares `schema` ("1"), `name`, `kind`, `license` (an SPDX id), `recipe` (`cline-fork`, `esbuild-bundle` or `none`) and the pin of its
  kind:
  - `git-tag`: `[source]` `url`, `tag`, `tag_object`, `commit`, and `patches_lock`, the existing lock of the patch series (`cline.toml` only).
  - `git-path-bundle`: `[source]` `url` and `commit`, one `[[files]]` per path (`path`, git `blob` id, `sha256`, `as`, the name in the build
    layout) and `[[inputs]]` for a path taken from another source (`from`, at that source's own pin).
  - `release-asset`: `[assets.<platform>]` (`linux-x64`, `linux-arm64`, ...) with `url`, `sha256` of the download and, for an archive, `member` and
    `member_sha256`.
- A pin is a full 40-hex commit or a SHA-256. A branch name, an abbreviated id or the word "head" fails `engine-fetch check`.
  A deploy and an image build only fetch what a file pins. They never resolve a branch or a tag to find a newer commit, and an engine never
  updates itself (root invariant 3).
- `engine-fetch update <name> --to-head|--to-tag|--to-commit` is the one explicit step that moves a pin. It resolves the reference with
  `git ls-remote`, fetches that commit, runs the whole build into a scratch directory and rewrites the file and the dependent hashes only after
  it passes. The reader rewrites one value in place, so every comment and every other line stays. A patched source (`cline`) is never re-pinned
  by `update`; it is a rebase (`engine/cline/rebase.sh`).
- `cline.toml` is the one place the Cline tag and commit are pinned by hand. `engine/cline/lock.toml [upstream]` and `engine/lock.toml [cline]`
  carry the same values and `engine-fetch check` fails when they differ. `opencode.toml` equals `engine/lock.toml [opencode]` the same way.
- `recipes/<recipe>/` holds the small files we wrote that a build lays beside the fetched source: the stubs for the two imports of
  `createTool` that a JSON-schema tool never reaches, the manifest and notice templates of the plugin folder. They are our code (MIT-0), a few
  lines each, and `third_party_scan.py` allows exactly these paths.

## Work Guidance

- Change a declaration test first (`scripts/tests/test_engine_source.py`, `scripts/tests/test_engine_fetch.py`).
- `scripts/engine-fetch update` moves the pin of the goal plugin only, and keeps the hashes of its files and the plugin lock in step. The pins of `esbuild` and `opencode` are edited by hand in their
  file, followed by `scripts/engine-fetch fetch <name> --dest <new directory>` and `scripts/engine-fetch check`. The pin of `cline` moves by a rebase (`engine/cline/rebase.sh`).
- Add an engine: write `<name>.toml`. A recipe that already exists (`none`, `esbuild-bundle`, `cline-fork`) needs nothing more. A new recipe id is added to `RECIPES` in `scripts/lib/engine_source.py` and to the
  build dispatch of `scripts/lib/engine_fetch.py`, with its code in `scripts/lib/engine_recipes.py`. Then run `scripts/engine-fetch check`.

## Verification

```bash
scripts/engine-fetch check                                  # the source files, their agreement with the older locks, the repository scan
python3 scripts/lib/engine_source.py                        # SOURCE-LINT PASS
python3 -m unittest scripts/tests/test_engine_source.py scripts/tests/test_engine_fetch.py
```

## Child DOX Index

None.
