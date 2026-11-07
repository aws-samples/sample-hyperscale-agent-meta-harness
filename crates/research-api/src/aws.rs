//! The AWS implementations of the ports: the research table (DynamoDB), the harness bucket (S3) and the asynchronous start of
//! `research-job` (Lambda). Errors become [`Fail`] with no service detail beyond a short kind; nothing here logs a request or a body.

use crate::ports::{
    Blobs, Clock, Delta, Fail, IdKind, Idem, Invoker, JobEvent, JobRecord, PutOutcome, SystemClock,
    Table,
};
use aws_sdk_dynamodb::types::{AttributeValue, PutRequest, ReturnValue, WriteRequest};
use harness_protocol::research::{
    ItemRef, JobState, PolicyItem, ResearchError, ResearchErrorCode, ResearchLimits, ResearchUsage,
};
use std::collections::HashMap;

type Item = HashMap<String, AttributeValue>;

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}

fn n(v: u64) -> AttributeValue {
    AttributeValue::N(v.to_string())
}

fn run_pk(run: &str) -> String {
    PolicyItem::pk_for(run)
}

fn unavailable(what: &str) -> Fail {
    Fail::internal(format!("the research table could not {what}"))
}

/// Seconds a counter or rate item is kept: DynamoDB's TTL removes it later.
const COUNTER_KEEP_S: u64 = 14 * 24 * 3600;

fn num(item: &Item, name: &str) -> u64 {
    item.get(name)
        .and_then(|v| v.as_n().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn usage_of(item: &Item) -> ResearchUsage {
    ResearchUsage {
        searches: u32::try_from(num(item, "searches")).unwrap_or(u32::MAX),
        fetched_urls: u32::try_from(num(item, "fetchedUrls")).unwrap_or(u32::MAX),
        repos: u32::try_from(num(item, "repos")).unwrap_or(u32::MAX),
        outbound_bytes: num(item, "outboundBytes"),
        context_bytes: num(item, "contextBytes"),
    }
}

/// One counter of the reserve expression: the attribute, what is added and the cap, and the word used when the cap holds the call.
struct Part {
    attr: &'static str,
    add: u64,
    cap: u64,
    code: ResearchErrorCode,
    what: &'static str,
}

fn parts(d: &Delta, l: &ResearchLimits) -> Vec<Part> {
    use ResearchErrorCode::{LimitBytes, LimitCalls};
    [
        (
            "searches",
            u64::from(d.searches),
            u64::from(l.searches),
            LimitCalls,
            "the search count",
        ),
        (
            "fetchedUrls",
            u64::from(d.fetched_urls),
            u64::from(l.fetched_urls),
            LimitCalls,
            "the fetched-URL count",
        ),
        (
            "repos",
            u64::from(d.repos),
            u64::from(l.repos),
            LimitCalls,
            "the repository count",
        ),
        (
            "outboundBytes",
            d.outbound_bytes,
            l.outbound_bytes,
            LimitBytes,
            "the outbound text budget",
        ),
        (
            "contextBytes",
            d.context_bytes,
            l.context_bytes,
            LimitBytes,
            "the research storage budget",
        ),
    ]
    .into_iter()
    .filter(|(_, add, ..)| *add > 0)
    .map(|(attr, add, cap, code, what)| Part {
        attr,
        add,
        cap,
        code,
        what,
    })
    .collect()
}

/// The update and condition expressions of `reserve`, built from the non-zero parts. Public so a test can pin them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservePlan {
    pub update: String,
    pub condition: String,
    /// `:name` to number.
    pub values: Vec<(String, u64)>,
}

/// `Err` when one part alone is larger than its cap (the call can never fit, and a negative bound would let a missing attribute through).
pub fn reserve_plan(d: &Delta, l: &ResearchLimits) -> Result<ReservePlan, Fail> {
    let ps = parts(d, l);
    if ps.is_empty() {
        return Err(Fail::internal("reserve with nothing to reserve"));
    }
    let mut adds = Vec::new();
    let mut conds = Vec::new();
    let mut values = Vec::new();
    for p in &ps {
        if p.add > p.cap {
            return Err(Fail::new(
                p.code,
                format!("{} is used up for this run", p.what),
            ));
        }
        adds.push(format!("{a} :add_{a}", a = p.attr));
        conds.push(format!(
            "(attribute_not_exists({a}) OR {a} <= :max_{a})",
            a = p.attr
        ));
        values.push((format!(":add_{}", p.attr), p.add));
        values.push((format!(":max_{}", p.attr), p.cap - p.add));
    }
    Ok(ReservePlan {
        update: format!(
            "ADD {} SET #ttl = if_not_exists(#ttl, :ttl)",
            adds.join(", ")
        ),
        condition: conds.join(" AND "),
        values,
    })
}

/// The research table in DynamoDB. Keys: `PK` = `run#<run>` (or `stage`), `SK` = `cfg`, `ctr`, `rate#<minute>`, `idem#<key>`,
/// `seen#<hash>`, `job#<id>`.
#[derive(Clone, Debug)]
pub struct DynamoTable {
    pub client: aws_sdk_dynamodb::Client,
    pub table: String,
}

impl DynamoTable {
    fn key(&self, pk: &str, sk: &str) -> (AttributeValue, AttributeValue) {
        (s(pk), s(sk))
    }

    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, Fail> {
        let (p, k) = self.key(pk, sk);
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", p)
            .key("SK", k)
            .consistent_read(true)
            .send()
            .await
            .map_err(|_| unavailable("read an item"))?;
        Ok(out.item)
    }

    /// `ADD calls 1` while the window holds fewer than `cap`.
    async fn tick(&self, pk: &str, sk: &str, cap: u32, ttl: u64) -> Result<(), Fail> {
        let (p, k) = self.key(pk, sk);
        let r = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", p)
            .key("SK", k)
            .update_expression("ADD calls :one SET #ttl = :ttl")
            .condition_expression("attribute_not_exists(calls) OR calls < :cap")
            .expression_attribute_names("#ttl", "ttl")
            .expression_attribute_values(":one", n(1))
            .expression_attribute_values(":cap", n(u64::from(cap)))
            .expression_attribute_values(":ttl", n(ttl))
            .send()
            .await;
        match r {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Err(Fail::new(
                    ResearchErrorCode::LimitRate,
                    format!("{cap} calls a minute"),
                )
                .with_retry_after(30))
            }
            Err(_) => Err(unavailable("count a call")),
        }
    }
}

impl Table for DynamoTable {
    async fn get_policy(&self, run: &str) -> Result<Option<PolicyItem>, Fail> {
        match self.get(&run_pk(run), "cfg").await? {
            None => Ok(None),
            Some(item) => serde_dynamo::from_item(item)
                .map(Some)
                .map_err(|_| Fail::internal("the policy row could not be read")),
        }
    }

    async fn reserve(
        &self,
        run: &str,
        d: &Delta,
        limits: &ResearchLimits,
    ) -> Result<ResearchUsage, Fail> {
        let plan = reserve_plan(d, limits)?;
        let ttl = SystemClock.now_ms() / 1000 + COUNTER_KEEP_S;
        let (p, k) = self.key(&run_pk(run), "ctr");
        let mut req = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", p)
            .key("SK", k)
            .update_expression(&plan.update)
            .condition_expression(&plan.condition)
            .expression_attribute_names("#ttl", "ttl")
            .expression_attribute_values(":ttl", n(ttl))
            .return_values(ReturnValue::AllNew);
        for (name, v) in &plan.values {
            req = req.expression_attribute_values(name, n(*v));
        }
        match req.send().await {
            Ok(out) => Ok(usage_of(&out.attributes.unwrap_or_default())),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                // Read the counters to say which cap held the call.
                let now = self.get(&run_pk(run), "ctr").await?.unwrap_or_default();
                let used = usage_of(&now);
                let held = [
                    (
                        d.searches > 0
                            && u64::from(used.searches) + u64::from(d.searches)
                                > u64::from(limits.searches),
                        ResearchErrorCode::LimitCalls,
                        "the search count",
                    ),
                    (
                        d.fetched_urls > 0
                            && u64::from(used.fetched_urls) + u64::from(d.fetched_urls)
                                > u64::from(limits.fetched_urls),
                        ResearchErrorCode::LimitCalls,
                        "the fetched-URL count",
                    ),
                    (
                        d.repos > 0
                            && u64::from(used.repos) + u64::from(d.repos) > u64::from(limits.repos),
                        ResearchErrorCode::LimitCalls,
                        "the repository count",
                    ),
                    (
                        d.outbound_bytes > 0
                            && used.outbound_bytes + d.outbound_bytes > limits.outbound_bytes,
                        ResearchErrorCode::LimitBytes,
                        "the outbound text budget",
                    ),
                    (
                        d.context_bytes > 0
                            && used.context_bytes + d.context_bytes > limits.context_bytes,
                        ResearchErrorCode::LimitBytes,
                        "the research storage budget",
                    ),
                ]
                .into_iter()
                .find(|(over, ..)| *over);
                let (code, what) = held
                    .map_or((ResearchErrorCode::LimitCalls, "a limit"), |(_, c, w)| {
                        (c, w)
                    });
                Err(Fail::new(code, format!("{what} is used up for this run")))
            }
            Err(_) => Err(unavailable("reserve a count")),
        }
    }

    async fn rate_tick(&self, run: &str, minute: u64, cap: u32) -> Result<(), Fail> {
        self.tick(
            &run_pk(run),
            &format!("rate#{minute}"),
            cap,
            (minute + 3) * 60,
        )
        .await
    }

    async fn stage_tick(&self, minute: u64, cap: u32) -> Result<(), Fail> {
        self.tick("stage", &format!("lim#{minute}"), cap, (minute + 3) * 60)
            .await
    }

    async fn usage(&self, run: &str) -> Result<ResearchUsage, Fail> {
        Ok(self
            .get(&run_pk(run), "ctr")
            .await?
            .map(|i| usage_of(&i))
            .unwrap_or_default())
    }

    async fn next_id(&self, run: &str, kind: IdKind) -> Result<u32, Fail> {
        let attr = match kind {
            IdKind::Item => "itemSeq",
            IdKind::Search => "searchSeq",
        };
        let ttl = SystemClock.now_ms() / 1000 + COUNTER_KEEP_S;
        let (p, k) = self.key(&run_pk(run), "ctr");
        let out = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", p)
            .key("SK", k)
            .update_expression(format!(
                "ADD {attr} :one SET #ttl = if_not_exists(#ttl, :ttl)"
            ))
            .expression_attribute_names("#ttl", "ttl")
            .expression_attribute_values(":one", n(1))
            .expression_attribute_values(":ttl", n(ttl))
            .return_values(ReturnValue::UpdatedNew)
            .send()
            .await
            .map_err(|_| unavailable("number an item"))?;
        u32::try_from(num(&out.attributes.unwrap_or_default(), attr))
            .map_err(|_| Fail::internal("sequence overflow"))
    }

    async fn idem_get(&self, run: &str, key: &str, req_hash: &str) -> Result<Idem, Fail> {
        Ok(
            match self.get(&run_pk(run), &format!("idem#{key}")).await? {
                None => Idem::Fresh,
                Some(item) => {
                    let stored = item
                        .get("reqHash")
                        .and_then(|v| v.as_s().ok())
                        .map(String::as_str);
                    if stored == Some(req_hash) {
                        Idem::Replay(
                            item.get("response")
                                .and_then(|v| v.as_s().ok())
                                .cloned()
                                .unwrap_or_default(),
                        )
                    } else {
                        Idem::Conflict
                    }
                }
            },
        )
    }

    async fn idem_put(
        &self,
        run: &str,
        key: &str,
        req_hash: &str,
        response: &str,
        expires_s: u64,
    ) -> Result<(), Fail> {
        let (p, k) = self.key(&run_pk(run), &format!("idem#{key}"));
        let r = self
            .client
            .put_item()
            .table_name(&self.table)
            .item("PK", p)
            .item("SK", k)
            .item("reqHash", s(req_hash))
            .item("response", s(response))
            .item("ttl", n(expires_s))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await;
        match r {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(())
            }
            Err(_) => Err(unavailable("store an answer")),
        }
    }

    async fn seen_put(&self, run: &str, hashes: &[String], expires_s: u64) -> Result<(), Fail> {
        // BatchWriteItem refuses two writes to one key in a batch: each hash is written once.
        let mut unique: Vec<&String> = Vec::with_capacity(hashes.len());
        let mut seen = std::collections::HashSet::new();
        for h in hashes {
            if seen.insert(h) {
                unique.push(h);
            }
        }
        for chunk in unique.chunks(25) {
            let mut writes: Vec<WriteRequest> = chunk
                .iter()
                .map(|h| {
                    let put = PutRequest::builder()
                        .item("PK", s(run_pk(run)))
                        .item("SK", s(format!("seen#{h}")))
                        .item("ttl", n(expires_s))
                        .build();
                    match put {
                        Ok(p) => Ok(WriteRequest::builder().put_request(p).build()),
                        Err(_) => Err(Fail::internal("seen item")),
                    }
                })
                .collect::<Result<_, _>>()?;
            for attempt in 0..4u64 {
                let out = self
                    .client
                    .batch_write_item()
                    .request_items(&self.table, writes.clone())
                    .send()
                    .await
                    .map_err(|_| unavailable("record seen addresses"))?;
                writes = out
                    .unprocessed_items
                    .and_then(|mut m| m.remove(&self.table))
                    .unwrap_or_default();
                if writes.is_empty() {
                    break;
                }
                if attempt == 3 {
                    return Err(unavailable("record every seen address"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50 << attempt)).await;
            }
        }
        Ok(())
    }

    async fn seen_get(&self, run: &str, hashes: &[String]) -> Result<Vec<String>, Fail> {
        use aws_sdk_dynamodb::types::KeysAndAttributes;
        let mut found = Vec::new();
        let mut unique: Vec<&String> = Vec::with_capacity(hashes.len());
        let mut once = std::collections::HashSet::new();
        for h in hashes {
            if once.insert(h) {
                unique.push(h);
            }
        }
        for chunk in unique.chunks(100) {
            let mut pending: Vec<HashMap<String, AttributeValue>> = chunk
                .iter()
                .map(|h| {
                    HashMap::from([
                        ("PK".to_string(), s(run_pk(run))),
                        ("SK".to_string(), s(format!("seen#{h}"))),
                    ])
                })
                .collect();
            // A throttled batch comes back partly answered with the keys it left: those are asked again, a few times.
            for attempt in 0..4u64 {
                let keys = KeysAndAttributes::builder()
                    .consistent_read(true)
                    .set_keys(Some(pending.clone()))
                    .build()
                    .map_err(|_| Fail::internal("seen keys"))?;
                let out = self
                    .client
                    .batch_get_item()
                    .request_items(&self.table, keys)
                    .send()
                    .await
                    .map_err(|_| unavailable("read seen addresses"))?;
                for item in out
                    .responses
                    .and_then(|mut m| m.remove(&self.table))
                    .unwrap_or_default()
                {
                    if let Some(h) = item
                        .get("SK")
                        .and_then(|v| v.as_s().ok())
                        .and_then(|v| v.strip_prefix("seen#"))
                    {
                        found.push(h.to_string());
                    }
                }
                pending = out
                    .unprocessed_keys
                    .and_then(|mut m| m.remove(&self.table))
                    .map(|k| k.keys)
                    .unwrap_or_default();
                if pending.is_empty() {
                    break;
                }
                if attempt == 3 {
                    return Err(unavailable("read every seen address"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50 << attempt)).await;
            }
        }
        Ok(found)
    }

    async fn job_create(&self, job: &JobRecord) -> Result<bool, Fail> {
        let mut item: Item = serde_dynamo::to_item(job).map_err(|_| Fail::internal("job item"))?;
        item.insert("PK".into(), s(run_pk(&job.run)));
        item.insert("SK".into(), s(format!("job#{}", job.job_id)));
        item.insert("ttl".into(), n(job.created_s + COUNTER_KEEP_S));
        let r = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .condition_expression("attribute_not_exists(PK)")
            .send()
            .await;
        match r {
            Ok(_) => Ok(true),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(false)
            }
            Err(_) => Err(unavailable("create a job")),
        }
    }

    async fn job_get(&self, run: &str, job_id: &str) -> Result<Option<JobRecord>, Fail> {
        match self.get(&run_pk(run), &format!("job#{job_id}")).await? {
            None => Ok(None),
            Some(item) => serde_dynamo::from_item(item)
                .map(Some)
                .map_err(|_| Fail::internal("the job item could not be read")),
        }
    }

    async fn job_move(
        &self,
        run: &str,
        job_id: &str,
        from: &[JobState],
        to: JobState,
        now_s: u64,
        item: Option<&ItemRef>,
        error: Option<&ResearchError>,
    ) -> Result<bool, Fail> {
        let state = |st: JobState| -> Result<String, Fail> {
            serde_json::to_value(st)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .ok_or_else(|| Fail::internal("job state"))
        };
        let (p, k) = self.key(&run_pk(run), &format!("job#{job_id}"));
        // `state`, `item` and `error` are DynamoDB reserved words: every use goes through a name alias.
        let mut sets = vec!["#state = :to", "updatedS = :now"];
        let mut removes: Vec<&str> = Vec::new();
        let mut req = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("PK", p)
            .key("SK", k)
            .expression_attribute_names("#state", "state")
            .expression_attribute_names("#item", "item")
            .expression_attribute_names("#error", "error")
            .expression_attribute_values(":to", s(state(to)?))
            .expression_attribute_values(":now", n(now_s));
        let mut ins = Vec::new();
        for (i, f) in from.iter().enumerate() {
            let name = format!(":from{i}");
            req = req.expression_attribute_values(&name, s(state(*f)?));
            ins.push(name);
        }
        match item {
            Some(i) => {
                let av: AttributeValue =
                    serde_dynamo::to_attribute_value(i).map_err(|_| Fail::internal("item"))?;
                req = req.expression_attribute_values(":item", av);
                sets.push("#item = :item");
            }
            None => removes.push("#item"),
        }
        match error {
            Some(e) => {
                let av: AttributeValue =
                    serde_dynamo::to_attribute_value(e).map_err(|_| Fail::internal("error"))?;
                req = req.expression_attribute_values(":error", av);
                sets.push("#error = :error");
            }
            None => removes.push("#error"),
        }
        let mut update = format!("SET {}", sets.join(", "));
        if !removes.is_empty() {
            update.push_str(&format!(" REMOVE {}", removes.join(", ")));
        }
        let r = req
            .update_expression(update)
            .condition_expression(format!("#state IN ({})", ins.join(", ")))
            .send()
            .await;
        match r {
            Ok(_) => Ok(true),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_conditional_check_failed_exception()) =>
            {
                Ok(false)
            }
            Err(_) => Err(unavailable("move a job")),
        }
    }
}

/// The harness bucket: every write is create-only (`If-None-Match: *`), so an object is never replaced.
#[derive(Clone, Debug)]
pub struct S3Blobs {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
}

impl Blobs for S3Blobs {
    async fn put_new(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<PutOutcome, Fail> {
        // `PreconditionFailed` (412) says the key holds an object. `ConditionalRequestConflict` (409) says another write to the key is in
        // flight: it is tried again a few times and never taken for "the object exists".
        for attempt in 0..4u64 {
            let r = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(aws_sdk_s3::primitives::ByteStream::from(body.clone()))
                .content_type(content_type)
                .if_none_match("*")
                .send()
                .await;
            match r {
                Ok(_) => return Ok(PutOutcome::Created),
                Err(e) => {
                    let code = e
                        .as_service_error()
                        .and_then(|se| aws_sdk_s3::error::ProvideErrorMetadata::code(se));
                    match code {
                        Some("PreconditionFailed") => return Ok(PutOutcome::Exists),
                        Some("ConditionalRequestConflict") if attempt < 3 => {
                            tokio::time::sleep(std::time::Duration::from_millis(100 << attempt))
                                .await;
                        }
                        _ => return Err(Fail::internal("the bucket refused a write")),
                    }
                }
            }
        }
        Err(Fail::internal("the bucket refused a write"))
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fail> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => out
                .body
                .collect()
                .await
                .map(|b| Some(b.into_bytes().to_vec()))
                .map_err(|_| Fail::internal("the bucket read failed")),
            Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => Ok(None),
            Err(_) => Err(Fail::internal("the bucket read failed")),
        }
    }
}

/// Starts `research-job` with an asynchronous invoke (no retry is configured on the function).
#[derive(Clone, Debug)]
pub struct LambdaInvoker {
    pub client: aws_sdk_lambda::Client,
    pub function: String,
}

impl Invoker for LambdaInvoker {
    async fn start_job(&self, event: &JobEvent) -> Result<(), Fail> {
        let payload = serde_json::to_vec(event).map_err(|_| Fail::internal("job event"))?;
        let out = self
            .client
            .invoke()
            .function_name(&self.function)
            .invocation_type(aws_sdk_lambda::types::InvocationType::Event)
            .payload(aws_sdk_lambda::primitives::Blob::new(payload))
            .send()
            .await
            .map_err(|_| Fail::internal("the job function could not be invoked"))?;
        if out.status_code() == 202 {
            Ok(())
        } else {
            Err(Fail::internal("the job function did not accept the start"))
        }
    }
}
