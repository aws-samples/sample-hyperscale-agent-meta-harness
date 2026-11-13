//! What the ops need from AWS, as traits. [`crate::aws`] implements them over DynamoDB, KMS, S3, AgentCore and
//! CodeArtifact; the tests implement them in memory.

use crate::error::RunSessionError;
use dispatcher::endpoints::{EndpointState, Pin};
use ham_core::runtag::dynamo::Answered;
use ham_core::runtag::edit::TagState;
use ham_core::runtag::vocab::VocabItem;
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{AgentItem, GoalItem, RunItem, TurnItem};
use std::future::Future;
use worker_api::handler::TokenContext;

pub type Res<T> = Result<T, RunSessionError>;

pub trait Store: Send + Sync {
    fn get_run(&self, run_id: &str) -> impl Future<Output = Res<Option<RunItem>>> + Send;
    fn get_root_goal(&self, run_id: &str) -> impl Future<Output = Res<Option<GoalItem>>> + Send;
    /// `requestSha256` of the run's `run.created` event (the oldest harness event of the run), `None` when it has none.
    fn run_created_request_sha256(
        &self,
        run_id: &str,
    ) -> impl Future<Output = Res<Option<String>>> + Send;
    /// The console link of the pull request the run opened: the `url` of its `delivery.pr` event, `None` when it opened none. A follow-up
    /// run hands it to the repo-gateway, which reads the pull request, aims the new one and links the two.
    fn delivery_pr_url(&self, run_id: &str) -> impl Future<Output = Res<Option<String>>> + Send;
    fn get_agent(
        &self,
        run_id: &str,
        agent_id: &str,
    ) -> impl Future<Output = Res<Option<AgentItem>>> + Send;
    /// Writes the Agent item unless one exists: the item is bound once per run (its nonce hash is the running agentd's), and
    /// a retry must not reset what worker-api has recorded on it since (status, heartbeat, a revoked binding).
    fn put_agent(&self, agent: &AgentItem) -> impl Future<Output = Res<()>> + Send;
    /// Moves the Agent item to the next slice. `next` is the whole new item (its generation and session generation, session id, refresh
    /// nonce binding and `nonce_ciphertext`); the write is conditional on the stored item still being at `from_generation` with `from_version`, so
    /// a slice that lost a race writes nothing. `true`: written. `false`: the item is not as read (another attempt rotated it, or worker-api wrote to
    /// it); the caller reads it again.
    fn rotate_agent(
        &self,
        next: &AgentItem,
        from_generation: u32,
        from_version: u64,
    ) -> impl Future<Output = Res<bool>> + Send;
    /// Records the run's last slice on the Run item. Only raises the number; a repeat is harmless.
    fn set_run_slice(&self, run_id: &str, slice: u32) -> impl Future<Output = Res<()>> + Send;
    /// Claims one of the stage's Instances slots for `run_id`: `true` when the run holds one now (it had one, or there was room under
    /// `max`), `false` when `max` runs hold theirs. A repeat for the same run is `true` and counts once. A store with no count claims nothing and
    /// always answers yes.
    fn claim_instances_slot(
        &self,
        run_id: &str,
        max: u32,
    ) -> impl Future<Output = Res<bool>> + Send {
        let _ = (run_id, max);
        async { Ok(true) }
    }
    /// Gives the run's Instances slot back; a run that holds none is fine, so a repeat is harmless.
    fn release_instances_slot(&self, run_id: &str) -> impl Future<Output = Res<()>> + Send {
        let _ = run_id;
        async { Ok(()) }
    }
    /// The run's session is over: marks the Agent item `stopped` and revokes its credential binding, so the next
    /// `/internal/creds` refresh is refused. Repeating it is harmless; an agent that is gone or unbound is fine.
    fn end_agent(&self, run_id: &str, agent_id: &str) -> impl Future<Output = Res<()>> + Send;
    /// The run takes no more messages of the user and the open ones end: what is pending or held ends `rejected`
    /// (`run-sealed`), a message agentd holds in `dispatching` becomes `delivery-unknown`, and a delivered one is left as it is. Repeating it
    /// is harmless. A store without a message ledger has nothing to close.
    fn close_messages(&self, _run_id: &str) -> impl Future<Output = Res<()>> + Send {
        async { Ok(()) }
    }
    fn get_turn(
        &self,
        run_id: &str,
        agent_id: &str,
        n: u32,
    ) -> impl Future<Output = Res<Option<TurnItem>>> + Send;
    /// Writes the Turn item unless one exists; `false` when it existed.
    fn create_turn(&self, turn: &TurnItem) -> impl Future<Output = Res<bool>> + Send;
    /// Replaces the encrypted task token of an existing Turn item: a retried `waitForTaskToken` state holds a new token,
    /// and worker-api completes whichever the Turn item holds.
    fn replace_turn_token(
        &self,
        run_id: &str,
        agent_id: &str,
        n: u32,
        ciphertext: &str,
    ) -> impl Future<Output = Res<()>> + Send;
    fn mark_turn_invoked(
        &self,
        run_id: &str,
        agent_id: &str,
        n: u32,
    ) -> impl Future<Output = Res<()>> + Send;
    /// Sets the Run item's python runtime pin unless it has an endpoint; returns the stored pin.
    fn pin_runtime(&self, run_id: &str, pin: &Pin) -> impl Future<Output = Res<Pin>> + Send;
    /// Sets the Run item's `phase` (no version change, as the dispatcher's pin).
    fn set_run_phase(&self, run_id: &str, phase: GoalPhase)
    -> impl Future<Output = Res<()>> + Send;
    /// Moves the Run item and the root Goal from `run.status` to `to` and publishes `event`, together; `false` when
    /// the run no longer shows `run.status` or the event id exists.
    fn set_run_status(
        &self,
        run: &RunItem,
        to: GoalStatus,
        event: &Envelope,
    ) -> impl Future<Output = Res<bool>> + Send;
    /// [`Store::set_run_status`] for a run that stopped delivered: the same move and the same event, and the Run item also gets the plain
    /// reason (`code: sentence`) and the resume mode, in the same write.
    fn set_run_stop(
        &self,
        run: &RunItem,
        to: GoalStatus,
        event: &Envelope,
        reason: &str,
        mode: harness_protocol::goal::ResumeMode,
    ) -> impl Future<Output = Res<bool>> + Send;
    /// Writes one event item; the same id again is the same item.
    fn put_event(&self, event: &Envelope) -> impl Future<Output = Res<()>> + Send;
    /// Records the end of a run on its search item: the outcome word when there is one, and the time. An upsert that
    /// sets only those fields (and the key fields where the item is new), and never replaces a later time with an earlier one, so it holds
    /// in any order with the start's write and the backfill. A repeat is harmless.
    fn touch_run_index(
        &self,
        run: &RunItem,
        outcome: Option<&str>,
        last_activity: &str,
    ) -> impl Future<Output = Res<()>> + Send;
    /// The name stored on the run's search item, `None` when the item holds none or does not exist. A consistent read:
    /// a retry of the naming op must see the name its first invocation wrote. A store without search items has no names.
    fn run_name(&self, run: &RunItem) -> impl Future<Output = Res<Option<String>>> + Send {
        let _ = run;
        async { Ok(None) }
    }
    /// Stores the run's name on its search item with `source` and the route `model` that wrote it, only when the item holds no
    /// name; `false` when it held one, which then stands. The update sets the three name attributes and, where the item is new,
    /// `runId` and `createdAt`, and nothing else: the time, the outcome and the prompt head belong to other writers. A store
    /// without search items stores nothing.
    fn put_run_name(
        &self,
        run: &RunItem,
        name: &str,
        source: &str,
        model: &str,
    ) -> impl Future<Output = Res<bool>> + Send {
        let _ = (run, name, source, model);
        async { Ok(false) }
    }
    /// What the tag branch needs of the run's search item before it acts: the marker that says the automatic step finished and the
    /// tags the person removed, in one consistent read of those two attributes. `None` when the item does not exist. A value of the wrong type is
    /// no value, except that the removal list keeps the entries it can read: the person's removal wins, so it must never be forgotten.
    /// A store without search items has no tag state.
    fn tag_state(&self, run: &RunItem) -> impl Future<Output = Res<Option<TagMarks>>> + Send {
        let _ = run;
        async { Ok(None) }
    }
    /// The vocabulary window: the tag lists of the owner's `limit` newest search items (at most 200), newest first, read from the
    /// partition of the stored run's owner with one eventually consistent `Query` that projects the three tag lists only. An item that
    /// cannot be decoded is skipped and counted. A store without search items has no vocabulary.
    fn tag_vocabulary(
        &self,
        run: &RunItem,
        limit: u32,
    ) -> impl Future<Output = Res<VocabRead>> + Send {
        let _ = (run, limit);
        async { Ok(VocabRead::default()) }
    }
    /// The tag lists of the run a follow-up continues (`run.continue_from`), read from the follow-up's own owner partition after the
    /// parent's Run item (for its `createdAt`). `None` when the run continues nothing, the parent run or its search item does not exist,
    /// or the parent belongs to another principal (its item is then never read).
    fn parent_tags(&self, run: &RunItem) -> impl Future<Output = Res<Option<TagState>>> + Send {
        let _ = run;
        async { Ok(None) }
    }
    /// A follow-up copies its parent's user and off lists once (`runtag::dynamo::inherit_update`): `false` when the item already holds a
    /// revision or an earlier copy, which then stand (a person's edit that came first wins), and when both lists are empty (nothing to
    /// write). A store without search items writes nothing.
    fn put_run_tags_inherit(
        &self,
        run: &RunItem,
        parent_run_id: &str,
        user: &[String],
        off: &[String],
    ) -> impl Future<Output = Res<bool>> + Send {
        let _ = (run, parent_run_id, user, off);
        async { Ok(false) }
    }
    /// Stores the automatic tags once (`runtag::dynamo::auto_update`): the list (left out when empty), the marker, the route
    /// and prompt version when the model answered. `false` when the item holds the marker already, which then stands. A store without
    /// search items writes nothing.
    fn put_run_tags_auto(
        &self,
        run: &RunItem,
        tags: &[String],
        at: &str,
        answered: Option<Answered<'_>>,
    ) -> impl Future<Output = Res<bool>> + Send {
        let _ = (run, tags, at, answered);
        async { Ok(false) }
    }
    /// Writes the run's research policy row into the research table unless the run already has one, so a retried `prepare`
    /// keeps the first. The default does nothing: a store with no research table leaves research off, because the route answers `disabled`
    /// to a run without a row.
    fn put_research_policy(
        &self,
        _item: &harness_protocol::research::PolicyItem,
    ) -> impl Future<Output = Res<()>> + Send {
        async { Ok(()) }
    }
    /// Clears a run whose execution ended early: the Run item's `status`, `version` and `aborted`, the root Goal's
    /// `status` and `version` and the events, together. `false` on a lost race (the caller re-reads).
    fn abort_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        events: &[Envelope],
    ) -> impl Future<Output = Res<bool>> + Send;
    /// Whether the run's Agent item registered `transcript.v1`: its image writes a stored conversation. A store that has not
    /// implemented it answers no, and the run is left to the run index's own fields.
    fn transcript_supported(&self, run_id: &str) -> impl Future<Output = Res<bool>> + Send {
        let _ = run_id;
        async { Ok(false) }
    }
    /// The text of the answers the run's user gave, oldest first: the `Instruct` controls that carry text and an origin of
    /// `user`. Sources the engine cannot write.
    fn user_answers(&self, run_id: &str) -> impl Future<Output = Res<Vec<String>>> + Send {
        let _ = run_id;
        async { Ok(Vec::new()) }
    }
    /// Writes a run's search body into its chat item under the owner's partition: `pk` and `sk` name the item (`CHAT#`, never the
    /// run index's `SRCH#`, which the run list reads whole); only the body's attributes are set (an update, so a repeat of this call is
    /// safe), and never `ownerPrincipalArn`.
    fn put_search_body(&self, item: &SearchWrite<'_>) -> impl Future<Output = Res<()>> + Send {
        let _ = item;
        async {
            Err(RunSessionError::Unavailable(
                "this store holds no search items".into(),
            ))
        }
    }
}

/// What the tag branch reads of the search item before it acts: the marker and the person's removals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagMarks {
    /// The marker: the automatic step finished (the model answered, even with an empty list, or inherited tags were written).
    pub at: Option<String>,
    /// The tags the person removed from this run.
    pub off: Vec<String>,
}

/// The vocabulary window as read: the tag lists of the items that decoded, newest first, and how many items did not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VocabRead {
    pub items: Vec<VocabItem>,
    pub skipped: usize,
}

/// What one search-body write carries.
#[derive(Clone, Debug)]
pub struct SearchWrite<'a> {
    /// `OWNER#<sha256 of the run owner's ARN>` and `CHAT#<createdAt>#<runId>` (the chat item, not the run index's).
    pub pk: &'a str,
    pub sk: &'a str,
    pub run_id: &'a str,
    pub created_at: &'a str,
    pub body: &'a ham_core::runsearch::SearchBody,
    /// When the body was built (RFC 3339).
    pub indexed_at: &'a str,
}

pub trait Tokens: Send + Sync {
    /// KMS-encrypts the task token under `ctx`; returns the base64 ciphertext.
    fn encrypt(&self, token: &str, ctx: &TokenContext) -> impl Future<Output = Res<String>> + Send;
    /// The plaintext of a ciphertext [`Tokens::encrypt`] made under the same `ctx` (the session nonce is read back
    /// by a retry of `start-session`); KMS refuses any other context.
    fn decrypt(
        &self,
        ciphertext: &str,
        ctx: &TokenContext,
    ) -> impl Future<Output = Res<String>> + Send;
}

/// The package index token of the session: minted here with the Lambda's own role,
/// outside the VM, and handed to agentd in the invocation.
pub trait PackageTokens: Send + Sync {
    /// A token lasting `duration_s`; `None` on a stage without a package index.
    fn token(&self, duration_s: u32)
    -> impl Future<Output = Result<Option<String>, String>> + Send;
}

/// The worker runtime at a qualifier (the run's pinned endpoint).
pub trait Sessions: Send + Sync {
    /// The worker runtime's ARN, looked up now (the SSM parameter the runtimes stack writes). `None` for an adapter with no lookup:
    /// a test, or a deployment whose `Config.runtime_arn` is fixed. The real adapter reads on every call, so the function does not
    /// have to be able to read the parameter to start (a half-deployed stage once killed it at init, and with it every op, the
    /// Blocked state and the reaper), and a warm function sees a parameter that was written after it started. `Invalid` means the
    /// stage has no runtime (no retry can help this run); `Unavailable` means the read failed for a reason that can pass.
    fn runtime_arn(&self) -> impl Future<Output = Res<Option<String>>> + Send {
        async { Ok(None) }
    }
    /// The Instances worker runtime's ARN, looked up now like [`Sessions::runtime_arn`]. `None` is an adapter with no Instances
    /// parameter named; `Invalid` is a stage whose parameter is absent or holds no ARN (this run can never start on it).
    fn instances_runtime_arn(&self) -> impl Future<Output = Res<Option<String>>> + Send {
        async { Ok(None) }
    }
    /// The id of the capacity provider the Instances runtime runs on, looked up now like [`Sessions::instances_runtime_arn`]: the stack that
    /// creates the provider publishes the id as an SSM parameter, and the function holds only the parameter's name. `None` is an adapter with no
    /// parameter named; `Invalid` is a stage whose parameter is absent or holds no provider id; `Unavailable` is a read that may pass on a retry.
    fn capacity_provider_id(&self) -> impl Future<Output = Res<Option<String>>> + Send {
        async { Ok(None) }
    }
    fn endpoint(
        &self,
        runtime_arn: &str,
        name: &str,
    ) -> impl Future<Output = Res<Option<EndpointState>>> + Send;
    fn invoke(
        &self,
        runtime_arn: &str,
        qualifier: &str,
        session_id: &str,
        body: &[u8],
    ) -> impl Future<Output = Res<Vec<u8>>> + Send;
    /// `StopRuntimeSession`; `false` when the session had already ended (AgentCore's not-found). Any other failure is an
    /// error: a session that could not be stopped is still running.
    fn stop(
        &self,
        runtime_arn: &str,
        qualifier: &str,
        session_id: &str,
    ) -> impl Future<Output = Res<bool>> + Send;
    /// `DeleteCapacityProviderSession`: ends an Instances session, its machine and its billing; `false` when the session had already
    /// ended. Any other failure is an error: a machine that could not be deleted is still billing. `StopRuntimeSession` alone does not end it.
    fn delete_provider_session(
        &self,
        capacity_provider_id: &str,
        session_id: &str,
    ) -> impl Future<Output = Res<bool>> + Send {
        let _ = (capacity_provider_id, session_id);
        async { Ok(false) }
    }
}

/// One object of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
}

/// A page of a listing, in key order, and whether more keys follow the last one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectPage {
    pub objects: Vec<ObjectInfo>,
    pub truncated: bool,
}

/// What a HEAD of one version of an object told: the SHA-256 checksum S3 stored with it, as S3 reports it (base64), when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectFacts {
    pub sha256_base64: Option<String>,
}

/// Objects of the harness bucket, by key.
pub trait Blobs: Send + Sync {
    fn get(&self, key: &str) -> impl Future<Output = Res<Option<Vec<u8>>>> + Send;
    fn exists(&self, key: &str) -> impl Future<Output = Res<bool>> + Send;
    /// Writes the object unless the key exists; `false` when it existed.
    fn put_if_absent(&self, key: &str, body: &[u8]) -> impl Future<Output = Res<bool>> + Send;
    /// At most `max_keys` (1 to 1,000) objects under `prefix` whose keys sort after `start_after`, in key order. A client that
    /// cannot list reports itself unavailable.
    fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> impl Future<Output = Res<ObjectPage>> + Send {
        let _ = (prefix, start_after, max_keys);
        async {
            Err(RunSessionError::Unavailable(
                "this client cannot list".into(),
            ))
        }
    }
    /// One object's bytes, refused above `max_bytes` (an object the engine may have written is never read unbounded: the function
    /// has 256 MB). `None`: no such key, or larger than `max_bytes`.
    fn get_bounded(
        &self,
        key: &str,
        max_bytes: usize,
    ) -> impl Future<Output = Res<Option<Vec<u8>>>> + Send {
        let _ = (key, max_bytes);
        async {
            Err(RunSessionError::Unavailable(
                "this client cannot read objects".into(),
            ))
        }
    }
    /// One object's bytes at `version_id`, or at the current version when `None`, refused above `max_bytes`. `None`: no such key, no
    /// such version, or larger than `max_bytes`. A client that cannot read a version reports itself unavailable, never the current object.
    fn get_version_bounded(
        &self,
        key: &str,
        version_id: Option<&str>,
        max_bytes: usize,
    ) -> impl Future<Output = Res<Option<Vec<u8>>>> + Send {
        let _ = (key, version_id, max_bytes);
        async {
            Err(RunSessionError::Unavailable(
                "this client cannot read a version of an object".into(),
            ))
        }
    }
    /// A HEAD of `version_id`, or of the current version when `None`, with the stored checksum. `None`: no such key or version.
    fn head_version(
        &self,
        key: &str,
        version_id: Option<&str>,
    ) -> impl Future<Output = Res<Option<ObjectFacts>>> + Send {
        let _ = (key, version_id);
        async {
            Err(RunSessionError::Unavailable(
                "this client cannot read a version of an object".into(),
            ))
        }
    }
}
