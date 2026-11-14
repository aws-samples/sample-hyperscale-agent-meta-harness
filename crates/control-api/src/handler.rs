//! `/v1` routing independent of the Lambda event types, so tests drive it directly (architecture
//! section 6.1). Store, S3 and Step Functions access are traits; `crate::aws` implements them.
//!
//! The routes of a direct run: `POST /v1/direct/runs` and `GET /v1/direct/models` (`crate::direct`), the caller's run
//! list with its index and `POST /v1/runs/index` (`crate::run_index`), one run with its cost, its event log read forward or from the end
//! (`crate::chat_events`), its artifacts and their download URLs, the download of its snapshot (`crate::snapshot`),
//! `POST /v1/runs/{runId}/cancel` and `POST /v1/runs/{runId}/input` (`crate::controls`, `crate::input`), the messages to a running Cline
//! (`crate::messages`: send, list, read and withdraw), `GET /v1/models`, `GET /v1/repos`,
//! `POST /v1/admin/repos` and `GET /v1/whoami`, and the edit of a run's tags (`crate::tags`: `POST /v1/tags/edit`).

use crate::body::parse_body;
use crate::controls;
use crate::ids::now_rfc3339;
use crate::repo_names::RepoNames;
/// Re-exported because the [`Store`] methods of the run index take and return it: an implementor in a crate that does not depend on
/// `ham-core` (the shared test stand-in is built into `ham-admin`'s tests) must be able to name it.
pub use ham_core::run_index::RunIndexItem;
use ham_core::runtag::dynamo::UpdateSpec;
use harness_protocol::api::ApiError;
use harness_protocol::api::v1::{
    ArtifactView, DownloadUrlResponse, ListArtifactsResponse, ListReposResponse, RepoAdminAction,
    RepoAdminRequest, RepoAdminResponse, RepoAllowView, RunDetail, RunLinks, RunSummary,
    WhoAmIResponse,
};
use harness_protocol::common::is_ulid;
use harness_protocol::envelope::Envelope;
use harness_protocol::messages::{MessageView, MessagesPage, SendMessageRequest};
use harness_protocol::store::{
    AgentItem, ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RepoAllowItem, RunItem,
    TurnItem,
};
use harness_protocol::target::RepoTarget;
use message_ledger::{Admitted, CloseReason, Closed, LedgerError};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

/// The most of the prompt the root Goal item keeps, in bytes. DynamoDB caps an item at 400 KB and the item holds the prompt (`objective`) beside a
/// few small attributes, so a longer prompt is cut here, at a character boundary. The prompt has no size of its own: it is whole in
/// `runs/<runId>/input/request.json`, which is what the worker reads, and the request as a whole is held to the platform's bound. `GET
/// /v1/runs/{id}` gives the whole prompt back from there when the item holds only its head.
pub const OBJECTIVE_ITEM_MAX_BYTES: usize = 256 * 1024;

/// The first `max` bytes of `text`, cut back to a character boundary.
pub fn head_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
/// Events per cursor read; a DynamoDB Query page also stops at 1 MB.
pub const EVENTS_PAGE: usize = 500;
/// Lifetime of an artifact download URL; the session credentials can end it sooner.
pub const DOWNLOAD_URL_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A conditional write lost to a concurrent writer.
    #[error("conditional write conflict")]
    Conflict,
    /// A `nextToken` this API did not issue to the caller.
    #[error("the page token is not valid for this caller")]
    BadCursor,
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// A call that the stage's own setup refuses (a role without the grant, a parameter name the service does not accept). The same request
    /// fails the same way until an operator finishes the setup, so no caller may tell the user to retry. Only the calls that name it produce it.
    #[error("the stage is not set up for this request: {0}")]
    NotSetUp(String),
}

pub trait Store: Send + Sync {
    /// Writes the claim unless one exists for its caller and key; returns the stored claim.
    fn claim_idempotency(
        &self,
        claim: &IdempotencyItem,
    ) -> impl Future<Output = Result<IdempotencyItem, StoreError>> + Send;
    /// Writes the Run item, the root Goal and `run.created` together; `Conflict` if the run exists.
    fn create_run(
        &self,
        run: &RunItem,
        goal: &GoalItem,
        created: &Envelope,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    fn get_run(
        &self,
        run_id: &str,
    ) -> impl Future<Output = Result<Option<RunItem>, StoreError>> + Send;
    fn get_root_goal(
        &self,
        run_id: &str,
    ) -> impl Future<Output = Result<Option<GoalItem>, StoreError>> + Send;
    /// The owner's runs, newest first, one page at a time.
    fn list_runs(
        &self,
        owner: &str,
        next: Option<&str>,
    ) -> impl Future<Output = Result<(Vec<RunItem>, Option<String>), StoreError>> + Send;
    /// One shard's events after the cursor, in `eventId` order.
    fn events(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Envelope>, StoreError>> + Send;
    /// The run's `ART#` items in `artifactId` order.
    fn list_artifacts(
        &self,
        run_id: &str,
    ) -> impl Future<Output = Result<Vec<ArtifactItem>, StoreError>> + Send;
    fn get_artifact(
        &self,
        run_id: &str,
        artifact_id: &str,
    ) -> impl Future<Output = Result<Option<ArtifactItem>, StoreError>> + Send;
    /// Queues one control for an agent; `Conflict` if its id exists.
    fn put_control(
        &self,
        control: &ControlItem,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// One event of one shard by id (protocol 1.37.0, the user correction path: the answer route proves the question exists). A store
    /// that has not implemented it reports itself unavailable.
    fn get_event(
        &self,
        _run_id: &str,
        _shard: u8,
        _event_id: &str,
    ) -> impl Future<Output = Result<Option<Envelope>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store reads no single event".into(),
            ))
        }
    }
    /// One queued control by its id (protocol 1.37.0): the answer route compares a repeated answer with the stored one.
    fn get_control(
        &self,
        _run_id: &str,
        _agent_id: &str,
        _control_id: &str,
    ) -> impl Future<Output = Result<Option<ControlItem>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store reads no single control".into(),
            ))
        }
    }
    /// Removes `deliveredAt` from a control agentd never acknowledged (protocol 1.37.0), under the condition that it is set; `false`
    /// when it was not.
    fn rearm_control(
        &self,
        _run_id: &str,
        _agent_id: &str,
        _control_id: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store cannot re-arm a control".into(),
            ))
        }
    }
    /// The newest `limit` events of one shard, returned oldest first (protocol 1.37.0: the run list's tail read).
    fn latest_events(
        &self,
        _run_id: &str,
        _shard: u8,
        _limit: usize,
    ) -> impl Future<Output = Result<Vec<Envelope>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store has no tail read".into(),
            ))
        }
    }
    /// The newest `limit` events of one shard strictly older than `before` (the newest overall when `None`), returned oldest first.
    /// A reverse Query that follows `LastEvaluatedKey` until `limit` events are found or the shard is read.
    fn events_before(
        &self,
        _run_id: &str,
        _shard: u8,
        _before: Option<&str>,
        _limit: usize,
    ) -> impl Future<Output = Result<Vec<Envelope>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store has no reverse event read".into(),
            ))
        }
    }
    /// At most `limit` of the agent's controls whose ids lie from `oldest` to `newest` (both inclusive; `SK BETWEEN CTRL#<agent>#<oldest>
    /// AND CTRL#<agent>#<newest>`, in id order, a consistent read) and whether more exist.
    fn list_controls(
        &self,
        _run_id: &str,
        _agent_id: &str,
        _oldest: &str,
        _newest: &str,
        _limit: usize,
    ) -> impl Future<Output = Result<(Vec<ControlItem>, bool), StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store cannot list controls".into(),
            ))
        }
    }
    /// The caller's run-index items whose runs were created from `oldest` to `newest` (both inclusive, RFC 3339 as stored): one Query of
    /// the owner partition, every page. A store with no index answers none.
    fn run_index_between(
        &self,
        _owner: &str,
        _oldest: &str,
        _newest: &str,
    ) -> impl Future<Output = Result<Vec<RunIndexItem>, StoreError>> + Send {
        async { Ok(Vec::new()) }
    }
    /// Writes the start fields of the run's index item where they are not set yet and touches nothing else; `true` when the item was
    /// absent or lacked its prompt head, `false` when it was complete. A store with no index writes nothing.
    fn put_run_index(
        &self,
        _owner: &str,
        _item: &RunIndexItem,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async { Ok(false) }
    }
    /// What a tag edit needs of the run's search item: one consistent `GetItem` of the item under `owner`'s partition that projects the
    /// ids, the repository, the three lists an edit reads or replaces and the revision, and nothing else. `None` when the run has no item
    /// yet. A store with no index reports itself unavailable, so an edit never reads an absent index as an empty one.
    fn get_run_tag_state(
        &self,
        _owner: &str,
        _run: &RunItem,
    ) -> impl Future<Output = Result<Option<TagRead>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store holds no search items".into(),
            ))
        }
    }
    /// One `UpdateItem` of a tag edit on the run's search item under `owner`'s partition: the expression, condition, names and values of
    /// `spec` as they are (`ham_core::runtag::dynamo::edit_update`). `Err(StoreError::Conflict)` when its condition failed, which is a lost
    /// race: nothing was written.
    fn put_run_tags(
        &self,
        _owner: &str,
        _run: &RunItem,
        _spec: &UpdateSpec,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store cannot write tags".into(),
            ))
        }
    }
    /// Every repository allowlist entry, in target order.
    fn repo_allow_list(
        &self,
    ) -> impl Future<Output = Result<Vec<RepoAllowItem>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store holds no allowlist".into(),
            ))
        }
    }
    /// Writes the entry and adds its target to the listing, together.
    fn repo_allow_put(
        &self,
        _item: &RepoAllowItem,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store holds no allowlist".into(),
            ))
        }
    }
    /// Deletes the entry and removes its target from the listing, together; absent is a success.
    fn repo_allow_delete(
        &self,
        _target: &RepoTarget,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this store holds no allowlist".into(),
            ))
        }
    }
    /// The run's Agent and Turn items, for its cost (W10). A store without them (the test fakes)
    /// reports none, so the run shows no cost.
    fn run_usage(&self, run_id: &str) -> impl Future<Output = Result<RunUsage, StoreError>> + Send {
        let _ = run_id;
        async { Ok(RunUsage::default()) }
    }
    /// The run's Agent items (`ham agents`, steer, cancel, the shell target).
    fn list_agents(
        &self,
        run_id: &str,
    ) -> impl Future<Output = Result<Vec<AgentItem>, StoreError>> + Send {
        async move { Ok(self.run_usage(run_id).await?.agents) }
    }
    /// Writes `run` and `goal` (each at its stored version plus one) and `event`, together;
    /// `Conflict` if either item changed since it was read.
    fn update_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        event: Option<&Envelope>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        let _ = (run, goal, event);
        async {
            Err(StoreError::Unavailable(
                "this store cannot change a run".into(),
            ))
        }
    }
    /// Admits a message of the user to the run: the ledger writes the message, the run's control item and
    /// the owner's counter in one transaction. A store without a ledger (the test fakes) reports itself unavailable.
    fn message_admit(
        &self,
        _run_id: &str,
        _principal: &str,
        _req: &SendMessageRequest,
    ) -> impl Future<Output = Result<Admitted, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// One message of the run, as the owner sees it.
    fn message_get(
        &self,
        _run_id: &str,
        _message_id: &str,
    ) -> impl Future<Output = Result<Option<MessageView>, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// The run's messages in the order admitted, with the session and whether the run still takes messages.
    fn message_list(
        &self,
        _run_id: &str,
    ) -> impl Future<Output = Result<MessagesPage, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// Withdraws a message nobody has claimed.
    fn message_withdraw(
        &self,
        _run_id: &str,
        _message_id: &str,
    ) -> impl Future<Output = Result<MessageView, LedgerError>> + Send {
        async { Err(no_ledger()) }
    }
    /// Closes the run to messages and rejects the open ones (a cancel: `run-cancelled`). A store without a ledger has none to close.
    fn message_close(
        &self,
        _run_id: &str,
        _reason: CloseReason,
    ) -> impl Future<Output = Result<Closed, LedgerError>> + Send {
        async { Ok(Closed::default()) }
    }
    /// What the harness records about a run's stored conversation: whether its image writes one and what the worker declared at
    /// turn-complete. A store that has not implemented it reports a run whose image writes none.
    fn transcript_facts(
        &self,
        run_id: &str,
    ) -> impl Future<Output = Result<crate::transcript::TranscriptFacts, StoreError>> + Send {
        let _ = run_id;
        async { Ok(crate::transcript::TranscriptFacts::default()) }
    }
    /// One read of an owner's search items: the partition `owner_pk`, sort keys from `sk_floor` up, newest first, after
    /// `sk_after` when set, at most `limit` items. A store that has not implemented it reports itself unavailable.
    fn search_items(
        &self,
        owner_pk: &str,
        sk_floor: &str,
        sk_after: Option<&str>,
        limit: usize,
    ) -> impl Future<Output = Result<crate::search::SearchPage, StoreError>> + Send {
        let _ = (owner_pk, sk_floor, sk_after, limit);
        async {
            Err(StoreError::Unavailable(
                "this store holds no search items".into(),
            ))
        }
    }
}

fn no_ledger() -> LedgerError {
    LedgerError::Unavailable("this store keeps no messages".into())
}

/// What a tag edit reads of a run's search item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagRead {
    /// The item as the list row reads it, narrowed to the attributes of the projection: a tag attribute of a wrong type reads as no value.
    pub item: RunIndexItem,
    /// The attributes an edit replaces or compares against (`runTagsUser`, `runTagsOff`, `runTagsRev`) that the item holds and the reader
    /// could not take as written, or `item` when the whole item could not be read. An edit on such a run is refused: writing over a list
    /// nobody can read would destroy it.
    pub unreadable: Vec<String>,
}

/// A run's Agent and Turn items.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunUsage {
    pub agents: Vec<AgentItem>,
    pub turns: Vec<TurnItem>,
}

pub trait Blobs: Send + Sync {
    /// Stores a JSON object; returns its `s3://` URI.
    fn put(
        &self,
        key: &str,
        body: &[u8],
    ) -> impl Future<Output = Result<String, StoreError>> + Send;
    /// The artifacts bucket; `put` and `presign_get` address keys in it.
    fn bucket(&self) -> &str;
    /// A presigned GET of one object and the URL's true expiry: `ttl` after signing, or the
    /// signing credentials' expiry when that is sooner.
    fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
    ) -> impl Future<Output = Result<DownloadUrlResponse, StoreError>> + Send;
    /// At most `max_keys` (1 to 1,000) objects under `prefix` whose keys sort after `start_after`, in key order (S3 ListObjectsV2 with
    /// StartAfter). A client that cannot list reports itself unavailable.
    fn list_objects(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> impl Future<Output = Result<ObjectPage, StoreError>> + Send {
        let _ = (prefix, start_after, max_keys);
        async { Err(StoreError::Unavailable("this client cannot list".into())) }
    }
    /// One object's bytes, refused above `max_bytes`, and only while its ETag is `if_match` when that is set. `None`: no such key.
    fn get_object(
        &self,
        key: &str,
        max_bytes: usize,
        if_match: Option<&str>,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, ObjectError>> + Send {
        let _ = (key, max_bytes, if_match);
        async {
            Err(ObjectError::Store(StoreError::Unavailable(
                "this client cannot read objects".into(),
            )))
        }
    }
    /// [`Blobs::presign_get`] for one version of the object (`versionId` in the signed URL), so the URL keeps naming the bytes whose header
    /// was checked after the key is overwritten. A store that has no versions ignores `version_id`.
    fn presign_get_version(
        &self,
        key: &str,
        version_id: Option<&str>,
        ttl: Duration,
    ) -> impl Future<Output = Result<DownloadUrlResponse, StoreError>> + Send {
        let _ = version_id;
        self.presign_get(key, ttl)
    }
    /// Whether an object named exactly `key` exists, asked by a bucket LISTING with the key as the prefix (`s3:ListBucket`, which the role is
    /// granted for the prefix of one key only). A GET of an absent key is 403 for a role that may not list the bucket, and 404 only for one
    /// that may, so "absent" and "not allowed" cannot be told apart by a read. The default reads one byte, for a store that answers an absent
    /// key with `None`.
    fn exists(&self, key: &str) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async move { Ok(self.read_prefix(key, 1).await?.is_some()) }
    }
    /// The first `max_bytes` (at least 1) of one object with its whole size, ETag, version and write time (a ranged GET); `None` when the
    /// key does not exist.
    fn read_prefix(
        &self,
        _key: &str,
        _max_bytes: u64,
    ) -> impl Future<Output = Result<Option<ObjectPrefix>, StoreError>> + Send {
        async {
            Err(StoreError::Unavailable(
                "this blob store has no ranged read".into(),
            ))
        }
    }
}

/// One object of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
}

/// A page of a listing: the objects in key order, and whether the bucket holds more after the last one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectPage {
    pub objects: Vec<ObjectInfo>,
    pub truncated: bool,
}

/// Why an object was not returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectError {
    /// It is larger than the caller allows.
    TooLarge,
    /// Its ETag is no longer the one the listing showed.
    Changed,
    Store(StoreError),
}

/// The first bytes of an object and what S3 said about the whole object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPrefix {
    pub bytes: Vec<u8>,
    /// The whole object's size.
    pub size: u64,
    /// The object's ETag as S3 sends it (quoted).
    pub etag: Option<String>,
    /// The version of the object that was read (`x-amz-version-id`), when the bucket keeps versions.
    pub version_id: Option<String>,
    /// The object's write time, RFC 3339 UTC to the second.
    pub last_modified: Option<String>,
}

pub trait Execution: Send + Sync {
    /// Starts the `run` execution `name`; a start of a name that already has one is a success.
    fn start(
        &self,
        name: &str,
        input: &Value,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// The deployment new runs execute on, read fresh for each run (a deploy that changes only
    /// another stack leaves this Lambda's containers running). Defaults to none recorded.
    fn deployment(&self) -> impl Future<Output = Result<DeployedAs, StoreError>> + Send {
        async { Ok(DeployedAs::default()) }
    }
    /// Whether the stage has an Instances worker runtime: its runtime ARN is published as the SSM parameter
    /// `<deployment prefix>/worker-python-instances-runtime-arn`. Asked only for a request that names `instances`, so a stage whose role cannot
    /// read the parameter yet still starts every other run. Defaults to no. `Ok(false)` is a stage without the parameter; `Err(Unavailable)` is a read
    /// that may pass on a retry (a throttle, a service fault, no answer); `Err(NotSetUp)` is a read the stage's own setup refuses (a missing grant).
    fn instances_available(&self) -> impl Future<Output = Result<bool, StoreError>> + Send {
        async { Ok(false) }
    }
    /// The stage's deploy lock: the SSM parameter `<deployment prefix>/deploy-lock` that setup and deploy create before they change the stage,
    /// write again while they work and delete at the end. `Ok(None)` is a stage without the parameter, the normal case; `Err(Unavailable)` is a read
    /// that may pass on a retry (a throttle, a service fault, no answer); `Err(NotSetUp)` is a read the stage's own setup refuses (a role without the
    /// grant). Asked only by a request that would launch a run, right before the write that creates it (`crate::admission`). Defaults to no lock.
    fn deploy_lock(&self) -> impl Future<Output = Result<Option<DeployLock>, StoreError>> + Send {
        async { Ok(None) }
    }
    /// Stops the `run` execution `name` with `cause`; a name with no execution is a success.
    fn stop(&self, name: &str, cause: &str) -> impl Future<Output = Result<(), StoreError>> + Send {
        let _ = (name, cause);
        async {
            Err(StoreError::Unavailable(
                "this execution client cannot stop a run".into(),
            ))
        }
    }
}

/// The stage's `deployment-digest` and `deployed-source-tree` parameters; either is `None` until
/// the stage's first deploy writes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeployedAs {
    pub digest: Option<String>,
    pub source_tree: Option<String>,
}

/// The stage's deploy lock as SSM holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployLock {
    /// The parameter's text: `key=value` lines (token, holder, identity, route, started, commit, inputs, build and, for a hold that deploys
    /// nothing, `deploys=no`).
    pub value: String,
    /// When SSM wrote the parameter last (`LastModifiedDate`), in Unix seconds; `None` when the answer named no time.
    pub written_epoch_s: Option<i64>,
}

/// How the engine of a run is named in the words an answer carries: the display name of the Run item's engine, the default engine's when the item
/// names none.
pub(crate) fn engine_name_of(run: &harness_protocol::store::RunItem) -> String {
    harness_protocol::common::Engine::or_default(run.engine)
        .display_name()
        .to_string()
}

#[derive(Debug)]
pub struct Deps<S, B, E> {
    pub store: S,
    pub blobs: B,
    pub execution: E,
    pub account: String,
    /// IAM principal ARN patterns (`*` any run of characters) allowed on `/v1/admin/*`.
    pub operators: Vec<String>,
    /// The CodeCommit repository names the stage lets an operator allow (`HAMH_REPO_NAMES`, from the `repoNames` of the account-file entry; default `hamh-e2e-*`).
    pub repo_names: RepoNames,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V1Request {
    pub method: String,
    /// Path from `/v1` on, without the stage.
    pub path: String,
    /// `requestContext.identity.userArn` from API Gateway IAM auth.
    pub caller_arn: Option<String>,
    /// `requestContext.identity.sourceIp`: the client's egress IP, logged on the access line.
    pub source_ip: Option<String>,
    /// Lowercase header names.
    pub headers: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V1Response {
    pub status: u16,
    pub body: String,
    /// The seconds of a `Retry-After` header; `None` sends no such header.
    pub retry_after_s: Option<u32>,
}

pub(crate) type Reply = (V1Response, Vec<String>);

pub(crate) fn json<T: Serialize>(status: u16, v: &T) -> Reply {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    (
        V1Response {
            status,
            body,
            retry_after_s: None,
        },
        vec![],
    )
}

pub(crate) fn error(status: u16, code: &str, message: &str) -> Reply {
    json(
        status,
        &ApiError {
            code: code.into(),
            message: message.into(),
            request_id: None,
        },
    )
}

/// 503 for the caller; the store's reason goes to the log only.
pub(crate) fn unavailable(e: &StoreError, retry: &str) -> Reply {
    let (resp, _) = error(
        503,
        "unavailable",
        &format!("the harness is unavailable; {retry}"),
    );
    (resp, vec![e.to_string()])
}

/// 422 `compute-not-set-up` for a request that names the Instances machine on a stage whose setup for it is incomplete (the role cannot read the
/// Instances runtime's parameter, for example). It carries no retry words, because `ham` retries every 5xx and a retry cannot help; it says what to
/// do meanwhile. The store's reason goes to the log only.
pub(crate) fn compute_not_set_up(e: &StoreError) -> Reply {
    let (resp, _) = error(
        422,
        "compute-not-set-up",
        "this stage's setup for Instances is incomplete, so it cannot start a run on Instances yet; tell the person who runs the stage, and meanwhile start the run with --compute microvm, or set run.compute to microvm or auto",
    );
    (resp, vec![e.to_string()])
}

pub(crate) fn not_found(what: &str) -> Reply {
    error(404, "not-found", what)
}

fn not_allowed(method: &str) -> Reply {
    error(
        405,
        "method-not-allowed",
        &format!("{method} is not allowed on this route"),
    )
}

pub async fn handle<S: Store, B: Blobs, E: Execution>(
    req: V1Request,
    deps: &Deps<S, B, E>,
) -> V1Response {
    handle_logged(req, deps).await.0
}

/// The response plus diagnostics (store and execution failures) for the Lambda to log.
pub async fn handle_logged<S: Store, B: Blobs, E: Execution>(
    req: V1Request,
    deps: &Deps<S, B, E>,
) -> Reply {
    let Some(caller) = req.caller_arn.clone() else {
        return error(403, "forbidden", "the request carries no IAM caller");
    };
    let path = req.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let method = req.method.as_str();
    match (segments.as_slice(), method) {
        (["v1", "direct", "runs"], "POST") => {
            crate::direct::start_direct_run(&req, &caller, deps).await
        }
        (["v1", "direct", "models"], "GET") => crate::direct::list_models(),
        (["v1", "runs"], "GET") => crate::run_index::list(&req, &caller, deps).await,
        (["v1", "runs", "index"], "POST") => crate::run_index::backfill(&req, &caller, deps).await,
        (["v1", "runs", id], "GET") => get_run(id, &caller, deps).await,
        (["v1", "runs", id, "events"], "GET") => {
            crate::chat_events::read(&req, id, &caller, deps).await
        }
        (["v1", "runs", id, "transcript"], "GET") => {
            crate::transcript::get_transcript(&req, id, &caller, deps).await
        }
        (["v1", "runs", id, "head"], "GET") => crate::head::get_head(id, &caller, deps).await,
        (["v1", "runs", id, "retry"], "POST") => crate::retry::retry(&req, id, &caller, deps).await,
        (["v1", "runs", id, "resume"], "POST") => {
            crate::resume::resume(&req, id, &caller, deps).await
        }
        (["v1", "search"], "GET") => crate::search::get_search(&req, &caller, deps).await,
        (["v1", "tags", "edit"], "POST") => crate::tags::edit(&req, &caller, deps).await,
        (["v1", "runs", id, "snapshot", "download-url"], "POST") => {
            crate::snapshot::download_url(id, &caller, deps).await
        }
        (["v1", "runs", id, "artifacts"], "GET") => artifacts(id, &caller, deps).await,
        (["v1", "runs", id, "artifacts", art, "download-url"], "POST") => {
            download_url(id, art, &caller, deps).await
        }
        (["v1", "runs", id, "cancel"], "POST") => {
            let discard = req.query.get("discard").map(String::as_str) == Some("true");
            controls::cancel(id, &caller, discard, deps).await
        }
        (["v1", "runs", id, "input"], "POST") => {
            controls::answer_input(&req, id, &caller, deps).await
        }
        (["v1", "runs", id, "messages"], "POST") => {
            crate::messages::send(&req, id, &caller, deps).await
        }
        (["v1", "runs", id, "messages"], "GET") => crate::messages::list(id, &caller, deps).await,
        (["v1", "runs", id, "messages", message], "GET") => {
            crate::messages::get(id, message, &caller, deps).await
        }
        (["v1", "runs", id, "messages", message, "withdraw"], "POST") => {
            crate::messages::withdraw(id, message, &caller, deps).await
        }
        (["v1", "models"], "GET") => models(),
        (["v1", "repos"], "GET") => list_repos(deps).await,
        (["v1", "admin", "repos"], "POST") => admin_repos(&req, &caller, deps).await,
        (["v1", "whoami"], "GET") => json(
            200,
            &WhoAmIResponse {
                principal_arn: caller,
                account: deps.account.clone(),
            },
        ),
        (
            ["v1", "runs"]
            | ["v1", "direct", "runs" | "models"]
            | ["v1", "runs", _]
            | [
                "v1",
                "runs",
                _,
                "events" | "artifacts" | "cancel" | "input" | "transcript" | "head" | "retry"
                | "resume",
            ]
            | ["v1", "runs", _, "artifacts", _, "download-url"]
            | ["v1", "runs", _, "snapshot", "download-url"]
            | ["v1", "runs", _, "messages"]
            | ["v1", "runs", _, "messages", _]
            | ["v1", "runs", _, "messages", _, "withdraw"]
            | ["v1", "models" | "whoami" | "repos" | "search"]
            | ["v1", "tags", "edit"]
            | ["v1", "admin", "repos"],
            _,
        ) => not_allowed(method),
        _ => not_found("no such route"),
    }
}

fn view(i: RepoAllowItem) -> RepoAllowView {
    RepoAllowView {
        target: i.target,
        added_by: i.added_by,
        added_at: i.added_at,
    }
}

/// `GET /v1/repos`: the allowlist `ham doctor` prints, in target order.
async fn list_repos<S: Store, B: Blobs, E: Execution>(deps: &Deps<S, B, E>) -> Reply {
    match deps.store.repo_allow_list().await {
        Ok(mut items) => {
            items.sort_by_key(|a| a.target.to_string());
            json(
                200,
                &ListReposResponse {
                    repos: items.into_iter().map(view).collect(),
                },
            )
        }
        Err(e) => unavailable(&e, "retry"),
    }
}

/// `*` matches any run of characters; everything else is literal.
/// Whether `arn` matches an `OPERATOR_PRINCIPALS` pattern (`*` matches any run of characters).
pub fn principal_matches(pattern: &str, arn: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if parts.len() == 1 {
        return pattern == arn;
    }
    if arn.len() < first.len() + last.len() || !arn.starts_with(first) || !arn.ends_with(last) {
        return false;
    }
    let mut rest = &arn[first.len()..arn.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// What `repo-gateway` would refuse about an entry, as the exact reason. A `cc:` name has to be covered by the stage's repository-name rule (a rule that lists the default entry
/// also covers a repository made under the earlier names): the operator decides which other repositories are allowed. A review-record member that an older
/// client still sends is not read.
fn allow_refusal(r: &RepoAdminRequest, names: &RepoNames) -> Option<String> {
    match &r.target {
        RepoTarget::CodeCommit { name }
            if !names.allows(name) && !names.allows_earlier_name(name) // COMPAT-CAH-DATA
            =>
        {
            Some(names.refusal(name))
        }
        _ => None,
    }
}

/// `POST /v1/admin/repos` (`ham-admin repos allow|deny`): operator principals only.
async fn admin_repos<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    if !deps.operators.iter().any(|p| principal_matches(p, caller)) {
        return error(
            403,
            "not-an-operator",
            &format!(
                "ham-admin repos needs an operator principal ({}); the caller is {caller}",
                deps.operators.join(", ")
            ),
        );
    }
    let r: RepoAdminRequest = match parse_body("RepoAdminRequest", &req.body) {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    match r.action {
        RepoAdminAction::Allow => {
            if let Some(reason) = allow_refusal(&r, &deps.repo_names) {
                return error(422, "refused", &reason);
            }
            let item = RepoAllowItem {
                target: r.target.clone(),
                added_by: caller.to_string(),
                added_at: now_rfc3339(),
            };
            if let Err(e) = deps.store.repo_allow_put(&item).await {
                return unavailable(&e, "retry");
            }
            json(
                200,
                &RepoAdminResponse {
                    action: r.action,
                    target: r.target,
                    entry: Some(view(item)),
                },
            )
        }
        RepoAdminAction::Deny => {
            if let Err(e) = deps.store.repo_allow_delete(&r.target).await {
                return unavailable(&e, "retry");
            }
            json(
                200,
                &RepoAdminResponse {
                    action: r.action,
                    target: r.target,
                    entry: None,
                },
            )
        }
    }
}

/// 1-128 visible ASCII characters (the OpenAPI `Idempotency-Key` pattern).
pub(crate) fn valid_idempotency_key(k: &str) -> bool {
    (1..=128).contains(&k.len()) && k.bytes().all(|b| (b'!'..=b'~').contains(&b))
}

/// The `GET /v1/runs` summary of a Run item (the reaper's `aborted` included).
pub fn run_summary(run: RunItem) -> RunSummary {
    RunSummary {
        run_id: run.run_id,
        status: run.status,
        phase: run.phase,
        target: run.target,
        created_at: run.created_at,
        owner_principal_arn: run.owner_principal_arn,
        kind: None,
        aborted: run.aborted,
        needs_input: None,
        continue_from: run.continue_from,
        // Why a paused or stalled run stopped and how it can go on: the session (slice) the run is in or ended in.
        status_reason: run.status_reason,
        resume_mode: run.resume_mode,
        session: run.slice,
    }
}

/// The run if it exists and the caller owns it; another caller's run reads as absent.
pub(crate) async fn owned<S: Store>(
    store: &S,
    run_id: &str,
    caller: &str,
) -> Result<Option<RunItem>, StoreError> {
    if !is_ulid(run_id) {
        return Ok(None);
    }
    Ok(store
        .get_run(run_id)
        .await?
        .filter(|r| r.owner_principal_arn == caller))
}

async fn get_run<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let run = match owned(&deps.store, run_id, caller).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    };
    let goal = match deps.store.get_root_goal(run_id).await {
        Ok(g) => g,
        Err(e) => return unavailable(&e, "retry"),
    };
    let usage = match deps.store.run_usage(run_id).await {
        Ok(u) => u,
        Err(e) => return unavailable(&e, "retry"),
    };
    let (objective, mut logs) = match &goal {
        Some(g) => {
            let (text, note) = full_objective(run_id, &run.prompt_sha256, &g.objective, deps).await;
            (Some(text), note.into_iter().collect::<Vec<String>>())
        }
        None => (None, Vec::new()),
    };
    let cost = crate::cost::run_cost(&usage.agents, &usage.turns, crate::cost::pinned());
    let budget = run.budget;
    // The root goal's counter is the old line's budget counter; a direct run never moves it, so the tokens its turns reported (the sum of
    // the lines of `cost`) are the honest number. The larger of the two is shown, so a run that has both reads the same as before.
    let tokens_used = goal
        .as_ref()
        .map_or(0, |g| g.tokens_used)
        .max(crate::cost::total_tokens(cost.as_ref()));
    let mut detail = RunDetail {
        summary: run_summary(run),
        objective,
        budget,
        tokens_used,
        cost_usd: cost.as_ref().map_or(0.0, crate::cost::total_usd),
        links: RunLinks::default(),
        cost,
    };
    // The reply is one Lambda reply (6 MiB): a whole prompt read from the stored request must leave it room; otherwise the stored head stays.
    if !detail_fits_a_reply(&detail) {
        detail.objective = goal.as_ref().map(|g| g.objective.clone());
        logs.push(format!(
            "run {run_id}: the whole prompt does not fit one reply; the stored head is returned"
        ));
    }
    let (resp, mut more) = json(200, &detail);
    more.extend(logs);
    (resp, more)
}

/// Whether the serialized `detail` fits one reply of the Lambda that serves the front door: its size as the proxy's reply holds it (the body is one
/// JSON string there, as in a request) is at most [`harness_protocol::direct::MAX_REQUEST_BYTES`].
pub fn detail_fits_a_reply(detail: &RunDetail) -> bool {
    serde_json::to_vec(detail).is_ok_and(|b| {
        harness_protocol::direct::event_body_bytes(&b)
            <= harness_protocol::direct::MAX_REQUEST_BYTES
    })
}

/// The prompt of a run for `GET /v1/runs/{id}`, and a line for the log when the whole prompt could not be given. The root Goal item holds at most
/// [`OBJECTIVE_ITEM_MAX_BYTES`] of it, cut back at most 3 bytes to a character boundary: a stored text shorter than that cannot be a cut head and is
/// returned as it is, without a read. A text that may be a cut head is compared with the prompt's SHA-256 (the Run item keeps it); when it differs,
/// the whole prompt is read from the stored request and returned if it hashes the same. The stored head stays when the request cannot be read.
async fn full_objective<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    prompt_sha256: &str,
    stored: &str,
    deps: &Deps<S, B, E>,
) -> (String, Option<String>) {
    if stored.len() + 4 <= OBJECTIVE_ITEM_MAX_BYTES
        || crate::ids::sha256_hex(stored.as_bytes()) == prompt_sha256
    {
        return (stored.to_string(), None);
    }
    /// The one member of the stored request that is read: the rules, skills and attachments beside it are skipped.
    #[derive(serde::Deserialize)]
    struct PromptOnly {
        prompt: String,
    }
    let head = |why: &str| {
        (
            stored.to_string(),
            Some(format!(
                "run {run_id}: the whole prompt was not read ({why}); the stored head is returned"
            )),
        )
    };
    let key = harness_protocol::store::s3::request_key(run_id);
    let max = harness_protocol::direct::PLATFORM_REQUEST_BYTES as u64 + 1024 * 1024;
    let object = match deps.blobs.read_prefix(&key, max).await {
        Ok(Some(o)) => o,
        Ok(None) => return head("the stored request is absent"),
        Err(_) => return head("the stored request could not be read"),
    };
    if object.size > object.bytes.len() as u64 {
        return head("the stored request is larger than the route reads");
    }
    match serde_json::from_slice::<PromptOnly>(&object.bytes) {
        Ok(request) if crate::ids::sha256_hex(request.prompt.as_bytes()) == prompt_sha256 => {
            (request.prompt, None)
        }
        Ok(_) => head("the stored prompt is not the prompt of the run"),
        Err(_) => head("the stored request is not a request"),
    }
}

async fn artifacts<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    match owned(&deps.store, run_id, caller).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    }
    match deps.store.list_artifacts(run_id).await {
        Ok(mut items) => {
            items.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
            let artifacts = items
                .into_iter()
                .map(|a| ArtifactView {
                    artifact_id: a.artifact_id,
                    kind: a.kind,
                    s3_uri: a.s3_uri,
                    sha256: a.sha256,
                    size_bytes: a.size_bytes,
                })
                .collect();
            json(200, &ListArtifactsResponse { artifacts })
        }
        Err(e) => unavailable(&e, "retry"),
    }
}

/// 1-128 characters from the ULID and key-safe set; anything else cannot name an artifact.
fn valid_item_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The object key of `s3_uri` when it lies in `bucket` under `runs/<runId>/`.
fn downloadable_key<'a>(s3_uri: &'a str, bucket: &str, run_id: &str) -> Option<&'a str> {
    let (b, key) = s3_uri.strip_prefix("s3://")?.split_once('/')?;
    let run_prefix = format!("runs/{run_id}/");
    let ok = b == bucket && key.len() > run_prefix.len() && key.starts_with(&run_prefix);
    ok.then_some(key)
}

/// A presigned GET of one artifact. It writes nothing, so it needs no Idempotency-Key.
async fn download_url<S: Store, B: Blobs, E: Execution>(
    run_id: &str,
    artifact_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    match owned(&deps.store, run_id, caller).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    }
    let missing = || not_found(&format!("no artifact {artifact_id} in run {run_id}"));
    if !valid_item_id(artifact_id) {
        return missing();
    }
    let artifact = match deps.store.get_artifact(run_id, artifact_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return missing(),
        Err(e) => return unavailable(&e, "retry"),
    };
    let Some(key) = downloadable_key(&artifact.s3_uri, deps.blobs.bucket(), run_id) else {
        let (resp, _) = missing();
        return (
            resp,
            vec![format!(
                "artifact {artifact_id} of run {run_id} is outside runs/{run_id}/ in the artifacts \
                 bucket; not presigned"
            )],
        );
    };
    match deps.blobs.presign_get(key, DOWNLOAD_URL_TTL).await {
        Ok(url) => json(200, &url),
        Err(e) => unavailable(&e, "retry"),
    }
}

/// `GET /v1/models`: the five models of `engine/catalog/models.json` as catalog rows, read from no table (`ham doctor` reads it).
fn models() -> Reply {
    json(200, &crate::direct_models::models_response())
}
