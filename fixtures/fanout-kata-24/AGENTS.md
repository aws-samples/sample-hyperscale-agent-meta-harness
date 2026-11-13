# fixtures/fanout-kata-24

## Purpose

A long-task fixture: a Python package of 24 independent helper modules plus one integration module that dispatches to all of them, and the prompt in `prompt.txt` that asks an agent to work on it. `repo/` holds the repository an agent starts from.

## Ownership

Owned by `fixtures/AGENTS.md`.

## Local Contracts

- `repo/`: the repository an agent works on. `README.md` fixes the semantics of every module the
  oracle tests check (one table row per helper, then the integration module).
- `prompt.txt`: the one-line prompt the composer types; it names the README and
  the integration module (`kata.integration`, which the agent creates) and carries no oracle value.
- `oracle/`: tests owned by the E2E harness, one file per module (`test_<module>_oracle.py`, R1-R25);
  they never enter a bundle or a prompt. Plain asserts, so the self-test runs
  them without pytest.
- `reference/`: a solution laid over `repo/` only by the self-test and the PR-check test; it never
  enters a fixture repository or a prompt.

## Work Guidance

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest discover -s scripts/tests -p 'test_*fanout_kata_24.py'
```

## Child DOX Index

None.
