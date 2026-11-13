//! The network work of the chat-client features that sit beside the Runs and run screens: the stored conversation, messages to a
//! working agent, the search across runs. The app asks with one effect, [`Job`], and the event loop answers with one message,
//! [`Done`]; the state each feature keeps is in its own module under `app.rs` (`history`, `send`, `openbox`, `engine_ui`), which is where a
//! [`Done`] is read. Nothing here keeps state, and no error names a link or a credential.

use crate::api::{Api, ApiError};
use crate::http::ControlPlane;
use ham_core::messages::{MessageResponse, MessagesPage, SendMessageResponse};
use ham_core::runsearch::{SearchQuery, SearchResponse};
use ham_core::stored::Read;
use ham_core::transcript::{TranscriptPage, TranscriptQuery};
use harness_protocol::messages::SendMessageRequest;

/// One request of a chat-client feature.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// One page of the stored conversation of `run_id`; `read` says which end the page belongs to.
    Transcript {
        run_id: String,
        query: TranscriptQuery,
        read: Read,
    },
    /// The run's messages with their states.
    Messages { run_id: String },
    /// `POST /v1/runs/{id}/messages`. The request's message id is the idempotency key: asking again with the same request is safe.
    SendMessage {
        run_id: String,
        request: SendMessageRequest,
    },
    /// Withdraws a message nobody has claimed.
    Withdraw { run_id: String, message_id: String },
    /// The search across runs.
    Search { query: SearchQuery },
    /// Before a follow-up starts: the newest run that continues from `run_id` and is not finished, if there is one (the run
    /// rows carry `continueFrom`).
    FindFollowUp { run_id: String },
    /// The second Enter of a follow-up: `POST /v1/direct/runs` with `continueFrom` naming the finished run `run_id` and `text` exactly as
    /// it stood in the box. `head` is the delivered commit the person was shown (`GET /v1/runs/{id}/head`); it goes into `continueFrom`, where
    /// control-api refuses the start with 409 `head-mismatch` when the delivery is another commit, and into the start key. `None` when the
    /// lookup did not answer (a stage without the route): the start then leaves the head to control-api, as `ham continue` does.
    StartFollowUp {
        run_id: String,
        text: String,
        head: Option<String>,
        /// The engine of the finished run: the follow-up runs on it.
        engine: Option<String>,
    },
    /// The second Enter of the follow-up of a repository run: like [`Job::StartFollowUp`], with the repository the finished run used
    /// as the target, so the new run opens a pull request in it (a zip alone would not).
    StartRepoFollowUp {
        run_id: String,
        text: String,
        head: Option<String>,
        repo: harness_protocol::target::RepoTarget,
        engine: Option<String>,
    },
    /// The second Enter (or `r`) of the retry of a blocked run: `POST /v1/runs/{id}/retry`, which starts the run again with the request it
    /// was started with. The call carries no prompt, rules or settings; the key is `retry-<run id>`.
    RetryRun { run_id: String },
    /// The confirmed `R` of the Runs screen: `POST /v1/runs/{id}/resume` for each run, four at a time, key `resume-<run id>`. The runs are
    /// the ones the plan took when the question was asked; the service decides each again.
    ResumeRuns { run_ids: Vec<String> },
    /// The commit a finished run delivered, the engine's last message and the files it changed.
    Head { run_id: String },
    /// The full prompt of an earlier run: the run list sends only a title, so a recalled prompt is read from the run's detail.
    FullPrompt { run_id: String },
    /// The settings hash of the engine file as it is now: asked when a follow-up is prepared, so the
    /// confirmation can say when the settings differ from those of the run it continues. Reads one file; makes no call. `engine` is the id
    /// of the engine the follow-up runs on.
    EngineSettings { engine: String },
}

/// Why a send or a withdrawal did not succeed: the server's answer (`status`, its `code` and its `message`), or `status` 0 when there was
/// none (a network error, an answer this client cannot read).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendFailure {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl From<ApiError> for SendFailure {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Http { status, error } => Self {
                status,
                code: error.code,
                message: error.message,
            },
            other => Self {
                status: 0,
                code: String::new(),
                message: other.to_string(),
            },
        }
    }
}

/// The answer to a [`Job`].
#[derive(Clone, Debug)]
pub enum Done {
    Transcript {
        run_id: String,
        read: Read,
        result: Result<TranscriptPage, String>,
    },
    Messages {
        run_id: String,
        result: Result<MessagesPage, String>,
    },
    Sent {
        run_id: String,
        message_id: String,
        result: Result<SendMessageResponse, SendFailure>,
    },
    Withdrawn {
        run_id: String,
        message_id: String,
        result: Result<MessageResponse, SendFailure>,
    },
    Searched {
        query: String,
        result: Result<SearchResponse, String>,
    },
    /// The answer to [`Job::FindFollowUp`]: the run that is going and its status, `None` when there is none, or the failure's words.
    FollowUpFound {
        run_id: String,
        result: Result<Option<(String, harness_protocol::goal::GoalStatus)>, String>,
    },
    /// The answer to [`Job::StartFollowUp`]: the id of the new run, or the server's own words.
    FollowUpStarted {
        run_id: String,
        result: Result<String, String>,
    },
    /// [`Done::FollowUpStarted`] with the line of provider data sharing: the switch of the account's mode before the start, or why ham left it
    /// alone. The screen shows it beside the outcome.
    FollowUpNoted {
        run_id: String,
        result: Result<String, String>,
        note: String,
    },
    /// The answer to [`Job::RetryRun`]: the id of the new run, or the plain words for the service's refusal (`ham_core::retry::explain`).
    RetryStarted {
        run_id: String,
        result: Result<String, String>,
    },
    /// [`Done::RetryStarted`] for a retry that started after ham looked at the account of its backend: the line of provider data
    /// sharing with it. The screen shows it beside the outcome.
    RetryNoted {
        run_id: String,
        result: Result<String, String>,
        note: String,
    },
    /// The answer to [`Job::ResumeRuns`]: what each run did, in the order they were asked.
    Resumed {
        outcomes: Vec<ham_core::resume::Outcome>,
    },
    /// [`Done::Resumed`] when ham looked at the account of a run that started: the line of provider data sharing of each such run,
    /// with the id of the new run.
    ResumedNoted {
        outcomes: Vec<ham_core::resume::Outcome>,
        notes: Vec<(String, String)>,
    },
    /// The answer to [`Job::Head`]: the checked delivered head, or why there is none (`status` 0: no answer or an answer this client cannot use).
    Head {
        run_id: String,
        result: Result<ham_core::head::DeliveredHead, SendFailure>,
    },
    /// The answer to [`Job::FullPrompt`]: the run's prompt (`RunDetail.objective`), or why it could not be read.
    FullPrompt {
        run_id: String,
        result: Result<String, String>,
    },
    /// The answer to [`Job::EngineSettings`]: the canonical hash of the settings the next run would send (`None` when the user has no engine
    /// file or it sets no key), or the words of the refusal when the file is refused.
    EngineSettings {
        result: Result<Option<String>, String>,
    },
}

/// Runs one job against the API.
pub async fn perform<P: ControlPlane>(api: &Api<P>, job: Job) -> Done {
    perform_with_env(api, job, &ham_core::envnames::os_env).await
}

/// [`perform`] with the environment the user's rules and settings are found through (a test passes its own).
pub async fn perform_with_env<P: ControlPlane>(
    api: &Api<P>,
    job: Job,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Done {
    match job {
        Job::Transcript {
            run_id,
            query,
            read,
        } => {
            let result = api
                .transcript_page(&run_id, &query)
                .await
                .map_err(|e| e.to_string());
            Done::Transcript {
                run_id,
                read,
                result,
            }
        }
        Job::Messages { run_id } => {
            let result = api.list_messages(&run_id).await.map_err(|e| e.to_string());
            Done::Messages { run_id, result }
        }
        Job::SendMessage { run_id, request } => {
            let message_id = request.message_id.clone();
            let result = api
                .send_message(&run_id, &request)
                .await
                .map_err(SendFailure::from);
            Done::Sent {
                run_id,
                message_id,
                result,
            }
        }
        Job::Withdraw { run_id, message_id } => {
            let result = api
                .withdraw_message(&run_id, &message_id)
                .await
                .map_err(SendFailure::from);
            Done::Withdrawn {
                run_id,
                message_id,
                result,
            }
        }
        Job::Search { query } => {
            let q = query.q.clone();
            let result = api.search_runs(&query).await.map_err(|e| e.to_string());
            Done::Searched { query: q, result }
        }
        Job::FindFollowUp { run_id } => {
            let result = api
                .list_runs()
                .await
                .map_err(|e| e.to_string())
                .map(|rows| {
                    rows.into_iter()
                        .find(|r| {
                            r.continue_from.as_deref() == Some(run_id.as_str())
                                && !r.status.is_terminal()
                        })
                        .map(|r| (r.run_id, r.status))
                });
            Done::FollowUpFound { run_id, result }
        }
        Job::StartFollowUp {
            run_id,
            text,
            head,
            engine,
        } => {
            let mut note = None;
            let result = start_follow_up(
                api,
                &run_id,
                &text,
                head.as_deref(),
                ham_core::command::TargetArg::S3Only,
                engine.as_deref(),
                env,
                &mut note,
            )
            .await;
            follow_up_done(run_id, result, note)
        }
        Job::StartRepoFollowUp {
            run_id,
            text,
            head,
            repo,
            engine,
        } => {
            let mut note = None;
            let result = start_follow_up(
                api,
                &run_id,
                &text,
                head.as_deref(),
                ham_core::command::TargetArg::Repo(repo),
                engine.as_deref(),
                env,
                &mut note,
            )
            .await;
            follow_up_done(run_id, result, note)
        }
        // A resume and a retry start the request the service stored: the check of provider data sharing comes first, on the backend
        // that holds the run. `run.provider_sharing` of the file is the agreement to a switch; a file that cannot be read counts as off.
        Job::ResumeRuns { run_ids } => {
            let crate::cli::Resumed { outcomes, notes } =
                crate::cli::resume_many(api, &run_ids, None, sharing_setting(env)).await;
            if notes.is_empty() {
                Done::Resumed { outcomes }
            } else {
                Done::ResumedNoted { outcomes, notes }
            }
        }
        Job::RetryRun { run_id } => {
            let cleared = match crate::sharing::preflight_stored(
                api,
                &run_id,
                crate::sharing::StoredModel::Unread,
                sharing_setting(env),
            )
            .await
            {
                Ok(c) => c,
                Err(stop) => {
                    return Done::RetryStarted {
                        run_id,
                        result: Err(stop),
                    };
                }
            };
            let result = cleared
                .api
                .retry_run(&run_id)
                .await
                .map(|r| r.run_id)
                .map_err(|e| {
                    cleared.failed(match e {
                        ApiError::Http { status, error } => {
                            ham_core::retry::explain(status, &error.code, &error.message)
                        }
                        other => ham_core::retry::explain(0, "", &other.to_string()),
                    })
                });
            match (cleared.note, result.is_ok()) {
                (Some(note), true) => Done::RetryNoted {
                    run_id,
                    result,
                    note,
                },
                _ => Done::RetryStarted { run_id, result },
            }
        }
        Job::Head { run_id } => {
            let result = api.delivered_head(&run_id).await.map_err(SendFailure::from);
            Done::Head { run_id, result }
        }
        Job::EngineSettings { engine } => {
            let result = crate::rules_cmd::engine_settings(env, &engine)
                .map(|c| c.map(|c| c.canonical_sha256()))
                .map_err(|e| e.to_string());
            Done::EngineSettings { result }
        }
        Job::FullPrompt { run_id } => {
            let result = api
                .get_run(&run_id)
                .await
                .map_err(|e| e.to_string())
                .and_then(|detail| {
                    detail
                        .objective
                        .filter(|o| !o.trim().is_empty())
                        .ok_or_else(|| "the run has no stored prompt".to_string())
                });
            Done::FullPrompt { run_id, result }
        }
    }
}

/// The follow-up's start: the ordinary direct-run request with `target` and the member `continueFrom`.
/// The target is `s3-only` (a zip alone) or the repository the finished run used (a pull request there); control-api checks that a repository is
/// the source run's own. The rules and skills are read from disk first, as for `ham continue`, so a missing rules directory stops the start
/// before anything is sent.
/// The head the person was shown goes into `continueFrom` (control-api resolves the delivered head itself and refuses with 409 `head-mismatch`
/// when it is another commit, so the run that starts is the one that was confirmed); without one the head is left to control-api. The key is `fu-`
/// plus 32 hex digits made from the source run, the seed commit (the full head, or none) and the text: a retry of the same text returns the
/// same run, and an edited text, or another head, starts another.
#[allow(clippy::too_many_arguments)]
async fn start_follow_up<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    text: &str,
    head: Option<&str>,
    target: ham_core::command::TargetArg,
    engine: Option<&str>,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
    note: &mut Option<String>,
) -> Result<String, String> {
    use ham_core::command::DirectArgs;
    let (cfg, _) = crate::config_cmd::load(env)?;
    let args = DirectArgs {
        prompt: text.to_string(),
        target,
        base: None,
        model: None,
        rules: None,
        no_rules: false,
        continue_file: None,
        input_wait: None,
        skills: None,
        no_skills: false,
        continue_from: Some(run_id.to_string()),
        goal: None,
        research: None,
        allow_third_party: false,
        attach: Vec::new(),
        compute: None,
        max_run_hours: None,
        engine: engine.map(str::to_string),
        provider_sharing: None,
    };
    // A follow-up reads the file afresh and inherits no flag of the source run: the goal loop, research, rules, skills, wait, model and
    // report switch are what the file says now (the same function the command line and the composer call).
    let resolved = ham_core::request::resolve_run_settings(&cfg, env, &args, true)
        .map_err(|e| e.to_string())?;
    let inputs = crate::rules_cmd::direct_inputs_for(env, &resolved.args)?;
    let mut request = resolved.request(inputs).map_err(|e| e.to_string())?;
    if let (Some(head), Some(from)) = (head, request.continue_from.as_mut()) {
        from.head_sha = Some(head.to_string());
    }
    let key = ham_core::followup::start_key(run_id, head.unwrap_or(""), text);
    // Provider data sharing in the account: the same check as every other start, right before it; its line goes to `note`. The check
    // aims the client at the backend that holds the finished run, and the start is sent through that client.
    let cleared =
        crate::sharing::preflight(api, &request, crate::sharing::agreement_of(&resolved)).await?;
    note.clone_from(&cleared.note);
    cleared
        .api
        .start_direct_run(&request, &key)
        .await
        .map(|r| r.run_id)
        .map_err(|e| e.to_string())
}

/// `run.provider_sharing` of the config file as it is now; a file that cannot be read counts as off.
fn sharing_setting(env: &(dyn Fn(&str) -> Option<String> + Sync)) -> bool {
    crate::config_cmd::load(env).is_ok_and(|(cfg, _)| crate::cli::sharing_setting(&cfg))
}

/// The answer of a follow-up: with its line of provider data sharing when there is one.
fn follow_up_done(run_id: String, result: Result<String, String>, note: Option<String>) -> Done {
    match note {
        Some(note) => Done::FollowUpNoted {
            run_id,
            result,
            note,
        },
        None => Done::FollowUpStarted { run_id, result },
    }
}
