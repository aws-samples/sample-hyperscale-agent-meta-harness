//! DynamoDB, S3 and Step Functions implementations of the handler traits. Items are the
//! harness-protocol serde types plus `PK`/`SK` from their key builders.

use crate::handler::{
    Blobs, DeployedAs, Execution, ObjectError, ObjectInfo, ObjectPage, ObjectPrefix, RunUsage,
    Store, StoreError, TagRead,
};
use crate::ids::rfc3339;
use aws_sdk_dynamodb::error::DisplayErrorContext;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::types::{
    AttributeValue, Delete, KeysAndAttributes, Put, ReturnValue, TransactWriteItem, Update,
};
use aws_sdk_s3::config::{ProvideCredentials as _, SharedCredentialsProvider};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_ssm::operation::get_parameter::GetParameterError;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ham_core::run_index::{RunIndexItem, owner_pk, search_sk};
use ham_core::runsearch::{SK_CEILING, SearchItem, chat_ceiling, chat_floor};
use ham_core::runtag::dynamo::{Av, UpdateSpec};
use ham_core::transcript::{TRANSCRIPT_FEATURE, TranscriptSummary};
use harness_protocol::api::v1::DownloadUrlResponse;
use harness_protocol::direct::{DIRECT_AGENT_ID, DIRECT_TURN};
use harness_protocol::envelope::Envelope;
use harness_protocol::messages::{MessageView, MessagesPage, SendMessageRequest};
use harness_protocol::store::{
    AgentItem, ArtifactItem, ControlItem, EventItem, GoalItem, IdempotencyItem, Keyed,
    RepoAllowItem, RunItem, keys,
};
use harness_protocol::target::RepoTarget;
use message_ledger::dynamo::DynamoTable;
use message_ledger::{Admitted, CloseReason, Closed, Ledger, LedgerError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Runs per `GET /v1/runs` page.
pub const RUNS_PAGE: i32 = 100;
/// The most pages of one Query a read follows. A Query page is up to 1 MB, so this bounds the bytes and the time of a read whose items
/// do not decode or whose window is huge; a read that needs more fails (`Unavailable`) rather than answer a short list as if it were whole.
const MAX_QUERY_PAGES: usize = 20;

fn out_of_pages(what: &str) -> StoreError {
    StoreError::Unavailable(format!(
        "the {what} read needs more than {MAX_QUERY_PAGES} pages"
    ))
}

type Item = HashMap<String, AttributeValue>;

fn unavailable<E: std::error::Error>(e: E) -> StoreError {
    StoreError::Unavailable(DisplayErrorContext(e).to_string())
}

/// The codes with which AWS services answer a request they will not take now but may take later.
const THROTTLE_CODES: [&str; 6] = [
    "ThrottlingException",
    "Throttling",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "RequestLimitExceeded",
];

/// The codes with which AWS services answer a request whose credentials or clock were wrong for a moment: a token that expired between its issue
/// and the call, a signature rejected over a clock that is a few minutes off, a key that is being rotated. The next attempt signs afresh. They are
/// no sign that the stage is not set up.
const CREDENTIAL_CODES: [&str; 6] = [
    "ExpiredTokenException",
    "ExpiredToken",
    "RequestExpired",
    "InvalidSignatureException",
    "SignatureDoesNotMatch",
    "UnrecognizedClientException",
];

/// Whether a failed SSM read may pass on a retry: a throttle, a fault of the service (HTTP 5xx), a credential or clock failure of the moment, or
/// a call that got no answer (timeout, connection, a reply that could not be read). A refusal of the call itself (a missing grant, a name or a key
/// the service does not accept) comes back the same every time, and so does a request that could not be built.
fn may_pass(e: &aws_sdk_ssm::error::SdkError<GetParameterError>) -> bool {
    use aws_sdk_ssm::error::{ProvideErrorMetadata as _, SdkError};
    match e {
        SdkError::ServiceError(se) => {
            let status = se.raw().status().as_u16();
            status >= 500
                || status == 429
                || se
                    .err()
                    .code()
                    .is_some_and(|c| THROTTLE_CODES.contains(&c) || CREDENTIAL_CODES.contains(&c))
        }
        SdkError::ConstructionFailure(_) => false,
        _ => true,
    }
}

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}

/// What one page of the run index window left out. Counts only, so the log line holds no key, text or tag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageSkips {
    /// Items that did not decode at all (a wrong type in an attribute that is not a tag attribute). The row keeps its bare summary.
    pub items: usize,
    /// Tag attributes of a wrong type that the item was read without (or, for the removal list, read in part).
    pub tag_attributes: usize,
    /// The part of `tag_attributes` that was the removal list. A removal list that is lost shows its tags again, so it is counted apart.
    pub off_attributes: usize,
}

/// The one log line of a page that dropped anything: `{"event":"run-index-skip","count":<items>,"tagAttributes":<n>,"offAttributes":<n>}`.
/// `None` for a page that dropped nothing.
pub fn skip_line(skips: &PageSkips) -> Option<String> {
    if *skips == PageSkips::default() {
        return None;
    }
    Some(
        serde_json::json!({
            "event": "run-index-skip",
            "count": skips.items,
            "tagAttributes": skips.tag_attributes,
            "offAttributes": skips.off_attributes,
        })
        .to_string(),
    )
}

/// The tag attributes of a raw item that carry a value (not DynamoDB NULL), read before the item is decoded, and whether the removal list
/// is a list of text.
struct TagPresence {
    lists: [bool; 2],
    scalars: [bool; 5],
    off: Option<bool>,
}

fn tag_presence(raw: &Item) -> TagPresence {
    let has = |name: &str| {
        raw.get(name)
            .is_some_and(|v| !matches!(v, AttributeValue::Null(_)))
    };
    let off = raw
        .get("runTagsOff")
        .filter(|v| !matches!(v, AttributeValue::Null(_)))
        .map(|v| match v {
            AttributeValue::L(items) => items.iter().all(|i| matches!(i, AttributeValue::S(_))),
            AttributeValue::Ss(_) => true,
            _ => false,
        });
    TagPresence {
        lists: [has("runTags"), has("runTagsUser")],
        scalars: [
            has("runTagsAt"),
            has("runTagsModel"),
            has("runTagsVer"),
            has("runTagsInh"),
            has("runTagsRev"),
        ],
        off,
    }
}

/// The tag attributes that were present and yet are not in the decoded item as written: (all of them, the removal list among them).
fn tag_attribute_skips(presence: &TagPresence, item: &RunIndexItem) -> (usize, usize) {
    let lost = [
        (presence.lists[0], item.run_tags.is_none()),
        (presence.lists[1], item.run_tags_user.is_none()),
        (presence.scalars[0], item.run_tags_at.is_none()),
        (presence.scalars[1], item.run_tags_model.is_none()),
        (presence.scalars[2], item.run_tags_ver.is_none()),
        (presence.scalars[3], item.run_tags_inh.is_none()),
        (presence.scalars[4], item.run_tags_rev.is_none()),
    ];
    let mut attributes = lost.iter().filter(|(there, none)| *there && *none).count();
    // The removal list is counted when it is not a list of text: a list with a stray entry is read in part, a value that is not a list at all
    // is lost. Both are damage the operator should see, since a lost removal shows its tag again.
    let off = usize::from(
        presence
            .off
            .is_some_and(|text_list| !text_list || item.run_tags_off.is_none()),
    );
    attributes += off;
    (attributes, off)
}

/// Decodes one page of search items. An item that cannot be read as a whole is left out and counted; an item whose tag attributes were
/// of a wrong type is kept (the reader of `RunIndexItem` leaves those attributes out) and counted.
fn decode_run_index_page(raw: Vec<Item>) -> (Vec<RunIndexItem>, PageSkips) {
    let mut skips = PageSkips::default();
    let mut items = Vec::with_capacity(raw.len());
    for entry in raw {
        let presence = tag_presence(&entry);
        match serde_dynamo::from_item::<_, RunIndexItem>(entry) {
            Ok(item) => {
                let (attributes, off) = tag_attribute_skips(&presence, &item);
                skips.tag_attributes += attributes;
                skips.off_attributes += off;
                items.push(item);
            }
            Err(_) => skips.items += 1,
        }
    }
    (items, skips)
}

/// The attributes one tag edit reads of the search item: the ids that make its key and its creation, the repository that names the
/// derived project, the three lists, and the revision it compares against. Nothing else of the item is asked for.
const TAG_STATE_ATTRIBUTES: [&str; 7] = [
    "runId",
    "createdAt",
    "repository",
    "runTags",
    "runTagsUser",
    "runTagsOff",
    "runTagsRev",
];

/// A raw item of the tag-state read, decoded by the same reader as the run list's window ([`decode_run_index_page`]), plus which of the
/// attributes an edit replaces or compares against (`runTagsUser`, `runTagsOff`, `runTagsRev`) were there and could not be read as
/// written. The automatic list is not among them: an edit never replaces it. An item that cannot be read at all is reported as `item`.
fn decode_tag_read(raw: Item, run: &RunItem) -> TagRead {
    let presence = tag_presence(&raw);
    match serde_dynamo::from_item::<_, RunIndexItem>(raw) {
        Ok(item) => {
            let mut unreadable = Vec::new();
            if presence.lists[1] && item.run_tags_user.is_none() {
                unreadable.push("runTagsUser".to_string());
            }
            if presence
                .off
                .is_some_and(|text_list| !text_list || item.run_tags_off.is_none())
            {
                unreadable.push("runTagsOff".to_string());
            }
            if presence.scalars[4] && item.run_tags_rev.is_none() {
                unreadable.push("runTagsRev".to_string());
            }
            TagRead { item, unreadable }
        }
        Err(_) => TagRead {
            item: RunIndexItem {
                run_id: run.run_id.clone(),
                created_at: run.created_at.clone(),
                ..RunIndexItem::default()
            },
            unreadable: vec!["item".to_string()],
        },
    }
}

/// An attribute value of an `UpdateSpec` as the SDK sends it.
fn attribute_value(v: &Av) -> AttributeValue {
    match v {
        Av::S(text) => s(text),
        Av::N(n) => AttributeValue::N(n.to_string()),
        Av::L(items) => AttributeValue::L(items.iter().map(s).collect()),
        Av::Ss(items) => AttributeValue::Ss(items.clone()),
    }
}

fn item_of<T: Serialize + Keyed>(t: &T) -> Result<Item, StoreError> {
    let key = t.key();
    let mut item: Item =
        serde_dynamo::to_item(t).map_err(|e| StoreError::Unavailable(e.to_string()))?;
    item.insert("PK".into(), s(key.pk));
    item.insert("SK".into(), s(key.sk));
    Ok(item)
}

/// A transaction's answer: a failed condition (a concurrent writer, an existing event id) is
/// `Conflict`.
fn transaction_result<T, R>(
    res: Result<T, aws_sdk_dynamodb::error::SdkError<TransactWriteItemsError, R>>,
) -> Result<(), StoreError>
where
    R: std::fmt::Debug,
{
    match res {
        Ok(_) => Ok(()),
        Err(e) => {
            let conditional = matches!(
                e.as_service_error(),
                Some(TransactWriteItemsError::TransactionCanceledException(t))
                    if t.cancellation_reasons().iter()
                        .any(|r| r.code() == Some("ConditionalCheckFailed"))
            );
            if conditional {
                Err(StoreError::Conflict)
            } else {
                Err(unavailable(e))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct DynamoStore {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}

/// The opaque `nextToken`: the caller it was issued to and the GSI1 `LastEvaluatedKey`, whose
/// attributes are all strings.
#[derive(Serialize, Deserialize)]
struct PageToken {
    o: String,
    k: BTreeMap<String, String>,
}

fn encode_token(owner: &str, lek: &Item) -> Result<String, StoreError> {
    let mut k = BTreeMap::new();
    for (name, v) in lek {
        let AttributeValue::S(text) = v else {
            return Err(StoreError::Unavailable(format!(
                "LastEvaluatedKey attribute {name} is not a string"
            )));
        };
        k.insert(name.clone(), text.clone());
    }
    let json = serde_json::to_vec(&PageToken {
        o: owner.to_string(),
        k,
    })
    .map_err(|e| StoreError::Unavailable(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(json))
}

fn decode_token(owner: &str, token: &str) -> Result<Item, StoreError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| StoreError::BadCursor)?;
    let t: PageToken = serde_json::from_slice(&bytes).map_err(|_| StoreError::BadCursor)?;
    let key_owner = t.k.get("ownerPrincipalArn").map(String::as_str);
    if t.o != owner || key_owner != Some(owner) {
        return Err(StoreError::BadCursor);
    }
    for required in ["PK", "SK", "createdAt"] {
        if !t.k.contains_key(required) {
            return Err(StoreError::BadCursor);
        }
    }
    Ok(t.k.into_iter().map(|(n, v)| (n, s(v))).collect())
}

impl DynamoStore {
    /// The message ledger of the run's messages over this table.
    fn ledger(&self) -> Ledger<DynamoTable> {
        Ledger::new(DynamoTable::new(self.client.clone(), self.table.clone()))
    }

    /// The window read of [`Store::run_index_between`]: the owner's search items from `oldest` to `newest`, following the pages. A page
    /// that dropped anything (see [`PageSkips`]) hands one [`skip_line`] to `log`; the trait method logs to stdout, a test passes its own
    /// sink. The items of the page that did decode always join, so one bad item never costs a row its title, model or name.
    pub async fn run_index_window(
        &self,
        owner: &str,
        oldest: &str,
        newest: &str,
        log: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Vec<RunIndexItem>, StoreError> {
        // A window is about a page of runs, so one page of the Query almost always holds it; a window that needs more is a failure.
        let mut items: Vec<RunIndexItem> = Vec::new();
        let mut start: Option<Item> = None;
        for page in 1..=MAX_QUERY_PAGES {
            let out = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                .expression_attribute_values(":pk", s(owner_pk(owner)))
                // `~` sorts above every character of a run id, so the upper bound takes every run created in the newest second.
                .expression_attribute_values(":lo", s(format!("SRCH#{oldest}")))
                .expression_attribute_values(":hi", s(format!("SRCH#{newest}#~")))
                .consistent_read(true)
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            let (page_items, skips) = decode_run_index_page(out.items.unwrap_or_default());
            items.extend(page_items);
            if let Some(line) = skip_line(&skips) {
                log(&line);
            }
            match out.last_evaluated_key {
                Some(lek) if !lek.is_empty() => {
                    if page >= MAX_QUERY_PAGES {
                        return Err(out_of_pages("run index"));
                    }
                    start = Some(lek);
                }
                _ => break,
            }
        }
        Ok(items)
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        pk: String,
        sk: String,
    ) -> Result<Option<T>, StoreError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(pk))
            .key("SK", s(sk))
            .consistent_read(true)
            .send()
            .await
            .map_err(unavailable)?;
        match out.item {
            Some(item) => serde_dynamo::from_item(item)
                .map(Some)
                .map_err(|e| StoreError::Unavailable(e.to_string())),
            None => Ok(None),
        }
    }

    /// `ADD` or `DELETE` of one target in the allowlist listing's string set.
    fn index_update(&self, op: &str, target: &RepoTarget) -> Result<TransactWriteItem, StoreError> {
        Ok(TransactWriteItem::builder()
            .update(
                Update::builder()
                    .table_name(&self.table)
                    .key("PK", s(keys::REPO_ALLOW_INDEX_PK))
                    .key("SK", s(keys::META))
                    .update_expression(format!("{op} targets :t"))
                    .expression_attribute_values(":t", AttributeValue::Ss(vec![target.to_string()]))
                    .build()
                    .map_err(unavailable)?,
            )
            .build())
    }

    /// Every item of the run whose sort key starts with `prefix` (consistent, every page).
    async fn run_items<T: for<'de> Deserialize<'de>>(
        &self,
        run_id: &str,
        prefix: &str,
    ) -> Result<Vec<T>, StoreError> {
        let mut out = Vec::new();
        let mut start: Option<Item> = None;
        loop {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND begins_with(SK, :sk)")
                .expression_attribute_values(":pk", s(keys::run_pk(run_id)))
                .expression_attribute_values(":sk", s(prefix))
                .consistent_read(true)
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            for item in page.items.unwrap_or_default() {
                out.push(
                    serde_dynamo::from_item::<_, T>(item)
                        .map_err(|e| StoreError::Unavailable(e.to_string()))?,
                );
            }
            match page.last_evaluated_key {
                Some(lek) if !lek.is_empty() => start = Some(lek),
                _ => break,
            }
        }
        Ok(out)
    }

    /// An update of the fields run control owns on the item at `key` (its `status` and `version`,
    /// plus `extra` as `(attribute, value)`) while the stored `version` is `version - 1`. Never a
    /// whole-item Put: run-session pins the runtime endpoint on the Run item, which a Put would
    /// erase.
    fn versioned_update(
        &self,
        pk: String,
        sk: &str,
        status: harness_protocol::goal::GoalStatus,
        version: u64,
        extra: Option<(&str, AttributeValue)>,
    ) -> Result<TransactWriteItem, StoreError> {
        self.versioned_update_with(pk, sk, status, version, extra, None)
    }

    /// [`Self::versioned_update`] that also sets `named` through an expression attribute name
    /// (`#<attribute> = :<attribute>`): the reaper's `aborted`.
    fn versioned_update_with(
        &self,
        pk: String,
        sk: &str,
        status: harness_protocol::goal::GoalStatus,
        version: u64,
        extra: Option<(&str, AttributeValue)>,
        named: Option<(&str, AttributeValue)>,
    ) -> Result<TransactWriteItem, StoreError> {
        let word = serde_json::to_value(status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut expr = "SET #status = :status, #v = :next".to_string();
        let mut update = Update::builder()
            .table_name(&self.table)
            .key("PK", s(pk))
            .key("SK", s(sk))
            .condition_expression("#v = :v")
            .expression_attribute_names("#v", "version")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":status", s(word))
            .expression_attribute_values(":next", AttributeValue::N(version.to_string()))
            .expression_attribute_values(
                ":v",
                AttributeValue::N(version.saturating_sub(1).to_string()),
            );
        if let Some((name, value)) = extra {
            expr.push_str(&format!(", {name} = :{name}"));
            update = update.expression_attribute_values(format!(":{name}"), value);
        }
        if let Some((name, value)) = named {
            expr.push_str(&format!(", #{name} = :{name}"));
            update = update
                .expression_attribute_names(format!("#{name}"), name)
                .expression_attribute_values(format!(":{name}"), value);
        }
        let update = update
            .update_expression(expr)
            .build()
            .map_err(unavailable)?;
        Ok(TransactWriteItem::builder().update(update).build())
    }

    fn conditional_put(&self, item: Item) -> Result<TransactWriteItem, StoreError> {
        let put = Put::builder()
            .table_name(&self.table)
            .set_item(Some(item))
            .condition_expression("attribute_not_exists(PK)")
            .build()
            .map_err(unavailable)?;
        Ok(TransactWriteItem::builder().put(put).build())
    }
}

impl Store for DynamoStore {
    async fn claim_idempotency(
        &self,
        claim: &IdempotencyItem,
    ) -> Result<IdempotencyItem, StoreError> {
        let res = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item_of(claim)?))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await;
        match res {
            Ok(_) => Ok(claim.clone()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                let key = claim.key();
                self.get(key.pk, key.sk).await?.ok_or_else(|| {
                    StoreError::Unavailable(
                        "the idempotency claim exists but reads as absent".into(),
                    )
                })
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn create_run(
        &self,
        run: &RunItem,
        goal: &GoalItem,
        created: &Envelope,
    ) -> Result<(), StoreError> {
        let event = EventItem {
            envelope: created.clone(),
        };
        let res = self
            .client
            .transact_write_items()
            .transact_items(self.conditional_put(item_of(run)?)?)
            .transact_items(self.conditional_put(item_of(goal)?)?)
            .transact_items(
                TransactWriteItem::builder()
                    .put(
                        Put::builder()
                            .table_name(&self.table)
                            .set_item(Some(item_of(&event)?))
                            .build()
                            .map_err(unavailable)?,
                    )
                    .build(),
            )
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                let conditional = matches!(
                    e.as_service_error(),
                    Some(TransactWriteItemsError::TransactionCanceledException(t))
                        if t.cancellation_reasons().iter()
                            .any(|r| r.code() == Some("ConditionalCheckFailed"))
                );
                if conditional {
                    Err(StoreError::Conflict)
                } else {
                    Err(unavailable(e))
                }
            }
        }
    }

    async fn get_run(&self, run_id: &str) -> Result<Option<RunItem>, StoreError> {
        self.get(keys::run_pk(run_id), keys::META.into()).await
    }

    async fn get_root_goal(&self, run_id: &str) -> Result<Option<GoalItem>, StoreError> {
        self.get(keys::run_pk(run_id), keys::goal_sk(None)).await
    }

    async fn list_runs(
        &self,
        owner: &str,
        next: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        let start = next.map(|t| decode_token(owner, t)).transpose()?;
        let out = self
            .client
            .query()
            .table_name(&self.table)
            .index_name("GSI1")
            .key_condition_expression("ownerPrincipalArn = :o")
            .expression_attribute_values(":o", s(owner))
            .scan_index_forward(false)
            .limit(RUNS_PAGE)
            .set_exclusive_start_key(start)
            .send()
            .await
            .map_err(unavailable)?;
        // A Run item that does not parse (an older shape) is skipped so it cannot hide the rest.
        let runs = out
            .items
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| serde_dynamo::from_item::<_, RunItem>(item).ok())
            .collect();
        let next = match out.last_evaluated_key {
            Some(lek) if !lek.is_empty() => Some(encode_token(owner, &lek)?),
            _ => None,
        };
        Ok((runs, next))
    }

    async fn events(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        let mut q = self
            .client
            .query()
            .table_name(&self.table)
            .expression_attribute_values(":pk", s(keys::event_pk(run_id, shard)))
            .consistent_read(true)
            .limit(i32::try_from(limit).unwrap_or(i32::MAX));
        q = match after {
            Some(a) => q
                .key_condition_expression("PK = :pk AND SK > :after")
                .expression_attribute_values(":after", s(a)),
            None => q.key_condition_expression("PK = :pk"),
        };
        let out = q.send().await.map_err(unavailable)?;
        // A malformed item is skipped so it cannot stall the cursor.
        Ok(out
            .items
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| serde_dynamo::from_item::<_, EventItem>(item).ok())
            .map(|e| e.envelope)
            .collect())
    }

    async fn list_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        let mut out = Vec::new();
        let mut start: Option<Item> = None;
        loop {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND begins_with(SK, :art)")
                .expression_attribute_values(":pk", s(keys::run_pk(run_id)))
                .expression_attribute_values(":art", s(keys::artifact_sk("")))
                .consistent_read(true)
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            for item in page.items.unwrap_or_default() {
                out.push(
                    serde_dynamo::from_item::<_, ArtifactItem>(item)
                        .map_err(|e| StoreError::Unavailable(e.to_string()))?,
                );
            }
            match page.last_evaluated_key {
                Some(lek) if !lek.is_empty() => start = Some(lek),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
        Ok(out)
    }

    async fn run_usage(&self, run_id: &str) -> Result<RunUsage, StoreError> {
        Ok(RunUsage {
            agents: self.run_items(run_id, &keys::agent_sk("")).await?,
            turns: self.run_items(run_id, "TURN#").await?,
        })
    }

    async fn list_agents(&self, run_id: &str) -> Result<Vec<AgentItem>, StoreError> {
        self.run_items(run_id, &keys::agent_sk("")).await
    }

    async fn update_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        event: Option<&Envelope>,
    ) -> Result<(), StoreError> {
        let budget = match &run.budget {
            Some(b) => Some((
                "budget",
                serde_dynamo::to_attribute_value(b)
                    .map_err(|e| StoreError::Unavailable(e.to_string()))?,
            )),
            None => None,
        };
        let mut tx = self
            .client
            .transact_write_items()
            .transact_items(self.versioned_update(
                keys::run_pk(&run.run_id),
                keys::META,
                run.status,
                run.version,
                budget,
            )?);
        if let Some(g) = goal {
            let tokens = g
                .token_budget
                .map(|t| ("tokenBudget", AttributeValue::N(t.to_string())));
            tx = tx.transact_items(self.versioned_update(
                keys::run_pk(&g.run_id),
                &keys::goal_sk(g.task_id.as_deref()),
                g.status,
                g.version,
                tokens,
            )?);
        }
        if let Some(e) = event {
            let item = item_of(&EventItem {
                envelope: e.clone(),
            })?;
            tx = tx.transact_items(self.conditional_put(item)?);
        }
        transaction_result(tx.send().await)
    }

    async fn get_artifact(
        &self,
        run_id: &str,
        artifact_id: &str,
    ) -> Result<Option<ArtifactItem>, StoreError> {
        self.get(keys::run_pk(run_id), keys::artifact_sk(artifact_id))
            .await
    }

    async fn repo_allow_list(&self) -> Result<Vec<RepoAllowItem>, StoreError> {
        let index = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(keys::REPO_ALLOW_INDEX_PK))
            .key("SK", s(keys::META))
            .consistent_read(true)
            .send()
            .await
            .map_err(unavailable)?;
        let targets: Vec<RepoTarget> = match index.item.as_ref().and_then(|i| i.get("targets")) {
            Some(AttributeValue::Ss(v)) => v.iter().filter_map(|t| t.parse().ok()).collect(),
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for chunk in targets.chunks(100) {
            let mut keys_left: Vec<Item> = chunk
                .iter()
                .map(|t| {
                    HashMap::from([
                        ("PK".to_string(), s(keys::repo_allow_pk(t))),
                        ("SK".to_string(), s(keys::META)),
                    ])
                })
                .collect();
            // BatchGetItem may leave keys unprocessed; they are asked again.
            for _ in 0..5 {
                if keys_left.is_empty() {
                    break;
                }
                let ka = KeysAndAttributes::builder()
                    .set_keys(Some(std::mem::take(&mut keys_left)))
                    .consistent_read(true)
                    .build()
                    .map_err(unavailable)?;
                let page = self
                    .client
                    .batch_get_item()
                    .request_items(&self.table, ka)
                    .send()
                    .await
                    .map_err(unavailable)?;
                for item in page
                    .responses
                    .unwrap_or_default()
                    .remove(&self.table)
                    .unwrap_or_default()
                {
                    if let Ok(i) = serde_dynamo::from_item::<_, RepoAllowItem>(item) {
                        out.push(i);
                    }
                }
                keys_left = page
                    .unprocessed_keys
                    .unwrap_or_default()
                    .remove(&self.table)
                    .map(|k| k.keys)
                    .unwrap_or_default();
            }
            if !keys_left.is_empty() {
                return Err(StoreError::Unavailable(
                    "the allowlist read stayed throttled".into(),
                ));
            }
        }
        out.sort_by_key(|a| a.target.to_string());
        Ok(out)
    }

    async fn repo_allow_put(&self, item: &RepoAllowItem) -> Result<(), StoreError> {
        self.client
            .transact_write_items()
            .transact_items(
                TransactWriteItem::builder()
                    .put(
                        Put::builder()
                            .table_name(&self.table)
                            .set_item(Some(item_of(item)?))
                            .build()
                            .map_err(unavailable)?,
                    )
                    .build(),
            )
            .transact_items(self.index_update("ADD", &item.target)?)
            .send()
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn repo_allow_delete(&self, target: &RepoTarget) -> Result<(), StoreError> {
        self.client
            .transact_write_items()
            .transact_items(
                TransactWriteItem::builder()
                    .delete(
                        Delete::builder()
                            .table_name(&self.table)
                            .key("PK", s(keys::repo_allow_pk(target)))
                            .key("SK", s(keys::META))
                            .build()
                            .map_err(unavailable)?,
                    )
                    .build(),
            )
            .transact_items(self.index_update("DELETE", target)?)
            .send()
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn put_control(&self, control: &ControlItem) -> Result<(), StoreError> {
        let res = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item_of(control)?))
            .condition_expression("attribute_not_exists(PK)")
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

    async fn get_event(
        &self,
        run_id: &str,
        shard: u8,
        event_id: &str,
    ) -> Result<Option<Envelope>, StoreError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(keys::event_pk(run_id, shard)))
            .key("SK", s(event_id))
            .consistent_read(true)
            .send()
            .await
            .map_err(unavailable)?;
        // An item that does not parse reads as absent, as the event cursor skips it.
        Ok(out
            .item
            .and_then(|i| serde_dynamo::from_item::<_, EventItem>(i).ok())
            .map(|e| e.envelope))
    }

    async fn get_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<Option<ControlItem>, StoreError> {
        self.get(keys::run_pk(run_id), keys::control_sk(agent_id, control_id))
            .await
    }

    async fn rearm_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<bool, StoreError> {
        let res = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(keys::run_pk(run_id)))
            .key("SK", s(keys::control_sk(agent_id, control_id)))
            .update_expression("REMOVE deliveredAt")
            .condition_expression("attribute_exists(deliveredAt)")
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

    async fn latest_events(
        &self,
        run_id: &str,
        shard: u8,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        self.events_before(run_id, shard, None, limit).await
    }

    async fn events_before(
        &self,
        run_id: &str,
        shard: u8,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        // DynamoDB refuses a Limit of 0.
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Newest first from DynamoDB. A Query stops at 1 MB with a LastEvaluatedKey however few items that is, so the read goes on for
        // what is still missing; a malformed item is skipped and counts as missing.
        let mut events: Vec<Envelope> = Vec::new();
        let mut start: Option<Item> = None;
        let mut pages = 0;
        loop {
            pages += 1;
            let want = limit - events.len();
            let mut q = self
                .client
                .query()
                .table_name(&self.table)
                .expression_attribute_values(":pk", s(keys::event_pk(run_id, shard)))
                .scan_index_forward(false)
                .consistent_read(true)
                .limit(i32::try_from(want).unwrap_or(i32::MAX))
                .set_exclusive_start_key(start.take());
            q = match before {
                Some(b) => q
                    .key_condition_expression("PK = :pk AND SK < :before")
                    .expression_attribute_values(":before", s(b)),
                None => q.key_condition_expression("PK = :pk"),
            };
            let out = q.send().await.map_err(unavailable)?;
            events.extend(
                out.items
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|item| serde_dynamo::from_item::<_, EventItem>(item).ok())
                    .map(|e| e.envelope),
            );
            match out.last_evaluated_key {
                Some(lek) if !lek.is_empty() && events.len() < limit => {
                    if pages >= MAX_QUERY_PAGES {
                        return Err(out_of_pages("event"));
                    }
                    start = Some(lek);
                }
                _ => break,
            }
        }
        events.reverse();
        Ok(events)
    }

    async fn list_controls(
        &self,
        run_id: &str,
        agent_id: &str,
        oldest: &str,
        newest: &str,
        limit: usize,
    ) -> Result<(Vec<ControlItem>, bool), StoreError> {
        // One more than asked, to know whether there is more.
        let wanted = limit + 1;
        let mut controls: Vec<ControlItem> = Vec::new();
        let mut start: Option<Item> = None;
        let mut pages = 0;
        loop {
            pages += 1;
            let out = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                .expression_attribute_values(":pk", s(keys::run_pk(run_id)))
                .expression_attribute_values(":lo", s(keys::control_sk(agent_id, oldest)))
                .expression_attribute_values(":hi", s(keys::control_sk(agent_id, newest)))
                .consistent_read(true)
                .limit(i32::try_from(wanted - controls.len()).unwrap_or(i32::MAX))
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            // A control of a kind this build cannot read is skipped, as a malformed event is.
            controls.extend(
                out.items
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|item| serde_dynamo::from_item::<_, ControlItem>(item).ok()),
            );
            match out.last_evaluated_key {
                Some(lek) if !lek.is_empty() && controls.len() < wanted => {
                    if pages >= MAX_QUERY_PAGES {
                        return Err(out_of_pages("control"));
                    }
                    start = Some(lek);
                }
                _ => break,
            }
        }
        let more = controls.len() > limit;
        controls.truncate(limit);
        Ok((controls, more))
    }

    async fn run_index_between(
        &self,
        owner: &str,
        oldest: &str,
        newest: &str,
    ) -> Result<Vec<RunIndexItem>, StoreError> {
        // The Lambda's log is stdout, as `main` writes its access lines.
        self.run_index_window(owner, oldest, newest, &mut |line: &str| println!("{line}"))
            .await
    }

    async fn put_run_index(&self, owner: &str, item: &RunIndexItem) -> Result<bool, StoreError> {
        // Every start field is set only where it is not set yet. The outcome and the end time are not here at all: they are the end of the
        // run's, and a retry of the start or the backfill must not undo them. The key carries no `ownerPrincipalArn` (GSI1 is sparse on it).
        let mut sets = vec![
            "runId = if_not_exists(runId, :r)",
            "createdAt = if_not_exists(createdAt, :c)",
            "promptHead = if_not_exists(promptHead, :p)",
            "promptTruncated = if_not_exists(promptTruncated, :pt)",
            "repository = if_not_exists(repository, :repo)",
            "lastActivity = if_not_exists(lastActivity, :la)",
        ];
        let mut update = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(owner)))
            .key("SK", s(search_sk(&item.created_at, &item.run_id)))
            .expression_attribute_values(":r", s(&item.run_id))
            .expression_attribute_values(":c", s(&item.created_at))
            .expression_attribute_values(":p", s(&item.prompt_head))
            .expression_attribute_values(":pt", AttributeValue::Bool(item.prompt_truncated))
            .expression_attribute_values(":repo", s(&item.repository))
            .expression_attribute_values(":la", s(&item.last_activity))
            .return_values(ReturnValue::AllOld);
        if let Some(m) = &item.model {
            sets.push("model = if_not_exists(model, :m)");
            update = update.expression_attribute_values(":m", s(m));
        }
        let out = update
            .update_expression(format!("SET {}", sets.join(", ")))
            .send()
            .await
            .map_err(unavailable)?;
        Ok(out
            .attributes
            .is_none_or(|old| !old.contains_key("promptHead")))
    }

    async fn get_run_tag_state(
        &self,
        owner: &str,
        run: &RunItem,
    ) -> Result<Option<TagRead>, StoreError> {
        // One consistent GetItem: the revision it returns is the base of the compare-and-set that follows.
        let mut get = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(owner)))
            .key("SK", s(search_sk(&run.created_at, &run.run_id)))
            .consistent_read(true)
            .projection_expression(
                TAG_STATE_ATTRIBUTES
                    .iter()
                    .map(|a| format!("#{a}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        for a in TAG_STATE_ATTRIBUTES {
            get = get.expression_attribute_names(format!("#{a}"), a);
        }
        let out = get.send().await.map_err(unavailable)?;
        Ok(out.item.map(|raw| decode_tag_read(raw, run)))
    }

    async fn put_run_tags(
        &self,
        owner: &str,
        run: &RunItem,
        spec: &UpdateSpec,
    ) -> Result<(), StoreError> {
        // The spec is sent as the library built it. The key is the caller's partition and the run's own creation time, the same sort key the
        // start writes, so an edit before the start's write makes the item the start then completes.
        let mut update = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", s(owner_pk(owner)))
            .key("SK", s(search_sk(&run.created_at, &run.run_id)))
            .update_expression(&spec.expression);
        if let Some(condition) = &spec.condition {
            update = update.condition_expression(condition);
        }
        for (alias, name) in &spec.names {
            update = update.expression_attribute_names(alias, name);
        }
        for (alias, value) in &spec.values {
            update = update.expression_attribute_values(alias, attribute_value(value));
        }
        match update.send().await {
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

    async fn message_admit(
        &self,
        run_id: &str,
        principal: &str,
        req: &SendMessageRequest,
    ) -> Result<Admitted, LedgerError> {
        self.ledger().admit(run_id, principal, req).await
    }

    async fn message_get(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageView>, LedgerError> {
        self.ledger().get_message(run_id, message_id).await
    }

    async fn message_list(&self, run_id: &str) -> Result<MessagesPage, LedgerError> {
        self.ledger().list(run_id).await
    }

    async fn message_withdraw(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<MessageView, LedgerError> {
        self.ledger().withdraw(run_id, message_id).await
    }

    async fn message_close(
        &self,
        run_id: &str,
        reason: CloseReason,
    ) -> Result<Closed, LedgerError> {
        self.ledger().close(run_id, reason).await
    }

    /// Reads the Agent item's `features` and the Turn item's `transcript` summary as raw attributes, so an image that predates them
    /// reads as one that writes no transcript. Both reads are consistent.
    async fn transcript_facts(
        &self,
        run_id: &str,
    ) -> Result<crate::transcript::TranscriptFacts, StoreError> {
        use crate::transcript::{SummaryFact, TranscriptFacts};
        let agent: Option<Value> = self
            .get(keys::run_pk(run_id), keys::agent_sk(DIRECT_AGENT_ID))
            .await?;
        let supported = agent
            .as_ref()
            .and_then(|a| a.get("features"))
            .and_then(Value::as_array)
            .is_some_and(|f| f.iter().any(|w| w.as_str() == Some(TRANSCRIPT_FEATURE)));
        if !supported {
            return Ok(TranscriptFacts::default());
        }
        let turn: Option<Value> = self
            .get(
                keys::run_pk(run_id),
                keys::turn_sk(DIRECT_AGENT_ID, DIRECT_TURN),
            )
            .await?;
        let summary = match turn
            .as_ref()
            .and_then(|t| t.get("transcript"))
            .filter(|v| !v.is_null())
        {
            None => SummaryFact::Absent,
            Some(v) => match serde_json::from_value::<TranscriptSummary>(v.clone()) {
                Ok(s) if s.validate() => SummaryFact::Valid(s),
                _ => SummaryFact::Invalid,
            },
        };
        Ok(TranscriptFacts {
            supported: true,
            purged: false,
            summary,
        })
    }

    /// The run index's items of one page of the owner's runs (one Query, newest first, eventually consistent; small items), each joined
    /// to its chat body (the Queries of the chat items over the window the page spans, a page of them about every 30 runs). Items the
    /// search cannot read are left out and still counted as examined.
    ///
    /// The bodies are items of their own because the run list reads the run index's items whole: a 32 KiB body in each would make a page
    /// of 100 runs read up to 3.2 MB.
    async fn search_items(
        &self,
        owner_pk: &str,
        sk_floor: &str,
        sk_after: Option<&str>,
        limit: usize,
    ) -> Result<crate::search::SearchPage, StoreError> {
        let mut q = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
            .expression_attribute_values(":pk", s(owner_pk))
            .expression_attribute_values(":lo", s(sk_floor))
            .expression_attribute_values(":hi", s(SK_CEILING))
            .scan_index_forward(false)
            .limit(i32::try_from(limit.clamp(1, 1000)).unwrap_or(100));
        if let Some(after) = sk_after {
            q = q.set_exclusive_start_key(Some(HashMap::from([
                ("PK".to_string(), s(owner_pk)),
                ("SK".to_string(), s(after)),
            ])));
        }
        let out = q.send().await.map_err(unavailable)?;
        let raw = out.items();
        // An item that cannot be decoded still counts as examined, and its sort key is where a search that goes on goes on after.
        let mut items: Vec<SearchItem> = raw
            .iter()
            .filter_map(|i| serde_dynamo::from_item::<_, SearchItem>(i.clone()).ok())
            .collect();
        let last_sk = raw.last().and_then(|i| match i.get("SK") {
            Some(AttributeValue::S(sk)) => Some(sk.clone()),
            _ => None,
        });
        let next_sk = out.last_evaluated_key().and_then(|k| match k.get("SK") {
            Some(AttributeValue::S(sk)) => Some(sk.clone()),
            _ => None,
        });
        let examined = u32::try_from(raw.len()).unwrap_or(u32::MAX);
        // The bodies of the window the readable items span.
        let oldest = items.iter().map(|i| i.created_at.as_str()).min();
        let newest = items.iter().map(|i| i.created_at.as_str()).max();
        if let (Some(oldest), Some(newest)) = (oldest, newest) {
            let bodies = self
                .chat_bodies(owner_pk, &chat_floor(oldest), &chat_ceiling(newest))
                .await?;
            for item in &mut items {
                if let Some(chat) = bodies.get(&item.run_id) {
                    item.body.clone_from(&chat.body);
                    item.segs.clone_from(&chat.segs);
                    item.coverage = chat.coverage;
                    item.body_through = chat.body_through;
                    item.indexed_at.clone_from(&chat.indexed_at);
                }
            }
        }
        Ok(crate::search::SearchPage {
            items,
            examined,
            last_sk,
            next_sk,
        })
    }
}

/// The most pages of chat bodies one search read follows. A page holds about 30 bodies of 32 KiB, so a read of the 100 runs it asks the
/// run index for ends in four; a read that needs more than this is a fault.
pub const SEARCH_BODY_MAX_PAGES: usize = 12;

impl DynamoStore {
    /// The chat items of the owner's runs created between the two keys, by run id (eventually consistent, no filter, no index).
    async fn chat_bodies(
        &self,
        owner_pk: &str,
        lo: &str,
        hi: &str,
    ) -> Result<HashMap<String, SearchItem>, StoreError> {
        let mut bodies: HashMap<String, SearchItem> = HashMap::new();
        let mut start: Option<Item> = None;
        for page in 1..=SEARCH_BODY_MAX_PAGES {
            let out = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("PK = :pk AND SK BETWEEN :lo AND :hi")
                .expression_attribute_values(":pk", s(owner_pk))
                .expression_attribute_values(":lo", s(lo))
                .expression_attribute_values(":hi", s(hi))
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(unavailable)?;
            for item in out.items() {
                if let Ok(chat) = serde_dynamo::from_item::<_, SearchItem>(item.clone())
                    && !chat.run_id.is_empty()
                {
                    bodies.insert(chat.run_id.clone(), chat);
                }
            }
            match out.last_evaluated_key() {
                Some(lek) if !lek.is_empty() => {
                    if page >= SEARCH_BODY_MAX_PAGES {
                        return Err(StoreError::Unavailable(format!(
                            "the chat bodies read needs more than {SEARCH_BODY_MAX_PAGES} pages"
                        )));
                    }
                    start = Some(lek.clone());
                }
                _ => break,
            }
        }
        Ok(bodies)
    }
}

#[derive(Clone, Debug)]
pub struct S3Blobs {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
    /// The client's credentials provider, read for the expiry of the credentials a URL is signed
    /// with (the S3 client does not expose it).
    pub credentials: Option<SharedCredentialsProvider>,
}

impl Blobs for S3Blobs {
    async fn put(&self, key: &str, body: &[u8]) -> Result<String, StoreError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/json")
            .body(body.to_vec().into())
            .send()
            .await
            .map_err(unavailable)?;
        Ok(format!("s3://{}/{key}", self.bucket))
    }

    fn bucket(&self) -> &str {
        &self.bucket
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        // One listing whose prefix is the key and which asks for one entry: keys sort after their own prefix, so the key itself, when it
        // exists, is the first entry; a longer key that merely starts with it is not the key.
        let out = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(key)
            .max_keys(1)
            .send()
            .await
            .map_err(unavailable)?;
        Ok(out.contents().iter().any(|o| o.key() == Some(key)))
    }

    async fn read_prefix(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<Option<ObjectPrefix>, StoreError> {
        let res = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(format!("bytes=0-{}", max_bytes.saturating_sub(1)))
            .send()
            .await;
        let out = match res {
            Ok(o) => o,
            Err(e) => {
                // A missing key is no object; a ranged GET of an empty object is S3's 416 `InvalidRange`: an object of 0 bytes.
                let code = e
                    .as_service_error()
                    .and_then(aws_sdk_s3::error::ProvideErrorMetadata::code);
                return match code {
                    Some("NoSuchKey") => Ok(None),
                    Some("InvalidRange") => Ok(Some(ObjectPrefix {
                        bytes: Vec::new(),
                        size: 0,
                        etag: None,
                        version_id: None,
                        last_modified: None,
                    })),
                    _ => Err(unavailable(e)),
                };
            }
        };
        let etag = out.e_tag().map(str::to_string);
        let version_id = out.version_id().map(str::to_string);
        let last_modified = out
            .last_modified()
            .map(|t| rfc3339(u64::try_from(t.secs()).unwrap_or(0)));
        // `Content-Range: bytes 0-15/123456` carries the whole size; a store that ignored the range sends the whole object.
        let total = out
            .content_range()
            .and_then(|r| r.rsplit('/').next())
            .and_then(|n| n.parse::<u64>().ok());
        let content_length = out.content_length();
        // A store that ignored the range would send the whole object: only the first `max_bytes` are read, and the rest is dropped unread.
        let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let mut body = out.body;
        let mut bytes: Vec<u8> = Vec::new();
        while bytes.len() < cap {
            match body.try_next().await.map_err(unavailable)? {
                Some(chunk) => bytes.extend_from_slice(&chunk),
                None => break,
            }
        }
        bytes.truncate(cap);
        let size = total
            .or_else(|| content_length.and_then(|n| u64::try_from(n).ok()))
            .unwrap_or(bytes.len() as u64);
        Ok(Some(ObjectPrefix {
            bytes,
            size,
            etag,
            version_id,
            last_modified,
        }))
    }

    /// Signed now for `ttl`. A URL signed with session credentials stops working when they
    /// expire, so the reported expiry is the sooner of the two; a provider that reports no expiry
    /// (the Lambda environment variables) leaves `ttl`.
    async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
    ) -> Result<DownloadUrlResponse, StoreError> {
        self.presign_get_version(key, None, ttl).await
    }

    async fn presign_get_version(
        &self,
        key: &str,
        version_id: Option<&str>,
        ttl: Duration,
    ) -> Result<DownloadUrlResponse, StoreError> {
        let now = SystemTime::now();
        let config = PresigningConfig::builder()
            .start_time(now)
            .expires_in(ttl)
            .build()
            .map_err(unavailable)?;
        let signed = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .set_version_id(version_id.map(str::to_string))
            .presigned(config)
            .await
            .map_err(unavailable)?;
        let mut expires = now + ttl;
        if let Some(provider) = &self.credentials
            && let Ok(creds) = provider.provide_credentials().await
            && let Some(lapse) = creds.expiry()
        {
            expires = expires.min(lapse);
        }
        // Whole seconds, rounded down, so the reported expiry is never late.
        let secs = expires
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(DownloadUrlResponse {
            url: signed.uri().to_string(),
            expires_at: rfc3339(secs),
        })
    }

    /// ListObjectsV2 with `StartAfter`; the keys, sizes and ETags of one page, in key order.
    async fn list_objects(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<ObjectPage, StoreError> {
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .max_keys(i32::try_from(max_keys.clamp(1, 1000)).unwrap_or(1000));
        if let Some(after) = start_after {
            req = req.start_after(after);
        }
        let out = req.send().await.map_err(unavailable)?;
        let objects = out
            .contents()
            .iter()
            .filter_map(|o| {
                Some(ObjectInfo {
                    key: o.key()?.to_string(),
                    size: u64::try_from(o.size().unwrap_or(0)).unwrap_or(0),
                    etag: o.e_tag().map(str::to_string),
                })
            })
            .collect();
        Ok(ObjectPage {
            objects,
            truncated: out.is_truncated().unwrap_or(false),
        })
    }

    /// GetObject, refused above `max_bytes` by its length and again while streaming, and only while its ETag is `if_match`.
    async fn get_object(
        &self,
        key: &str,
        max_bytes: usize,
        if_match: Option<&str>,
    ) -> Result<Option<Vec<u8>>, ObjectError> {
        use aws_sdk_s3::error::ProvideErrorMetadata as _;
        let mut req = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some(etag) = if_match {
            req = req.if_match(etag);
        }
        match req.send().await {
            Ok(out) => {
                if out
                    .content_length()
                    .is_some_and(|l| l > i64::try_from(max_bytes).unwrap_or(i64::MAX))
                {
                    return Err(ObjectError::TooLarge);
                }
                let mut body = out.body;
                let mut buf: Vec<u8> = Vec::new();
                while let Some(part) = body
                    .try_next()
                    .await
                    .map_err(|e| ObjectError::Store(unavailable(e)))?
                {
                    if buf.len() + part.len() > max_bytes {
                        return Err(ObjectError::TooLarge);
                    }
                    buf.extend_from_slice(&part);
                }
                Ok(Some(buf))
            }
            Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => Ok(None),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.code() == Some("PreconditionFailed")) =>
            {
                Err(ObjectError::Changed)
            }
            Err(e) => Err(ObjectError::Store(unavailable(e))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SfnExecution {
    pub client: aws_sdk_sfn::Client,
    pub state_machine_arn: String,
    pub ssm: aws_sdk_ssm::Client,
    /// `/hamh/<stage>`: the deploy writes `<prefix>/deployment-digest` and
    /// `<prefix>/deployed-source-tree` after every stack is up.
    pub deployment_prefix: String,
}

impl Execution for SfnExecution {
    async fn start(&self, name: &str, input: &Value) -> Result<(), StoreError> {
        let res = self
            .client
            .start_execution()
            .state_machine_arn(&self.state_machine_arn)
            .name(name)
            .input(input.to_string())
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            // Names are unique per state machine and the input is the run id alone, so an existing
            // execution of this name is this run's.
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_execution_already_exists()) =>
            {
                Ok(())
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn stop(&self, name: &str, cause: &str) -> Result<(), StoreError> {
        let res = self
            .client
            .stop_execution()
            .execution_arn(execution_arn(&self.state_machine_arn, name))
            .cause(cause)
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            // A run whose execution never started has nothing to stop.
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_execution_does_not_exist()) =>
            {
                Ok(())
            }
            Err(e) => Err(unavailable(e)),
        }
    }

    async fn instances_available(&self) -> Result<bool, StoreError> {
        let name = format!(
            "{}/worker-python-instances-runtime-arn",
            self.deployment_prefix
        );
        match self.ssm.get_parameter().name(&name).send().await {
            Ok(out) => Ok(out
                .parameter()
                .and_then(|p| p.value())
                .is_some_and(|v| v.starts_with("arn:aws:bedrock-agentcore:"))),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|s| s.is_parameter_not_found()) =>
            {
                Ok(false)
            }
            // A throttle, a fault of SSM or no answer may pass on a retry. Anything else SSM refuses (a role without `ssm:GetParameter` on the
            // parameter answers AccessDenied before it looks for the parameter) is the stage's own setup, and a retry cannot help.
            Err(e) if may_pass(&e) => Err(unavailable(e)),
            Err(e) => Err(StoreError::NotSetUp(DisplayErrorContext(e).to_string())),
        }
    }

    async fn deployment(&self) -> Result<DeployedAs, StoreError> {
        let digest = format!("{}/deployment-digest", self.deployment_prefix);
        let tree = format!("{}/deployed-source-tree", self.deployment_prefix);
        let out = self
            .ssm
            .get_parameters()
            .names(&digest)
            .names(&tree)
            .send()
            .await
            .map_err(unavailable)?;
        let value = |name: &str| {
            out.parameters()
                .iter()
                .find(|p| p.name() == Some(name))
                .and_then(|p| p.value())
                .map(str::to_string)
        };
        Ok(DeployedAs {
            digest: value(&digest),
            source_tree: value(&tree),
        })
    }
}

/// The ARN of execution `name` of the state machine `state_machine_arn`
/// (`arn:aws:states:<region>:<account>:stateMachine:<machine>` to `...:execution:<machine>:<name>`).
pub fn execution_arn(state_machine_arn: &str, name: &str) -> String {
    format!(
        "{}:{name}",
        state_machine_arn.replacen(":stateMachine:", ":execution:", 1)
    )
}
