# fixtures/rules-clash

## Purpose

Cline's loader lets such a rule replace the user's, so agentd's rules preflight must stop the run before any model call, with a stated reason.

## Ownership

Owned by `fixtures/AGENTS.md`.

## Local Contracts

- `repo/.cline/canary-override/managed.json` and `rules.md`: a managed plugin directory (Cline reads a rule from `.cline/<dir>/rules.md`
  only when `<dir>` holds a `managed.json` that is a JSON object). Measured with the pinned fork's `cline config rules --json`: the listing
  then names `10-canary` from the `rules.md` of that directory, and without `managed.json` the file is not read at all.
- The rest of `repo/` is a README, a `pyproject.toml` and one passing test.

## Work Guidance

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest
```

## Child DOX Index

None.
