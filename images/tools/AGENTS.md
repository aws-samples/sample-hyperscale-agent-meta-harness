# images/tools

## Purpose

The pinned tool installers: `fetch.sh`,
`install.sh <dest> <python> <uv> <gitleaks>` (the Python tool set: uv, gitleaks, ruff, mypy, bandit, pip-audit, diff-cover and the pytest
wheels, from `tools.txt` and `runtime.txt`) and `lang/install.sh <dest> <python|go|jvm|direct|rust|all> [<cache>]`.
The worker image runs `python` or `direct`.

## Ownership

Owned by `images/AGENTS.md`. `scripts/gates-lang-tools.sh` runs `lang/install.sh ... all` into `gate-tools-lang` under `HAMH_TOOLS`.

## Local Contracts

- `tools.txt` and `runtime.txt` are compiled from the `.in` files with the pinned uv (`uv pip compile --universal --python-version 3.12
  --generate-hashes`); `install.sh` replaces its destination directory. `lang/lang.lock` pins each language tool download by SHA-256 per
  architecture. The pins never change without a measured hash.
- The worker image key (`scripts/lib/digests.py` IMAGE_PATHS) hashes this directory: a change here builds a new image.
- The Rust block of `lang/install.sh` serves the `rust`, `direct` and `all` families: the real files are in `<dest>/rust/` (rustc finds its standard library
  relative to its own file) and the commands a session runs are linked into `<dest>/bin`, the directory the engine's PATH holds. `direct` gets the slim set.
- `build-gitleaks.sh <out> [arm64|amd64]` builds gitleaks from the module version and the Go toolchain pinned in `gitleaks.lock` (the Go archive by SHA-256, the module by `go.sum` and the Go checksum database; `-trimpath`, no cgo, no VCS stamp), because the release binary of 8.30.1, the newest release, carries the standard library of Go 1.24.11. The build is reproducible: the arm64 build on x86_64 and on another cache gave the same SHA-256, which `images/codebuild/worker-python.yml` and `delivery.yml` pin (`GITLEAKS_SHA256`). `scripts/tests/test_b634_image_recipes.py` keeps the pins together. The session Go of `lang/lang.lock` is a different pin.
- `scripts/tests/test_fetch_retry.py` runs `fetch.sh` against a local server; `scripts/tests/test_tools_lock.py` the gate-tools step.

## Work Guidance

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest scripts/tests/test_fetch_retry.py scripts/tests/test_tools_lock.py
```

## Child DOX Index

| Path | Covers |
| --- | --- |
| `lang/js/AGENTS.md` | The pinned JS tools' package manifest and lockfile (eslint, typescript-eslint, eslint-plugin-security, prettier, typescript, pnpm) |
