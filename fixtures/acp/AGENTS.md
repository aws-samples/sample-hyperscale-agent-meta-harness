# fixtures/acp

## Purpose

ACP JSON-RPC transcripts that the in-process fake engine of `crates/ham-e2e` replays (`crates/ham-e2e/src/acp.rs`, tested by
`crates/ham-e2e/tests/acp_replay.rs`). The Cline turns are synthetic; the OpenCode turns are recorded. `acp_replay.rs` also scans every `.jsonl`
file here for host paths and credentials.

## Ownership

Owned by `fixtures/AGENTS.md`; read by `crates/ham-e2e`.

## Local Contracts

- One JSON object per line. Line 1 is `{"header": {...}}` with `engine`, `version`, `recorded`
  and, for synthetic files, `"synthetic": true` and the placeholders used.
- Every other line is `{"dir": "client->agent" | "agent->client", "msg": <JSON-RPC message>}`.
- `cline-<version>-turn*.jsonl`: synthetic session/prompt turns; placeholders `<session>`,
  `<id>`, `<prompt>` and `<cwd>` are filled at replay time. Edits are ACP `diff` tool-call content
  with `<cwd>`-relative paths.
- The Cline turn 1 file carries a plain completion claim.

## Work Guidance

## Verification

```bash
cargo nextest run -p ham-e2e --test acp_replay
```

## Child DOX Index

None.
