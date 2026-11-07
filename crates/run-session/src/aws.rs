//! DynamoDB, KMS, S3, AgentCore and CodeArtifact implementations of [`crate::ports`].
//!
//! The adapters are `DynamoStore`, `KmsTokens`, `S3Blobs`, `AgentCoreSessions` and `CodeArtifactTokens`, trimmed to what
//! the six ops call. Items are the harness-protocol serde types plus `PK` and `SK` from their key builders.

use crate::error::RunSessionError;
use crate::ports::{
    Blobs, ObjectInfo, ObjectPage, PackageTokens, Res, SearchWrite, Sessions, Store, TagMarks,
    Tokens, VocabRead,
};
use aws_sdk_dynamodb::error::DisplayErrorContext;
use aws_sdk_dynamodb::types::{AttributeValue, Put, Select, TransactWriteItem, Update};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use dispatcher::endpoints::{EndpointState, Pin};
use ham_core::run_index::{owner_pk, search_sk};
use ham_core::runtag::dynamo::{self, Answered, Av, UpdateSpec};
use ham_core::runtag::edit::TagState;
use ham_core::runtag::vocab::VocabItem;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{AgentItem, EventItem, GoalItem, Keyed, RunItem, TurnItem, keys};
use message_ledger::dynamo::DynamoTable;
use message_ledger::{CloseReason, Ledger};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use worker_api::handler::TokenContext;

type Item = HashMap<String, AttributeValue>;

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}

fn context<E: std::error::Error>(e: E) -> String {
    DisplayErrorContext(e).to_string()
}

fn unavailable(m: impl Into<String>) -> RunSessionError {
    RunSessionError::Unavailable(m.into())
}

fn item_of<T: Serialize + Keyed>(t: &T) -> Res<Item> {
    let key = t.key();
    let mut item: Item = serde_dynamo::to_item(t).map_err(|e| unavailable(e.to_string()))?;
    item.insert("PK".into(), s(key.pk));
    item.insert("SK".into(), s(key.sk));
    Ok(item)
}

fn status_word(x: GoalStatus) -> AttributeValue {
    s(serde_json::to_value(x)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default())
}

fn phase_word(x: GoalPhase) -> AttributeValue {
    s(serde_json::to_value(x)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default())
}

fn conditional_failed(
    e: &aws_sdk_dynamodb::error::SdkError<
        aws_sdk_dynamodb::operation::update_item::UpdateItemError,
    >,
) -> bool {
    e.as_service_error()
        .is_some_and(|se| se.is_conditional_check_failed_exception())
}

fn put_conditional_failed(
    e: &aws_sdk_dynamodb::error::SdkError<aws_sdk_dynamodb::operation::put_item::PutItemError>,
) -> bool {
    e.as_service_error()
        .is_some_and(|se| se.is_conditional_check_failed_exception())
}

fn transaction_conflict(
    e: &aws_sdk_dynamodb::error::SdkError<
        aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError,
    >,
) -> bool {
    e.as_service_error().is_some_and(|se| match se {
        aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError::TransactionCanceledException(t) => t
            .cancellation_reasons()
            .iter()
            .any(|r| r.code() == Some("ConditionalCheckFailed")),
        _ => false,
    })
}

/// A value of a tag writer's spec as DynamoDB takes it: text, a number, a list of text, or a set of text.
fn av(v: &Av) -> AttributeValue {
    match v {
        Av::S(text) => AttributeValue::S(text.clone()),
        Av::N(n) => AttributeValue::N(n.to_string()),
        Av::L(items) => {
            AttributeValue::L(items.iter().map(|t| AttributeValue::S(t.clone())).collect())
        }
        Av::Ss(items) => AttributeValue::Ss(items.clone()),
    }
}

/// A list of tags as the item holds it: a list of text, all of it or nothing (one entry of another type makes the list unreadable, and
/// the tag lists that are not removals only hide a tag when they are lost), or a set of text. Any other type is no list.
fn tag_list(v: Option<&AttributeValue>) -> Vec<String> {
    match v {
        Some(AttributeValue::L(items)) => items
            .iter()
            .map(|i| i.as_s().ok().cloned())
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default(),
        Some(AttributeValue::Ss(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// The tags the person removed, read for what can be read: the person's removal wins on every read, so a list that is only partly readable
/// keeps the entries it can read, and a lone text is a list of one. Only a value that is no text at all is no removal.
fn removed_list(v: Option<&AttributeValue>) -> Vec<String> {
    match v {
        Some(AttributeValue::L(items)) => items
            .iter()
            .filter_map(|i| i.as_s().ok().cloned())
            .collect(),
        Some(AttributeValue::Ss(items)) => items.clone(),
        Some(AttributeValue::S(text)) => vec![text.clone()],
        _ => Vec::new(),
    }
}

/// What the vocabulary read decodes an item of the window into: the three projected attributes and nothing else. It is deliberately not
/// the run-index item, which needs a run id and a creation time that the projection does not return.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct VocabRow {
    run_tags: Vec<String>,
    run_tags_user: Vec<String>,
    run_tags_off: Vec<String>,
}

/// The key of the item that counts the stage's active Instances sessions: a string set `runs` of run ids.
const INSTANCES_SLOTS_PK: &str = "STAGE#instances";
const INSTANCES_SLOTS_SK: &str = "ACTIVE";

#[derive(Clone, Debug)]
pub struct DynamoStore {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
    /// The research route's table; `None` writes no policy row.
    pub research_table: Option<String>,
}

impl DynamoStore {
    async fn get_item<T: DeserializeOwned>(&self, pk: String, sk: String) -> Res<Option<T>> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(pk))
            .key("SK", s(sk))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| unavailable(context(e)))?;
        match out.item {
            Some(item) => serde_dynamo::from_item(item)
                .map(Some)
                .map_err(|e| unavailable(e.to_string())),
            None => Ok(None),
        }
    }

    async fn put(&self, item: Item, condition: Option<&str>) -> Res<bool> {
        let res = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .set_condition_expression(condition.map(str::to_string))
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
            Err(e) => Err(unavailable(context(e))),
        }
    }

    fn versioned_update(
        &self,
        sk: &str,
        run_id: &str,
        status: GoalStatus,
        version: u64,
        aborted: Option<AttributeValue>,
    ) -> Res<TransactWriteItem> {
        let mut expr = "SET #status = :status, #v = :next".to_string();
        let mut update = Update::builder()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(sk))
            .condition_expression("#v = :v")
            .expression_attribute_names("#v", "version")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":status", status_word(status))
            .expression_attribute_values(":next", AttributeValue::N(version.to_string()))
            .expression_attribute_values(
                ":v",
                AttributeValue::N(version.saturating_sub(1).to_string()),
            );
        if let Some(a) = aborted {
            expr.push_str(", #aborted = :aborted");
            update = update
                .expression_attribute_names("#aborted", "aborted")
                .expression_attribute_values(":aborted", a);
        }
        let update = update
            .update_expression(expr)
            .build()
            .map_err(|e| unavailable(context(e)))?;
        Ok(TransactWriteItem::builder().update(update).build())
    }

    /// One `UpdateItem` of a tag writer on the run's search item, exactly as the spec of `ham_core::runtag::dynamo` says. `false` when the
    /// condition failed.
    async fn send_tag_update(&self, run: &RunItem, spec: &UpdateSpec) -> Res<bool> {
        let mut update = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(&run.owner_principal_arn)))
            .key("SK", s(search_sk(&run.created_at, &run.run_id)))
            .update_expression(&spec.expression);
        if let Some(condition) = &spec.condition {
            update = update.condition_expression(condition);
        }
        for (alias, name) in &spec.names {
            update = update.expression_attribute_names(alias, name);
        }
        for (alias, value) in &spec.values {
            update = update.expression_attribute_values(alias, av(value));
        }
        match update.send().await {
            Ok(_) => Ok(true),
            Err(e) if conditional_failed(&e) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    /// A consistent read of some tag attributes of the search item at `sk` in `owner`'s partition (aliased names, so no attribute is ever
    /// read as a reserved word).
    async fn tag_attributes(&self, owner: &str, sk: String, names: &[&str]) -> Res<Option<Item>> {
        let mut get = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(owner)))
            .key("SK", s(sk))
            .consistent_read(true);
        let mut projection = Vec::new();
        for name in names {
            let alias = format!("#{name}");
            get = get.expression_attribute_names(&alias, *name);
            projection.push(alias);
        }
        let out = get
            .projection_expression(projection.join(", "))
            .send()
            .await
            .map_err(|e| unavailable(context(e)))?;
        Ok(out.item)
    }

    fn event_put(&self, event: &Envelope) -> Res<TransactWriteItem> {
        let put = Put::builder()
            .table_name(&self.table)
            .set_item(Some(item_of(&EventItem {
                envelope: event.clone(),
            })?))
            .condition_expression("attribute_not_exists(PK)")
            .build()
            .map_err(|e| unavailable(context(e)))?;
        Ok(TransactWriteItem::builder().put(put).build())
    }
}

impl Store for DynamoStore {
    async fn get_run(&self, run_id: &str) -> Res<Option<RunItem>> {
        self.get_item(keys::run_pk(run_id), keys::META.into()).await
    }

    async fn get_root_goal(&self, run_id: &str) -> Res<Option<GoalItem>> {
        self.get_item(keys::run_pk(run_id), keys::goal_sk(None))
            .await
    }

    async fn run_created_request_sha256(&self, run_id: &str) -> Res<Option<String>> {
        // Harness events are on shard 0 and sort by their ULID; `run.created` is the run's first.
        let page = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk")
            .expression_attribute_values(":pk", s(keys::event_pk(run_id, 0)))
            .consistent_read(true)
            .limit(25)
            .send()
            .await
            .map_err(|e| unavailable(context(e)))?;
        for item in page.items.unwrap_or_default() {
            let Ok(e) = serde_dynamo::from_item::<_, EventItem>(item) else {
                continue;
            };
            if e.envelope.event_type != EventType::RunCreated {
                continue;
            }
            if let Payload::Inline { value } = &e.envelope.payload {
                return Ok(value
                    .get("requestSha256")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string));
            }
        }
        Ok(None)
    }

    async fn delivery_pr_url(&self, run_id: &str) -> Res<Option<String>> {
        // Harness events are on shard 0 and sort by their ULID; `delivery.pr` is written near the end of the run, so the newest come first.
        let mut start: Option<HashMap<String, AttributeValue>> = None;
        loop {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk")
                .expression_attribute_values(":pk", s(keys::event_pk(run_id, 0)))
                .consistent_read(true)
                .scan_index_forward(false)
                .limit(50)
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(|e| unavailable(context(e)))?;
            for item in page.items.unwrap_or_default() {
                let Ok(e) = serde_dynamo::from_item::<_, EventItem>(item) else {
                    continue;
                };
                if e.envelope.event_type != EventType::DeliveryPr {
                    continue;
                }
                if let Payload::Inline { value } = &e.envelope.payload
                    && let Some(url) = value.get("url").and_then(serde_json::Value::as_str)
                {
                    return Ok(Some(url.to_string()));
                }
            }
            match page.last_evaluated_key {
                Some(key) => start = Some(key),
                None => return Ok(None),
            }
        }
    }

    async fn get_agent(&self, run_id: &str, agent_id: &str) -> Res<Option<AgentItem>> {
        self.get_item(keys::run_pk(run_id), keys::agent_sk(agent_id))
            .await
    }

    async fn put_research_policy(&self, item: &harness_protocol::research::PolicyItem) -> Res<()> {
        let Some(table) = &self.research_table else {
            return Ok(());
        };
        let attrs: HashMap<String, AttributeValue> = serde_dynamo::to_item(item)
            .map_err(|e| RunSessionError::Invalid(format!("policy row: {e}")))?;
        // One row per run: a retried prepare keeps the first.
        let res = self
            .client
            .put_item()
            .table_name(table)
            .set_item(Some(attrs))
            .condition_expression("attribute_not_exists(PK)")
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
            Err(e) => Err(RunSessionError::Unavailable(format!(
                "research policy row: {}",
                e.into_service_error()
            ))),
        }
    }

    async fn put_agent(&self, agent: &AgentItem) -> Res<()> {
        // Written once per run: the item that exists holds the running agentd's nonce hash and what worker-api recorded
        // since (status, heartbeat, a revoked binding); a retry of `start-session` leaves it as it is.
        self.put(item_of(agent)?, Some("attribute_not_exists(PK)"))
            .await
            .map(|_| ())
    }

    async fn rotate_agent(
        &self,
        next: &AgentItem,
        from_generation: u32,
        from_version: u64,
    ) -> Res<bool> {
        // One conditional replace: the item is still the one the caller read (same slice, same version), so a worker-api write since the read, or
        // another attempt's rotation, leaves it as it is and the caller reads again.
        let res = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item_of(next)?))
            .condition_expression("attribute_exists(PK) AND #sg = :g AND #v = :v")
            .expression_attribute_names("#sg", "sessionGeneration")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":g", AttributeValue::N(from_generation.to_string()))
            .expression_attribute_values(":v", AttributeValue::N(from_version.to_string()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(e) if put_conditional_failed(&e) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn set_run_slice(&self, run_id: &str, slice: u32) -> Res<()> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::META))
            .update_expression("SET #sl = :n")
            .condition_expression(
                "attribute_exists(PK) AND (attribute_not_exists(#sl) OR #sl < :n)",
            )
            .expression_attribute_names("#sl", "slice")
            .expression_attribute_values(":n", AttributeValue::N(slice.to_string()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            // The number is already this or higher: nothing to raise.
            Err(e) if conditional_failed(&e) => Ok(()),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn close_messages(&self, run_id: &str) -> Res<()> {
        Ledger::new(DynamoTable::new(self.client.clone(), self.table.clone()))
            .close(run_id, CloseReason::Finalize)
            .await
            .map(|_| ())
            .map_err(|e| unavailable(e.to_string()))
    }

    async fn end_agent(&self, run_id: &str, agent_id: &str) -> Res<()> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::agent_sk(agent_id)))
            .update_expression("SET #s = :stopped, #c.#r = :true, #v = #v + :one")
            .condition_expression("attribute_exists(#c)")
            .expression_attribute_names("#s", "status")
            .expression_attribute_names("#c", "creds")
            .expression_attribute_names("#r", "revoked")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":stopped", s("stopped"))
            .expression_attribute_values(":true", AttributeValue::Bool(true))
            .expression_attribute_values(":one", AttributeValue::N("1".into()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            // No Agent item, or one with no credential binding: nothing to revoke.
            Err(e) if conditional_failed(&e) => Ok(()),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn get_turn(&self, run_id: &str, agent_id: &str, n: u32) -> Res<Option<TurnItem>> {
        self.get_item(keys::run_pk(run_id), keys::turn_sk(agent_id, n))
            .await
    }

    async fn create_turn(&self, turn: &TurnItem) -> Res<bool> {
        self.put(item_of(turn)?, Some("attribute_not_exists(PK)"))
            .await
    }

    async fn replace_turn_token(
        &self,
        run_id: &str,
        agent_id: &str,
        n: u32,
        ciphertext: &str,
    ) -> Res<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::turn_sk(agent_id, n)))
            .update_expression("SET taskTokenCiphertext = :c")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":c", s(ciphertext))
            .send()
            .await
            .map(|_| ())
            .map_err(|e| unavailable(context(e)))
    }

    async fn mark_turn_invoked(&self, run_id: &str, agent_id: &str, n: u32) -> Res<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::turn_sk(agent_id, n)))
            .update_expression("SET invokedAt = :now")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(
                ":now",
                s(dispatcher::items::rfc3339(
                    crate::ops::system_clock_ms() / 1000,
                )),
            )
            .send()
            .await
            .map(|_| ())
            .map_err(|e| unavailable(context(e)))
    }

    async fn pin_runtime(&self, run_id: &str, pin: &Pin) -> Res<Pin> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::META))
            .update_expression("SET runtimeVersion = :v, runtimeEndpoint = :e")
            .condition_expression("attribute_exists(PK) AND attribute_not_exists(runtimeEndpoint)")
            .expression_attribute_values(":v", s(&pin.version))
            .expression_attribute_values(":e", s(&pin.endpoint))
            .send()
            .await;
        match res {
            Ok(_) => Ok(pin.clone()),
            Err(e) if conditional_failed(&e) => {
                // Another attempt of the run pinned it first (or the run is gone).
                let run = self.get_run(run_id).await?;
                run.and_then(|r| {
                    Some(Pin {
                        endpoint: r.runtime_endpoint?,
                        version: r.runtime_version.unwrap_or_default(),
                    })
                })
                .ok_or_else(|| {
                    RunSessionError::Invalid(format!("run {run_id} has no Run item to pin"))
                })
            }
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn claim_instances_slot(&self, run_id: &str, max: u32) -> Res<bool> {
        // One item holds the run ids of the active Instances sessions as a string set. The condition admits a run that is already in the set (a
        // repeat counts once) and any run while the set is smaller than `max`; two runs racing for the last slot cannot both pass it.
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(INSTANCES_SLOTS_PK))
            .key("SK", s(INSTANCES_SLOTS_SK))
            .update_expression("ADD runs :r")
            .condition_expression(
                "attribute_not_exists(runs) OR contains(runs, :id) OR size(runs) < :max",
            )
            .expression_attribute_values(":r", AttributeValue::Ss(vec![run_id.to_string()]))
            .expression_attribute_values(":id", s(run_id))
            .expression_attribute_values(":max", AttributeValue::N(max.to_string()))
            .send()
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(e) if conditional_failed(&e) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn release_instances_slot(&self, run_id: &str) -> Res<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(INSTANCES_SLOTS_PK))
            .key("SK", s(INSTANCES_SLOTS_SK))
            .update_expression("DELETE runs :r")
            .expression_attribute_values(":r", AttributeValue::Ss(vec![run_id.to_string()]))
            .send()
            .await
            .map(|_| ())
            .map_err(|e| unavailable(context(e)))
    }

    async fn set_run_phase(&self, run_id: &str, phase: GoalPhase) -> Res<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::META))
            .update_expression("SET #phase = :p")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_names("#phase", "phase")
            .expression_attribute_values(":p", phase_word(phase))
            .send()
            .await
            .map(|_| ())
            .map_err(|e| unavailable(context(e)))
    }

    async fn set_run_status(&self, run: &RunItem, to: GoalStatus, event: &Envelope) -> Res<bool> {
        self.move_run_status(run, to, event, None).await
    }

    async fn set_run_stop(
        &self,
        run: &RunItem,
        to: GoalStatus,
        event: &Envelope,
        reason: &str,
        mode: harness_protocol::goal::ResumeMode,
    ) -> Res<bool> {
        self.move_run_status(run, to, event, Some((reason, mode)))
            .await
    }

    async fn put_event(&self, event: &Envelope) -> Res<()> {
        // The same event id is the same item: a retry writes it again.
        self.put(
            item_of(&EventItem {
                envelope: event.clone(),
            })?,
            None,
        )
        .await
        .map(|_| ())
    }

    async fn touch_run_index(
        &self,
        run: &RunItem,
        outcome: Option<&str>,
        last_activity: &str,
    ) -> Res<()> {
        // The key fields only where the item is new, the end fields always; the start's fields (prompt head, model, repository) are not
        // here, so the start and the backfill cannot be undone by this and cannot undo it. A time that is not later than the one stored is
        // a delayed repeat of an earlier attempt: its condition fails and nothing is lost.
        let mut sets = vec![
            "runId = if_not_exists(runId, :r)",
            "createdAt = if_not_exists(createdAt, :c)",
            "lastActivity = :t",
        ];
        let mut update = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(&run.owner_principal_arn)))
            .key("SK", s(search_sk(&run.created_at, &run.run_id)))
            .expression_attribute_values(":r", s(&run.run_id))
            .expression_attribute_values(":c", s(&run.created_at))
            .expression_attribute_values(":t", s(last_activity));
        // On equal times the write goes through only when it cannot replace another outcome: the start and the end of a run may share a
        // second, and two attempts of the same end carry the same word.
        let condition = if let Some(o) = outcome {
            sets.push("outcome = :o");
            update = update.expression_attribute_values(":o", s(o));
            "attribute_not_exists(lastActivity) OR lastActivity < :t OR (lastActivity = :t AND (attribute_not_exists(outcome) OR outcome = :o))"
        } else {
            "attribute_not_exists(lastActivity) OR lastActivity < :t OR (lastActivity = :t AND attribute_not_exists(outcome))"
        };
        update = update.condition_expression(condition);
        match update
            .update_expression(format!("SET {}", sets.join(", ")))
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if conditional_failed(&e) => Ok(()),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn run_name(&self, run: &RunItem) -> Res<Option<String>> {
        // One consistent read of the one attribute: a retry of the naming op must see the name its first invocation wrote.
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(&run.owner_principal_arn)))
            .key("SK", s(search_sk(&run.created_at, &run.run_id)))
            .consistent_read(true)
            .projection_expression("runName")
            .send()
            .await
            .map_err(|e| unavailable(context(e)))?;
        Ok(out
            .item
            .and_then(|item| item.get("runName").and_then(|v| v.as_s().ok()).cloned()))
    }

    async fn put_run_name(
        &self,
        run: &RunItem,
        name: &str,
        source: &str,
        model: &str,
    ) -> Res<bool> {
        // The one writer of the three name attributes (the end of a run sets `lastActivity` and `outcome`, the start sets the head).
        // The key fields only where the item is new: the name may land before the start's write. The attribute is `runName`, not
        // `name`: NAME is a DynamoDB reserved word. The condition makes the name write-once; a failed condition means the item
        // holds a name, which stands.
        match self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(&run.owner_principal_arn)))
            .key("SK", s(search_sk(&run.created_at, &run.run_id)))
            .update_expression(
                "SET runId = if_not_exists(runId, :r), createdAt = if_not_exists(createdAt, :c), \
                 runName = :n, runNameSource = :s, runNameModel = :m",
            )
            .condition_expression("attribute_not_exists(runName)")
            .expression_attribute_values(":r", s(&run.run_id))
            .expression_attribute_values(":c", s(&run.created_at))
            .expression_attribute_values(":n", s(name))
            .expression_attribute_values(":s", s(source))
            .expression_attribute_values(":m", s(model))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if conditional_failed(&e) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn tag_state(&self, run: &RunItem) -> Res<Option<TagMarks>> {
        let item = self
            .tag_attributes(
                &run.owner_principal_arn,
                search_sk(&run.created_at, &run.run_id),
                &[dynamo::RUN_TAGS_AT, dynamo::RUN_TAGS_OFF],
            )
            .await?;
        Ok(item.map(|item| TagMarks {
            at: item
                .get(dynamo::RUN_TAGS_AT)
                .and_then(|v| v.as_s().ok())
                .cloned(),
            off: removed_list(item.get(dynamo::RUN_TAGS_OFF)),
        }))
    }

    async fn tag_vocabulary(&self, run: &RunItem, limit: u32) -> Res<VocabRead> {
        // The owner's partition comes from the stored run; nothing a caller sent can steer it. The spec is the one the IAM statement
        // `TagVocabulary` was proved against: the three tag lists projected, `Select` set, no filter expression.
        let mut spec = dynamo::vocabulary_query(&owner_pk(&run.owner_principal_arn));
        spec.limit = limit.clamp(1, spec.limit);
        let mut query = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression(&spec.key_condition)
            .consistent_read(spec.consistent_read)
            .scan_index_forward(spec.scan_index_forward)
            .limit(i32::try_from(spec.limit).unwrap_or(i32::MAX));
        for (alias, name) in &spec.names {
            query = query.expression_attribute_names(alias, name);
        }
        for (alias, value) in &spec.values {
            query = query.expression_attribute_values(alias, av(value));
        }
        if let Some(projection) = &spec.projection {
            query = query.projection_expression(projection);
        }
        if let Some(select) = &spec.select {
            query = query.select(Select::from(select.as_str()));
        }
        if let Some(filter) = &spec.filter {
            query = query.filter_expression(filter);
        }
        let page = query.send().await.map_err(|e| unavailable(context(e)))?;
        let mut read = VocabRead::default();
        for item in page.items.unwrap_or_default() {
            match serde_dynamo::from_item::<_, VocabRow>(item) {
                Ok(row) => read.items.push(VocabItem {
                    auto: row.run_tags,
                    user: row.run_tags_user,
                    off: row.run_tags_off,
                }),
                Err(_) => read.skipped += 1,
            }
        }
        Ok(read)
    }

    async fn parent_tags(&self, run: &RunItem) -> Res<Option<TagState>> {
        let Some(parent_id) = run.continue_from.as_deref() else {
            return Ok(None);
        };
        let Some(parent) = self.get_run(parent_id).await? else {
            return Ok(None);
        };
        // A follow-up and the run it continues belong to one principal (control-api checked it when it made the run). The item is read in
        // the follow-up's own partition, so a parent of another principal would simply not be there; the check says so without a read.
        if parent.owner_principal_arn != run.owner_principal_arn {
            return Ok(None);
        }
        let item = self
            .tag_attributes(
                &run.owner_principal_arn,
                search_sk(&parent.created_at, &parent.run_id),
                &[
                    dynamo::RUN_TAGS,
                    dynamo::RUN_TAGS_USER,
                    dynamo::RUN_TAGS_OFF,
                ],
            )
            .await?;
        Ok(item.map(|item| TagState {
            auto: tag_list(item.get(dynamo::RUN_TAGS)),
            user: tag_list(item.get(dynamo::RUN_TAGS_USER)),
            off: removed_list(item.get(dynamo::RUN_TAGS_OFF)),
        }))
    }

    async fn put_run_tags_inherit(
        &self,
        run: &RunItem,
        parent_run_id: &str,
        user: &[String],
        off: &[String],
    ) -> Res<bool> {
        let Some(spec) =
            dynamo::inherit_update(&run.run_id, &run.created_at, parent_run_id, user, off)
        else {
            return Ok(false);
        };
        self.send_tag_update(run, &spec).await
    }

    async fn put_run_tags_auto(
        &self,
        run: &RunItem,
        tags: &[String],
        at: &str,
        answered: Option<Answered<'_>>,
    ) -> Res<bool> {
        let spec = dynamo::auto_update(&run.run_id, &run.created_at, tags, at, answered);
        self.send_tag_update(run, &spec).await
    }

    async fn abort_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        events: &[Envelope],
    ) -> Res<bool> {
        let aborted = match &run.aborted {
            Some(a) => {
                Some(serde_dynamo::to_attribute_value(a).map_err(|e| unavailable(e.to_string()))?)
            }
            None => None,
        };
        let mut tx = self
            .client
            .transact_write_items()
            .transact_items(self.versioned_update(
                keys::META,
                &run.run_id,
                run.status,
                run.version,
                aborted,
            )?);
        if let Some(g) = goal {
            tx = tx.transact_items(self.versioned_update(
                &keys::goal_sk(g.task_id.as_deref()),
                &g.run_id,
                g.status,
                g.version,
                None,
            )?);
        }
        for e in events {
            tx = tx.transact_items(self.event_put(e)?);
        }
        match tx.send().await {
            Ok(_) => Ok(true),
            Err(e) if transaction_conflict(&e) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    /// The Agent item's raw `features` attribute holds `transcript.v1`; an image that predates it reads as no.
    async fn transcript_supported(&self, run_id: &str) -> Res<bool> {
        let agent: Option<serde_json::Value> = self
            .get_item(keys::run_pk(run_id), keys::agent_sk(DIRECT_AGENT_ID))
            .await?;
        Ok(agent
            .as_ref()
            .and_then(|a| a.get("features"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|f| {
                f.iter()
                    .any(|w| w.as_str() == Some(ham_core::transcript::TRANSCRIPT_FEATURE))
            }))
    }

    /// The run's `Instruct` controls from the user that carry text, oldest first (the control ids are ULIDs).
    async fn user_answers(&self, run_id: &str) -> Res<Vec<String>> {
        use harness_protocol::api::internal::PendingControl;
        use harness_protocol::direct_input::InstructOrigin;
        use harness_protocol::store::ControlItem;
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
            .expression_attribute_values(":pk", s(keys::run_pk(run_id)))
            .expression_attribute_values(":sk", s(keys::control_prefix(DIRECT_AGENT_ID)))
            .consistent_read(true)
            .limit(50)
            .send()
            .await
            .map_err(|e| unavailable(context(e)))?;
        Ok(out
            .items()
            .iter()
            .filter_map(|i| serde_dynamo::from_item::<_, ControlItem>(i.clone()).ok())
            .filter_map(|c| match c.control {
                PendingControl::Instruct {
                    text: Some(text),
                    origin: InstructOrigin::User,
                    ..
                } => Some(text),
                _ => None,
            })
            .collect())
    }

    /// One UpdateItem that sets the body's attributes of the run's chat item and nothing else: a repeat of the call writes the same
    /// values. `ownerPrincipalArn` is never set, so the run-list index (sparse on it) never sees it, and the key is the chat item's,
    /// not the run index's, so the run list's range never holds a body.
    async fn put_search_body(&self, w: &SearchWrite<'_>) -> Res<()> {
        let body = w.body;
        let mut names = vec![
            ("#r", "runId"),
            ("#c", "createdAt"),
            ("#b", "body"),
            ("#s", "segs"),
            ("#v", "coverage"),
            ("#i", "indexedAt"),
        ];
        let mut sets = vec![
            "#r = :r", "#c = :c", "#b = :b", "#s = :s", "#v = :v", "#i = :i",
        ];
        let mut removes: Vec<&str> = Vec::new();
        let coverage = serde_json::to_value(body.coverage)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut req = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(w.pk))
            .key("SK", s(w.sk))
            .expression_attribute_values(":r", s(w.run_id))
            .expression_attribute_values(":c", s(w.created_at))
            .expression_attribute_values(":b", s(body.body.clone()))
            .expression_attribute_values(":s", s(ham_core::runsearch::segs_to_string(&body.segs)))
            .expression_attribute_values(":v", s(coverage))
            .expression_attribute_values(":i", s(w.indexed_at));
        match body.through {
            Some(t) => {
                names.push(("#t", "bodyThrough"));
                sets.push("#t = :t");
                req = req.expression_attribute_values(":t", AttributeValue::N(t.to_string()));
            }
            None => {
                names.push(("#t", "bodyThrough"));
                removes.push("#t");
            }
        }
        for (k, v) in names {
            req = req.expression_attribute_names(k, v);
        }
        let mut expr = format!("SET {}", sets.join(", "));
        if !removes.is_empty() {
            expr.push_str(&format!(" REMOVE {}", removes.join(", ")));
        }
        // A partial body never replaces a full one (a run indexed again, such as after being blocked and resumed, may read less than
        // it did): the write is conditional, and a failed condition is not an error.
        if body.coverage == ham_core::runsearch::Coverage::Partial {
            req = req
                .condition_expression("attribute_not_exists(#v) OR #v = :p")
                .expression_attribute_values(":p", s("partial"));
        }
        match req.update_expression(expr).send().await {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(())
            }
            Err(e) => Err(unavailable(context(e))),
        }
    }
}

/// The data key; the ciphertext is bound to the turn by its encryption context.
#[derive(Clone, Debug)]
pub struct KmsTokens {
    pub client: aws_sdk_kms::Client,
    pub key_id: String,
}
impl DynamoStore {
    /// The one write of `set_run_status` and `set_run_stop`: the Run item and the root Goal move to `to` and the event is written, together. A
    /// stop also stores the plain reason and the resume mode on the Run item.
    async fn move_run_status(
        &self,
        run: &RunItem,
        to: GoalStatus,
        event: &Envelope,
        stop: Option<(&str, harness_protocol::goal::ResumeMode)>,
    ) -> Res<bool> {
        // Each item's version moves too, so a control-api write from an older read conflicts.
        let update = |sk: String, condition: &str, with_from: bool, meta: bool| {
            let expression = if meta && stop.is_some() {
                "SET #status = :to, #v = #v + :one, statusReason = :reason, resumeMode = :mode"
            } else {
                "SET #status = :to, #v = #v + :one"
            };
            let mut u = Update::builder()
                .table_name(&self.table)
                .key("PK", s(keys::run_pk(&run.run_id)))
                .key("SK", s(sk))
                .update_expression(expression)
                .condition_expression(condition)
                .expression_attribute_names("#status", "status")
                .expression_attribute_names("#v", "version")
                .expression_attribute_values(":to", status_word(to))
                .expression_attribute_values(":one", AttributeValue::N("1".into()));
            if with_from {
                u = u.expression_attribute_values(":from", status_word(run.status));
            }
            if let (true, Some((reason, mode))) = (meta, stop) {
                u = u
                    .expression_attribute_values(":reason", s(reason))
                    .expression_attribute_values(
                        ":mode",
                        s(serde_json::to_value(mode)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default()),
                    );
            }
            u.build().map_err(|e| unavailable(context(e)))
        };
        let meta = update(keys::META.into(), "#status = :from", true, true)?;
        let goal = update(keys::goal_sk(None), "attribute_exists(PK)", false, false)?;
        let res = self
            .client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().update(meta).build())
            .transact_items(TransactWriteItem::builder().update(goal).build())
            .transact_items(self.event_put(event)?)
            .send()
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(e) if transaction_conflict(&e) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }
}

impl Tokens for KmsTokens {
    async fn encrypt(&self, token: &str, ctx: &TokenContext) -> Res<String> {
        let out = self
            .client
            .encrypt()
            .key_id(&self.key_id)
            .plaintext(aws_sdk_kms::primitives::Blob::new(token.as_bytes()))
            .encryption_context("run", &ctx.run_id)
            .encryption_context("agent", &ctx.agent_id)
            .encryption_context("turn", ctx.turn.to_string())
            .send()
            .await
            .map_err(|e| unavailable(context(e)))?;
        let blob = out
            .ciphertext_blob
            .ok_or_else(|| unavailable("KMS returned no ciphertext"))?;
        Ok(STANDARD.encode(blob.into_inner()))
    }

    async fn decrypt(&self, ciphertext: &str, ctx: &TokenContext) -> Res<String> {
        let blob = STANDARD
            .decode(ciphertext.trim())
            .map_err(|e| RunSessionError::Invalid(format!("the stored ciphertext: {e}")))?;
        let res = self
            .client
            .decrypt()
            .key_id(&self.key_id)
            .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(blob))
            .encryption_context("run", &ctx.run_id)
            .encryption_context("agent", &ctx.agent_id)
            .encryption_context("turn", ctx.turn.to_string())
            .send()
            .await;
        let out = match res {
            Ok(o) => o,
            // The ciphertext is not this context's (or not a ciphertext at all): no retry can read it.
            Err(e)
                if e.as_service_error().is_some_and(|se| {
                    se.is_invalid_ciphertext_exception() || se.is_incorrect_key_exception()
                }) =>
            {
                return Err(RunSessionError::Invalid(context(e)));
            }
            Err(e) => return Err(unavailable(context(e))),
        };
        let plain = out
            .plaintext
            .ok_or_else(|| unavailable("KMS returned no plaintext"))?;
        String::from_utf8(plain.into_inner())
            .map_err(|_| RunSessionError::Invalid("the decrypted value is not text".into()))
    }
}

/// The CodeArtifact token: `GetAuthorizationToken` signed with the Lambda's own credentials, outside the worker
/// VM, so the worker session's credentials (the engine's) carry no CodeArtifact grant. `index` is `None` on a stage
/// without a package index (no `PACKAGES_DOMAIN`), which mints nothing.
#[derive(Clone, Debug)]
pub struct CodeArtifactTokens {
    pub index: Option<agentd::packages::PackagesIndex>,
    pub region: String,
    pub credentials: aws_credential_types::provider::SharedCredentialsProvider,
}

impl CodeArtifactTokens {
    /// `PACKAGES_DOMAIN`, `PACKAGES_OWNER`, `PACKAGES_REPOSITORY` (default `packages`) and `PACKAGES_ENDPOINT` (tests
    /// only), with `config`'s credentials and Region.
    pub fn from_env(config: &aws_config::SdkConfig) -> Option<Self> {
        Self::from_lookup(config, &|k: &str| std::env::var(k).ok())
    }

    /// [`CodeArtifactTokens::from_env`] over any variable lookup.
    pub fn from_lookup(
        config: &aws_config::SdkConfig,
        get: &impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let credentials = config.credentials_provider()?;
        let index = agentd::packages::PackagesIndex::from_vars(&|k: &str| {
            get(&k.replace("AGENTD_PACKAGES_", "PACKAGES_"))
        });
        Some(Self {
            index,
            region: config
                .region()
                .map(|r| r.to_string())
                .unwrap_or_else(|| dispatcher::stage::REGION.to_string()),
            credentials,
        })
    }
}

impl PackageTokens for CodeArtifactTokens {
    async fn token(&self, duration_s: u32) -> Result<Option<String>, String> {
        use aws_credential_types::provider::ProvideCredentials;
        let Some(ix) = &self.index else {
            return Ok(None);
        };
        let c = self
            .credentials
            .provide_credentials()
            .await
            .map_err(|e| format!("run-session credentials: {e}"))?;
        let creds = harness_protocol::api::internal::ProcessCredentials {
            version: 1,
            access_key_id: c.access_key_id().to_string(),
            secret_access_key: c.secret_access_key().to_string(),
            session_token: c.session_token().unwrap_or_default().to_string(),
            expiration: String::new(),
        };
        agentd::packages::mint_token(
            &agentd::http::SignedHttp::new(),
            &creds,
            ix,
            &self.region,
            duration_s,
        )
        .await
        .map(Some)
    }
}

#[derive(Clone, Debug)]
pub struct S3Blobs {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

impl Blobs for S3Blobs {
    async fn get(&self, key: &str) -> Res<Option<Vec<u8>>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(o) => {
                let body = o
                    .body
                    .collect()
                    .await
                    .map_err(|e| unavailable(e.to_string()))?;
                Ok(Some(body.into_bytes().to_vec()))
            }
            Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => Ok(None),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn exists(&self, key: &str) -> Res<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|se| se.is_not_found()) => Ok(false),
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn put_if_absent(&self, key: &str, body: &[u8]) -> Res<bool> {
        use aws_sdk_s3::error::ProvideErrorMetadata as _;
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/json")
            .if_none_match("*")
            .body(aws_sdk_s3::primitives::ByteStream::from(body.to_vec()))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            // 412 PreconditionFailed: the object exists.
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.code() == Some("PreconditionFailed")) =>
            {
                Ok(false)
            }
            Err(e) => Err(unavailable(context(e))),
        }
    }

    /// ListObjectsV2 with `StartAfter`: keys and sizes of one page, in key order.
    async fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Res<ObjectPage> {
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .max_keys(i32::try_from(max_keys.clamp(1, 1000)).unwrap_or(1000));
        if let Some(after) = start_after {
            req = req.start_after(after);
        }
        let out = req.send().await.map_err(|e| unavailable(context(e)))?;
        Ok(ObjectPage {
            objects: out
                .contents()
                .iter()
                .filter_map(|o| {
                    Some(ObjectInfo {
                        key: o.key()?.to_string(),
                        size: u64::try_from(o.size().unwrap_or(0)).unwrap_or(0),
                    })
                })
                .collect(),
            truncated: out.is_truncated().unwrap_or(false),
        })
    }

    /// GetObject refused above `max_bytes` by its length and again while streaming; an object that is missing or too large is `None`.
    async fn get_bounded(&self, key: &str, max_bytes: usize) -> Res<Option<Vec<u8>>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => {
                if out
                    .content_length()
                    .is_some_and(|l| l > i64::try_from(max_bytes).unwrap_or(i64::MAX))
                {
                    return Ok(None);
                }
                let mut body = out.body;
                let mut buf: Vec<u8> = Vec::new();
                while let Some(part) = body
                    .try_next()
                    .await
                    .map_err(|e| unavailable(e.to_string()))?
                {
                    if buf.len() + part.len() > max_bytes {
                        return Ok(None);
                    }
                    buf.extend_from_slice(&part);
                }
                Ok(Some(buf))
            }
            Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => Ok(None),
            Err(e) => Err(unavailable(context(e))),
        }
    }
}

/// Where the worker runtime's ARN is published: the SSM parameter the runtimes stack writes.
#[derive(Clone, Debug)]
pub struct RuntimeParameter {
    pub client: aws_sdk_ssm::Client,
    pub name: String,
}

/// The worker runtime at a qualifier, plus the control-plane read of its endpoints.
#[derive(Clone, Debug)]
pub struct AgentCoreSessions {
    pub client: aws_sdk_bedrockagentcore::Client,
    pub control: aws_sdk_bedrockagentcorecontrol::Client,
    /// The first back-off after a `RetryableConflictException` (0 in tests).
    pub conflict_base_ms: u64,
    /// Where `Sessions::runtime_arn` reads the runtime ARN; `None` has no lookup.
    pub runtime_parameter: Option<RuntimeParameter>,
    /// Where `Sessions::instances_runtime_arn` reads the Instances runtime's ARN; `None` has no lookup.
    pub instances_parameter: Option<RuntimeParameter>,
    /// Where `Sessions::capacity_provider_id` reads the id of the capacity provider the Instances runtime runs on; `None` has no lookup.
    pub capacity_provider_parameter: Option<RuntimeParameter>,
}

/// The runtime ID of a runtime ARN (`...:runtime/<id>`).
pub fn runtime_id(runtime_arn: &str) -> Option<&str> {
    runtime_arn
        .rsplit_once(":runtime/")
        .map(|(_, id)| id)
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

/// A runtime that answered 4xx refused the request itself (AgentCore reports "Received error (<status>) from
/// runtime"), so a retry cannot help; anything else may pass on a retry, as may a 429. agentd's 409 ("a turn is already
/// running in this session") is a [`RunSessionError::Conflict`]: for the session of this run it means an earlier
/// attempt's invocation was accepted (`start-session` takes it so).
pub fn runtime_error(m: String) -> RunSessionError {
    let status = m
        .split("Received error (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(409) => RunSessionError::Conflict(m),
        Some(429) => RunSessionError::Unavailable(m),
        Some(400..=499) => RunSessionError::Invalid(m),
        _ => RunSessionError::Unavailable(m),
    }
}

fn session_error(
    other: impl Fn(String) -> RunSessionError,
) -> impl Fn(dispatcher::aws::AwsError) -> RunSessionError {
    move |e| match e {
        dispatcher::aws::AwsError::Qualifier { .. } => RunSessionError::Invalid(e.to_string()),
        e => other(e.to_string()),
    }
}

impl Sessions for AgentCoreSessions {
    async fn runtime_arn(&self) -> Res<Option<String>> {
        match &self.runtime_parameter {
            None => Ok(None),
            Some(p) => resolve_runtime_arn(&p.client, &p.name).await.map(Some),
        }
    }

    async fn instances_runtime_arn(&self) -> Res<Option<String>> {
        match &self.instances_parameter {
            None => Ok(None),
            Some(p) => resolve_instances_runtime_arn(&p.client, &p.name)
                .await
                .map(Some),
        }
    }

    async fn capacity_provider_id(&self) -> Res<Option<String>> {
        match &self.capacity_provider_parameter {
            None => Ok(None),
            Some(p) => resolve_capacity_provider_id(&p.client, &p.name)
                .await
                .map(Some),
        }
    }

    async fn delete_provider_session(
        &self,
        capacity_provider_id: &str,
        session_id: &str,
    ) -> Res<bool> {
        dispatcher::aws::delete_capacity_provider_session(
            &self.client,
            capacity_provider_id,
            session_id,
        )
        .await
        .map_err(session_error(RunSessionError::Unavailable))
    }

    async fn endpoint(&self, runtime_arn: &str, name: &str) -> Res<Option<EndpointState>> {
        let id = runtime_id(runtime_arn).ok_or_else(|| {
            RunSessionError::Invalid(format!("{runtime_arn} is not a runtime ARN"))
        })?;
        if !dispatcher::endpoints::is_endpoint_name(name) {
            return Err(RunSessionError::Invalid(format!(
                "{name:?} is not an endpoint name"
            )));
        }
        match self
            .control
            .get_agent_runtime_endpoint()
            .agent_runtime_id(id)
            .endpoint_name(name)
            .send()
            .await
        {
            Ok(out) => Ok(Some(EndpointState {
                name: name.to_string(),
                status: out.status.as_str().to_string(),
                live_version: out.live_version.filter(|v| !v.is_empty()),
            })),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_resource_not_found_exception()) =>
            {
                Ok(None)
            }
            Err(e) => Err(unavailable(context(e))),
        }
    }

    async fn invoke(
        &self,
        runtime_arn: &str,
        qualifier: &str,
        session_id: &str,
        body: &[u8],
    ) -> Res<Vec<u8>> {
        dispatcher::aws::invoke_at(
            &self.client,
            runtime_arn,
            qualifier,
            session_id,
            body.to_vec(),
            self.conflict_base_ms,
        )
        .await
        .map_err(session_error(runtime_error))
    }

    async fn stop(&self, runtime_arn: &str, qualifier: &str, session_id: &str) -> Res<bool> {
        dispatcher::aws::stop_session_at(
            &self.client,
            runtime_arn,
            qualifier,
            session_id,
            self.conflict_base_ms,
        )
        .await
        .map_err(session_error(RunSessionError::Unavailable))
    }
}

/// The runtime ARN the runtimes stack published as an SSM parameter. A parameter that does not exist, or holds no runtime ARN, is
/// `Invalid`: the stage has no worker runtime, so this run can never start and the machine does not retry it. Any other failure
/// (throttling, a service fault, a missing grant) is `Unavailable` and is retried.
pub async fn resolve_runtime_arn(client: &aws_sdk_ssm::Client, parameter: &str) -> Res<String> {
    resolve_runtime_arn_of(client, parameter, "the worker runtime is not deployed").await
}

/// [`resolve_runtime_arn`] for the Instances worker runtime: the same reading, the words name the Instances runtime.
pub async fn resolve_instances_runtime_arn(
    client: &aws_sdk_ssm::Client,
    parameter: &str,
) -> Res<String> {
    resolve_runtime_arn_of(
        client,
        parameter,
        "the Instances worker runtime is not deployed",
    )
    .await
}

/// The capacity provider's id the stack published as an SSM parameter. Read when an Instances session starts, never cached, so a warm
/// function sees a parameter that was written after it started. A parameter that does not exist, or holds no capacity provider id, is `Invalid`: the
/// stage's Instances part is not set up, so this run can never start on it and the machine does not retry it. Any other failure (throttling, a
/// service fault, a missing grant) is `Unavailable` and is retried.
pub async fn resolve_capacity_provider_id(
    client: &aws_sdk_ssm::Client,
    parameter: &str,
) -> Res<String> {
    let not_set_up = |why: String| {
        RunSessionError::Invalid(format!(
            "the Instances capacity provider is not set up: {why}"
        ))
    };
    let out = match client.get_parameter().name(parameter).send().await {
        Ok(out) => out,
        Err(e) => {
            let missing = e
                .as_service_error()
                .is_some_and(|s| s.is_parameter_not_found());
            let text = context(e);
            return Err(if missing {
                not_set_up(format!("SSM parameter {parameter} does not exist ({text})"))
            } else {
                unavailable(text)
            });
        }
    };
    out.parameter
        .and_then(|p| p.value)
        .filter(|v| dispatcher::ids::is_capacity_provider_id(v))
        .ok_or_else(|| not_set_up(format!("{parameter} holds no capacity provider id")))
}

async fn resolve_runtime_arn_of(
    client: &aws_sdk_ssm::Client,
    parameter: &str,
    what: &str,
) -> Res<String> {
    let not_deployed = |why: String| RunSessionError::Invalid(format!("{what}: {why}"));
    let out = match client.get_parameter().name(parameter).send().await {
        Ok(out) => out,
        Err(e) => {
            let missing = e
                .as_service_error()
                .is_some_and(|s| s.is_parameter_not_found());
            let text = context(e);
            return Err(if missing {
                not_deployed(format!("SSM parameter {parameter} does not exist ({text})"))
            } else {
                unavailable(text)
            });
        }
    };
    out.parameter
        .and_then(|p| p.value)
        .filter(|v| v.starts_with("arn:aws:bedrock-agentcore:") && runtime_id(v).is_some())
        .ok_or_else(|| not_deployed(format!("{parameter} holds no runtime ARN")))
}

/// [`resolve_runtime_arn`] with its error as text.
pub async fn runtime_arn(client: &aws_sdk_ssm::Client, parameter: &str) -> Result<String, String> {
    resolve_runtime_arn(client, parameter)
        .await
        .map_err(|e| e.to_string())
}
