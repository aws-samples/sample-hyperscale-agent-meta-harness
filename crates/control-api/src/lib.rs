//! `/v1` handler behind the public API Gateway: the caller is the IAM
//! principal from the request context, and every run route checks it against the run's owner.
//! `direct` and `direct_models` are the direct run's `POST /v1/direct/runs` and its static model list
//! (`engine/catalog/models.json`); `controls` holds the cancel of a run and the answer to a stuck run's question (`input` holds the pieces of that answer that need no store); `cost` prices a run's token usage (W10) for
//! `GET /v1/runs/{runId}`. `chat_events` reads a run's events forward or from the end (`tail`, `before`) with the user's answers;
//! `run_index` keeps the owner-keyed search item that `GET /v1/runs` joins to its rows (title, model, last activity) and filters on; `snapshot`
//! is the download URL of the work in progress; `messages` is the owner's side of message control,
//! over the `message-ledger` crate. `head` is the commit a finished run delivered, read from the
//! verified bundle's header. `transcript` reads a run's stored conversation and
//! `search` the bounded chat-content index; `cursor` is the opaque position token the first issues and the second mints for a hit.
//! `tags` is the edit of a run's tags: `POST /v1/tags/edit` changes the two lists the person owns on the run's search item.
//! `retry` is the start of a blocked run again: `POST /v1/runs/{runId}/retry` reads the run's stored request and starts a new run with the
//! same bytes, linked to the blocked one; `direct` holds the checks and the write that it shares with a fresh start.
//! `resume` is the continuation of a run that stopped delivered, `paused` or `stalled`: `POST /v1/runs/{runId}/resume` starts a follow-up
//! run from the code the stopped run delivered, with the stopped run's request and a continuation paragraph on the prompt.
//! `version` is the backend's version (the root `VERSION` file that `build.rs` embeds) with the header that carries it on every answer;
//! `version_check` is the shape check `build.rs` shares with the library.

pub mod aws;
pub mod body;
pub mod chat_events;
pub mod controls;
pub mod cost;
pub mod cursor;
pub mod direct;
pub mod direct_models;
pub mod handler;
pub mod head;
pub mod ids;
pub mod input;
pub mod lambda;
pub mod messages;
pub mod repo_names;
pub mod resume;
pub mod retry;
pub mod run_index;
pub mod search;
pub mod snapshot;
pub mod tags;
pub mod transcript;
pub mod version;
pub mod version_check;
