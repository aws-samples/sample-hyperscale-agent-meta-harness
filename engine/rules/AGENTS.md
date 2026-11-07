# engine/rules

## Purpose

Holds `deps/`, the recorded dependency data of the Go module store and the Maven notes of the enterprise n-tier project. The user's own
rules for a run are not stored here: `ham` sends the files of `~/.config/ham/rules/` with the run (root invariant 5).

## Ownership

Owned by `engine/AGENTS.md`. `deps/AGENTS.md` owns the files below it.

## Local Contracts

- The directory holds `deps/` only. It is under `engine/` outside `engine/catalog/` and `engine/policy/`, so a change to a file below it moves the
  worker image key, and `scripts/image-key-guard.sh --update` moves the pin (`engine/policy/image-key.pin`) in the same commit.

## Work Guidance

## Verification

```bash
python3 scripts/dox_lint.py .
```

## Child DOX Index

| Path | Covers |
| --- | --- |
| `deps/AGENTS.md` | `go-approved-modules.txt` (the Go module roots the module store may hold) and `jvm.json` (the Maven lines that resolve without a HIGH, CRITICAL or unrated advisory) |
