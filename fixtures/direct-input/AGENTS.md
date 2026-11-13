# fixtures/direct-input

## Purpose

The golden files of the user correction path of a direct run: the JSON that agentd writes when Cline needs the user,
that control-api stores and `ham` reads, and that the answer carries back. One writer and every reader test against the same bytes.

## Ownership

Owned by `fixtures/AGENTS.md`; the types are in `crates/harness-protocol/src/direct_input.rs`.

## Local Contracts

- `needs-input-*.json` are `turn.progress` events of kind `needs-input` (the engine stop question, the replay-rejected question and the non-blocking outage notice); `input-received*.json`,
  `input-ignored.json`, `input-closed.json` and `new-session-*.json` are the acknowledgement and closing events; `control-instruct.json` is the stored `ControlItem` of an answer;
  `answer-request.json` is the `POST /v1/runs/{runId}/input` body; `list-needs-input.json` is a run list row with `needsInput`; `summary-interventions.json` holds the new `summary.json`
  members; `a15-error-text.txt` is the exact provider error that must classify as `replay_rejected`.
- The files parse into their types and write back the same JSON (`crates/harness-protocol/tests/wire_direct_input.rs`); agentd's builders must produce them (compared as JSON values), control-api
  and `ham` parse them. The ids and times are examples, not data of a real run. No text a user typed appears here.
- A change to a file here is a change of the contract: bump the protocol version and tell every package that reads it.

## Work Guidance

## Verification

`cargo nextest run -p harness-protocol --test wire_direct_input`

## Child DOX Index

None.
