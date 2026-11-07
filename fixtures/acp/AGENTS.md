# fixtures/acp

## Purpose

ACP JSON-RPC transcripts that the in-process fake engine of `crates/ham-e2e` replays (`crates/ham-e2e/src/acp.rs`, tested by
`crates/ham-e2e/tests/acp_replay.rs`). The handshakes were recorded from Cline 3.0.65 and OpenCode 1.18.32. The turns are synthetic.
`acp_replay.rs` also scans every `.jsonl` file here for host paths and credentials.

## Ownership

Owned by `fixtures/AGENTS.md`; read by `crates/ham-e2e`.

## Local Contracts

- One JSON object per line. Line 1 is `{"header": {...}}` with `engine`, `version`, `recorded`
  and, for synthetic files, `"synthetic": true` and the placeholders used.
- Every other line is `{"dir": "client->agent" | "agent->client", "msg": <JSON-RPC message>}`.
- `<engine>-<version>-turn*.jsonl`: synthetic session/prompt turns; placeholders `<session>`,
  `<id>`, `<prompt>` and `<cwd>` are filled at replay time. Edits are ACP `diff` tool-call content
  with `<cwd>`-relative paths.
- `opencode-1.18.32-turn1-greenfield-<go|typescript|events>.jsonl`: one turn that writes a new Go module (`go.mod` and a `queue` package), a
  TypeScript package (`package.json`, `tsconfig.json`, a cart source file with its test, and a `compose.yaml` with PostgreSQL) or the Go events
  module with a `compose.yaml` of PostgreSQL and RabbitMQ into an empty worktree. No test replays these three files; `acp_replay.rs` only scans
  them.
- The Cline turn 1 file carries a plain completion claim.

## Work Guidance

## Verification

```bash
cargo nextest run -p ham-e2e --test acp_replay
```

## Child DOX Index

None.
