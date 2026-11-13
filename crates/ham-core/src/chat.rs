//! The chat view model: what the run screen's transcript shows, as ordered [`Block`]s with a role, an origin and a stable
//! id. Today's source is the progress feed ([`ChatModel::from_progress_feed`]): the prompt, the tails of Cline's messages, one row per
//! tool call, the harness notices, Cline's final message (from the evidence artifact) and the delivery card, labelled
//! [`PROGRESS_FEED_LABEL`] because it is not the full conversation. The full-history source of a later issue builds the same
//! [`Block`]s with the constructors below and hands them to [`ChatModel::from_blocks`]: message, thought, tool call with a folded
//! body, diff, plan, usage, notice, and an unknown kind kept collapsed (never dropped).
//!
//! The role word of a block (`you`, the engine's name, `tool`, `notice`) comes from the block's structure, never from its text, and the
//! engine's name is made of letters, digits, `.`, `_` and `-` only. Block text is terminal safe ([`display_text`]) and redacted
//! ([`redact_tokens`]) when the block is built; the transcript widget sanitizes again when it lays a block out. A block of the user's
//! own prompt has origin [`Origin::User`], one the harness wrote [`Origin::Harness`], and anything the engine wrote [`Origin::Engine`].
//! Notices come from events with agent authority (agentd posts them) and so carry the same trust as the "Cline needs you" card.
//!
//! What a stored conversation brings: every record of it is engine origin, whatever its own `author` field says,
//! so [`Block::author`] keeps that field as advisory text and nothing draws a role or chrome from it ([`ChatModel::role_word`] gives the
//! word `you` only to a block of [`Origin::User`]); a gap in the stored record (a cap, a conflict, an unreadable chunk, an abnormal end)
//! is a [`BlockKind::Gap`] block, never silence; a record is identified by its writer's incarnation and number
//! ([`Block::record_id`]), so two writers that overlapped never share an id; and a body longer than [`MAX_BODY_CHARS`] is cut with a
//! visible marker ([`Block::truncated`]) so a flooded record cannot stall layout.

use crate::notice::{NoticeKind, cut, goal_blocked_line, notice_of_for};
use crate::redact::redact_tokens;
use crate::render::{ended_on, model_name};
use crate::sanitize::{display_line, display_text};
use crate::state::{RunState, ToolTouch, ToolTracker};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;
use std::collections::HashMap;

/// What the transcript says about a run that has no stored conversation.
/// The id of the goal loop's round counter block (one block that a later round event replaces).
const GOAL_ROUNDS_ID: &str = "goal:rounds";

pub const PROGRESS_FEED_LABEL: &str = "progress feed only: not the full conversation";

/// The name shown for an engine whose name is unknown or unusable.
const DEFAULT_ENGINE: &str = "engine";
/// The longest engine name drawn as a role word.
const ENGINE_NAME_MAX: usize = 12;
/// The overlap two message tails need before they are joined into one text.
const MIN_OVERLAP: usize = 8;
/// The most characters of one block's body that are kept; more is cut and marked. It is the most characters of a final message
/// ([`harness_protocol::direct::MAX_CLINE_MESSAGE_CHARS`]), so a message is shown whole. A larger cap makes a draw dearer: the body of a block is
/// made safe again on every build of the view, and with one million characters a draw of a dashboard of 5,000 agents took twice as long
/// (0.59 s against 0.30 s) and failed its bound of half a second.
pub const MAX_BODY_CHARS: usize = harness_protocol::direct::MAX_CLINE_MESSAGE_CHARS;
/// The longest `author` label kept.
const AUTHOR_MAX: usize = 24;

/// Who a block speaks for; the word is drawn from this and nothing in the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    You,
    Engine,
    Tool,
    Notice,
}

/// Where a block's content came from. Only [`Origin::User`] and [`Origin::Harness`] are things the engine cannot write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    User,
    Engine,
    Harness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Message,
    Thought,
    ToolCall,
    Diff,
    Plan,
    Usage,
    Notice(NoticeKind),
    Delivery,
    /// The stored record is incomplete here (see [`GapCause`]).
    Gap(GapCause),
    /// A kind this client has no name for: shown collapsed under its label, never dropped.
    Unknown,
}

/// Why the stored conversation has a hole: the names the read route gives a range it cannot fully show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GapCause {
    /// Thoughts or binary output are not stored.
    Excluded,
    /// A size cap cut it.
    Truncated,
    /// The writer shed it under load.
    Shed,
    /// Redaction withheld it.
    Suppressed,
    /// Storage failed.
    Failed,
    /// Two writers overlapped, or a header did not match its key.
    Conflict,
    /// The run ran on an image that stores no transcript.
    Unsupported,
    /// The run ended abnormally: nothing is known about the tail.
    TailUnknown,
    /// A chunk could not be read.
    Unreadable,
}

impl GapCause {
    /// The line a gap block shows.
    pub fn words(self) -> &'static str {
        match self {
            Self::Excluded => "[not stored: thoughts and binary output are left out]",
            Self::Truncated => "[cut: a size limit]",
            Self::Shed => "[dropped under load]",
            Self::Suppressed => "[withheld by redaction]",
            Self::Failed => "[storage failed]",
            Self::Conflict => "[conflict: two writers overlapped]",
            Self::Unsupported => "[this run stores no conversation]",
            Self::TailUnknown => "[the run ended abnormally: the tail is unknown]",
            Self::Unreadable => "[chunk unreadable]",
        }
    }
}

/// Where a model's blocks came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The run's events: tails of messages, tool rows and notices ([`PROGRESS_FEED_LABEL`]).
    ProgressFeed,
    /// The stored conversation.
    FullHistory,
}

/// One block of the transcript. Build it with the constructors: they make the text safe and set `rev`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Stable across rebuilds and new events: the widget's wrap cache, folds and selection are keyed by it.
    pub id: String,
    pub role: Role,
    pub origin: Origin,
    pub kind: BlockKind,
    /// One line: a tool call's title, a diff's path, an unknown kind's label; empty for a message.
    pub head: String,
    /// The text: a message, a tool call's folded body, a diff, a plan's lines, a delivery card's rows.
    pub body: String,
    /// `execute`, `read`, `edit`, ... of a tool call.
    pub tool_kind: Option<String>,
    /// `pending`, `in_progress`, `completed` or `failed` of a tool call.
    pub status: Option<String>,
    /// Starts folded: only the head is drawn until the user opens it.
    pub folded: bool,
    /// The `author` the stored record named (`user`, `assistant`, ...): advisory text for a detail line, never a role and never chrome.
    pub author: Option<String>,
    /// The body was cut at [`MAX_BODY_CHARS`].
    pub truncated: bool,
    /// A hash of what the block says; it changes when the text, the head or the status changes, so a cache can tell.
    pub rev: u64,
}

fn safe_body(text: &str) -> String {
    redact_tokens(&display_text(text))
}

fn safe_head(text: &str) -> String {
    redact_tokens(&display_line(text))
}

/// `text` made safe and cut at [`MAX_BODY_CHARS`] characters, with a marker that says how much is not shown.
fn capped_body(text: &str) -> (String, bool) {
    let total = text.chars().count();
    if total <= MAX_BODY_CHARS {
        return (safe_body(text), false);
    }
    let kept: String = text.chars().take(MAX_BODY_CHARS).collect();
    let more = total - MAX_BODY_CHARS;
    (
        format!("{}\n[{more} more characters not shown]", safe_body(&kept)),
        true,
    )
}

/// FNV-1a over the parts, with a separator byte between them.
fn fnv(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0x1f)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn kind_tag(k: &BlockKind) -> String {
    match k {
        BlockKind::Notice(n) => format!("notice:{n:?}"),
        other => format!("{other:?}"),
    }
}

impl Block {
    #[allow(clippy::too_many_arguments)]
    fn make(
        id: &str,
        role: Role,
        origin: Origin,
        kind: BlockKind,
        head: &str,
        body: &str,
        tool_kind: Option<&str>,
        status: Option<&str>,
        folded: bool,
    ) -> Block {
        let head = safe_head(head);
        let (body, truncated) = capped_body(body);
        let tool_kind = tool_kind.map(safe_head).filter(|t| !t.is_empty());
        let status = status.map(safe_head).filter(|t| !t.is_empty());
        let mut b = Block {
            id: id.to_string(),
            role,
            origin,
            kind,
            head,
            body,
            tool_kind,
            status,
            folded,
            author: None,
            truncated,
            rev: 0,
        };
        b.rev = b.hash();
        b
    }

    fn hash(&self) -> u64 {
        fnv(&[
            &kind_tag(&self.kind),
            &self.head,
            &self.body,
            self.tool_kind.as_deref().unwrap_or(""),
            self.status.as_deref().unwrap_or(""),
            self.author.as_deref().unwrap_or(""),
        ])
    }

    /// The same block with the advisory `author` of its stored record (made safe and cut to a short label).
    pub fn with_author(mut self, author: &str) -> Block {
        let label: String = safe_head(author).chars().take(AUTHOR_MAX).collect();
        self.author = Some(label).filter(|a| !a.trim().is_empty());
        self.rev = self.hash();
        self
    }

    /// The id of a record of a stored conversation: the writer's incarnation (a ULID, made once per agentd process) and the record
    /// number, which starts again at 0 in each incarnation. Two writers that overlapped therefore never share an id.
    pub fn record_id(incarnation: &str, n: u64) -> String {
        let inc: String = incarnation
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(26)
            .collect();
        format!("r:{inc}:{n}")
    }

    /// A hole in the stored record, named by its cause. `detail` (a range, a count) follows the cause's words.
    pub fn gap(id: &str, cause: GapCause, detail: Option<&str>) -> Block {
        let text = match detail.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => format!("{} {d}", cause.words()),
            None => cause.words().to_string(),
        };
        Block::make(
            id,
            Role::Notice,
            Origin::Harness,
            BlockKind::Gap(cause),
            "",
            &text,
            None,
            None,
            false,
        )
    }

    /// A message of the user or the engine.
    pub fn message(id: &str, role: Role, origin: Origin, text: &str) -> Block {
        Block::make(
            id,
            role,
            origin,
            BlockKind::Message,
            "",
            text,
            None,
            None,
            false,
        )
    }

    /// The engine's reasoning, folded until opened.
    pub fn thought(id: &str, text: &str) -> Block {
        Block::make(
            id,
            Role::Engine,
            Origin::Engine,
            BlockKind::Thought,
            "thinking",
            text,
            None,
            None,
            true,
        )
    }

    /// A tool call: its title, kind and status on one line, its input and output in the folded body.
    pub fn tool_call(
        id: &str,
        title: &str,
        tool_kind: Option<&str>,
        status: Option<&str>,
        body: &str,
    ) -> Block {
        let title = if title.trim().is_empty() {
            "tool call"
        } else {
            title
        };
        Block::make(
            id,
            Role::Tool,
            Origin::Engine,
            BlockKind::ToolCall,
            title,
            body,
            tool_kind,
            status,
            true,
        )
    }

    /// A change to a file: the path on the head line, the diff in the folded body.
    pub fn diff(id: &str, path: &str, text: &str) -> Block {
        Block::make(
            id,
            Role::Tool,
            Origin::Engine,
            BlockKind::Diff,
            path,
            text,
            None,
            None,
            true,
        )
    }

    /// The engine's plan: one `[x]`, `[~]` or `[ ]` line per entry (completed, in progress, other).
    pub fn plan(id: &str, entries: &[(&str, &str)]) -> Block {
        let body = entries
            .iter()
            .map(|(status, text)| {
                let mark = match *status {
                    "completed" => "[x]",
                    "in_progress" => "[~]",
                    _ => "[ ]",
                };
                format!("{mark} {text}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        Block::make(
            id,
            Role::Engine,
            Origin::Engine,
            BlockKind::Plan,
            "",
            &body,
            None,
            None,
            false,
        )
    }

    /// A usage line (tokens, cost).
    pub fn usage(id: &str, line: &str) -> Block {
        Block::make(
            id,
            Role::Notice,
            Origin::Engine,
            BlockKind::Usage,
            "",
            line,
            None,
            None,
            false,
        )
    }

    /// A notice line of the harness.
    pub fn notice(id: &str, kind: NoticeKind, text: &str) -> Block {
        Block::make(
            id,
            Role::Notice,
            Origin::Harness,
            BlockKind::Notice(kind),
            "",
            text,
            None,
            None,
            false,
        )
    }

    /// A card of the run screen (status and agents, the question, the blocked panel, the delivery card): a harness block with the title as
    /// its head line and the lines as its body.
    pub fn card(id: &str, title: &str, body: &str, kind: NoticeKind, folded: bool) -> Block {
        Block::make(
            id,
            Role::Notice,
            Origin::Harness,
            BlockKind::Notice(kind),
            title,
            body,
            None,
            None,
            folded,
        )
    }

    /// The delivery card: `label value` rows of the harness's own events. It never holds a download link.
    pub fn delivery(id: &str, rows: &[(String, String)]) -> Block {
        let body = rows
            .iter()
            .map(|(label, value)| format!("{label:<9}{value}"))
            .collect::<Vec<_>>()
            .join("\n");
        Block::make(
            id,
            Role::Notice,
            Origin::Harness,
            BlockKind::Delivery,
            "Delivery",
            &body,
            None,
            None,
            false,
        )
    }

    /// An update or event of a kind this client does not know: kept, collapsed, under its label.
    pub fn unknown(id: &str, label: &str, raw: &str) -> Block {
        Block::make(
            id,
            Role::Notice,
            Origin::Engine,
            BlockKind::Unknown,
            label,
            raw,
            None,
            None,
            true,
        )
    }

    /// The role the block counts as when its word and colour are drawn: a block claims `You` only with [`Origin::User`] and `Notice`
    /// only with [`Origin::Harness`]; anything else that is not a tool call is the engine's.
    pub fn effective_role(&self) -> Role {
        match (self.role, self.origin) {
            (Role::You, Origin::User) => Role::You,
            (Role::Notice, Origin::Harness) => Role::Notice,
            (Role::Tool, _) => Role::Tool,
            _ => Role::Engine,
        }
    }

    /// The same block with another origin (a block built from a harness event rather than from the engine).
    fn with_origin(mut self, origin: Origin) -> Block {
        self.origin = origin;
        self
    }
}

/// Cline's final message as the client knows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FinalMessage<'a> {
    /// Not looked up (yet): the model shows nothing for it.
    #[default]
    NotLoaded,
    /// Looked up and not there (no evidence artifact, or it holds no message): a delivered run says so.
    Unavailable,
    Text(&'a str),
}

/// What the progress feed cannot supply from events: the prompt (`RunDetail.objective`), the final message and where the zip landed.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChatInputs<'a> {
    pub prompt: Option<&'a str>,
    pub final_message: FinalMessage<'a>,
    pub zip_path: Option<&'a str>,
}

/// The transcript's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatModel {
    pub source: Source,
    /// The engine's role word.
    pub engine: String,
    pub blocks: Vec<Block>,
}

/// `raw` as a role word: its leading letters, digits, `.`, `_` and `-`, at most [`ENGINE_NAME_MAX`]; `engine` when nothing is left.
fn engine_label(raw: &str) -> String {
    let word: String = raw
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(ENGINE_NAME_MAX)
        .collect();
    if word.is_empty() {
        DEFAULT_ENGINE.to_string()
    } else {
        word
    }
}

impl ChatModel {
    /// A model of blocks from any source.
    pub fn from_blocks(source: Source, engine: &str, blocks: Vec<Block>) -> ChatModel {
        ChatModel {
            source,
            engine: engine_label(engine),
            blocks,
        }
    }

    /// The line the transcript shows above the conversation: [`PROGRESS_FEED_LABEL`] for the progress feed, nothing for a full history.
    pub fn label(&self) -> Option<&'static str> {
        (self.source == Source::ProgressFeed).then_some(PROGRESS_FEED_LABEL)
    }

    /// The word drawn in the block's gutter.
    ///
    /// `you` is the word of a block of [`Origin::User`] only, and `notice` of [`Origin::Harness`] only: a record of the stored
    /// conversation that claims to be the user's or the harness's is engine origin and wears the engine's word.
    pub fn role_word(&self, b: &Block) -> String {
        match b.effective_role() {
            Role::You => "you".to_string(),
            Role::Notice => "notice".to_string(),
            Role::Tool => "tool".to_string(),
            Role::Engine => self.engine.clone(),
        }
    }

    /// The progress feed of a run: the events the client has read, the prompt and the other inputs.
    pub fn from_progress_feed(state: &RunState, inputs: &ChatInputs<'_>) -> ChatModel {
        let view = state.view();
        let engine = view
            .agents
            .values()
            .find_map(|a| a.engine.as_deref())
            .map_or_else(|| DEFAULT_ENGINE.to_string(), engine_label);
        let mut b = Builder {
            engine: Some(crate::engines::of_view(&view)),
            ..Builder::default()
        };
        if let Some(p) = inputs.prompt.filter(|p| !p.trim().is_empty()) {
            b.blocks
                .push(Block::message("prompt", Role::You, Origin::User, p));
        }
        for e in state.accepted() {
            b.event(e);
        }
        b.close_message();
        let delivered = view.zip.is_some() || view.result.is_some();
        match inputs.final_message {
            FinalMessage::Text(t) if !t.trim().is_empty() => b.final_message(t),
            FinalMessage::Unavailable if delivered => b.blocks.push(Block::notice(
                "final-missing",
                NoticeKind::RunStatus,
                "final message not available",
            )),
            _ => {}
        }
        if let Some(a) = &view.aborted {
            b.blocks.push(Block::notice(
                "status",
                NoticeKind::RunStatus,
                &format!("Run ended early: {}", a.line()),
            ));
        } else if let Some(bl) = &view.blocked {
            b.blocks.push(Block::notice(
                "status",
                NoticeKind::RunStatus,
                &format!("Run blocked: {}", bl.reason),
            ));
        }
        let mut rows: Vec<(String, String)> = Vec::new();
        if let Some(r) = &view.result {
            rows.push(("Outcome".into(), r.label.clone()));
            if let Some(d) = &r.detail {
                rows.push(("Detail".into(), d.clone()));
            }
        }
        if let Some(m) = view.direct_model_id() {
            rows.push(("Model".into(), model_name(m)));
        }
        if let Some(end) = ended_on(&view) {
            let n = end.switches;
            rows.push((
                "Ran on".into(),
                format!(
                    "{} after {n} switch{}",
                    end.text,
                    if n == 1 { "" } else { "es" }
                ),
            ));
        }
        if let Some(pr) = &view.pr_url {
            rows.push(("PR".into(), pr.clone()));
        }
        if let Some(z) = &view.zip {
            let sha: String = z.sha256.chars().take(12).collect();
            let place = inputs
                .zip_path
                .map_or_else(|| z.artifact_id.clone(), str::to_string);
            rows.push((
                "Zip".into(),
                format!("{place} (sha256 {sha}, {} bytes)", z.size_bytes),
            ));
        }
        if delivered || view.pr_url.is_some() {
            b.blocks.push(Block::delivery("delivery", &rows));
        }
        ChatModel {
            source: Source::ProgressFeed,
            engine,
            blocks: b.blocks,
        }
    }
}

/// `acc` with the newer message tail `tail` joined on. The collector sends the last 500 characters of the message so far at most once
/// every 5 seconds, so a tail either repeats what is held, extends it, overlaps its end, or (when the engine wrote more than a tail
/// between two progress events) leaves a gap, which is marked `…`.
pub fn stitch_tail(acc: &str, tail: &str) -> String {
    if acc.is_empty() || tail.starts_with(acc) {
        return tail.to_string();
    }
    if acc.contains(tail) {
        return acc.to_string();
    }
    let a: Vec<char> = acc.chars().collect();
    let t: Vec<char> = tail.chars().collect();
    let most = a.len().min(t.len());
    for k in (MIN_OVERLAP..=most).rev() {
        if a[a.len() - k..] == t[..k] {
            let mut out = acc.to_string();
            out.extend(&t[k..]);
            return out;
        }
    }
    format!("{acc} \u{2026} {tail}")
}

#[derive(Default)]
struct Open {
    index: usize,
    id: String,
    /// The stitched, not yet sanitized text.
    acc: String,
}

/// One pass over the events in event-id order.
#[derive(Default)]
struct Builder {
    /// The engine the run is on, for the words of the notices (the default engine until [`ChatModel::from_progress_feed`] reads the view).
    engine: Option<&'static crate::engines::Engine>,
    blocks: Vec<Block>,
    open: Option<Open>,
    tools: ToolTracker,
    tool_block: HashMap<usize, usize>,
}

fn payload(e: &Envelope) -> Option<&Value> {
    match &e.payload {
        Payload::Inline { value } => Some(value),
        Payload::ClaimCheck { .. } => None,
    }
}

impl Builder {
    fn close_message(&mut self) {
        self.open = None;
    }

    fn event(&mut self, e: &Envelope) {
        match e.event_type {
            EventType::ToolSummary => {
                if let Some(t) = self.tools.apply(e) {
                    self.close_message();
                    self.touch_tool(t, e);
                }
            }
            EventType::TurnProgress => {
                if let Some(t) = self.tools.apply(e) {
                    if t.created {
                        self.close_message();
                    }
                    self.touch_tool(t, e);
                    return;
                }
                let Some(v) = payload(e) else { return };
                match v.get("kind").and_then(Value::as_str) {
                    Some("message") => {
                        if let Some(text) = v.get("text").and_then(Value::as_str) {
                            self.tail(e, text);
                        }
                    }
                    Some(_) => self.notice(e),
                    None => {}
                }
            }
            EventType::PermissionRequested => self.notice(e),
            EventType::TurnStarted | EventType::TurnEnded => self.close_message(),
            _ => {}
        }
    }

    fn tail(&mut self, e: &Envelope, text: &str) {
        match &mut self.open {
            Some(o) => {
                o.acc = stitch_tail(&o.acc, text);
                self.blocks[o.index] = Block::message(&o.id, Role::Engine, Origin::Engine, &o.acc);
            }
            None => {
                let id = format!("m:{}", e.event_id);
                self.blocks
                    .push(Block::message(&id, Role::Engine, Origin::Engine, text));
                self.open = Some(Open {
                    index: self.blocks.len() - 1,
                    id,
                    acc: text.to_string(),
                });
            }
        }
    }

    fn touch_tool(&mut self, t: ToolTouch, e: &Envelope) {
        let row = &self.tools.rows()[t.index];
        let id = if row.call_id.is_empty() {
            format!("t:{}", e.event_id)
        } else {
            format!("t:{}", row.call_id)
        };
        let block = Block::tool_call(
            &id,
            &row.title,
            row.kind.as_deref(),
            row.status.as_deref(),
            "",
        );
        match self.tool_block.get(&t.index) {
            Some(&i) => {
                // The id stays the one the call began with.
                let keep = self.blocks[i].id.clone();
                self.blocks[i] = Block { id: keep, ..block };
            }
            None => {
                self.tool_block.insert(t.index, self.blocks.len());
                self.blocks.push(block);
            }
        }
    }

    fn notice(&mut self, e: &Envelope) {
        let engine = self.engine.unwrap_or_else(crate::engines::default_engine);
        let Some(n) = notice_of_for(e, engine) else {
            return;
        };
        self.close_message();
        if n.kind == NoticeKind::Other {
            let v = payload(e).cloned().unwrap_or(Value::Null);
            let label = v.get("kind").and_then(Value::as_str).unwrap_or("event");
            let raw = cut(&serde_json::to_string(&v).unwrap_or_default(), 600);
            self.blocks.push(
                Block::unknown(&format!("u:{}", e.event_id), label, &raw)
                    .with_origin(Origin::Harness),
            );
        } else if n.kind == NoticeKind::GoalRound {
            // The goal loop's round counter is one line that counts: a later round event replaces the text and keeps the place.
            let block = Block::notice(GOAL_ROUNDS_ID, n.kind, &n.text);
            match self.blocks.iter().position(|b| b.id == GOAL_ROUNDS_ID) {
                Some(i) => self.blocks[i] = block,
                None => self.blocks.push(block),
            }
        } else {
            self.blocks
                .push(Block::notice(&format!("n:{}", e.event_id), n.kind, &n.text));
        }
        // The engine's own BLOCKED line follows the brake that quotes it, on a line of its own.
        if let Some(line) = goal_blocked_line(e) {
            self.blocks.push(Block::notice(
                &format!("n:{}:blocked", e.event_id),
                NoticeKind::GoalBlocked,
                &line,
            ));
        }
    }

    /// Cline's final message: it replaces the last tail when that tail is a part of it, else it follows the last block.
    fn final_message(&mut self, text: &str) {
        let block = Block::message("final", Role::Engine, Origin::Engine, text);
        let replaces = self.blocks.last().is_some_and(|l| {
            l.role == Role::Engine
                && l.kind == BlockKind::Message
                && l.id.starts_with("m:")
                && !l.body.is_empty()
                && (block.body.contains(&l.body) || l.body.contains(&block.body))
        });
        if replaces {
            self.blocks.pop();
        }
        self.blocks.push(block);
    }
}

/// The engine's final message out of an evidence artifact: `evidence/direct.json` (`summary.clineMessage`, the last 8,000 characters of the
/// message) or, when that holds none, `EVIDENCE.md` (the quote under the heading of the engine's last message: `## Cline's last message`,
/// `## OpenCode's last message`, one heading for each engine of the table). `None` when neither says anything.
pub fn final_message_from_evidence(text: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return crate::head::cline_message_in_evidence(&v)
            .filter(|m| !m.trim().is_empty())
            .map(str::to_string);
    }
    let headings: Vec<String> = crate::engines::roadmap()
        .iter()
        .map(crate::engines::Engine::evidence_heading)
        .collect();
    let mut lines = text
        .lines()
        .skip_while(|l| !headings.iter().any(|h| l.trim() == h));
    lines.next()?;
    let quote: Vec<&str> = lines
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| l.starts_with('>'))
        .collect();
    if quote.is_empty() {
        return None;
    }
    let msg = quote
        .iter()
        .map(|l| {
            l.strip_prefix("> ")
                .unwrap_or(l.strip_prefix('>').unwrap_or(l))
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!msg.trim().is_empty()).then_some(msg)
}
