# fixtures

## Purpose

Test data shared across crates, scripts and the CDK app. `engines/` holds the transcripts the fake ACP engine replays, and `acp/` holds
recorded transcripts of the real engines. `python-calc` and `fanout-kata-24` hold the repositories, prompts and
oracle tests of the live acceptance, `rules-clash` holds the repository of a rules-name clash, and `ntier` holds the three sample projects `web`, `events` and `enterprise`.
`direct-probe` is the isolation probe. `replays` holds measured traces that a test replays through a module. `fanout-kata`, `load-kata` and `ts-lib` hold only oracle tests,
and `direct-input`, `direct-messages`, `lifecycle`, `research` and `rules` hold golden and recorded files. `setup/aws/` holds the canned answers and the expected account files of the tests of the full setup (`ec2-describe-availability-zones-<region>.json`, `accounts-expected-<region>.json`; read by `scripts/tests/setup_fake_aws.py` and `test_setup_full_units.py`). `aws-profiles`, the shared profile fixtures, holds AWS config and credentials files named `aws-config` and `aws-credentials` because the repository ignores files named `credentials*`, plus `kinds.tsv`, the expected kind of each profile, read by `crates/ham/tests/aws_profile_kinds.rs` and `scripts/tests/test_setup_script.py` so ham and `setup.sh` name profile kinds the same way.

## Ownership

Owned by the root contract. `engines/<name>/` is owned with the matching `engine/profiles/<name>.toml`. The oracle directories are read by `infra/lib/stacks/delivery.ts`.

## Local Contracts

- `setup/golden/` holds the golden transcripts of `setup.sh` with the default names (`help.txt`, `client-dry-run.txt`, `client-install.txt`, `client-failed-checks.txt`), each with the exit code, standard output and standard error and the temporary folder written `<TMP>`; `scripts/tests/test_setup_messages.py` compares them byte for byte, and `HAMH_WRITE_GOLDEN=1` rewrites them.
- `setup/runner-failed-build.txt` contains the end of the CodeBuild log for a runner build that fails in its `deploy` phase because the CDK app cannot open the account file.
  It includes the CDK step's output, the runner's `DEPLOY-FAIL` and `SETUP-FAIL` lines, and CodeBuild's closing `[Container]` lines, with one log line per line.
  It contains no account id, build id, or host name. `scripts/tests/test_setup_failure_excerpt.py` reads it.
- `setup/accounts-generated.json`: the example of the account file `setup.sh` generates for the account it deploys into, read through `HAMH_ACCOUNTS_FILE` by the tests of the override (placeholder account `111122223333`, Region `us-east-1`).
- `engines/<name>/`: data a local fake engine reads. The commit that adds a fake engine touches
  only `engine/profiles/**` and `fixtures/engines/**`. Only `engines/fake-acp/` exists; `fake-acp-engine`
  (`crates/ham-e2e`), the tests of `crates/ham-e2e` and the tests of `crates/agentd` read it.
  - `engines/fake-acp/transcripts/*.jsonl`: one JSON object per line,
    `{"direction": "client"|"agent", "message": <JSON-RPC 2.0 message>}`, in wire order; a string
    such as `{workspace}` stands for the launch value. `handshake.jsonl` is the initialize and session/new exchange; turn transcripts
    are added by the fake engine's owner (ham-e2e).
    `turn-1-percent-change.jsonl` is the first prompt (two edits, `end_turn`);
    `turn-2-permission-refused.jsonl` (recorded prompt `Install the requests package with pip, then
    read src/calc.py`): a pending
    `run_commands: pip install requests` call, an agent request session/request_permission (id 1001,
    options `allow_once` and `reject_once`), the client's recorded rejection (a `client` entry with no
    `method`, the wait point), a failed update for that call, a further `read_files: src/calc.py`
    call, a message and `end_turn`. Each turn file runs once per engine process: a
    session/prompt is answered from the turn whose recorded prompt text equals its text, else from
    the first unused turn in name order (agentd starts a fresh engine per invocation, so a turn
    is reached as the first prompt of a process).
  - `engines/fake-acp/transcripts/direct/mcp_*/` and `engines/fake-acp/transcripts/direct/mcp_fresh_done/`: the scenarios of the tool hook. Their tool calls are
    played by the fake engine's MCP client (`crates/ham-e2e/src/fake_mcp.rs`) and are never relayed as written: `mcp_call` (one call of `code_search`), `mcp_connect_none`,
    `mcp_connect_handshake_only` and `mcp_connect_full` (the same call after a `mcp-connect:` directive), `mcp_delete_settings` (the engine deletes its registration file after it connected),
    `mcp_two_calls_edit_settings` (two calls, the engine rewrites its file), and `mcp_stuck_then_fresh` with `mcp_fresh_done` (the loop-guard scenario of `stuck_engine_stop_then_done`
    with a call and a rewrite of the file in the first session, then a fresh session with a call of its own).
- `direct-probe/` and `containers/`: the scripts of the isolation probe, and the Testcontainers probe project of the in-session container
  smoke; each has its own `AGENTS.md`.
- `fanout-kata/oracle/`, `load-kata/oracle/` and `ts-lib/oracle/`: oracle tests with no repository beside them (pytest files in `fanout-kata`;
  pytest files, `cards.json` and a checker module in `load-kata`; vitest files in `ts-lib`). `infra/lib/stacks/delivery.ts` bundles each as a
  CDK asset: the PR-check build reads `oracle_fanout_kata`, `oracle_load_kata` and `oracle_ts_lib` as its secondary sources.
- Recorded data only: no secrets, session tokens, cookies, customer data or real credentials;
  transcripts carry placeholder IDs only. Synthetic entries carry `"synthetic": true` or say so in
  their child `AGENTS.md`.

- `replays/<name>/`: a trace measured on a real job, kept as data and read by `crates/agentd/tests/direct_goal_command_replay.rs`, which writes each row as a `/proc` for the real probe.

## Work Guidance

## Verification

```bash
cargo nextest run -p ham-e2e --test acp_replay --test fake_acp_engine --test fake_permission
cargo nextest run -p agentd --test direct_invocation
uv run --no-project --python 3.12 python -m unittest
```

## Child DOX Index

| Path | Covers |
| --- | --- |
| `acp/AGENTS.md` | Real-engine ACP transcripts: handshakes and synthetic turns |
| `python-calc/AGENTS.md` | Calculator fixture: repo, oracle tests, the CodeCommit PR-check buildspec and pins |
| `fanout-kata-24/AGENTS.md` | Sample project whose oracle files the delivery stack carries |
| `rules-clash/AGENTS.md` | Name-clash fixture: the `hamh-e2e-rules-clash` repo whose managed rule in `.cline` takes the name of the user's canary rule, PR-check buildspec and pins |
| `ntier/AGENTS.md` | See that file. |
| `direct-probe/AGENTS.md` | Isolation probe sources that the agentd sandbox tests read |
| `direct-input/AGENTS.md` | The golden files of the user correction path: the `needs-input` question, its acknowledgements, the answer, the `instruct` control, the run-list member and the summary members |
| `direct-messages/AGENTS.md` | The golden files of message control: the send request and answers, the messages page, the hand-out notice, the session report, the claim, seal and outcome calls and the stored message items |
| `lifecycle/AGENTS.md` | The live lifecycle validation: the baseline arm's rule files and the real output of the research judges that the driver's parser is tested on |
| `research/AGENTS.md` | Row DIRECT-RESEARCH: the prompt of both arms (the pinned is-plain-obj repository and commit, the `ham-research` steps, one `curl` and `limits`) and the record-directory contract the judges read (`record.json`, the stored research objects, the placement listing, the delivered trees) |
| `rules/AGENTS.md` | The earlier texts of the default rules file (`fixtures/rules/default-earlier/`, from git), whose hashes `crates/ham-core/assets/default-rules.history.json` lists; the tests of `ham rules init` read them |
