//! The switch of the account's Bedrock data retention mode before a run on a model that needs provider data sharing. The rules and the words are
//! `ham_core::sharing`; this module makes the calls, with the user's own credentials, in the stage's Region: `GET /v1/whoami` (the caller and the stage's
//! account), `GET /data-retention`, and, only when the run carries the user's agreement and the mode is `default` or `inherit` in the account of the
//! credentials, `PUT /data-retention` with `provider_data_share` and a read back. Every start of a new run calls [`preflight`] right before
//! `POST /v1/direct/runs`: the command line, the composer and the follow-up. A resume and a retry, which start the request the service stored, call
//! [`preflight_stored`] before their own call. A stop is returned before anything is sent, so no model is called.
//!
//! The account is the one of the backend the run starts on. With several backends every check first asks the plane which backend the start goes
//! to ([`ControlPlane::start_backend`]) and makes every call of the check, and the start itself, on a client aimed at that backend ([`Cleared`]).
//! A start that names no backend, or another backend than the one that holds the run it continues, is refused before any account is read.
//!
//! ham writes no file for it (root invariant 5): the record of the change is the line the user sees and the account's own record of the call.

use crate::api::Api;
use crate::http::ControlPlane;
use ham_core::models::Model;
use ham_core::request::{Origin, ResolvedRun};
use ham_core::settings::Key;
use ham_core::sharing::{self, Agreement, Plan, Put, Read, Shown, Survey, Switching};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::Engine;
use std::time::Duration;

/// Reads after a switch while the read still shows a mode that does not let the model run (a setting can take a moment to read back).
const READ_BACK_TRIES: usize = 3;
const READ_BACK_PAUSE: Duration = Duration::from_millis(500);

/// The Region's words when the read failed before any Region was named.
const STAGE_REGION: &str = "the stage's Region";

/// How many pages of the run list are read for the row of one run before its model counts as unknown.
const ROW_PAGES: usize = 3;

/// How the run agreed to sharing: `run.provider_sharing` of `config.toml`, or a choice for this run (`--provider-sharing on`, the answer `y`).
pub fn agreement_of(resolved: &ResolvedRun) -> Agreement {
    match resolved.origin(Key::RunProviderSharing) {
        Some(Origin::File) => Agreement::Setting,
        _ => Agreement::ThisRun,
    }
}

/// The row of the request's model when it needs provider data sharing.
pub fn sharing_model(request: &StartDirectRunRequest) -> Option<&'static Model> {
    ham_core::models::builtin_catalog()
        .check(Engine::or_default(request.engine), &request.model)
        .ok()
        .filter(|m| m.requires_provider_sharing)
}

/// The display name of the model `id` when a row of any engine's list says that it needs provider data sharing.
pub fn sharing_model_name(id: &str) -> Option<String> {
    ham_core::models::builtin_catalog()
        .sharing_row(id)
        .map(|m| m.name.clone())
}

fn read_of(
    answer: Option<Result<crate::http::RetentionAnswer, crate::http::TransportError>>,
) -> (Option<String>, Read) {
    match answer {
        Some(Ok(a)) => (
            Some(a.region),
            Read::from_answer(a.response.status, &a.response.body),
        ),
        Some(Err(e)) => (None, Read::Failed(e.to_string())),
        None => (None, Read::Failed("no answer".into())),
    }
}

/// `api` aimed at the backend a start goes to: the one it is aimed at already, else the one that holds `source` (the run a follow-up, a resume
/// or a retry continues), else the default one. A client with one backend comes back as it is. `Err` is the refusal of a start that has no
/// backend to go to, or that names another backend than the one that holds `source`; nothing was read or changed for it.
pub async fn aimed<P: ControlPlane>(api: &Api<P>, source: Option<&str>) -> Result<Api<P>, String> {
    match api.plane().start_backend(api.target(), source).await {
        Ok(Some(alias)) => Ok(api.on(&alias)),
        Ok(None) => Ok(api.clone()),
        Err(e) => Err(e.to_string()),
    }
}

/// A start that may go on: the client aimed at the backend whose account was looked at (the start is sent through it), and the line about the
/// account when there is one.
pub struct Cleared<P> {
    pub api: Api<P>,
    pub note: Option<String>,
}

impl<P> Cleared<P> {
    /// [`with_note`] for a start that failed after this check: the line first, because the account may already have changed.
    pub fn failed(&self, error: impl std::fmt::Display) -> String {
        with_note(self.note.as_deref(), error)
    }
}

impl<P> std::fmt::Debug for Cleared<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cleared")
            .field("note", &self.note)
            .finish_non_exhaustive()
    }
}

/// A start that needed no look at any account: it goes through `api` as it came.
fn untouched<P: ControlPlane>(api: &Api<P>) -> Cleared<P> {
    Cleared {
        api: api.clone(),
        note: None,
    }
}

/// What ham knows before it decides, on a client aimed at the backend the run starts on; `None` for a plane with no AWS account behind it (a
/// stand-in stage), which is never read or changed.
pub async fn survey<P: ControlPlane>(api: &Api<P>) -> Option<Survey> {
    let answer = api.plane().data_retention(api.target()).await?;
    let (region, read) = read_of(Some(answer));
    let (caller_arn, stage_account) = match api.whoami().await {
        Ok(w) => (Some(w.principal_arn), Some(w.account)),
        Err(_) => (None, None),
    };
    Some(Survey {
        region: region.unwrap_or_else(|| STAGE_REGION.to_string()),
        caller_arn,
        stage_account,
        read,
    })
}

/// [`survey`], with the survey of a backend that has no AWS account for a stand-in plane.
async fn survey_or_none<P: ControlPlane>(api: &Api<P>) -> Survey {
    survey(api)
        .await
        .unwrap_or_else(|| sharing::no_account(STAGE_REGION))
}

/// The question of the composer: its words, and what they showed the person. The answer `y` agrees to `shown` ([`preflight_agreed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub text: String,
    pub shown: Shown,
}

/// The question of the composer for `name` (`id`): the model, what sharing means, the account and the Region of the backend the run would start
/// on, and what ham changes there. A start with no backend to go to gets the refusal of the start instead.
pub async fn question<P: ControlPlane>(
    api: &Api<P>,
    name: &str,
    id: &str,
) -> Result<Question, String> {
    let api = aimed(api, None).await?;
    let s = survey_or_none(&api).await;
    let shown = Shown::of(&s);
    Ok(Question {
        text: sharing::ask_text(name, id, &s, &shown.plan),
        shown,
    })
}

async fn read_back<P: ControlPlane>(api: &Api<P>) -> Read {
    let mut last = Read::Failed("no answer".into());
    for i in 0..READ_BACK_TRIES {
        if i > 0 {
            tokio::time::sleep(READ_BACK_PAUSE).await;
        }
        last = read_of(api.plane().data_retention(api.target()).await).1;
        match &last {
            Read::Mode(m) if !sharing::mode_runs(m) => {}
            _ => break,
        }
    }
    last
}

/// The switch of `account` in the Region of `s` from `from`, and its read back: the line of the switch, or the one plain message that stops the
/// start (the switch was denied, refused, or did not leave a mode in which the model runs). `api` is aimed at the backend of that account.
async fn switch<P: ControlPlane>(
    api: &Api<P>,
    name: &str,
    s: &Survey,
    account: &str,
    from: &str,
    agreement: Agreement,
) -> Result<String, String> {
    let cx = Switching {
        name,
        account,
        region: &s.region,
        from,
    };
    let put = match api
        .plane()
        .put_data_retention(api.target(), sharing::SWITCH_TO)
        .await
    {
        Some(Ok(a)) => Put::from_answer(a.response.status, &a.response.body),
        // A call that may have reached Bedrock: the read back decides.
        Some(Err(e)) => Put::Unsure(e.to_string()),
        None => Put::Rejected("this backend cannot change the mode".into()),
    };
    let readback = match put {
        Put::Denied(_) | Put::Rejected(_) => Read::Failed("not read".into()),
        _ => read_back(api).await,
    };
    let now = sharing::settle(&put, &readback, &cx)?;
    Ok(sharing::switched_line(
        name, account, &s.region, from, &now, agreement,
    ))
}

/// What a plan does for a run that agreed: keep, leave with a line, or switch and read back.
async fn act<P: ControlPlane>(
    api: Api<P>,
    name: &str,
    s: &Survey,
    plan: Plan,
    agreement: Agreement,
) -> Result<Cleared<P>, String> {
    let note = match plan {
        Plan::Keep { .. } => None,
        Plan::Leave { why } => Some(sharing::leave_line(name, &s.region, &why)),
        Plan::Switch { account, from } => {
            Some(switch(&api, name, s, &account, &from, agreement).await?)
        }
    };
    Ok(Cleared { api, note })
}

/// Right before `POST /v1/direct/runs` of `request`. A request that does not carry `providerSharing: true`, or whose model needs nothing, is not looked
/// at and goes through `api` as it came. Otherwise the client is aimed at the backend the run starts on and the mode of that backend's account is
/// read: a mode that lets the model run is kept (no line), a mode ham cannot read or may not change is left alone with a line, and `default` or
/// `inherit` is switched and read back (the switch line). `Err` is the one plain message that stops the start: it has no backend to go to, or the
/// switch was denied, refused, or did not leave a mode in which the model runs. The caller sends the start through [`Cleared::api`].
pub async fn preflight<P: ControlPlane>(
    api: &Api<P>,
    request: &StartDirectRunRequest,
    agreement: Agreement,
) -> Result<Cleared<P>, String> {
    if request.provider_sharing != Some(true) {
        return Ok(untouched(api));
    }
    let Some(model) = sharing_model(request) else {
        return Ok(untouched(api));
    };
    let source = request.continue_from.as_ref().map(|c| c.run_id.as_str());
    let api = aimed(api, source).await?;
    let Some(s) = survey(&api).await else {
        return Ok(Cleared { api, note: None });
    };
    let plan = sharing::plan(&s);
    act(api, &model.name, &s, plan, agreement).await
}

/// What the answer `y` led to.
pub enum Agreed<P> {
    /// The account was as the question showed it: the start may go on.
    Go(Cleared<P>),
    /// The account is in another state than the question showed: nothing was changed, and this is the question for the state it is in now.
    AskAgain(Question),
}

impl<P> std::fmt::Debug for Agreed<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Agreed::Go(c) => f.debug_tuple("Go").field(c).finish(),
            Agreed::AskAgain(q) => f.debug_tuple("AskAgain").field(q).finish(),
        }
    }
}

/// [`preflight`] for the answer `y` to a question that showed `shown`. The person agreed to what was on screen, so the account is read again and
/// compared first: another account, Region or mode, a read that now succeeds or one that now fails is a state nobody agreed to, and ham then
/// changes nothing and asks again. Only a state equal to `shown` goes on to what the question said.
pub async fn preflight_agreed<P: ControlPlane>(
    api: &Api<P>,
    request: &StartDirectRunRequest,
    shown: &Shown,
) -> Result<Agreed<P>, String> {
    if request.provider_sharing != Some(true) {
        return Ok(Agreed::Go(untouched(api)));
    }
    let Some(model) = sharing_model(request) else {
        return Ok(Agreed::Go(untouched(api)));
    };
    let source = request.continue_from.as_ref().map(|c| c.run_id.as_str());
    let api = aimed(api, source).await?;
    let s = survey_or_none(&api).await;
    let now = Shown::of(&s);
    if now != *shown {
        return Ok(Agreed::AskAgain(Question {
            text: sharing::ask_again_text(&model.name, &model.id, &s, &now.plan),
            shown: now,
        }));
    }
    // A plane with no AWS account has nothing to say a line about, as in `preflight`.
    if s == sharing::no_account(STAGE_REGION) {
        return Ok(Agreed::Go(Cleared { api, note: None }));
    }
    act(api, &model.name, &s, now.plan, Agreement::ThisRun)
        .await
        .map(Agreed::Go)
}

/// What the caller of [`preflight_stored`] knows about the model of the stored run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredModel<'a> {
    /// Its row of the run list was read: the model the row names, if it names one.
    Row(Option<&'a str>),
    /// The row was not read: the check reads it.
    Unread,
}

/// The model a run was started on, as its row of the run list names it (`GET /v1/runs?q=<run id>`): `None` when the first pages hold no row of
/// the run, the row names no model (a stage without the run index), or the list could not be read.
pub async fn stored_model<P: ControlPlane>(api: &Api<P>, run_id: &str) -> Option<String> {
    let filter = ham_core::run_index::RunFilter {
        q: Some(run_id.to_string()),
        ..ham_core::run_index::RunFilter::default()
    };
    let walk = api.run_rows_walk(&filter, ROW_PAGES).await.ok()?;
    walk.rows
        .into_iter()
        .find(|r| r.summary.run_id == run_id)
        .and_then(|r| r.model)
}

/// Before `POST /v1/runs/{id}/resume` and `/retry`: the service starts the request `run_id` was started with, so what the client can know is the
/// model of that run, from its row of the run list. The client is aimed at the backend that holds the run; a run whose backend cannot be told
/// is not looked at (the call itself then says why it does not start). A model that needs nothing, or one ham does not know, is not looked at
/// either. For a model that needs provider data sharing the mode of that backend's account is read: a mode that lets the model run is kept, a
/// mode ham cannot read is left alone with a line, and a mode that has to be switched is switched only when `setting_on`
/// (`run.provider_sharing` of `config.toml`) says the user agrees to it; with the setting off nobody agreed to a switch for the new run, so ham
/// stops with one plain message and starts nothing. The caller sends its call through [`Cleared::api`].
pub async fn preflight_stored<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    model: StoredModel<'_>,
    setting_on: bool,
) -> Result<Cleared<P>, String> {
    let Ok(owner) = aimed(api, Some(run_id)).await else {
        return Ok(untouched(api));
    };
    let model = match model {
        StoredModel::Row(m) => m.map(str::to_string),
        StoredModel::Unread => stored_model(&owner, run_id).await,
    };
    let Some(name) = model.as_deref().and_then(sharing_model_name) else {
        return Ok(untouched(api));
    };
    let Some(s) = survey(&owner).await else {
        return Ok(Cleared {
            api: owner,
            note: None,
        });
    };
    match sharing::plan(&s) {
        Plan::Switch { account, from } if !setting_on => {
            Err(sharing::stored_stop(&name, &account, &s.region, &from))
        }
        plan => act(owner, &name, &s, plan, Agreement::Setting).await,
    }
}

/// The error of a start that failed after a line was due: the line first, because the account may already have changed.
pub fn with_note(note: Option<&str>, error: impl std::fmt::Display) -> String {
    match note {
        Some(n) => format!("{n} {error}"),
        None => error.to_string(),
    }
}
