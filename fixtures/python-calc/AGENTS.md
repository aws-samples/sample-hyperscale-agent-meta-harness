# fixtures/python-calc

## Purpose

`infra/lib/stacks/delivery.ts` bundles the oracle tests as the PR check's secondary source `oracle_python_calc`.

## Ownership

Owned by `fixtures/AGENTS.md`.

## Local Contracts

- `repo/`: the repository an agent works on. The README fixes the task semantics. Its `buildspec.yml` (pinned) runs the repository
  tests only.
- `oracle/`: tests owned by the E2E harness; they never enter a bundle or a prompt. The acceptance run executes them in the clean room after the zip's own tests,
  and the PR check runs them on the pull request's source commit. Their blob SHAs, and the `buildspec.yml` blob SHA, are pinned in the
  project's pin ledger.
- Changing a pinned file needs a new fixture name; a pin never changes.

## Work Guidance

## Verification

```bash
cd fixtures/python-calc/repo && python3 -m pytest -q
uv run --no-project --python 3.12 python -m unittest
```

## Child DOX Index

None.
