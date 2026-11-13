# security

## Purpose

This directory holds `scanner-kit.toml`, the pin of the scanner kit's rules package.

## Ownership

The root `AGENTS.md` owns this directory.

## Local Contracts

`scanner-kit.toml` defines the rules package pin.
Its top-level keys are `schema`, `name`, `version`, `license` and `description`.
`name` is `hamh-semgrep-rules`.
`version` is `1.0.0`.
The `[source]` table contains `url`, `commit` and `tree`.
The `[rules]` table contains `folders`, `files`, `bytes` and `files_sha256`.
`folders` selects `python` and `bash`.
The `[semgrep]` table contains `version`.
The scanner kit at `lifecycle/pack/.agents/skills/lifecycle-check/scripts/scanner_kit.py` installs pinned Semgrep `1.78.0` and `hamh-semgrep-rules` into a virtual environment.
It runs the pinned Python and Bash rules on files a run changes.
The rules are YAML files from one commit of `github.com/semgrep/semgrep-rules`.
They are third-party content under the Semgrep Rules License v1.0.
Keep this third-party content out of the repository.
`scripts/lib/third_party_scan.py` fails on a copy.

## Work Guidance

Treat a change to any pin value as a new version of the package.
Keep the same commit and package version in the kit script.
`scripts/scanner_kit_rules.py build` fetches the pinned commit.
It checks the commit, tree and selection of rule files against the pin.
It writes a reproducible wheel, the package distribution file.
`scripts/scanner_kit_rules.py publish` uploads the wheel to the stage's CodeArtifact PyPI repository.
Publishing is a dry run by default.
Use `--apply` to publish.

## Verification

Run `python3 scripts/scanner_kit_rules.py build` with network access to GitHub.
Run `python3 scripts/dox_lint.py` and require it to print `DOX PASS`.

## Child DOX Index

None.
