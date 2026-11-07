# engine/rules/deps

## Purpose

Recorded dependency data for two stacks of the n-tier projects: `go-approved-modules.txt`, the Go module roots the module store may
hold, and `jvm.json`, the notes on which Maven (Spring Boot) lines resolve without a HIGH, CRITICAL or unrated OSV advisory
and which dependencies fall outside the license allowlist.

## Ownership

Owned by `engine/rules/AGENTS.md`. Readers:

- `scripts/gomod-store.sh fill` takes `go-approved-modules.txt` as its default `--approved` list (`scripts/lib/gomod_store.py`
  `DEFAULT_APPROVED`).

No crate reads either file.

## Local Contracts

- `go-approved-modules.txt` lists one `module@version` per line (43 roots), after `#` header lines that give the generated date
  (`# generated:`), the Go toolchain version (`# go:`), the license allowlist, the root count (`# roots:`) and, at the end, the candidates
  with no clean version and the first module that failed. A root is listed when it and every module its `go mod tidy` downloads had
  no HIGH, CRITICAL or unrated OSV advisory and a LICENSE text on the allowlist on the generated date.
- `scripts/gomod-store.sh fill` refuses a module that is not on this list (exit 2, before any download or AWS call). The Go module
  store (bucket `hamh-<stage>-gomod-<account>`, packages stack) therefore holds these roots, the tool roots of `--tools`
  (`scripts/gomod-tool-roots.txt`) and the modules they pull in.
- Nothing in the tree regenerates the list. A change is a hand edit that keeps the `# generated:`, `# go:` and `# roots:` lines true.
  The directory is under `engine/`, so a change moves the worker image key (`scripts/image-key-guard.sh`).
- `jvm.json` is one JSON object with sorted keys and two-space indentation: `schema` (1), `profile` (`jvm`), `ecosystem` (`Maven`),
  `generated` (the date of the queries), `allowList` (SPDX ids), `sources` (the hosts queried), `frameworks` (the Spring
  Boot lines with the version, the pom property overrides and the advisory counts as resolved), `hints`, `promptGuidance` and
  `unverified` (facts no fetched source established).
- A hint is `{gate, match, verdict, text, claims}`: `match` holds lower-case Maven `group:artifact` names, exact or a prefix ending in
  `*`, and `claims` maps each matched pattern to the versions of the packages the hint calls clean. A `text` or a `promptGuidance`
  sentence is under 300 characters.
- `jvm.json` calls no 3.x Spring Boot line clean: Spring Framework 6.2.x has no fix for GHSA-pc63-qcmh-9cmg and GHSA-j9f9-w8pj-32f8,
  which only Spring Framework 7.0.9 (Boot 4.1.1 and 4.0.8) clears.

## Work Guidance

- Refresh `jvm.json` from new OSV and Maven Central queries when a Spring Boot line moves, and update `generated` and the recorded
  facts file in the same change. New advisories appear after a note's date, so the dates and fix versions age in weeks.

## Verification

```bash
$HAMH_PYTHON -m unittest scripts/tests/test_ntier_events_prompt.py
```

## Child DOX Index

None.
