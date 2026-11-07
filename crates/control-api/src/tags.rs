//! The edit of a run's tags: `POST /v1/tags/edit`. The person adds and removes tags on the runs they own; nothing else
//! changes `runTagsUser` and `runTagsOff`, the two lists of the run's search item that belong to the person. The tag branch of `name-run`
//! (run-session) writes the automatic list and never these two; the start, the end of a run and the backfill write no tag attribute.
//!
//! The wire types, the rules of an edit and the DynamoDB expression are `ham_core::runtag` (`wire`, `edit::apply_edit`,
//! `dynamo::edit_update`); this module is the route around them.
//!
//! One request holds at most 40 edits (`wire::MAX_EDITS`) and answers one result for each, in request order. A body that is not a
//! `TagEditRequest`, or that breaks a wire limit (edits, tags in an edit, a run id that is not plain text, a run twice in one batch), is a 400
//! before anything is read. A request that holds no edit is a 200 with no result.
//!
//! Per edit, `EDIT_GROUP` (8) at a time as the run list's joins read:
//!
//! 1. The run must exist and belong to the caller ([`crate::handler::owned`]); any other case, a run that does not exist included, is
//!    `not-found` with the same body, and nothing more is read or written. The owner partition of every read and write is the hash of the
//!    authenticated caller, never anything in the request.
//! 2. The search item's tag state is read once, consistently ([`Store::get_run_tag_state`]). No item is an empty state; the edit creates
//!    the item with `runId` and `createdAt` (the run's own, so the key is the start's key) and the start or the backfill completes it
//!    later without touching a tag attribute. If the item holds an attribute the edit would replace (`runTagsUser`, `runTagsOff`) or compare
//!    against (`runTagsRev`) in a form the reader cannot take as written, the edit is `invalid` with a plain message and nothing is
//!    written, because writing over a list nobody can read destroys it. The log line names the run and the attributes, no tag.
//! 3. [`ham_core::runtag::edit::apply_edit`] plans the new lists. A refusal (a tag that breaks a rule, a list past its cap) is `invalid`
//!    with the rule text; a plan equal to the state is `unchanged` (a replay): neither writes.
//! 4. Otherwise one conditional `UpdateItem`, [`ham_core::runtag::dynamo::edit_update`], with `runTagsRev` as the compare-and-set base. A
//!    lost race (`StoreError::Conflict`) reads again once and plans again from the new state, which may now be `unchanged` or `invalid`; a
//!    second lost race is `conflict`, with this edit not written.
//! 5. The tags in the answer are [`ham_core::runtag::effective::effective`] of the state the write was planned from, computed in memory:
//!    the same function the list row uses, with the item's own repository (else the run's target text) so that a removal recorded against
//!    the derived project matches the tag it hides.
//!
//! A store error (a throttle, an outage) is not a status of the edit: the whole request is a 503 "retry", after the group of eight that met
//! it has finished (no later group starts). A batch is not atomic: the edits applied before the error stay applied. Each edit is a delta
//! on the run's lists, so a retry of the batch answers `unchanged` for the ones that were applied, and it also repeats the person's edit
//! if another edit changed the same list in between. The store's reason goes to the log, not the body.
//!
//! A run whose stored tags cannot be read (step 2) stays uneditable through this route until the attribute is repaired: the edit refuses
//! and the lists are left as they are. Search items are never deleted or expired (only the route-health items of the table carry `ttl`), so
//! an edit that races a deletion cannot recreate a stub.

use crate::body::parse_body;
use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, StoreError, V1Request, error, json, owned, unavailable,
};
use crate::run_index::join_bounded;
use ham_core::run_index::target_text;
use ham_core::runtag::dynamo::edit_update;
use ham_core::runtag::edit::{TagState, apply_edit};
use ham_core::runtag::effective::effective;
use ham_core::runtag::wire::{TagEdit, TagEditRequest, TagEditResponse, TagEditResult, WireError};

/// Edits in flight at once.
pub const EDIT_GROUP: usize = 8;

/// What the person is told when a run's stored tags cannot be read. No attribute name: that is for the log.
pub const UNREADABLE_MESSAGE: &str =
    "the stored tags of this run cannot be read, so this edit was not applied";

/// One edit's result and the diagnostics it produced.
struct Done {
    result: TagEditResult,
    logs: Vec<String>,
}

impl From<TagEditResult> for Done {
    fn from(result: TagEditResult) -> Self {
        Done {
            result,
            logs: Vec::new(),
        }
    }
}

/// The words of a refused batch. A run id that broke the rule is caller text and is not printed back; the other refusals name
/// ids that passed the rule, a count or a limit.
fn wire_words(e: &WireError) -> String {
    match e {
        WireError::BadRunId { .. } => {
            "a run id is 1 to 64 letters, digits, hyphens or underscores".to_string()
        }
        other => other.to_string(),
    }
}

/// `POST /v1/tags/edit`.
pub(crate) async fn edit<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let request: TagEditRequest = match parse_body("TagEditRequest", &req.body) {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    if let Err(e) = request.validate() {
        return error(400, "bad-request", &wire_words(&e));
    }
    let store = &deps.store;
    let mut results = Vec::with_capacity(request.edits.len());
    let mut logs: Vec<String> = Vec::new();
    for group in request.edits.chunks(EDIT_GROUP) {
        let outcomes = join_bounded(
            group
                .iter()
                .map(|edit| edit_one(store, caller, edit))
                .collect(),
            EDIT_GROUP,
        )
        .await;
        let mut failure: Option<StoreError> = None;
        for outcome in outcomes {
            match outcome {
                Ok(done) => {
                    logs.extend(done.logs);
                    results.push(done.result);
                }
                Err(e) => failure = failure.or(Some(e)),
            }
        }
        if let Some(e) = failure {
            let (resp, reason) = unavailable(&e, "retry the same request");
            logs.extend(reason);
            return (resp, logs);
        }
    }
    let (resp, _) = json(200, &TagEditResponse { results });
    (resp, logs)
}

/// One edit, from the ownership check to the answer. `Err` is a store error, which fails the request.
async fn edit_one<S: Store>(store: &S, caller: &str, edit: &TagEdit) -> Result<Done, StoreError> {
    let Some(run) = owned(store, &edit.run_id, caller).await? else {
        return Ok(TagEditResult::not_found(&edit.run_id).into());
    };
    let mut attempt = 1;
    loop {
        let read = store.get_run_tag_state(caller, &run).await?;
        let (state, repository, revision) = match read {
            None => (TagState::default(), target_text(&run.target), None),
            Some(read) if !read.unreadable.is_empty() => {
                return Ok(Done {
                    result: TagEditResult::invalid(&edit.run_id, UNREADABLE_MESSAGE),
                    logs: vec![format!(
                        "tag edit refused: the search item of run {} holds tag attributes it cannot read: {}",
                        run.run_id,
                        read.unreadable.join(", ")
                    )],
                });
            }
            Some(read) => {
                let item = read.item;
                (
                    TagState {
                        auto: item.run_tags.clone().unwrap_or_default(),
                        user: item.run_tags_user.clone().unwrap_or_default(),
                        off: item.run_tags_off.clone().unwrap_or_default(),
                    },
                    item.tag_repository(&run.target),
                    item.run_tags_rev,
                )
            }
        };
        let next = match apply_edit(&state, &repository, &edit.add, &edit.remove) {
            Ok(next) => next,
            Err(e) => return Ok(TagEditResult::invalid(&edit.run_id, &e.message()).into()),
        };
        let shown = |s: &TagState| effective(&repository, &s.auto, &s.user, &s.off).tags;
        if next == state {
            return Ok(TagEditResult::unchanged(&edit.run_id, shown(&state)).into());
        }
        let spec = edit_update(
            &run.run_id,
            &run.created_at,
            &next.user,
            &next.off,
            revision,
        );
        match store.put_run_tags(caller, &run, &spec).await {
            Ok(()) => return Ok(TagEditResult::ok(&edit.run_id, shown(&next)).into()),
            Err(StoreError::Conflict) if attempt < 2 => attempt += 1,
            Err(StoreError::Conflict) => {
                return Ok(TagEditResult::conflict(&edit.run_id).into());
            }
            Err(e) => return Err(e),
        }
    }
}
