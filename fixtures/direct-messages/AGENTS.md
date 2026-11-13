# fixtures/direct-messages

## Purpose

These golden files cover message control. The JSON covers user
messages to a running Cline. control-api, worker-api, agentd and
ham write and read this JSON. One writer and every reader test
against the same bytes.

## Ownership

`fixtures/AGENTS.md` owns this directory.
The types are in `crates/harness-protocol/src/messages.rs`.
The items are in `crates/harness-protocol/src/store.rs`.

## Local Contracts

- `send-request*.json` and `send-response*.json` hold POST /v1/runs/{runId}/messages bodies.
  These cover a plain request and confirmation of a held message.
  They include the 202 answer and the 200 answer of a retry.
- `message-*.json` holds single-message answers: delivered, rejected, held.
- `messages-page.json` holds the GET /v1/runs/{runId}/messages answer.
- `notice.json` and `pending-control-message.json` hold the hand-out to agentd.
- `session-*.json`, `claim-*.json`, `seal-*.json` and `outcome-*.json` cover four internal calls.
  They include the answers to those calls.
- `item-*.json` holds stored items.
  These include a message, the control item, and the owner's rate counter.

The files parse into their types.
They write back the same JSON byte for byte.
`crates/harness-protocol/tests/wire_messages.rs` tests this.

The ids, session names and times are examples.
They are not data from a real run.
No text of a real user appears here.

A file change here changes the contract.
Bump the protocol version.
Tell every package that reads it.

## Work Guidance

## Verification

`cargo nextest run -p harness-protocol --test wire_messages`

## Child DOX Index

None.
