# crates

## Purpose

The Rust Cargo workspace members. All production code is Rust; every wire type
lives in `harness-protocol`.

## Ownership

Owned by the root contract. Each crate's `AGENTS.md` owns its local rules.

## Local Contracts

- Workspace root `Cargo.toml`: resolver 3, edition 2024, shared lints (`unsafe_code` forbidden;
  `unwrap_used`, `todo`, `dbg_macro` denied), path dependencies declared in `[workspace.dependencies]`.
- Toolchain: `rust-toolchain.toml` pins the `tools.lock` Rust version; the two must match.
- A crate depends on another workspace crate only through `[workspace.dependencies]`.
- Wire types (anything serialized across a process or network boundary) are defined only in
  `harness-protocol`; other crates import them.
- Account, home Region and profile: only the `harness-protocol` build script's constants
  (`target::{E2E_ACCOUNT, E2E_REGION, E2E_PROFILE, OPERATOR_POOL_MEMBER, OPERATOR_POOL_MEMBERS}`,
  `repo_read::{STS_REGION, STS_HOST}`), generated from the root `accounts.json` entry `HAMH_AWS_PROFILE`
  selects (unset: the active entry). Tests pass under every selection: expectations built from the
  constants, recorded fixtures unchanged.

## Work Guidance

- TDD: a failing test first in a test-only commit, red record kept with the change's records.
- Property tests (`proptest`) for reducers, parsers and validators;
  `insta` snapshots for schemas and rendered output.
- A fake of an AWS API rejects what the real API rejects (formats, name patterns, enums, required
  pairs), with the API reference cited in the fake. A permissive fake let a runtime version pass as
  InvokeAgentRuntime's `qualifier` (an endpoint name) and broke live dispatch.

## Verification

```bash
# Another accounts.json entry: select it with HAMH_AWS_PROFILE and build and test it in its own target dir.
HAMH_AWS_PROFILE=<profile of that entry> CARGO_TARGET_DIR="$TMPDIR/target-other" scripts/cargo-slot.sh cargo nextest run --workspace --locked
```

## Child DOX Index

| Path | Covers |
| --- | --- |
| `harness-protocol/AGENTS.md` | Wire types: event envelope, `/v1` and `/internal` API (with the direct run request `StartDirectRunRequest`, the model list and `ContinueSettings`), store items and S3 layout, catalog row, evidence header, engine profile, the AgentCore Runtime contract and the direct-run contract (`direct`: invocation with its failover routes and each model's default reasoning level, outcomes with their frozen labels, `summary.json` with attempts, switches and the launch reasoning level, evidence); schema export |
| `harness-core/AGENTS.md` | Pure logic: the engine-profile loader (bundled at build time), the diagnostic of an engine process that ended with its stderr scrub, secret redaction |
| `ham-core/AGENTS.md` | `ham` client state: the run reducer (`RunState`, `RunView`), the agent table, the command parser (the 21 subcommands, the `ham push` URL and branch checks, the refusal of a mistyped command word, and `--direct`), the command-line help texts, the command registry (one table of names, usage, effects and availability that feeds the parser, `/help` and the popup) and the chat box's pure router with its second-Enter confirmation, the Runs filter, the chat search, the prompt history, paste placeholder and external editor of the chat box, the status meters, reconnect state and alert bytes, request building, the user's rules validator with the default rules and `continue.md`, the model list with its alternate routes (`failover_routes`) and reasoning rows (`ModelReasoning`, `check_reasoning`) and the choice of the run's model (Claude Sonnet 5.5 by default), the config keys and the engine settings file (`engine_file`, `engine_config_card`), the shard polling plan, exit codes, the download plan, renderings and the stream sanitizer, and the wire types of the chat-client routes (`events_view`, `run_index`, `snapshot`), the bounded gzip reader (`gzip`), the stored conversation's chunk format and the transcript route's wire types (`transcript`), the stored conversation as the chat view's blocks (`stored`), the retry of a blocked run's key and words (`retry`), the search across runs (`runsearch`), the engine-neutral surface (`engines`), the follow-up digest (`followup`), the wire type of the delivered head with the validator's change list (`head`), the skills of a run on the card with the reserved `lifecycle-` namespace (`skills_card`, `skills`), the rules and words of the switch of the account's Bedrock data retention mode before a run on a model that needs provider data sharing (`sharing`), and the pure core of run names (`runname`: the request, the judge, the seed) and of the utility model that writes them (`utility`: its catalog file `crates/ham-core/assets/utility-models.json` with `UtilityModels::builtin()` and the smoke record beside it, the request and reply, the error classes, the rejection check of its fakes, and the prompt excerpt and raw-reply guard that run names and run tags share) |
| `ham/AGENTS.md` | The `ham` binary: SigV4 `/v1` transport, the 21 subcommands, `ham rules init|edit|show`, `ham engine <name> ...` (an engine's settings file, `engine_cmd`), direct runs on the user's model (`--model`, `HAMH_MODEL`, `ham config set default_model`, `ham models`, a TUI model picker), `ham pull` (and `ham pull --snapshot`, `snapshot_cmd`), `ham clone` (the delivered zip's `repo.bundle` as a local git repository, proven equal to the zip's source tree), `ham push` (the same repository pushed to another git URL with the user's own git), doctor, the switch of the account's Bedrock data retention mode before a start on a model that needs provider data sharing (`sharing`), the commands' text as data (`Output`, `CmdOutput`, `cli::run_captured`) and the Ratatui TUI (Runs screen, composer, dashboard with the short Link line and the `o` and `d` keys, Artifacts screen), the run's name on the Runs screen, the run screen and `ham runs` (`runs_table`, `run_name`), and the calls of the chat-client routes with the snapshot download and the transcript and search reads (`chat_routes`), with the stored conversation, the send-message experience, open anything and the engine-neutral screen as children of the TUI's `app` module (described in the TUI's own AGENTS.md) |
| `ham-admin/AGENTS.md` | The `ham-admin` operator binary: `repos allow`, `deny` and `list` through `/v1/admin/repos` |
| `agentd/AGENTS.md` | In-microVM supervisor: the runtime contract, the direct driver (ACP client, collector, stop table, provider retry and failover, the reasoning level of the launch, snapshot, `summary.json`), the credential_process helper, the package proxy start (with the Maven settings and the cargo home and build switches) and event posting |
| `ham-e2e/AGENTS.md` | `ham-e2e` holds the local fake engine of the project: the `fake-acp-engine` program that stands in for a coding engine in tests, and the library behind it. |
| `worker-api/AGENTS.md` | `/internal` Lambda: creds re-mint with nonce rotation, heartbeat, turn-complete, events |
| `control-api/AGENTS.md` | `/v1` Lambda: `POST /v1/direct/runs` and `GET /v1/direct/models` (default marked) with the static `models.json` list, idempotent run start, owner-only run reads and lists with cost, cursor event reads, the paged transcript read of a run's stored conversation (`GET /v1/runs/{id}/transcript`), chat-content search (`GET /v1/search`), artifact list and presigned download URLs, cancel, the repository allowlist, whoami, events from the end with the user's answers, the run index (title, model, last activity, filters, backfill), the tag edit (`POST /v1/tags/edit`), the snapshot download URL, the delivered head of a finished run (`GET /v1/runs/{id}/head`) and the retry of a blocked run (`POST /v1/runs/{id}/retry`: the stored request started again, linked to the blocked run) |
| `dispatcher/AGENTS.md` | Dispatcher library: run, agent and session ids, the Agent item with its credential binding, the session policy that leases a model and the routes a run may switch to, the endpoint pin and the AgentCore calls `run-session` is built on |
| `delivery/AGENTS.md` | Delivery Lambdas: repo-gateway (stage, new-repo, push-pr; the only git writer), bundle-validator (with the record of what a passing delivery changed, `changes.json`), packager (the zip and the evidence archive) |
| `message-ledger/AGENTS.md` | The rules that move a message of the user to a running Cline through its states, over a table that DynamoDB and the tests' in-memory copy both implement: admit, withdraw, the session report, the claim, the seal, the outcome and the close, each one a read followed by one conditional transaction |
| `pkgproxy/AGENTS.md` | The loopback package proxy of a direct run: the public API (`Secret`, `Config`, `start`, `Handle`, `StatsSnapshot`, the CodeArtifact and Go upstreams) that agentd starts so Cline can install packages (pip, npm, Maven, Go and cargo) without ever holding the CodeArtifact token; GET and HEAD only, redirects without the credential, link rewrite, streamed bodies, and for cargo the paths of a sparse registry, the `config.json` rewrite and the engine's `config.toml` (`cargo_config_toml`) |
| `run-session/AGENTS.md` | The `run-session` Lambda of a direct run: prepare, start-session (the newest task token wins, the failover routes and the session policy over them), plan-delivery (final bundle, else snapshot, with agentd's label), finalize, mark-blocked and run-ended (started by the machine `hamh-direct-run-ended`, which retries until the function answers), `name-run` (the display name of a run and, when `UTILITY_PURPOSES` lists `run-tags`, its topic and tech tags, asked of the utility model after the run exists in two branches that fail apart (`tag_run`); the machine's first state sends it and does not wait; the Converse client is `utility`), and the chat-content index that finalize and mark-blocked write for search after the session is stopped (`index`); the wiring makes no AWS call at start; the end of a run also updates its run-index item; tests run the real ASL over fakes |
| `research-api/AGENTS.md` | The research route of a direct run: the `research-api` Lambda (Tavily search and page fetch, repository job start and status, limits; the only reader of the Tavily key) and the `research-job` Lambda (GitHub archive download, no key), behind the private `/internal/research/*` routes |
