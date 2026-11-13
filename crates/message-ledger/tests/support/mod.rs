//! Shared arrangement of the ledger tests: a ledger over the in-memory table with a clock the test moves, ULIDs that sort by their number,
//! and requests for the calls.
#![allow(dead_code, clippy::unwrap_used)]

use harness_protocol::messages::{
    ClaimResponse, ClaimedMessage, ConfirmHeld, MessageMode, MessageView, SendMessageRequest,
};
use harness_protocol::messages::{MessageOutcome, OutcomeResponse};
use message_ledger::mem::MemTable;
use message_ledger::{Admitted, Ledger, LedgerError, ManualClock, Refusal};
use std::collections::BTreeMap;
use std::sync::Mutex;

pub const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
pub const RUN2: &str = "01M4ZHFBV5SDX7J92E6YRH4QTX";
pub const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
pub const OTHER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/someone";
/// 2026-10-16T14:02:11Z
pub const T0: i64 = 1_792_159_331;

const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID that sorts by `n` (the last six characters are `n` in base 32).
pub fn ulid(prefix: &str, n: u32) -> String {
    let mut tail = [b'0'; 6];
    let mut v = n;
    for c in tail.iter_mut().rev() {
        *c = ALPHABET[(v % 32) as usize];
        v /= 32;
    }
    let p: String = prefix
        .chars()
        .chain(std::iter::repeat('0'))
        .take(20)
        .collect();
    format!("{p}{}", std::str::from_utf8(&tail).unwrap())
}

/// An incarnation of agentd that started at `ms` (Unix milliseconds): a ULID with that time and the tail `n`.
pub fn gen_at(ms: u64, n: u32) -> String {
    let mut time = [b'0'; 10];
    let mut v = ms;
    for c in time.iter_mut().rev() {
        *c = ALPHABET[(v % 32) as usize];
        v /= 32;
    }
    let tail = ulid("", n);
    format!("{}{}", std::str::from_utf8(&time).unwrap(), &tail[10..])
}

pub fn mid(n: u32) -> String {
    ulid("01M5A2R7K3C9XJ4D8E6F", n)
}

/// The nth incarnation of agentd. The number is only a name: which incarnation is newer is what the server was told, never the order of
/// the ids.
pub fn gen_(n: u32) -> String {
    ulid("01M5G0000000000000000", n)
}

pub struct Rig {
    pub table: MemTable,
    pub clock: ManualClock,
    pub ledger: Ledger<MemTable, ManualClock>,
    /// For each incarnation, the session id it last reported and that report's number.
    sessions: Mutex<BTreeMap<u32, (String, u32)>>,
}

pub fn rig() -> Rig {
    let table = MemTable::new();
    let clock = ManualClock::at(T0);
    let ledger = Ledger::with_clock(table.clone(), clock.clone());
    Rig {
        table,
        clock,
        ledger,
        sessions: Mutex::new(BTreeMap::new()),
    }
}

pub fn send(n: u32, text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message_id: mid(n),
        text: text.to_string(),
        mode: MessageMode::Next,
        confirms: None,
    }
}

pub fn send_now(n: u32, text: &str) -> SendMessageRequest {
    SendMessageRequest {
        mode: MessageMode::Now,
        ..send(n, text)
    }
}

pub fn confirm(n: u32, text: &str, held: u32, epoch: &str) -> SendMessageRequest {
    SendMessageRequest {
        confirms: Some(ConfirmHeld {
            message_id: mid(held),
            epoch: epoch.to_string(),
        }),
        ..send(n, text)
    }
}

impl Rig {
    pub async fn admit(&self, n: u32, text: &str) -> Admitted {
        self.ledger.admit(RUN, OWNER, &send(n, text)).await.unwrap()
    }

    /// Admits messages 1 to `count` (text "m"), five to a clock minute so that the rate limits stay out of the way, and leaves the clock at
    /// the minute of the last one.
    pub async fn fill(&self, count: u32) {
        for n in 1..=count {
            self.clock.set(T0 + i64::from(n / 5) * 60);
            self.admit(n, "m").await;
        }
    }

    pub async fn refused(&self, req: &SendMessageRequest) -> Refusal {
        refusal(self.ledger.admit(RUN, OWNER, req).await.unwrap_err())
    }

    /// Reports session `epoch` of incarnation `g` the way agentd does: the same id again repeats the number of its last report, another
    /// id is the next session of that incarnation.
    pub async fn session(&self, g: u32, epoch: &str) -> harness_protocol::messages::SessionAck {
        let number = {
            let mut s = self.sessions.lock().unwrap();
            let e = s.entry(g).or_insert((String::new(), 0));
            if e.0 != epoch {
                *e = (epoch.to_string(), e.1 + 1);
            }
            e.1
        };
        self.session_n(g, epoch, number).await
    }

    /// Reports session `epoch` of incarnation `g` with an explicit number.
    pub async fn session_n(
        &self,
        g: u32,
        epoch: &str,
        number: u32,
    ) -> harness_protocol::messages::SessionAck {
        self.ledger
            .report_session(RUN, &gen_(g), epoch, number)
            .await
            .unwrap()
    }

    /// The outcome of the claim `attempt` of message `n`, posted by incarnation `g` from session `epoch`.
    pub async fn outcome(
        &self,
        g: u32,
        epoch: &str,
        n: u32,
        attempt: u32,
        o: &MessageOutcome,
    ) -> Result<OutcomeResponse, LedgerError> {
        self.ledger
            .outcome(RUN, &gen_(g), epoch, &mid(n), attempt, o)
            .await
    }

    pub async fn state(&self, n: u32) -> harness_protocol::messages::MessageState {
        self.view(n).await.state
    }

    pub async fn view(&self, n: u32) -> MessageView {
        self.ledger
            .get_message(RUN, &mid(n))
            .await
            .unwrap()
            .unwrap()
    }

    /// A claim with no id that the ledger refuses.
    pub async fn claim_err(&self, g: u32, epoch: &str, limit: u8) -> LedgerError {
        self.ledger
            .claim(RUN, &gen_(g), epoch, None, limit, false)
            .await
            .unwrap_err()
    }

    /// Claims with no id and a batch of `limit`.
    pub async fn claim(&self, g: u32, epoch: &str, limit: u8) -> ClaimResponse {
        self.ledger
            .claim(RUN, &gen_(g), epoch, None, limit, false)
            .await
            .unwrap()
    }
}

pub fn refusal(e: LedgerError) -> Refusal {
    match e {
        LedgerError::Refused(r) => r,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

pub fn claimed(r: ClaimResponse) -> Vec<ClaimedMessage> {
    match r {
        ClaimResponse::Claimed { messages, .. } => messages,
        other => panic!("expected a claim, got {other:?}"),
    }
}

pub fn ids(m: &[ClaimedMessage]) -> Vec<String> {
    m.iter().map(|c| c.message_id.clone()).collect()
}
