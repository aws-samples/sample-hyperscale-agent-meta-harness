//! Provider data sharing in the AWS account: what `ham` does with the Bedrock data retention mode of the stage's account before a run on a model
//! that needs provider data sharing, and the words it uses. The reads and the switch are made by the `ham` binary with the user's own credentials (the
//! Bedrock control plane's `GET` and `PUT /data-retention` in the stage's Region); this module only decides, so every rule is tested without a network.
//!
//! - A mode that already lets the model run is kept: `provider_data_share`, and `aws_review`, in which the model was measured working too.
//! - `default` and `inherit` are switched to `provider_data_share`, and only in the account the stage runs in, when the user's credentials belong to it.
//! - Anything else changes nothing: a read that is denied or does not answer, a mode `ham` does not know, credentials of another account. The run then
//!   goes as before, with one line, and Bedrock refuses the model if the mode does not allow it.
//! - After the switch, its answer and a fresh read decide whether the run may start ([`settle`]); a denied or failed switch stops it.
//! - The answer `y` to the question of the terminal app agrees to what the question showed and to nothing else ([`Shown`]): when the account is
//!   in another state by then, nothing is switched or started and the question is asked again with the new facts ([`ask_again_text`]).
//! - A resume or a retry starts the request the service stored. `run.provider_sharing` on is the agreement to a switch for it; with the setting
//!   off a mode that would have to be switched stops it with one message ([`stored_stop`]).
//!
//! The change is for the whole account in that Region and stays after the run: a run lasts hours on the service with the laptop closed and other runs may
//! rely on the mode, so `ham` never sets it back; every line names the command that does.

use serde_json::Value;

/// The mode `ham` switches to.
pub const SWITCH_TO: &str = "provider_data_share";

/// The modes in which a model that needs provider data sharing runs.
pub const RUNS_IN: [&str; 2] = ["provider_data_share", "aws_review"];

/// The modes `ham` switches from; any other mode it leaves as it is.
pub const SWITCH_FROM: [&str; 2] = ["default", "inherit"];

/// The permission of the read.
pub const READ_PERMISSION: &str = "bedrock:GetAccountDataRetention";

/// The permission of the switch.
pub const WRITE_PERMISSION: &str = "bedrock:PutAccountDataRetention";

/// What the agreement means, in the words of the setting and the refusal.
pub const MEANING: &str = "the prompts and answers of this run go to the model's provider, which may keep them up to 30 days for safety monitoring";

/// The mode lets a model that needs provider data sharing run.
pub fn mode_runs(mode: &str) -> bool {
    RUNS_IN.contains(&mode)
}

/// The AWS command that sets the mode of the account in `region`.
pub fn owner_command(region: &str, mode: &str) -> String {
    format!("aws bedrock put-account-data-retention --region {region} --mode {mode}")
}

/// The 12-digit account of an IAM or STS principal ARN (`arn:aws:sts::<account>:assumed-role/...`), `None` for anything else.
pub fn account_of_arn(arn: &str) -> Option<&str> {
    let mut parts = arn.split(':');
    if parts.next() != Some("arn") {
        return None;
    }
    let account = parts.nth(3)?;
    (account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit())).then_some(account)
}

/// One read of the mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    Mode(String),
    /// HTTP 403: the credentials may not read it.
    Denied,
    /// No usable answer: the words say why.
    Failed(String),
}

impl Read {
    /// The answer of `GET /data-retention` (or the reply of `PUT`): `{"mode": ..., "updatedAt": ...}`.
    pub fn from_answer(status: u16, body: &[u8]) -> Read {
        if status == 403 {
            return Read::Denied;
        }
        if !(200..300).contains(&status) {
            return Read::Failed(format!("HTTP {status}"));
        }
        match serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("mode").and_then(Value::as_str).map(str::to_string))
        {
            Some(m) => Read::Mode(m),
            None => Read::Failed("the answer holds no mode".into()),
        }
    }
}

/// What `ham` knows before it decides: the Region of the stage, the caller's principal and the stage's account (`GET /v1/whoami`), and the read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Survey {
    pub region: String,
    pub caller_arn: Option<String>,
    pub stage_account: Option<String>,
    pub read: Read,
}

/// What `ham` does with the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The mode already lets the model run: nothing changes.
    Keep { mode: String },
    /// `from` is switched to [`SWITCH_TO`] in `account`.
    Switch { account: String, from: String },
    /// Nothing changes, and the run goes as before: `why` says what `ham` could not check or may not change.
    Leave { why: String },
}

/// The plan for `s`: the accounts first, so a mode of another account than the stage's is never read as the stage's.
pub fn plan(s: &Survey) -> Plan {
    let caller = s.caller_arn.as_deref().and_then(account_of_arn);
    let (Some(caller), Some(stage)) = (caller, s.stage_account.as_deref()) else {
        return Plan::Leave {
            why: "ham could not tell which AWS account your credentials belong to".into(),
        };
    };
    if caller != stage {
        return Plan::Leave {
            why: format!(
                "your credentials belong to account {caller} and the stage runs in account {stage}; ham changes only the account of your credentials"
            ),
        };
    }
    match &s.read {
        Read::Denied => Plan::Leave {
            why: format!("your credentials may not read it (they need {READ_PERMISSION})"),
        },
        Read::Failed(why) => Plan::Leave {
            why: format!("the read did not answer ({why})"),
        },
        Read::Mode(m) if mode_runs(m) => Plan::Keep { mode: m.clone() },
        Read::Mode(m) if SWITCH_FROM.contains(&m.as_str()) => Plan::Switch {
            account: stage.to_string(),
            from: m.clone(),
        },
        Read::Mode(m) => Plan::Leave {
            why: format!("the mode is {m}, which ham does not change"),
        },
    }
}

/// What a question showed the person: the Region, the stage's account and what `ham` does there. The answer `y` agrees to this and to nothing
/// else, so the start that follows compares it with a fresh [`Survey`] before it changes anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub region: String,
    pub account: Option<String>,
    pub plan: Plan,
}

impl Shown {
    /// What a question built from `s` shows.
    pub fn of(s: &Survey) -> Shown {
        Shown {
            region: s.region.clone(),
            account: s.stage_account.clone(),
            plan: plan(s),
        }
    }
}

/// The survey of a backend with no AWS account behind it (a stand-in stage): nothing can be read there, so nothing is changed.
pub fn no_account(region: &str) -> Survey {
    Survey {
        region: region.to_string(),
        caller_arn: None,
        stage_account: None,
        read: Read::Failed("this backend has no AWS account to read".into()),
    }
}

/// How the user agreed to sharing for the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agreement {
    /// The answer `y` in the terminal app, or `--provider-sharing on`.
    ThisRun,
    /// `run.provider_sharing = true` in `config.toml`.
    Setting,
}

impl Agreement {
    fn words(self) -> &'static str {
        match self {
            Agreement::ThisRun => "you agreed for this run",
            Agreement::Setting => "run.provider_sharing is on",
        }
    }
}

/// The question of the terminal app before a run on a model that needs provider data sharing while the setting is off: the model, what sharing
/// means, the account and the Region, and what `ham` changes there. The keys are drawn beside it.
pub fn ask_text(name: &str, id: &str, s: &Survey, p: &Plan) -> String {
    let head = format!("{name} ({id}) needs provider data sharing: {MEANING}.");
    let account = s.stage_account.as_deref().unwrap_or("of the stage");
    let region = &s.region;
    let tail = match p {
        Plan::Switch { account, from } => format!(
            "To run it, ham switches the Bedrock data retention mode of AWS account {account} in {region} from {from} to {SWITCH_TO}, for every Bedrock call of that account in that Region; it stays after the run."
        ),
        Plan::Keep { mode } => format!(
            "The Bedrock data retention mode of AWS account {account} in {region} is already {mode}; ham changes nothing in the account."
        ),
        Plan::Leave { why } => format!(
            "ham cannot check the Bedrock data retention mode in {region} ({why}) and changes nothing in the account; if the mode does not allow the model, Bedrock refuses it."
        ),
    };
    format!("{head} {tail}")
}

/// The answer `n` or Esc to the question.
pub fn declined(name: &str, id: &str) -> String {
    format!(
        "Not started: you did not agree to provider data sharing for {name} ({id}). Nothing was sent and the AWS account was not changed."
    )
}

/// The first sentence of a question that is asked again.
pub const ASKED_AGAIN: &str = "The account is not as the last question showed it, so ham changed nothing and started nothing.";

/// The question asked again after `y`, when the account is no longer as the last question showed it: that nothing was changed or started, then
/// the question for the state the account is in now.
pub fn ask_again_text(name: &str, id: &str, s: &Survey, p: &Plan) -> String {
    format!("{ASKED_AGAIN} {}", ask_text(name, id, s, p))
}

/// The one message that stops a resume or a retry of a run on `name` while the account's mode would have to be switched and
/// `run.provider_sharing` is off: nobody agreed to a switch for the new run, so `ham` switches nothing and starts nothing.
pub fn stored_stop(name: &str, account: &str, region: &str, mode: &str) -> String {
    format!(
        "{name} needs provider data sharing, and the Bedrock data retention mode of AWS account {account} in {region} is {mode}. ham switches the mode only when you agree: run ham config set run.provider_sharing on and try again, or the account owner runs: {}. No run was started.",
        owner_command(region, SWITCH_TO)
    )
}

/// The one line after a switch: the account, the Region, the modes, the model, how the user agreed, and the command that sets the mode back.
pub fn switched_line(
    name: &str,
    account: &str,
    region: &str,
    from: &str,
    now: &str,
    agreement: Agreement,
) -> String {
    format!(
        "ham switched the Bedrock data retention mode of AWS account {account} in {region} from {from} to {now} for {name} ({}). It covers every Bedrock call of that account in that Region and stays after the run; to set it back: {}",
        agreement.words(),
        owner_command(region, from)
    )
}

/// The one line when `ham` leaves the mode alone and the run goes as before.
pub fn leave_line(name: &str, region: &str, why: &str) -> String {
    format!(
        "ham did not check or change the Bedrock data retention mode in {region}: {why}. If the mode does not allow {name}, Bedrock refuses it; the account owner runs: {}",
        owner_command(region, SWITCH_TO)
    )
}

/// The answer of `PUT /data-retention`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Put {
    /// 2xx: the reply, read as a read.
    Done(Read),
    /// 403: the credentials may not switch.
    Denied(String),
    /// Another 4xx: Bedrock refused the switch, so nothing changed.
    Rejected(String),
    /// A 5xx, a throttle, a lost connection or no answer: the switch may have taken effect, so the read back decides.
    Unsure(String),
}

/// Status, error code and message of an error answer, never the raw body.
fn summary(status: u16, body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return format!("HTTP {status}");
    };
    let code = ["__type", "code", "type"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str));
    let message: String = ["message", "Message"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    let text = match code {
        Some(c) => format!("HTTP {status}: {c}: {message}"),
        None => format!("HTTP {status}: {message}"),
    };
    crate::sanitize::sanitize_line(text.trim_end_matches([' ', ':']))
}

impl Put {
    pub fn from_answer(status: u16, body: &[u8]) -> Put {
        match status {
            200..=299 => Put::Done(Read::from_answer(status, body)),
            403 => Put::Denied(summary(status, body)),
            429 | 500..=599 => Put::Unsure(summary(status, body)),
            _ => Put::Rejected(summary(status, body)),
        }
    }
}

/// The switch `ham` made: for the words of its outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switching<'a> {
    pub name: &'a str,
    pub account: &'a str,
    pub region: &'a str,
    pub from: &'a str,
}

/// After the switch: the mode the account is in now when it lets the model run, else the one plain message that stops the start.
pub fn settle(put: &Put, readback: &Read, cx: &Switching<'_>) -> Result<String, String> {
    let Switching {
        name,
        account,
        region,
        ..
    } = *cx;
    let stop = |why: String| format!("{why} No run was started.");
    match put {
        Put::Denied(why) => Err(stop(format!(
            "ham may not switch the Bedrock data retention mode of AWS account {account} in {region}: your credentials need {WRITE_PERMISSION} ({why}). {name} runs only in a mode that allows it; the account owner runs: {}.",
            owner_command(region, SWITCH_TO)
        ))),
        Put::Rejected(why) => Err(stop(format!(
            "Bedrock refused the switch of the data retention mode of AWS account {account} in {region} to {SWITCH_TO} ({why})."
        ))),
        Put::Done(reply) => match (readback, reply) {
            (Read::Mode(m), _) if mode_runs(m) => Ok(m.clone()),
            (Read::Mode(m), _) => Err(stop(format!(
                "After the switch the Bedrock data retention mode of AWS account {account} in {region} reads {m}, not {SWITCH_TO}."
            ))),
            (_, Read::Mode(m)) if mode_runs(m) => Ok(m.clone()),
            _ => Err(stop(format!(
                "ham could not confirm the switch of the Bedrock data retention mode of AWS account {account} in {region}."
            ))),
        },
        Put::Unsure(why) => match readback {
            Read::Mode(m) if mode_runs(m) => Ok(m.clone()),
            Read::Mode(m) => Err(stop(format!(
                "The switch of the Bedrock data retention mode of AWS account {account} in {region} did not answer ({why}), and the mode reads {m}."
            ))),
            _ => Err(stop(format!(
                "The switch of the Bedrock data retention mode of AWS account {account} in {region} did not answer ({why}), and the mode could not be read back."
            ))),
        },
    }
}
