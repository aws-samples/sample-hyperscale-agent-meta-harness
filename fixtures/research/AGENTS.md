# fixtures/research

## Purpose

The prompt and the record-directory layout of the research route check, with a recorded sample run under `real-run/`.

## Ownership

Owned by `fixtures/AGENTS.md`.

## Local Contracts

- `prompt.md` is the prompt of both arms (the ON arm and the OFF arm, which `ham run --no-research` starts) of the `research-search` leg.
- `key-state-event.json` is the API Gateway proxy event of the operator's key-state question: `POST /internal/research/key-state`, no caller identity, body `{}`.
- The RECORD DIRECTORY is everything a judge reads; a judge makes no call:
  ```
  <dir>/record.json            schema hamh-research-record/1: mode offline|live, run, model, client, stage {account, region, deployment, provider,
                               firewallDomainsSha256, firewallExpectedSha256, secretHasCurrentVersion}, expectProvider, arms {on, off} each with
                               runId, outcome, zipMembers[], toolLog[{kind, title, at}] (the on arm of a composed record also toolLogSource "composed"; the
                               off arm also researchAnswer), and the live-only
                               members a7 {hostsDropped[], verdict}, a5 {unlistedAllowedFlows}, curlOutcome {command, failed, text}, gate
                               {blockedReason, smallPassed} (a hand-supplied `--gate-result` has these two; the gate probe adds the validator's answers `big` and
                               `small`, each a `ValidateOutput` {pass, violations[{rule, path, detail}]}, and `probeRun`; `problem` is a validator that answered with an error,
                               passed the copy or refused the probe under another rule: E11 fails with it; `unavailable` is a probe that could not be sent: E11 is not run
                               with it), gateProbe {skipped} (`--no-gate-probe <reason>`), cloudtrail {getSecretValueByPrincipal {research-api, worker}}, simulate
                               {workerSessionGetSecretValue}, canary {found}, selftest {codes[]}, limitsOutput (ham-research limits:
                               {searches, fetchedUrls, repos, outboundBytes, contextBytes} each {used, max}, a snapshot of some moment of the run),
                               routeUsage (the route's end-of-run count: summary.research.usage of the delivered evidence/direct.json, the same five counters as
                               plain numbers), routeCounts {placed, placedBytes} (summary.research.counts), verifyOutput {ok, differences} (.ham/research/verify.txt,
                               the output of `ham-research verify`), inputProblems {verify, limits, routeUsage, listing} (an input that EXISTS and cannot be read, with
                               the reason: the check fails with it; the key is absent when every input that exists was read); a member a run cannot have is null
  <dir>/s3/runs/<run>/agents/direct/research/{audit,items,searches}/...   the exact objects of harness-protocol `research` (audit objects, item
                               manifests in canonical form with their bodies, search records)
  <dir>/placed.json            what agentd placed: {schema hamh-research-placed/1, run, items {<id>: {files {<path>: <sha256>}}}}; the same
                               file may sit at s3/runs/<run>/agents/direct/context-listing.json. agentd writes it at the end of a run whose research lane started
                               none, and its placement is proved by `ham-research verify` saved as verify.txt
  <dir>/delivery/<run>/...     the delivered tree of each arm (the zip's top directory): MANIFEST.sha256 (`<sha256>  <path>`, sorted), src/...,
                               src/.ham/research/*.md (the notes), src/.ham/context/INDEX.md, and for the ON arm blob-hashes.txt (`<sha256>  <path>`
                               for every blob reachable in repo.bundle, written by the collector)
  ```
- A record composed from the simulator output has `toolLogSource: composed` on the ON arm: its tool log is built from the simulator's calls, and check E7 reports NOTRUN for it
  (no Cline read a file). Its outcome is `simulated`, its stage has a provider and no account, Region, deployment or firewall hash, and `sim.json` (schema
  `hamh-research-sim/1`) is the source of its run ids, self-test codes, counters, OFF answer and the manifest hash of each index row.
- The index is the markdown table `ham-research index --md` prints, with the columns `Origin | Date read | sha256 | Licence | Why it matters` (the columns the
  lifecycle pack and its scorer read; `index-md.example` holds the exact bytes of the standard record's table, and a Rust test pins the renderer to it). The
  sha256 is the SHA-256 of the item's `manifest.json` bytes and finds the item; a repository's origin is `origin@commit`; the date is the first ten
  characters of `retrievedAt`; the last column is the run's own words and is not judged. A note's Sources section is
  the list items and table rows below a `Sources` heading, up to the next heading; an entry marked `(not fetched)` claims no read.
- `index-md.golden` belongs to the research module's own writer, not to the record directory: it is the table that agentd's renderer (`crates/agentd/src/direct/research/index.rs`) writes for `ham-research index
  --md`, with the five columns `Origin | Date read | sha256 | Licence | Why it matters` that the lifecycle pack and its LC-2 check read, for a page whose address holds a pipe and a repository at a commit. A
  change to it is a change of the contract between the producer and the check: change the tests of both together.
- `summary-research.json` is the `research` member of `summary.json`, for a run that searched twice, fetched twice (the route stored a page that agentd could not place; one address was
  refused) and asked for one repository (the upstream failed): `usage` is the route's own count at the end of the run, `counts` is agentd's own, and the two differ on purpose. It is the one set of bytes that agentd's
  writer (`crates/agentd/tests/direct_research_hook.rs`) must produce and that `harness-protocol` (`wire_research.rs`, `wire_direct_runtime.rs`), `run-session` (`summary_research.rs`) and `ham-core` (`chat.rs`) read.
- Statuses: PASS, FAIL, NOTRUN (the record has nothing to judge the check on) and NA (the check does not apply: the key check on a stage with no key, and
  the refusals check E14 for a record with no selftest result, which is every live record now that the tool has no selftest).
  Offline, NOTRUN is reported and a record with only PASS and NOTRUN passes; live, a NOTRUN check fails the run. An audit object with `bytesOut` above
  zero marks a call the route charged, which is how the counters (E15) are added up.
- Every check is an ordinary invalid-input or record-consistency check: a changed hash, a missing object, a count that does not add up. The row sends
  nothing hostile. The offline simulator provokes the ordinary refusals in process, against the route handlers: `usage` (a query over
  400 characters), `refused_url` (a URL that is not https), `refused_unseen_url` (a URL never seen in the run) and `refused_host` (a repository on a host other than
  github.com); its record carries them as `selftest.codes` and E14 checks them against the audit objects.

## Work Guidance

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest scripts.tests.test_e2e_direct_research
```

## Child DOX Index

None.
