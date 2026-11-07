# images/tools/lang/js

## Purpose

The pinned JS tools of the language tool set: eslint with eslint's and typescript-eslint's recommended rules, eslint-plugin-security,
prettier, typescript 6.0.3 (typescript-eslint 8.70 supports below 6.1) and pnpm 12.

## Ownership

Owned by `images/tools/AGENTS.md`. `tools/lang/install.sh` installs it into every family's
language tool set.

## Local Contracts

- `package.json` pins exact versions; `package-lock.json` pins every package by integrity.
  `install.sh` runs `npm ci --ignore-scripts`, so no lifecycle script runs; pnpm 12 then runs
  through its package's bin script with its platform binary package from the lock.
- Regenerate the lockfile with the pinned Node (`npm install --package-lock-only
  --ignore-scripts`) inside the language test container; a changed lockfile reinstalls the set.
- `package.json` carries an `overrides` entry that pins `minimatch` to 11.0.1. `eslint` and `typescript-eslint` ask for `^10`, and `minimatch` below 11.0.1 has GHSA-g3hv-75wg-m249 (denial of service through glob-part amplification when a pattern is attacker-controlled). The lock pins `minimatch` 11.0.1 and its dependency `@isaacs/nonrecursive-glob-match` 1.0.3; all other locked versions are unchanged. Remove the override when the ranges of `eslint` and `typescript-eslint` allow 11.0.1 or later.

## Work Guidance

## Verification

```bash
scripts/gates-lang-tools.sh
```

## Child DOX Index

None.
