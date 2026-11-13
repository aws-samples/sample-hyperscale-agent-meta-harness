# crates/worker-api

## Purpose

The Lambda behind the private `/internal` API:
`/internal/creds` re-mints `harness-worker-session` credentials after the role-session, runtime
session, goal and nonce checks; `/internal/heartbeat`, `/internal/turn-complete` and
`/internal/events` record worker progress.

## Ownership

Owned by `crates/AGENTS.md`. Deployed by the control stack (`infra/lib/stacks/control.ts`) as
`hamh-<stage>-worker-api`, arm64, built with `cargo lambda build --release --arm64`.

## Local Contracts

- The caller is taken from the API Gateway IAM identity (`requestContext.identity.userArn`), never
  from the body; the body's run and agent must equal the caller's `<run>.<agent>` session name.
- Re-mint only for an unrevoked, bound agent whose `runtimeSessionId` matches, whose root goal is
  active and whose current nonce matches; the nonce rotates by a conditional write, so a replayed
  nonce fails. Tags, source identity, session policy and the 3600 s duration are reproduced.
- Denials return 403 with an `ApiError` that never echoes a nonce or credential.
- `/internal/events` writes the batch in ascending event-id order, one event per `BatchWriteItem` request, each stored before the next is sent:
  a `BatchWriteItem` of several puts performs them in parallel, so a
  reader could see a later id before an earlier one of the same batch and, keeping the largest id it has read, never return to it.
  What a reader can see is now always a prefix of the batch, and a
  failure stops the batch (agentd requeues all of it; the puts are idempotent). The role holds `dynamodb:BatchWriteItem` and not
  `dynamodb:PutItem` (`infra/test/control.test.ts` pins it), which is why the single put is a one-item batch. A batch of more than 100
  events is a backlog after an outage (agentd buffers up to 2,000): it goes out in sorted chunks of 25, as before. `tests/aws_adapters.rs`.
  After storing, it returns the agent's undelivered `CTRL#<agent>#` items;
  a
  batch with `takeControls: false` (a turn's final flush) takes none, and a control read failure
  leaves them queued and is logged (`handle_logged` diagnostics, one JSON line each). The role
  needs `dynamodb:Query` for it.
- Turn output: a turn-complete whose `bundleUri` is not exactly agentd's key for the caller and this
  turn, `s3://<HARNESS_BUCKET>/runs/<run>/agents/<agent>/turns/<turn>/bundle`, or that carries any `snapshotUri` (no
  producer writes one), is denied 403 `foreign_bundle` before anything is recorded or sent to Step Functions. The engine
  holds the session's credentials and can post turn-complete itself; a bundle at another key would become the run's head
  and reach delivery. A turn with no bundle passes. `HARNESS_BUCKET` is the stage's harness bucket
  (`infra/lib/stacks/control.ts`); an empty or missing value stops the Lambda at start.
- Completion key: the engine holds the session credentials and could post `/internal/events` and `/internal/turn-complete` itself, with the accepted bundle key after planting a bundle there. Both routes therefore need the header `x-hamh-completion-key` (`harness_protocol::store::COMPLETION_KEY_HEADER`, read by `lambda::to_internal` into `InternalRequest.completion_key`). The key is `CredsBinding::completion_key(first nonce)`: run-session and agentd derive it, the Agent item stores only `CredsBinding.completion_key_sha256`, and `handler::completion_guard` compares them (`CredsBinding::check_completion_key`, a comparison that does not stop at the first different byte) for the run and agent of the caller's IAM session, before the body is parsed, an event is written, a turn is recorded or `SendTaskSuccess` is called. A missing or wrong key, and a stored hash that is not a hash, are 403 `completion_key`. An item that names no hash (written before this version) and a run with no Agent item leave the route as it was. The heartbeat, the credential refresh and the message routes do not read the key. The key does not authenticate the objects agentd uploads: the engine's credentials can still overwrite them before delivery reads them. Tests: `tests/handler.rs` (`b593_*`), `crates/harness-protocol/tests/b593_completion_key.rs`.
- Output pins: `completion_guard` answers whether the call proved the run's key for an Agent item whose `creds.outputPinsRequired` is true. `turn_complete` keeps `TurnCompleteRequest.outputPins` only then, after
  `OutputPins::validate`; a member that is not well formed is 400 `bad_output_pins` and changes nothing, and a member on any other call (a run that does not require it, an item with no key) is dropped before `SendTaskSuccess`, so the state
  never holds a record the engine could have written. A required run's call with no `outputPins` is accepted (agentd stored no object); `plan-delivery` reads that as a session with no record. `tests/handler.rs` (`b759_*`).
- Dispatched turns: when `TURN#<agent>#<n>` carries a task-token ciphertext and no
  `endedAt`, `/internal/turn-complete` decrypts it with KMS under the context `{run, agent, turn}`
  of the caller and calls `SendTaskSuccess` with the request as output (telemetry, no verdict),
  then sets `endedAt`; a repeat sends nothing. A closed task (TaskTimedOut, TaskDoesNotExist,
  InvalidToken, a ciphertext bound to another turn) is accepted and logged; any other callback
  failure is 503 and leaves the turn open. `/internal/heartbeat` sends `SendTaskHeartbeat` for the
  open turn and never fails on it. Without a token both only record.
- Usage: a turn-complete that carries the engine-reported `usage` sets it on
  `TURN#<agent>#<n>` (`SET #u = :u` with `attribute_exists(PK)`; a turn without an item is left alone) after recording the turn as complete on the Agent item
  and before the callback; a failed write is a 503, so agentd's retry records it. control-api prices these for
  `GET /v1/runs/{runId}`.
- Store, STS, KMS and Step Functions sit behind traits; tests use fakes and never call AWS.
- The direct agent: agent `direct`, turn 1 (`tests/handler.rs`, the `the_direct_*` tests). Heartbeats and the turn result use the
  ciphertext the Turn item holds at that moment, so a retried `waitForTaskToken` state, whose `start-session` replaced the ciphertext with the
  newest token, is completed by `SendTaskSuccess` on the newest token; the output is the whole `TurnCompleteRequest` (agentd's summary is a preview of at
  most 32 KiB of JSON text, and the handler cuts a longer one the same way, so the state stays far inside the 256 KiB limit); a `bundleUri` other than `s3://<bucket>/runs/<run>/agents/direct/turns/1/bundle` and any
  `snapshotUri` are refused (403); a token Step Functions already closed is logged and the turn is still recorded as ended.
- Message control: Four more POST routes use flat names. The private API has one resource per route.
  `/internal/message-session` takes incarnation and session. `/internal/message-claim` claims pending messages. `/internal/message-seal` returns pending messages or closes admission. `/internal/message-outcome` reports message outcomes. These routes belong to the direct agent.
  The body's run and agent must match the caller's `<run>.<agent>` session. A mismatch returns 403. The agent must be `direct`; otherwise, return 403 `not_direct_agent`. An unknown member returns 400.
  `message-ledger`, the rules crate, supplies the rules. The `Store` trait gains `message_notices`, `cancel_queued`, `message_session`, `message_claim`, `message_seal` and `message_outcome`. Defaults are no notices, no Cancel, and "unavailable". `DynamoStore` implements these over `Ledger<DynamoTable>`.
  The Cancel check queries the agent's undelivered `CTRL#` items for a `cancel`. No IAM change is needed. GetItem, UpdateItem and Query already cover this. worker-api holds no PutItem. agentd's side can change a message's state. It cannot make a message.
  `message-session` takes `generation`, agentd's incarnation. This incarnation is a ULID made at startup. It also takes `epoch`, the engine's session id. It takes `number`, the session's ordinal within the incarnation. Numbers start at 1. A missing number or 0 returns 400. A lower number than this incarnation's last number is a late retry. That retry changes nothing. The same number with another session id returns 409 `bad-transition`. Messages pending in an older session become `held`. An unseen incarnation replaces the current incarnation unless its ULID is more than 60 seconds older. Within 60 seconds, ULID order does not decide the replacement. The control item's `retired` list records the replaced incarnation. That incarnation gets 409 `stale-generation` from then on. Messages it was dispatching become `delivery-unknown`.
  `message-claim` moves pending messages to `dispatching`. It uses one conditional write for each message. It claims interrupts first, then the oldest messages, at most 3. It returns each message's text and `attempt`, the claim number. The first claim of a message has attempt 1. Each release increases the next claim's attempt by one. The same incarnation's repeated claim returns its existing messages. Those messages have `redelivered: true` and the same attempt. A claim refuses to act with 503 when control and message items disagree.
  A queued run-level Cancel wins. The answer is `refused` `cancel-queued`. Open messages end `rejected` with `run-cancelled`. `message-seal` returns pending messages while admission stays open. Otherwise, it closes admission in the write that found nothing pending. A `normal` or `limit-reached` seal returns 409 `bad-transition` while agentd has a message `dispatching`. agentd posts the outcomes first. A `terminal` seal does not wait. `message-outcome` takes `messageId`, `attempt` and `outcome`, the claim result. The attempt identifies the claim being answered. Attempt 0 is allowed only for `rejected` messages agentd never claimed. Outcomes are `submitted` (turn), `failed`, `released` or `rejected`. Rejection allows only `engine-busy` and `continuation-limit`. The incarnation and attempt fence an outcome, not the session id. An earlier attempt returns 409 `stale-attempt` and settles nothing.
  No reported session returns 409 `no-session`. A retired or unknown incarnation returns 409 `stale-generation`. A claim or seal from another session returns 409 `stale-epoch`. That session differs from the current session. agentd stops acting on messages after `no-session` or `stale-generation`. It drops the call after `stale-attempt`.
  A refusal keeps its status and code. A store error returns 503 `store_unavailable`. Its reason goes only to the log. The access line never carries message text.
  `/internal/events` appends `PendingControl::Message(MessageNotice)`, a notice without text, to `pending`. It does this for every message pending in the current session. Interrupts come first. A final flush with `takeControls: false` skips these notices. An agent other than `direct` also skips them.
  The hand-out reads one item. It changes nothing. It repeats until the message is claimed. A failed read produces a diagnostic. The post still succeeds. Withdrawn, held, claimed or settled messages are never handed out.
- Stored conversation summary: a turn-complete can carry `transcript`, the worker's conversation summary. Its value is `TranscriptSummary {lastTranscriptSeq, entries}`. `TranscriptSummary::validate` checks the summary. `Store::record_turn_transcript` stores valid summaries on `TURN#<agent>#<n>`. The DynamoDB UpdateItem uses `SET #t = :t`. Its names are `{"#t": "transcript"}`. Its condition is `attribute_exists(PK)`. A failed condition is not an error. A turn without an item is left alone. The stored value holds `lastTranscriptSeq` and `entries`. The summary is the worker's report. A process the engine can reach writes it. The stored state means "the worker reports no loss". This does not prove that no loss occurred. Invalid summaries and failed writes are logged. Neither fails turn-complete or the callback to Step Functions. The transcript is telemetry. `tests/handler.rs` has 4 tests. They cover storing a summary and ignoring an invalid summary. They also cover store failure leaving the turn successful. The remaining test checks that an absent summary writes nothing. `tests/aws_adapters.rs` tests the UpdateItem body.
- Generation fence: the heartbeat and `turn-complete` carry the `turn` of their session, which is the turn the Agent item's `sessionGeneration` names while that session is the one the run is in. A call for any other turn of the direct agent is answered `409 stale-generation` before it records anything, so a session that was thought lost cannot write over the one that replaced it. An item with no generation lets every call through, as before. Tests: `tests/handler.rs`.

## Work Guidance

## Verification

```bash
cargo test -p worker-api
```

## Child DOX Index

None.
