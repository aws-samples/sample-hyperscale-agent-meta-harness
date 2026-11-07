# crates/message-ledger

## Purpose

- This crate holds rules for user messages sent to running Cline.
- These rules move each message through its states.
- The rules are written once.
- control-api uses `admit`, `withdraw`, `list`, and `get`.
- control-api closes messages on cancel.
- worker-api uses session report, claim, seal, outcome, and the hand-out.
- run-session closes messages at finalize.
- `harness_protocol::messages` holds wire and item types.

## Ownership

- `crates/AGENTS.md` owns this crate.
- This is a library crate.
- It has no binary.
- It has no Lambda.
- It depends on harness-protocol and aws-sdk-dynamodb.
- It uses no other new third-party crate.

## Local Contracts

- `table` provides the ledger's table calls.
  - `Table::get` consistently reads one item.
  - `query` consistently reads items with a sort-key prefix.
  - `transact` applies all operations or none.
  - `Cond` defines transaction conditions: Exists, NotExists, Eq, And, Or.
  - `WriteOp::Put` puts an item.
  - `WriteOp::Update` updates an item with set and remove.
  - `check_transaction` checks DynamoDB transaction limits.
  - A transaction must contain 1 to 100 operations.
  - Two operations cannot target the same item.
  - String keys must stay within 2,048 and 1,024 bytes.
  - An item cannot exceed 400 KB.
  - An update cannot change a key attribute.
  - An update cannot be empty.
  - The table contract also includes `diff`.
  - `compile` builds DynamoDB expressions.
  - It converts a condition and an update into those expressions.
  - The expressions use placeholders `#a0` and `:v0`.

- `mem::MemTable` is the tests' table.
  - It checks every condition against the stored item.
  - It writes nothing unless all conditions hold.
  - It refuses everything that `check_transaction` refuses.
  - `before_next_transact` runs a competing writer.
  - That writer runs between a read and the write.
  - It also offers `fail_next`, `stats`, and `put_raw`.
  - Control-api, worker-api, and run-session use it.
  - The ham stand-in stage also uses it.

- `dynamo::DynamoTable` is the real table.
  - It uses one `TransactWriteItems` call.
  - `TableError::ConditionFailed` reports a cancelled transaction.
  - It includes the index of the first failed condition.
  - `Contended` reports a conflict or throttling.
  - Those failures are retried.
  - `Invalid` reports a validation error.
  - A Put needs `dynamodb:PutItem`.
  - An Update needs `dynamodb:UpdateItem`.
  - The first control item write is an Update.
  - `attribute_not_exists(PK)` guards that write.
  - worker-api and run-session need no `PutItem`.
  - run-session needs `Query` on `RUN#*` for a finalize, which reads the run's message items (statement `RunMessages`).
  - Only control-api creates a message item or an owner counter.
  - It creates those items with a Put.

- `Ledger<T, C>` applies message rules.
  - Every operation reads the run's `MSGCTL`, the control item.
  - It also reads the message items it will change.
  - It applies a rule to copies of those items.
  - It writes the copies in one transaction.
  - The transaction checks the control item's `version`.
  - It checks each message item's `version` and `state`.
  - An intervening write causes a condition to fail.
  - That failed transaction writes nothing.
  - The operation starts again from the read.
  - `LedgerError::Unavailable` ends retries after 8 attempts.
  - A withdrawal and a claim of one message cannot both win.
  - A late message and the seal cannot both win.

- Operations:
  - `admit` is idempotent by the client's message id.
  - `Refusal` values carry refusal HTTP statuses and codes.
  - `bad-input` returns 400.
  - `not-found` returns 404.
  - `run-sealed` returns 409.
  - `too-many-pending` returns 409.
  - `run-limit` returns 409.
  - `idempotency-conflict` returns 409.
  - `not-held` returns 409.
  - `epoch-changed` returns 409.
  - `confirm-mismatch` returns 409.
  - `rate-limited` returns 429.
  - The operations include `withdraw`, `get_message`, and `list`.
  - `notices` uses one read of one item.
  - The operations include `report_session`, `claim`, `seal`, and `outcome`.
  - `close` takes `CloseReason::Finalize` or `Cancel`.

- Admission and confirmation:
  - Admission gives a message the `pending` state.
  - It assigns the next `seq` and the current session.
  - At most 3 messages are outstanding.
  - Outstanding states are pending, held, and dispatching.
  - Confirming a held or delivery-unknown message sends a copy.
  - The same write supersedes the original.
  - That write frees the original's slot.

- Sessions and incarnations:
  - agentd reports `report_session(run, generation, epoch, number)`.
  - `generation` identifies an incarnation.
  - `number` is the session ordinal within that incarnation.
  - Session numbering starts from 1.
  - A lower number in the same incarnation is a late retry.
  - That retry changes nothing.
  - The same number with another session id gets `bad-transition`.
  - A higher number for the current session changes only the number.
  - A new session id makes old-session pending messages `held`.
  - The first session adopts messages sent before it.
  - An unseen incarnation replaces the current one, subject to clock skew.
  - `GENERATION_CLOCK_SKEW_MS` is the allowed clock skew: 60 s.
  - An unseen incarnation's ULID can exceed that margin behind the current one.
  - Such a report is a delayed first report.
  - Its incarnation was already replaced.
  - It gets `stale-generation`.
  - Within the margin, ULID order does not decide replacement.
  - A replacement microVM with a slow clock remains the new agentd.
  - `retired` lists replaced incarnations on the control item.
  - It holds at most 32 incarnations.
  - The oldest entry is dropped.
  - Listed incarnations get `stale-generation` from then on.
  - Their dispatching messages become `delivery-unknown`.

- Claim and attempt:
  - A claim moves pending messages to dispatching.
  - It claims for the current incarnation and session.
  - It returns interrupts in mode now first.
  - It then returns the oldest messages.
  - `attempt` numbers a message's claims.
  - The first claim has attempt 1.
  - Each release increases the next claim's attempt by one.
  - A repeated claim by the same incarnation returns its held messages.
  - `redelivered` marks those returned messages.
  - Their attempt stays the same.
  - This also applies when the session changed since the claim.
  - Claim does not filter by the message's original session.
  - agentd can always find what it holds.
  - A queued Cancel wins.
  - It rejects open messages as `run-cancelled`.

- Control item agreement:
  - The control item lists every open message.
  - Each entry includes the message's state and session.
  - A differing message makes a claim refuse to act.
  - It also blocks close, seal, outcome, withdrawal, and confirmation.
  - An unlisted open message blocks those operations too.
  - These refusals report the store as unavailable.
  - They write nothing.
  - The ledger reads the control item again after finding disagreement.
  - A changed version means a transition raced between the reads.
  - The ledger reads again in that case.
  - An unchanged version means the disagreement is real.

- Outcome:
  - The call is `outcome(run, generation, epoch, message_id, attempt, outcome)`.
  - The incarnation fences the outcome.
  - The attempt also fences the outcome.
  - The session id is informative.
  - An earlier attempt than the current claim gets `stale-attempt`.
  - That refusal returns 409.
  - It settles nothing.
  - Repeating an applied outcome returns an answer without a write.
  - This includes a release that ended `rejected` after closure.
  - Rejecting a pending message accepts attempt 0.
  - Attempt 0 means "reject it as it is".
  - It also accepts the attempt of that message's last claim.
  - Any other attempt is stale.
  - A release returns the message to pending in mode next.
  - A changed session since the claim instead sends it to held.
  - A run closed meanwhile instead sends it to `rejected`.
  - That rejection uses `run-cancelled`.

- Seal:
  - `normal` and `limit-reached` wait for agentd's dispatching messages.
  - They get `bad-transition` while agentd still has such messages.
  - This applies on open runs and cancelled runs.
  - The refusal names the message ids.
  - agentd posts the outcomes first.
  - Only the current incarnation is asked to wait.
  - Other callers on a closed run get the closed answer.
  - `terminal` does not wait.
  - A normal seal can return pending messages and leave admission open.
  - Or it rejects held messages and closes admission together.

- Close:
  - `close` creates the control item when none exists.
  - A later admitted message then finds admission closed.
  - Finalize settles messages from their items regardless of the control list.
  - It uses one consistent query of the run's messages.
  - That query covers at most 100 messages.
  - Finalize turns `dispatching` into `delivery-unknown`.
  - It rejects pending and held messages with `run-sealed`.
  - It never touches a delivered message.
  - It rebuilds the control item's list.
  - `Closed.repaired` marks a wrong list.
  - Cancel trusts the list.
  - It refuses when a listed message differs from its entry.
  - That refusal exposes the fault.
  - Cancel rejects pending and held messages with `run-cancelled`.
  - It leaves `dispatching` messages to agentd.
  - Closing an already closed run still settles open messages.
  - Finalize after cancel turns unanswered messages into `delivery-unknown`.
  - Close keeps the first reason the run closed for.
  - It writes nothing when nothing is open.
  - The current incarnation's terminal seal does the same on closed runs.

- Keys:
  - Message items use `RUN#<runId>` / `MSG#<messageId>`.
  - Control items use `RUN#<runId>` / `MSGCTL`.
  - Rate counters use `MSGRATE#<principalArn>` / the clock minute.
  - Each rate counter has a DynamoDB ttl.
  - The owner limit is 30 a minute across runs.

## Work Guidance

- Write the failing test first.
- Test races with `MemTable::before_next_transact`.
- It runs a rival request between a read and the write.
- The result does not depend on timing.

## Verification

- Run `cargo nextest run -p message-ledger`.
- Run `cargo clippy -p message-ledger --all-targets -- -D warnings`.

## Child DOX Index

None.
