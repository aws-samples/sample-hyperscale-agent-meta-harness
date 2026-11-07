//! DynamoDB and STS implementations of the handler traits. Items are the harness-protocol serde
//! types plus `PK`/`SK` from their key builders.

use crate::creds::MintPlan;
use crate::handler::{
    CallbackError, Callbacks, MintError, Minter, Store, StoreError, TokenContext,
};
use aws_sdk_dynamodb::types::{AttributeValue, PutRequest, WriteRequest};
use harness_protocol::api::internal::{PendingControl, ProcessCredentials};
use harness_protocol::common::Usage;
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::GoalStatus;
use harness_protocol::messages::{
    ClaimRequest, ClaimResponse, MessageNotice, OutcomeRequest, OutcomeResponse, SealRequest,
    SealResponse, SessionAck, SessionReport,
};
use harness_protocol::store::{AgentItem, ControlItem, EventItem, Keyed, TurnItem, keys};
use harness_protocol::transcript::TranscriptSummary;
use message_ledger::dynamo::DynamoTable;
use message_ledger::{Ledger, LedgerError};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct DynamoStore {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}

fn unavailable<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Unavailable(e.to_string())
}

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}

/// The attributes of an item as stored: its serde form plus `PK` and `SK` from its key builder.
fn put_attrs<T: serde::Serialize + Keyed>(
    item: &T,
) -> Result<HashMap<String, AttributeValue>, StoreError> {
    let key = item.key();
    let mut attrs: HashMap<String, AttributeValue> =
        serde_dynamo::to_item(item).map_err(unavailable)?;
    attrs.insert("PK".into(), s(key.pk));
    attrs.insert("SK".into(), s(key.sk));
    Ok(attrs)
}

/// Query pages read per `take_controls` call; an agent has at most a few controls per turn.
const MAX_CONTROL_PAGES: usize = 10;

/// A batch of this many events or fewer is written one event per request, in ascending id order. A `BatchWriteItem` of several
/// puts performs them in parallel, so a reader that polls while a batch is being committed can see a later id before an earlier one of the same
/// batch; it keeps the largest id it has read and never asks for the earlier one again. On a test stage, 371 of 2,875 agent events (12.9
/// percent) were committed after a larger id, and no agent shard was in id order. One event per request makes what a reader can see always a
/// prefix of the batch. Each request costs one round trip (about 6 ms inside the Region); 100 events take well under the function's 15 s. A
/// larger batch is a backlog after an outage (agentd buffers up to 2,000 events): it goes out in sorted `BatchWriteItem` chunks of 25, as
/// before, because 2,000 sequential requests would not fit the timeout and a retry would write the same prefix again.
pub const SEQUENTIAL_EVENTS_MAX: usize = 100;

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant); avoids a date crate for one timestamp.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

impl DynamoStore {
    /// The message ledger of the run's messages over this table.
    fn ledger(&self) -> Ledger<DynamoTable> {
        Ledger::new(DynamoTable::new(self.client.clone(), self.table.clone()))
    }

    /// BatchWriteItem puts of `items` with their keys, 25 at a time; items the service returns
    /// unprocessed (throttling) are retried with backoff.
    async fn batch_put<T: serde::Serialize + Keyed>(
        &self,
        items: &[T],
        what: &str,
    ) -> Result<(), StoreError> {
        for chunk in items.chunks(25) {
            let mut writes = Vec::with_capacity(chunk.len());
            for item in chunk {
                let put = PutRequest::builder()
                    .set_item(Some(put_attrs(item)?))
                    .build()
                    .map_err(unavailable)?;
                writes.push(WriteRequest::builder().put_request(put).build());
            }
            let mut pending = HashMap::from([(self.table.clone(), writes)]);
            for attempt in 0..5u32 {
                let out = self
                    .client
                    .batch_write_item()
                    .set_request_items(Some(pending))
                    .send()
                    .await
                    .map_err(unavailable)?;
                match out.unprocessed_items {
                    Some(left) if !left.is_empty() => {
                        pending = left;
                        tokio::time::sleep(std::time::Duration::from_millis(50 << attempt)).await;
                    }
                    _ => {
                        pending = HashMap::new();
                        break;
                    }
                }
            }
            if !pending.is_empty() {
                return Err(StoreError::Unavailable(format!(
                    "unprocessed {what} after retries"
                )));
            }
        }
        Ok(())
    }

    /// Each item in a `BatchWriteItem` request of its own, one at a time in the order given: an item is stored (a throttled one is retried,
    /// see [`batch_put`](Self::batch_put)) before the next one is sent, so a failure leaves the items before it stored and none after it.
    /// One put in a `BatchWriteItem` is committed on its own, like a `PutItem`; the role holds `dynamodb:BatchWriteItem` and not
    /// `dynamodb:PutItem` (infra/test/control.test.ts pins that). The caller sorts.
    async fn put_in_order<T: serde::Serialize + Keyed>(
        &self,
        items: &[T],
        what: &str,
    ) -> Result<(), StoreError> {
        for item in items {
            self.batch_put(std::slice::from_ref(item), what).await?;
        }
        Ok(())
    }

    /// True when this call set `deliveredAt`; false when another call already had.
    async fn mark_delivered(&self, c: &ControlItem) -> Result<bool, StoreError> {
        let key = c.key();
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(key.pk))
            .key("SK", s(key.sk))
            .update_expression("SET deliveredAt = :at")
            .condition_expression("attribute_exists(PK) AND attribute_not_exists(deliveredAt)")
            .expression_attribute_values(":at", s(now_rfc3339()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(false)
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    fn agent_key(run: &str, agent: &str) -> HashMap<String, AttributeValue> {
        HashMap::from([
            ("PK".into(), s(keys::run_pk(run))),
            ("SK".into(), s(keys::agent_sk(agent))),
        ])
    }
}

impl Store for DynamoStore {
    async fn get_turn(
        &self,
        run: &str,
        agent: &str,
        n: u32,
    ) -> Result<Option<TurnItem>, StoreError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run)))
            .key("SK", s(keys::turn_sk(agent, n)))
            .consistent_read(true)
            .send()
            .await
            .map_err(unavailable)?;
        match out.item {
            Some(item) => serde_dynamo::from_item(item).map(Some).map_err(unavailable),
            None => Ok(None),
        }
    }

    async fn mark_turn_ended(&self, run: &str, agent: &str, n: u32) -> Result<(), StoreError> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run)))
            .key("SK", s(keys::turn_sk(agent, n)))
            .update_expression("SET endedAt = :now")
            .condition_expression("attribute_exists(PK) AND attribute_not_exists(endedAt)")
            .expression_attribute_values(":now", s(now_rfc3339()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(())
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn get_agent(&self, run: &str, agent: &str) -> Result<Option<AgentItem>, StoreError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(Self::agent_key(run, agent)))
            .consistent_read(true)
            .send()
            .await
            .map_err(unavailable)?;
        match out.item {
            Some(item) => serde_dynamo::from_item(item).map(Some).map_err(unavailable),
            None => Ok(None),
        }
    }

    async fn get_goal_status(&self, run: &str) -> Result<Option<GoalStatus>, StoreError> {
        #[derive(serde::Deserialize)]
        struct Status {
            status: GoalStatus,
        }
        let key = HashMap::from([
            ("PK".into(), s(keys::run_pk(run))),
            ("SK".into(), s(keys::goal_sk(None))),
        ]);
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(key))
            .consistent_read(true)
            .projection_expression("#s")
            .expression_attribute_names("#s", "status")
            .send()
            .await
            .map_err(unavailable)?;
        match out.item {
            Some(item) => serde_dynamo::from_item::<_, Status>(item)
                .map(|x| Some(x.status))
                .map_err(unavailable),
            None => Ok(None),
        }
    }

    async fn rotate_nonce(
        &self,
        run: &str,
        agent: &str,
        old_sha256: &str,
        new_sha256: &str,
    ) -> Result<(), StoreError> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .set_key(Some(Self::agent_key(run, agent)))
            .update_expression("SET #c.#n = :new, #v = #v + :one")
            .condition_expression("#c.#n = :old AND #c.#r = :false")
            .expression_attribute_names("#c", "creds")
            .expression_attribute_names("#n", "nonceSha256")
            .expression_attribute_names("#r", "revoked")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":new", s(new_sha256))
            .expression_attribute_values(":old", s(old_sha256))
            .expression_attribute_values(":false", AttributeValue::Bool(false))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Err(StoreError::Conflict)
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn put_events(
        &self,
        _run: &str,
        _agent: &str,
        events: &[Envelope],
    ) -> Result<(), StoreError> {
        let mut items: Vec<EventItem> = events
            .iter()
            .map(|e| EventItem {
                envelope: e.clone(),
            })
            .collect();
        // Ascending ids whatever order the batch arrived in (agentd posts its buffer in append order, which is id order, but a requeued
        // batch is prepended to newer events and the sort does not depend on that).
        items.sort_by(|a, b| a.envelope.event_id.cmp(&b.envelope.event_id));
        if items.len() <= SEQUENTIAL_EVENTS_MAX {
            self.put_in_order(&items, "events").await
        } else {
            self.batch_put(&items, "events").await
        }
    }

    async fn record_heartbeat(&self, run: &str, agent: &str, _turn: u32) -> Result<(), StoreError> {
        self.client
            .update_item()
            .table_name(&self.table)
            .set_key(Some(Self::agent_key(run, agent)))
            .update_expression("SET heartbeatAt = :now")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":now", s(now_rfc3339()))
            .send()
            .await
            .map(|_| ())
            .map_err(unavailable)
    }

    async fn record_turn_complete(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
    ) -> Result<(), StoreError> {
        self.client
            .update_item()
            .table_name(&self.table)
            .set_key(Some(Self::agent_key(run, agent)))
            .update_expression("SET lastCompletedTurn = :t, heartbeatAt = :now")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":t", AttributeValue::N(turn.to_string()))
            .expression_attribute_values(":now", s(now_rfc3339()))
            .send()
            .await
            .map(|_| ())
            .map_err(unavailable)
    }

    async fn record_turn_usage(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
        usage: &Usage,
    ) -> Result<(), StoreError> {
        let u = serde_dynamo::to_attribute_value(usage)
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run)))
            .key("SK", s(keys::turn_sk(agent, turn)))
            .update_expression("SET #u = :u")
            .expression_attribute_names("#u", "usage")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":u", u)
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|x| x.is_conditional_check_failed_exception()) =>
            {
                Ok(())
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn record_turn_transcript(
        &self,
        run: &str,
        agent: &str,
        turn: u32,
        summary: &TranscriptSummary,
    ) -> Result<(), StoreError> {
        let t = serde_dynamo::to_attribute_value(summary)
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run)))
            .key("SK", s(keys::turn_sk(agent, turn)))
            .update_expression("SET #t = :t")
            .expression_attribute_names("#t", "transcript")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":t", t)
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|x| x.is_conditional_check_failed_exception()) =>
            {
                Ok(())
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn take_controls(
        &self,
        run: &str,
        agent: &str,
    ) -> Result<Vec<PendingControl>, StoreError> {
        let mut out = Vec::new();
        let mut start = None;
        for _ in 0..MAX_CONTROL_PAGES {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .consistent_read(true)
                .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
                .filter_expression("attribute_not_exists(deliveredAt)")
                .expression_attribute_values(":pk", s(keys::run_pk(run)))
                .expression_attribute_values(":sk", s(keys::control_prefix(agent)))
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            for item in page.items.unwrap_or_default() {
                // A malformed item is skipped so it cannot block the agent's later controls.
                let Ok(c) = serde_dynamo::from_item::<_, ControlItem>(item) else {
                    continue;
                };
                if self.mark_delivered(&c).await? {
                    out.push(c.control);
                }
            }
            match page.last_evaluated_key {
                Some(k) if !k.is_empty() => start = Some(k),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn message_notices(&self, run: &str) -> Result<Vec<MessageNotice>, LedgerError> {
        self.ledger().notices(run).await
    }

    async fn cancel_queued(&self, run: &str, agent: &str) -> Result<bool, StoreError> {
        let mut start = None;
        for _ in 0..MAX_CONTROL_PAGES {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .consistent_read(true)
                .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
                .filter_expression("attribute_not_exists(deliveredAt)")
                .expression_attribute_values(":pk", s(keys::run_pk(run)))
                .expression_attribute_values(":sk", s(keys::control_prefix(agent)))
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            for item in page.items.unwrap_or_default() {
                if let Ok(c) = serde_dynamo::from_item::<_, ControlItem>(item)
                    && c.control == PendingControl::Cancel
                {
                    return Ok(true);
                }
            }
            match page.last_evaluated_key {
                Some(k) if !k.is_empty() => start = Some(k),
                _ => break,
            }
        }
        Ok(false)
    }

    async fn message_session(
        &self,
        run: &str,
        report: &SessionReport,
    ) -> Result<SessionAck, LedgerError> {
        self.ledger()
            .report_session(run, &report.generation, &report.epoch, report.number)
            .await
    }

    async fn message_claim(
        &self,
        req: &ClaimRequest,
        cancel_queued: bool,
    ) -> Result<ClaimResponse, LedgerError> {
        self.ledger()
            .claim(
                &req.run_id,
                &req.generation,
                &req.epoch,
                req.message_id.as_deref(),
                req.limit.unwrap_or(1),
                cancel_queued,
            )
            .await
    }

    async fn message_seal(&self, req: &SealRequest) -> Result<SealResponse, LedgerError> {
        self.ledger()
            .seal(&req.run_id, &req.generation, &req.epoch, req.mode)
            .await
    }

    async fn message_outcome(&self, req: &OutcomeRequest) -> Result<OutcomeResponse, LedgerError> {
        self.ledger()
            .outcome(
                &req.run_id,
                &req.generation,
                &req.epoch,
                &req.message_id,
                req.attempt,
                &req.outcome,
            )
            .await
    }
}

#[derive(Clone, Debug)]
pub struct StsMinter {
    pub client: aws_sdk_sts::Client,
    /// ARN of `harness-worker-session`.
    pub role_arn: String,
}

impl Minter for StsMinter {
    async fn mint(&self, plan: &MintPlan) -> Result<ProcessCredentials, MintError> {
        let mut req = self
            .client
            .assume_role()
            .role_arn(&self.role_arn)
            .role_session_name(&plan.role_session_name)
            .policy(&plan.session_policy)
            .duration_seconds(plan.duration_seconds);
        // Empty: the caller's session already carries a source identity, which STS keeps fixed
        // across role chaining (the operator path; the dispatcher and worker-api roles have none).
        if !plan.source_identity.is_empty() {
            req = req.source_identity(&plan.source_identity);
        }
        for (k, v) in &plan.tags {
            let tag = aws_sdk_sts::types::Tag::builder()
                .key(k)
                .value(v)
                .build()
                .map_err(|e| MintError(e.to_string()))?;
            req = req.tags(tag);
        }
        // The full error chain names the STS code and message ("service error" alone does not).
        let out = req
            .send()
            .await
            .map_err(|e| MintError(aws_sdk_sts::error::DisplayErrorContext(&e).to_string()))?;
        let c = out
            .credentials
            .ok_or_else(|| MintError("AssumeRole returned no credentials".into()))?;
        Ok(ProcessCredentials {
            version: 1,
            access_key_id: c.access_key_id,
            secret_access_key: c.secret_access_key,
            session_token: c.session_token,
            expiration: c
                .expiration
                .fmt(aws_sdk_sts::primitives::DateTimeFormat::DateTime)
                .map_err(|e| MintError(e.to_string()))?,
        })
    }
}

/// KMS and Step Functions for dispatched turns. The dispatcher encrypts each task token with the
/// data key under the context `{run, agent, turn}`; a ciphertext bound to another turn does not
/// decrypt, so a worker session can only complete its own turn.
#[derive(Clone, Debug)]
pub struct SfnCallbacks {
    pub kms: aws_sdk_kms::Client,
    pub sfn: aws_sdk_sfn::Client,
}

fn context_error<E: std::error::Error>(e: E) -> String {
    aws_sdk_kms::error::DisplayErrorContext(e).to_string()
}

impl SfnCallbacks {
    async fn token(&self, ciphertext: &str, ctx: &TokenContext) -> Result<String, CallbackError> {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD
            .decode(ciphertext)
            .map_err(|_| CallbackError::Closed("the task token ciphertext is not base64".into()))?;
        let out = self
            .kms
            .decrypt()
            .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(blob))
            .encryption_context("run", &ctx.run_id)
            .encryption_context("agent", &ctx.agent_id)
            .encryption_context("turn", ctx.turn.to_string())
            .send()
            .await;
        match out {
            Ok(o) => {
                let plain = o.plaintext.map(|b| b.into_inner()).unwrap_or_default();
                String::from_utf8(plain)
                    .map_err(|_| CallbackError::Closed("the task token is not UTF-8".into()))
            }
            Err(e)
                if e.as_service_error().is_some_and(|se| {
                    se.is_invalid_ciphertext_exception() || se.is_incorrect_key_exception()
                }) =>
            {
                Err(CallbackError::Closed(context_error(e)))
            }
            Err(e) => Err(CallbackError::Unavailable(context_error(e))),
        }
    }
}

/// TaskTimedOut, TaskDoesNotExist and InvalidToken mean the state is gone.
fn closed_or_unavailable(code: Option<&str>, text: String) -> CallbackError {
    match code {
        Some("TaskTimedOut" | "TaskDoesNotExist" | "InvalidToken") => CallbackError::Closed(text),
        _ => CallbackError::Unavailable(text),
    }
}

impl Callbacks for SfnCallbacks {
    async fn turn_done(
        &self,
        ciphertext: &str,
        ctx: &TokenContext,
        output: &str,
    ) -> Result<(), CallbackError> {
        use aws_sdk_sfn::error::ProvideErrorMetadata as _;
        let token = self.token(ciphertext, ctx).await?;
        self.sfn
            .send_task_success()
            .task_token(token)
            .output(output)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| {
                let code = e.code().map(str::to_string);
                closed_or_unavailable(code.as_deref(), context_error(e))
            })
    }

    async fn turn_alive(&self, ciphertext: &str, ctx: &TokenContext) -> Result<(), CallbackError> {
        use aws_sdk_sfn::error::ProvideErrorMetadata as _;
        let token = self.token(ciphertext, ctx).await?;
        self.sfn
            .send_task_heartbeat()
            .task_token(token)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| {
                let code = e.code().map(str::to_string);
                closed_or_unavailable(code.as_deref(), context_error(e))
            })
    }
}
