<!-- DOX framework vendored from https://github.com/agent0ai/dox, MIT License, Copyright (c) 2026 Agent Zero. Project sections follow the framework. -->

# DOX framework

- DOX is highly performant AGENTS.md hierarchy installed here
- Agent must follow DOX instructions across any edits

## Core Contract

- AGENTS.md files are binding work contracts for their subtrees
- Work products, source materials, instructions, records, assets, and durable docs must stay understandable from the nearest applicable AGENTS.md plus every parent AGENTS.md above it

## Read Before Editing

1. Read the root AGENTS.md
2. Identify every file or folder you expect to touch
3. Walk from the repository root to each target path
4. Read every AGENTS.md found along each route
5. If a parent AGENTS.md lists a child AGENTS.md whose scope contains the path, read that child and continue from there
6. Use the nearest AGENTS.md as the local contract and parent docs for repo-wide rules
7. If docs conflict, the closer doc controls local work details, but no child doc may weaken DOX

Do not rely on memory. Re-read the applicable DOX chain in the current session before editing.

## Update After Editing

Every meaningful change requires a DOX pass before the task is done.

Update the closest owning AGENTS.md when a change affects:

- purpose, scope, ownership, or responsibilities
- durable structure, contracts, workflows, or operating rules
- required inputs, outputs, permissions, constraints, side effects, or artifacts
- user preferences about behavior, communication, process, organization, or quality
- AGENTS.md creation, deletion, move, rename, or index contents

Update parent docs when parent-level structure, ownership, workflow, or child index changes. Update child docs when parent changes alter local rules. Remove stale or contradictory text immediately. Small edits that do not change behavior or contracts may leave docs unchanged, but the DOX pass still must happen.

## Hierarchy

- Root AGENTS.md is the DOX rail: project-wide instructions, global preferences, durable workflow rules, and the top-level Child DOX Index
- Child AGENTS.md files own domain-specific instructions and their own Child DOX Index
- Each parent explains what its direct children cover and what stays owned by the parent
- The closer a doc is to the work, the more specific and practical it must be

## Child Doc Shape

- Create a child AGENTS.md when a folder becomes a durable boundary with its own purpose, rules, responsibilities, workflow, materials, or quality standards
- Work Guidance must reflect the current standards of the project or user instructions; if there are no specific standards or instructions yet, leave it empty
- Verification must reflect an existing check; if no verification framework exists yet, leave it empty and update it when one exists

Default section order:
- Purpose
- Ownership
- Local Contracts
- Work Guidance
- Verification
- Child DOX Index

## Style

- Keep docs concise, current, and operational
- Document stable contracts, not diary entries
- Put broad rules in parent docs and concrete details in child docs
- Prefer direct bullets with explicit names
- Do not duplicate rules across many files unless each scope needs a local version
- Delete stale notes instead of explaining history
- Trim obvious statements, repeated rules, misplaced detail, and warnings for risks that no longer exist

## Closeout

1. Re-check changed paths against the DOX chain
2. Update nearest owning docs and any affected parents or children
3. Refresh every affected Child DOX Index
4. Remove stale or contradictory text
5. Run existing verification when relevant
6. Report any docs intentionally left unchanged and why

## User Preferences

When the user requests a durable behavior change, record it here or in the relevant child AGENTS.md

- TDD for every behavior: a failing test first, and keep its red output (command, commit, trimmed failure) with the change's records.
- Development, builds, and tests run on a Linux machine with the tools pinned in `tools.lock`.
  The `accounts.json` entry selected by `HAMH_AWS_PROFILE` hosts only the final full-stack build,
  deployment, and live E2E. The entry with role `active` is selected when the variable is unset.
- Temp files and tool caches go only in one scratch directory named by `TMPDIR` and the caches' own variables.
- Keep this DOX tree current: every durable directory has an AGENTS.md, and `scripts/dox_lint.py`
  must print DOX PASS before a change is done.
- Never create, store, request or hard-code a persistent password, in this stack or in any project an agent builds, and use AWS best practice in e2e tests too: IAM roles, STS, SigV4, IAM database authentication or OIDC. A throwaway local container uses trust auth on 127.0.0.1 or a per-run random secret made at run time. A test RDS instance enables IAM database authentication and is deleted by the run that created it. If a task cannot work without a persistent password, stop and tell the maintainers. The default rules pack carries the rule for agents.
- This project is opinionated by default and fully configurable by the user. A new default ships with its way to change it in the same change: a setting in `config.toml` or an engine file (a row in `crates/ham-core/src/settings.rs`), a user-owned file, or a switch on a command. Enforcement is a setting too: a hard gate is something the user turns on. DOX is no gate: it is seeded at the start of a run, the engine and the model keep the docs, and the harness adds no DOX check; `DOX off` as the first line of the starter switches it off. Root invariants 2, 3, 4 and 6 stay locked and no setting changes them.
- Security testing stays inside the selected AWS account. Pen testing, red-team and hostile-payload tests run only from EC2 to the same account. CloudFront-style outbound is fine. These tests never run from a contributor's development or build machine. No test, script or helper starts a process on a contributor's development or build machine whose command line or environment holds exploit-looking text (variable-substitution tricks, command substitution, touch/rm/cat fragments, pwned markers). A test that proves no shell reads an argument uses ordinary metacharacters and asserts the literal argument arrived. Security testing requires a maintainer's request. Fixing scanner findings is a code task.
- No internal tool, host, account number or profile name of the team appears in anything a user reads: not in `README.md`, `SECURITY.md`, `CONTRIBUTING.md`, `setup.sh`, `docs/`, the client source, the shipped lifecycle pack or the output of `ham` and `setup.sh`. Use "your AWS CLI profile", "the stage's AWS account", "a Linux build host" or a placeholder such as `<stage account>`.
- `ham` is configurable through TOML files, with one file for the client and one for each engine:
  `config.toml` holds the client's own settings and one file per engine in `engines/` holds that engine's settings. A person edits them with `ham config`, the Settings screen or a text editor. A default user touches nothing, and every key is opt-in.
  `crates/ham-core/src/settings.rs` is the one registry of client keys; a new key adds its row, its reader and the generated table in `docs/user-guide.md` (between the `ham-settings` markers) in the same change.
- The product stays neutral about the environment of whoever builds it. No route to a private git host, workstation path or contributor-specific network setting appears in the product or in a setting that exists only for this repository's scripts. Before building, classify each gap as a product feature, a defect in this repository's own scripts, or a limit of the build environment.

# Hyperscale Agent Meta Harness Engineering Contract

## Purpose

A serverless AWS service on Amazon Bedrock AgentCore. One prompt in the thin TUI `ham` starts a
remote goal loop of coding agents (two engines, Cline and OpenCode, run today and each run uses the one you choose; Pi, etc. are the harnesses the engine adapter design is meant for) on any smoke-tested
Bedrock model. The run keeps going with the laptop closed and returns what you can use without a git host: an S3 zip of
fully tested code (downloaded by `ham pull` or by the TUI), a local git clone of it on your machine through `ham clone` (built from the
`repo.bundle` every zip carries), and, for the runs that target a CodeCommit repository, a CodeCommit pull request link.
GitHub and GitLab delivery are not supported.

## Scope

This repository holds code only: the Rust Cargo workspace (`crates/`), toolchain pins (`tools.lock`), engine pins and profiles (`engine/`), images, state machines, the CDK app (`infra/`), the lifecycle pack (`lifecycle/`), contract schemas (`schemas/`),
fixtures, and scripts. `accounts.json` at the root lists the AWS accounts once, each entry with a role: `active`, `history` or `test`. `active` is the default entry; `history` is an earlier account whose records stay valid; `test` is for deploys and live
tests, chosen only by name, never the default and never the active entry. `HAMH_AWS_PROFILE` names the target entry; unset means the active entry. Each entry names its stage Availability Zones, their AgentCore AZ IDs, and its invocation-logging mode; it may
name the stage's repository names (`repoNames`, default `hamh-e2e-*`). Scripts (`scripts/lib/aws_target.py`, `scripts/lib/aws-target.sh`), the CDK app (`infra/lib/config.ts`) and every Rust binary (the `harness-protocol` build script) read it; Rust binaries
carry the selection they were built with. `HAMH_ACCOUNTS_FILE`, an absolute path, makes every one of those readers use another file in place of the committed one (the account file `setup.sh` generates for a user's own account; `scripts/AGENTS.md` lists the
readers); unset, the committed file is read.

The documentation set is `README.md` and `docs/` (`docs/AGENTS.md`). `setup.sh` at the root is the single entry point for a person's own machine: it asks first for the client only or the full setup
(the backend built and deployed in the user's own AWS account by a runner it installs, and the client built there and downloaded), and its AWS lifecycle lives in `scripts/setup/`;
`scripts/tests/test_setup_script.py` and the `test_setup_*` suites test it, and coding agents never run it.
`LICENSE` (MIT-0), `NOTICE` (the parts that derive from Cline, Apache-2.0, and the DOX framework text, MIT), `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md` and `CHANGELOG.md` (the running changelog, kept current by the Change rules) sit at the root. No coding engine's source or binary is stored in this repository: Cline, Cline's goal plugin and any later engine are fetched from a pinned upstream source at build time (`engine/sources/` holds the pins, `scripts/engine-fetch` the fetch, `scripts/lib/third_party_scan.py` fails on a copy).

## Invariants

1. Engines like Cline and OpenCode decide when the work is done. The harness records the engine's stop reason and final message and shows one label of 15 outcomes,
   written with the name of the engine that ran (`Engine::display_name`: Cline, OpenCode). The sentences of Cline are the frozen table: "Cline reported done" appears only when
   Cline ends its turn with a message and the harness counted at least one changed file (a tree it could not count keeps the label); a failure before the prompt shows
   "Cline did not start", and every other end shows "Cline stopped: <reason>" (a goal loop the harness ended itself reads "Cline stopped: the harness ended the run (<reason>)").
   A run of another engine reads the same sentences with its own name ("OpenCode reported done"), never Cline's, and the summary validator accepts a label only for the engine
   of the summary. The product never calls a run verified, tested, gated or evaluated. The harness owns the end state of the run: bundle validation,
   delivery (the S3 zip, the local git clone through `ham clone`, the CodeCommit pull request) and the events `ham` shows. Engine exit
   codes and finish reasons are telemetry; the label comes from the classifier of the engine's stop. A helper model's text is never an outcome label. Tags are display and search text, never an identifier or an outcome label; the status words and the four outcome words are never tags.
2. No GitHub token and no long-lived AWS credential ever enters an agent microVM. Engines get per-session STS credentials through
   `credential_process` with `AWS_EC2_METADATA_DISABLED=true`. The package-registry (CodeArtifact) token never reaches Cline: agentd starts the
   loopback package proxy `pkgproxy`, which holds the token, and Cline installs packages through the proxy.
   Helper model calls run in the run-session Lambda role, scoped to the utility model's ARNs, never with a worker session's credentials.
3. Engines are pinned by hash, never self-update, and run with telemetry off; the VPC egress
   allowlist drops every non-allowlisted host, including `*.cline.bot`.
4. Only harness-authored events move a run to complete or show links in the TUI.
5. `ham` is a thin control plane: no agent logic and no state of its own. Its local files are the user's own text and the artifacts it downloads: `config.toml`;
   one TOML file per engine in `engines/` (for now `engines/cline.toml`); the rules directory with `continue.md`; `review.md` beside `continue.md`; the skills directory; all under `~/.config/ham/`;
   and the download directory. `ham` sends rules, skills, `continue.md` and `review.md` with a run unread. It reads `config.toml` and the engine file only to check their keys
   against the settings registry and to turn their values into request members, which control-api and agentd check again. It writes one of these files only when
   the user asks (`ham config set`, `ham engine cline set`, `ham config migrate`, a Settings-screen action, an editor session), changes only the lines concerned, and writes no other file,
   no lock file and no cache. The harness decides which engine settings take effect and locks the rest (invariants 2, 3 and 6).
   `ham update` fetches the commit it installs into the download directory and runs that commit's `setup.sh --update`, which installs the client; when the
   installed client does not report the fetched version, `ham` renames the earlier client that setup kept beside it (`ham.prev`) back into place, only when that
   file is the client that ran before the update.
   While `~/.config/ham` holds none of the user's files, `ham` reads the folder `~/.config/cah` read-only.
   `ham config migrate` is the one command that copies those files on request; nothing writes `~/.config/cah`.
6. Bedrock is the only model provider. A model is selectable only with a passing smoke record. The harness's own helper calls outside the engine use the one utility model
   of `crates/ham-core/assets/utility-models.json` (default Claude Haiku 5.5). It needs a passing smoke record, is Bedrock only and is never a run model.
7. Every AWS resource carries the tag `project=hyperscale-agent-meta-harness`; `scripts/teardown.sh`
   removes them and prints a zero-resource inventory.
   Stages deployed before the rename carry `project=cline-agentcore-harness`; until VERSION 6.00 teardown, status and doctor also look for that value and name the generation they found, and afterwards they still report such a stage and refuse to work around it.
   Log groups carry no tag; teardown identifies them by stage prefix only after their writer is gone.
   For an `accounts.json` entry with role `test`, the CDK app also tags what it creates with `auto-delete=no`, so an account clean-up service that deletes untagged resources keeps them; see `infra/AGENTS.md`.
8. No persistent password is created, stored or committed, by this repository or by any project an agent builds. Identity replaces it (IAM roles,
   STS, SigV4, IAM database authentication, OIDC); a throwaway container uses trust authentication on 127.0.0.1 or a random secret made at run time.
9. A run never loses its work: "we must never lose code, always have at least a checkpoint style mechanism on s3 while running so we can
   always get back latest state of the working codebase up to the most recent write". It binds agentd, run-session, control-api and `ham`:
   while a run works, agentd saves a copy of the work tree to the stage's bucket when it changed (every `DEFAULT_SNAPSHOT_EVERY_S` seconds
   and after a tool call); when the engine has stopped, it saves the end state before the session's bundle and before anything can refuse
   a delivery; no check that refuses or fails a delivery removes or hides that copy; and however a run ends, `ham pull <runId> --snapshot`
   downloads the latest copy and every text about the run says so. A session that dies while the engine works loses at most one minute of
   file changes, or more for a repository whose copies take long. `docs/user-guide.md` ("Your work is kept") states the guarantee, the most that can be lost and how long copies are kept.

## Custom rules

Documentation follows the DOX framework above: every durable directory gets an AGENTS.md as it is
created, listed in its parent's Child DOX Index, and `scripts/dox_lint.py` gates every change.

Coding agents that work on this repository read this file and the child `AGENTS.md` files. The rules of a Cline run are the user's own
editable files (invariant 5); `ham rules init` writes the defaults from `crates/ham-core/assets/default-rules.md`. Cline runs with auto-approve on.

## Change rules

1. Add a failing test before changing behavior and keep its red output with the change's records.
2. Make the focused test pass, then run the full verification below.
3. Every architectural change updates the scripted E2E of the mandated user path.
4. No credentials, keys, or customer data in git. Test stand-ins for AWS key IDs, tokens and passwords are
   built at run time; a gitleaks scan covers the full history of every ref.
5. Development, builds, and tests run on a Linux machine with the tools pinned in `tools.lock`. Deploys and live E2E run only in the
   `accounts.json` entry that `HAMH_AWS_PROFILE` selects (the active entry when it is unset). An entry with role `test` serves TUI tests and live checks, and is chosen only by name.
   Never touch another project's untagged resources in any of these accounts.
   Temp files and tool caches go only in the scratch directory that `TMPDIR` names.
6. `VERSION` at the root is the one source of the tag the TUI shows on every screen, `ham vN.NN | backend vN.NN`: N is the major
   number, NN a two-digit counter. `crates/ham` and `crates/control-api` embed it at build time (`build.rs`); the backend
   reports its own on every answer in the `x-hamh-backend-version` header. Raise the counter in the change that alters what a user of
   `ham` sees or what the backend does, and the major number for each new release line. The file is no input of the worker image
   key or the deployment digest.
8. Every user- or maintainer-visible change adds a bullet under `## [Unreleased]` in `CHANGELOG.md` in the same change.
   Include features, architecture updates, notable fixes, changed defaults and removed behaviour. Each bullet is concise and specific.
   Use plain short sentences. Name the behaviour, command, setting or component. Omit IDs, names, infrastructure details and dates.
   Unnoticed changes (tests, formatting, internal wording) add no line. Security changes add no line: fixes, hardening, advisories, and changes to
   permissions, identity, isolation, redaction or scanners stay out of the file, which has no `Security` group. Raising `VERSION` moves Unreleased lines to a version heading.
   Use `## [N.NN] - YYYY-MM-DD`, dated when raised, newest first. Leave Unreleased empty. People and agents follow this; no check enforces it.

## Verification

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
python3 scripts/dox_lint.py       # prints DOX PASS
python3 -m unittest discover -s scripts/tests
```

Node runs only through `scripts/node.sh`. A check whose program is missing on your machine did not run: say so when you hand the work over.

## Child DOX Index

| Path | Covers |
| --- | --- |
| `crates/AGENTS.md` | Rust Cargo workspace members: harness-protocol, harness-core, ham-core, ham, ham-admin, ham-e2e, worker-api, control-api, agentd, dispatcher, delivery, pkgproxy, run-session, message-ledger, research-api |
| `docs/AGENTS.md` | The documentation set: the user guide (`README.md` and `setup.sh` at the root belong to the same set); `docs/images/` holds the README's architecture picture |
| `engine/AGENTS.md` | The pins of every third-party engine and plugin (`engine/sources/`: cline, goal-plugin, esbuild, opencode), the Cline patch series and engine profiles; `engine/cline/` is the Cline fork (lock, patches 0001-0015 on cli-v3.0.71); `engine/profiles/` holds `cline.toml`, `fake-acp.toml` and `opencode.toml`; `engine/catalog/` is the static model list `models.json` with its smoke records; `engine/rules/deps/` holds recorded dependency data: the Go approved module list (`go-approved-modules.txt`) and the Maven notes (`jvm.json`); `engine/policy/` holds `image-key.pin` |
| `fixtures/AGENTS.md` | Test data shared across crates, scripts and the CDK app. |
| `images/AGENTS.md` | Container images and CodeBuild ARM build specs: the Cline fork linux-arm64 binary, the worker image with its native services and pinned tool installers, the delivery image, and the language test container of the build host |
| `infra/AGENTS.md` | CDK v2 TypeScript app (L1) for the 9 `hamh-direct-*` stacks of stage `direct`; run through `scripts/node.sh` |
| `lifecycle/AGENTS.md` | The lifecycle pack v1: the contract and seven skills that carry the development lifecycle into a run (`lifecycle/pack/`), their SHA-256 lock (`pack.lock.json`, `lock.py`) and the step table; the client copy of the contract is `crates/ham-core/assets/lifecycle-rules.md` |
| `schemas/AGENTS.md` | Generated contract artifacts (JSON Schema 2020-12, OpenAPI 3.1) from harness-protocol |
| `security/AGENTS.md` | The scanner kit's rules package pin in `security/scanner-kit.toml` and the build and publish tool `scripts/scanner_kit_rules.py`. |
| `statemachines/AGENTS.md` | Amazon States Language definitions of this product: `direct` (one Cline session per run; a Blocked that cannot run fails in `RunMachineFailed`) and `direct-run-ended` (retries run-session until a run whose execution failed is cleared) |
| `scripts/AGENTS.md` | Repository tooling that deploys, tears down and sets up the stage: deploy and teardown with their libraries, the Node wrapper, the engine fetch, the Cline fork build, the worker image key guard, the Go module store, the research key helper and the DOX lint |
