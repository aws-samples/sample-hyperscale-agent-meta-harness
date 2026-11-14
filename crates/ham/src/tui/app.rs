//! TUI state and key handling, free of I/O: keys and API answers go in, [`Effect`]s come out, and
//! [`super::perform`] runs them.
//!
//! The composer sends `POST /v1/direct/runs`:
//! its fields are Prompt, Target, Repository, Base ref, Model and Submit. The Model field lists the supported models from
//! `engine/catalog/models.json` (`ham_core::models`, no network), opens on the user's default (`HAMH_MODEL`, then the config
//! file's `default_model`, else Claude Sonnet 5.5), and ↑↓ choose another for that run. A choice is saved only when the user presses
//! `d` on the field (`Effect::SaveDefaultModel`); the composer sends the model on the screen as an explicit choice.
//!
//! The dashboard is the chat view of the run (`super::chat`): the conversation with the status, agents, question and delivery cards at
//! its end. The app keeps the run's view and its agent rows (`ham_core::agents::rows`, grouped) from the last poll and makes the
//! conversation again after each one. Its keys: ↑↓ PgUp PgDn Home End scroll the conversation, Tab and Shift-Tab select a block,
//! Space folds it, `a` the Artifacts screen, `c` twice cancel, `C` clone, Esc or `r` the Runs screen, `n` a new
//! run, `q` quit. The Artifacts screen lists `GET .../artifacts`; Enter downloads the selected one.
//!
//! The link keys:
//! once a run is delivered its card shows the download link as one short line, and `o` opens the
//! pull request (else a download link made at that moment) in the browser through [`Effect::OpenUrl`] and [`Effect::OpenDownload`], and
//! `d` downloads the zip again through the same code path as `ham pull` ([`Effect::Download`]). The presigned URL itself is never drawn,
//! logged or put in a notice.
//!
//! The user correction path: while the focused run has an open blocking question (a `needs-input` event,
//! `RunView.input.open`), the dashboard shows the card "Cline needs you" and rings the bell once (`Effect::Bell`); `p` opens the typing
//! screen ([`Screen::Reply`]) for a message in the same Cline session, `s` for a fresh session on the same files, `m` first lists the
//! models of the question ([`Screen::Models`]), and `f` twice finishes the run with what exists. Ctrl-S sends (`Effect::Answer`,
//! `POST /v1/runs/{id}/input`), Esc goes back with the question still open and the text kept. The server queues the answer; the
//! card says "waiting for Cline to take it" until the run's events show it. The non-blocking outage notice has no keys.

use super::burst::Burst;
use super::chat::{ChatView, ScrollAction, Step};
use super::chat_search::ChatSearchUi;
use super::commands::{self, Armed, Bearer, Gesture, Panel};
use super::ext::{Done, Job};
use super::head::Head;
use super::input::{ChatBox, Kind};
use super::keys::{self, Action, Mode};
use super::mouse::{Action as MouseAction, BoxId, HitMap, ListId, Pointer, WheelSettings};
use super::rung::Rung;
use super::runs_view;
use super::settings;
use super::{copy, export};
use crate::clone::Cloned;
use crate::output::CmdOutput;
use crate::pull::{Pulled, local_name};
use crate::watch::finished;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ham_core::agents::{AgentGroup, AgentRow, group_counts, rows};
use ham_core::alert::{Alert, Alerts};
use ham_core::chat::{BlockKind, ChatInputs, ChatModel, Role};
use ham_core::command::{
    Command, DirectArgs, EngineAction, EngineCommand, Invocation, TargetArg, parse,
};
use ham_core::history::{HistoryRing, Recall, ReverseSearch};
use ham_core::input::{InputRequest, check_text};
use ham_core::meters::{Link, elapsed_s};
use ham_core::models::{ModelError, ModelPrefs, ModelSource, Models, builtin, resolve};
use ham_core::paste::{PasteStore, Pasted};
use ham_core::poll::PollPlan;
use ham_core::registry;
use ham_core::router::{
    ConfirmKey, ConfirmStep, RouteContext, RouteOutcome, Wait, complete, is_command_attempt,
    prepare_argv, route,
};
use ham_core::runfilter::{
    IndexEntry, IndexedRead, Refresh, RunRow, RunsFilter, RunsRead, select_after,
};
use ham_core::runtag::wire::{TagEditRequest, TagEditResponse};
use ham_core::sanitize::{sanitize, sanitize_line};
use ham_core::search::Key as SearchKey;
use ham_core::settings::Key as SettingKey;
use ham_core::state::{RunState, RunView};
use harness_protocol::api::v1::{
    ArtifactView, EventsPage, InputAccepted, InputAnswer, RunControlResponse, RunDetail,
    RunSummary, StartRunResponse,
};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::EventType;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

// The chat-client features that keep state of their own (each file starts with what it does): the stored conversation, messages to
// a working agent, open anything, the engine's capabilities. They are children of this module so that they can
// use its private state; this file only calls them.
#[path = "earlier.rs"]
mod earlier;
#[path = "engine_ui.rs"]
mod engine_ui;
#[path = "history.rs"]
mod history;
#[path = "model_title.rs"]
mod model_title;
#[path = "openbox.rs"]
mod openbox;
#[path = "run_name.rs"]
mod run_name;
#[path = "send.rs"]
mod send;
#[path = "tags.rs"]
mod tags;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Runs,
    Composer,
    Dashboard,
    Artifacts,
    /// `m` on a question: the models Cline may continue on.
    Models,
    /// `p`, `s` and the model choice: the typing screen of an answer.
    Reply,
    /// `s` on the Runs list and `/settings`: the settings of `config.toml`, one row for each key of the registry (`settings.rs`).
    Settings,
}

/// Work for [`super::perform`]; each yields at most one [`Msg`]. A few exist at a time (one per
/// key press or answer).
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    LoadRuns,
    /// One read of the Runs list for the screen: the newest page for the timer, every page for `r`. The answer is
    /// [`Msg::RunsRead`], which is applied against the list held when it arrives.
    RefreshRuns(Refresh),
    /// One read of the run's row for its name: `GET /v1/runs?q=<run id>&since=...`, answered by [`Msg::RunName`]. A read
    /// that fails answers with no name.
    ReadRunName {
        run_id: String,
    },
    /// The composer's Submit: `args` names the chosen model, so no default is applied; `perform` reads the
    /// user's rules, builds the request and sends `POST /v1/direct/runs` with a key made from the request and `nonce`.
    SubmitDirect {
        args: DirectArgs,
        nonce: String,
    },
    /// The answer `y` to the question of provider data sharing: the start the question held, with the agreement for this run, on the
    /// backend it was aimed at, and what the question showed. The account is switched only while it is in that state; else the answer is the
    /// question again.
    SubmitAgreed {
        args: DirectArgs,
        nonce: String,
        backend: Option<String>,
        shown: ham_core::sharing::Shown,
    },
    /// [`Effect::SubmitDirect`] on one of several backends: the run starts on `backend` and the key carries its alias.
    SubmitDirectOn {
        args: DirectArgs,
        nonce: String,
        backend: String,
    },
    /// `d` on the Model field: store the id as `default_model` in `config.toml` (the only thing in the TUI that writes it).
    SaveDefaultModel {
        model_id: String,
    },
    /// One read of every shard after its cursor.
    Poll {
        run_id: String,
        cursors: Vec<(u8, Option<String>)>,
    },
    Download {
        run_id: String,
        artifact_id: String,
    },
    /// Tokens and USD for the final card.
    LoadDetail {
        run_id: String,
    },
    /// `o` on a link that is known (the pull request): `program url` with no shell, then [`Msg::Opened`]. `what` names the link for the
    /// notice (`PR link`).
    OpenUrl {
        program: String,
        url: String,
        what: &'static str,
    },
    /// `o` on the bearer link a command returned (`/link`): `program url` with no shell, then [`Msg::Opened`]. The link is a [`Bearer`], so a
    /// dump of the effect hides it.
    OpenBearer {
        program: String,
        bearer: Bearer,
        what: &'static str,
    },
    /// `o` on the download link: a fresh presigned link is made when the key is pressed (a link made earlier may have lapsed), then
    /// `program url` with no shell. The link never leaves the effect; [`Msg::Opened`] carries only its expiry.
    OpenDownload {
        program: String,
        run_id: String,
        artifact_id: String,
    },
    /// `ham clone <run>`: the run's zip is fetched by the pull rules when it is not on this
    /// machine (a zip that verifies is kept), then cloned into `<cwd>/<repo name>`. `cwd` is the directory `ham` was started in; `None`
    /// clones next to the zip, in the download directory.
    Clone {
        run_id: String,
        cwd: Option<std::path::PathBuf>,
    },
    /// `/mouse on|off`: the terminal's mouse reports go on or off for this session. The config file is not touched.
    SetMouse {
        on: bool,
    },
    LoadArtifacts {
        run_id: String,
    },
    /// One artifact from the Artifacts screen.
    DownloadArtifact {
        run_id: String,
        artifact_id: String,
    },
    /// `c` twice on the dashboard: `POST /v1/runs/{id}/cancel`.
    Cancel {
        run_id: String,
    },
    /// Ctrl-S on the typing screen, or the second `f`: `POST /v1/runs/{id}/input`.
    Answer {
        run_id: String,
        answer: InputAnswer,
    },
    /// A command typed in the message box, after its second Enter when it needed one: `perform` runs it with `cli::run_captured` and
    /// answers with [`Msg::CommandDone`]. `argv` is what the parser read and `invocation` its parse.
    RunCommand {
        invocation: Invocation,
        argv: Vec<String>,
    },
    /// A command that needs the terminal (`push`, `rules edit`), after its second Enter: the event loop runs it as a child `ham` with `argv`, with
    /// the terminal handed over (`handoff`), and answers with [`Msg::CommandDone`]; `perform` never sees it.
    RunTerminal {
        argv: Vec<String>,
    },
    /// A request of a chat-client feature: a page of the stored conversation, the messages, a send, a withdrawal, the search across runs
    /// (`ext.rs`). The answer is [`Msg::Ext`].
    Ext(Job),
    /// The Settings screen's work: read `config.toml`, or change one key in it (`settings::perform`). The answer is [`Msg::Settings`].
    Settings(settings::Job),
    /// `d` in the tag explorer and the second Enter of `/tag`: the edit route is asked for each batch in turn (at most 40 edits in
    /// one), and a batch that fails as a whole stops the rest. The answer is [`Msg::TagsEdited`], one result for each batch asked.
    EditTags {
        origin: TagsOrigin,
        batches: Vec<TagEditRequest>,
    },
    /// A question the screen had not shown yet: one terminal bell.
    Bell,
    /// A run began to wait for the user while the window is not focused and the option is on: one terminal notification (OSC 9). The
    /// alert builds its own bytes from fixed words and the run id.
    Notify(ham_core::alert::Alert),
    /// `/copy`, `y` or the release of a mouse selection: the cleaned, masked text goes to the clipboard by the first route that works
    /// (`perform` runs it off the draw path and answers with [`Msg::Copied`]).
    Copy(super::copy::Prepared),
    /// Ctrl-G in the message box: the draft, with the text of every pasted block put back, goes to the user's editor. The event loop runs it
    /// (it hands over the terminal, so it needs the terminal) and answers with [`Msg::Edited`]; `perform` never sees it.
    EditDraft {
        text: String,
    },
    /// `/export`: the conversation as Markdown, built by the screen from its model, written by `perform` in the download directory (or
    /// where `arg` says) and answered with [`Msg::Exported`]. `stamp` makes the generated file name; `blocks` is the count for the notice.
    Export {
        run_id: String,
        arg: Option<String>,
        stamp: String,
        content: String,
        blocks: usize,
    },
    Quit,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Runs(Result<Vec<RunSummary>, String>),
    /// The answer to [`Effect::RefreshRuns`].
    RunsRead(Result<RunsRead, String>),
    /// A read of the list with the run index joined: the same as [`Msg::RunsRead`], and the title and model of the rows.
    RunsIndexed(Result<IndexedRead, String>),
    /// What each backend answered last, after a read of the list; `None` for a client with one backend.
    Backends(Option<ham_core::backends::BackendView>),
    Started(Result<StartRunResponse, String>),
    /// A start on a model that needs provider data sharing while the run has not agreed to it: the question the composer asks. Nothing was
    /// sent and the account was not changed.
    SharingAsk(Box<SharingAsk>),
    /// A start that went out with a line for the run screen: the switch of the account's Bedrock data retention mode, or why ham left it alone.
    StartedWith {
        response: StartRunResponse,
        note: String,
    },
    /// The answer to [`Effect::ReadRunName`]: the name in the run's row and its source (`ai`, `seed`), `None` when the read failed or the
    /// row holds no name. It names its run, because the person may have opened another since.
    RunName {
        run_id: String,
        name: Option<(String, Option<String>)>,
    },
    /// The answer to [`Effect::SaveDefaultModel`]: the stored model id, or why it was not stored.
    DefaultSaved(Result<String, String>),
    Pages(Result<Vec<(u8, EventsPage)>, String>),
    /// The answer to [`Effect::Download`]: the zip on this machine, or why not. It names its run, because the download runs on its own task
    /// and the person may have opened another run since; only the focused run's card takes it (as for [`Msg::Cloned`]).
    Downloaded {
        run_id: String,
        result: Result<Pulled, String>,
    },
    /// The answer to [`Effect::OpenUrl`] and [`Effect::OpenDownload`]: what was opened, or why not (a message without the link).
    Opened(Result<Opened, String>),
    /// The answer to [`Effect::Clone`]: the published clone, or why not. It names its run, because it arrives from a blocking thread
    /// and the person may have opened another run since; only the focused run's card takes it.
    Cloned {
        run_id: String,
        result: Result<Cloned, String>,
    },
    Detail(Result<RunDetail, String>),
    Artifacts(Result<Vec<ArtifactView>, String>),
    ArtifactPulled(Result<Pulled, String>),
    Cancelled(Result<RunControlResponse, String>),
    /// The answer to [`Effect::Answer`]: queued by the server, or its plain refusal message.
    Answered(Result<InputAccepted, String>),
    /// The answer to [`Effect::RunCommand`]: the command's words and what it wrote, with its exit code and any bearer link.
    CommandDone {
        argv: Vec<String>,
        out: CmdOutput,
    },
    /// The answer to [`Effect::Ext`].
    Ext(Done),
    /// The answer to [`Effect::Settings`].
    Settings(settings::Done),
    /// The answer to [`Effect::EditTags`]: for each batch asked, the route's answer or the words of the failure
    /// ([`crate::api::TAGS_UNSUPPORTED`] when the stage has no such route).
    TagsEdited {
        origin: TagsOrigin,
        results: Vec<Result<TagEditResponse, String>>,
    },
    /// The answer to [`Effect::Copy`]: the notice that names the path used, or why nothing was copied.
    Copied(Result<String, String>),
    /// The answer to [`Effect::Export`]: the notice with the file name, or why nothing was written.
    Exported(Result<String, String>),
    /// The answer to [`Effect::EditDraft`]: what the editor came to and how long it ran, or why it could not run.
    Edited(Result<ham_core::editor::Report, String>),
}

/// What asked for a tag edit, so the answer goes where the question came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagsOrigin {
    /// `/tag` in the message box of this run.
    Word { run_id: String },
    /// `d` in the tag explorer: this tag, from every held run that holds it.
    Remove { tag: String },
}

/// A link the browser was asked to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    /// `PR link` or `download link`.
    pub what: &'static str,
    /// When the link made for the key stops working (RFC 3339), for a download link; `None` for the pull request.
    pub expires_at: Option<String>,
}

/// Writes Unix seconds as the reader's clock (`06:12 MDT`); the process gives it the local zone (`crate::link::local_clock`).
pub type ClockText = fn(i64) -> String;

/// One line of the dashboard's agent list: a group heading or an agent row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListLine {
    Group(AgentGroup, usize),
    Agent(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArtifactList {
    NotLoaded,
    Loading,
    Loaded(Vec<ArtifactView>),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Repo,
    NewRepo,
    S3Only,
}

impl TargetKind {
    fn next(self) -> Self {
        match self {
            TargetKind::Repo => TargetKind::NewRepo,
            TargetKind::NewRepo => TargetKind::S3Only,
            TargetKind::S3Only => TargetKind::Repo,
        }
    }
    fn prev(self) -> Self {
        self.next().next()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Prompt,
    Target,
    Repo,
    Base,
    /// The start options of the command line that the other fields lack: `--rules`, `--no-rules`, `--continue`, `--input-wait`.
    Options,
    /// The backend of the run, drawn only while the client holds two backends or more.
    Backend,
    /// The one model of the run.
    Model,
    Submit,
}

/// The composer's fields, in Tab order.
const FIELDS: [Field; 7] = [
    Field::Prompt,
    Field::Target,
    Field::Repo,
    Field::Base,
    Field::Options,
    Field::Model,
    Field::Submit,
];

/// The composer's fields in Tab order while the client holds two backends or more: the Backend field comes before the Model.
const FIELDS_MULTI: [Field; 8] = [
    Field::Prompt,
    Field::Target,
    Field::Repo,
    Field::Base,
    Field::Options,
    Field::Backend,
    Field::Model,
    Field::Submit,
];

/// Who holds a run: the plane's run map, asked for each row the Runs list draws. It has no state of its own.
#[derive(Clone)]
pub struct OwnerLookup(pub std::sync::Arc<OwnerFn>);

/// The function behind [`OwnerLookup`].
pub type OwnerFn = dyn Fn(&str) -> Option<String> + Send + Sync;

impl std::fmt::Debug for OwnerLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnerLookup")
    }
}

/// One line of the composer's model list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectRow {
    pub alias: String,
    pub id: String,
    pub name: String,
    pub window: u32,
    pub selectable: bool,
    pub chosen: bool,
    /// `default`, `your default`, `default (HAMH_MODEL)`, `pending: ...` or empty.
    pub tag: String,
}

/// What [`App::direct_view`] gives the drawing code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectView {
    /// `Claude Sonnet 5.5 (built-in default)`: the model the run will use and where the choice came from.
    pub title: String,
    pub rows: Vec<DirectRow>,
    pub note: Option<String>,
    pub saved: Option<String>,
}

#[derive(Clone, Copy)]
enum PickMove {
    Up,
    Down,
    First,
    Last,
}

/// The line shown when a quit is asked for while text is unsent.
const UNSENT_NOTICE: &str = "unsent text is lost when ham ends: press Ctrl-C or q again to quit";

/// The notice of a paste on the dashboard while its box is not taking text.
const PASTE_IGNORED_BROWSING: &str = "paste ignored: press i or Enter to type, then paste";

/// The notice of a paste on a screen with no text box.
const PASTE_IGNORED: &str = "paste ignored: this screen has no text box";

/// Pasted text made safe to insert: every line break becomes `\n`, escape sequences and control characters (the paste's own
/// `ESC [ 201 ~` end mark included) are removed, tabs become four spaces.
fn clean_paste(raw: &str) -> String {
    sanitize(&raw.replace("\r\n", "\n").replace('\r', "\n")).replace('\t', "    ")
}

/// The timer's period: the unit of the TUI's poll clock.
const TICK: Duration = Duration::from_secs(1);

/// The ticks of the timer between two frames of the smoke under the mascot's feet, in the title band and in the empty Runs table alike: the
/// smoke moves one step on every tick, so one step a second.
const MASCOT_TICKS_PER_FRAME: u32 = 1;

/// The Runs list is read again by the timer every this many ticks,
/// and never while the last timer read is unanswered.
const RUNS_REFRESH_TICKS: u8 = 5;

/// The list ids and box numbers of the hit map (the screens number their own).
pub(crate) const RUNS_LIST: ListId = ListId(0);
pub(crate) const MODEL_LIST: ListId = ListId(1);
/// The tag explorer's table.
pub(crate) const TAGS_LIST: ListId = ListId(2);
pub(crate) const BOX_PROMPT: u16 = 0;
pub(crate) const BOX_TARGET: u16 = 1;
pub(crate) const BOX_REPO: u16 = 2;
pub(crate) const BOX_BASE: u16 = 3;
pub(crate) const BOX_OPTIONS: u16 = 4;
pub(crate) const BOX_MODEL: u16 = 5;
/// The composer's Submit line: a press starts the run, as Ctrl-S does.
pub(crate) const BOX_SUBMIT: u16 = 6;
pub(crate) const BOX_BACKEND: u16 = 7;
/// The run screen's message box.
pub(crate) const BOX_MESSAGE: u16 = 0;

/// The note when a command is typed on the Runs screen's filter line.
pub(crate) const RUNS_COMMAND_NOTE: &str =
    "Commands run in the message box of a run: open a run and type it there.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composer {
    pub prompt: ChatBox,
    pub kind: TargetKind,
    pub repo: ChatBox,
    pub base: ChatBox,
    /// The words of the start options, as on the command line (see [`OPTIONS_TAKE`]).
    pub options: ChatBox,
    pub focus: Field,
    pub error: Option<String>,
    /// A line that is not an error, such as a paste the focused field cannot take.
    pub notice: Option<String>,
    pub submitting: bool,
    /// The composer's model state.
    pub direct: DirectPick,
    /// The engine the run will start on: drawn in the Engine box beside the Target and changed with the up and down keys on the
    /// Target field.
    pub engine: EnginePick,
    /// The question about provider data sharing while it waits for `y`, `n` or Esc.
    pub sharing: Option<SharingAsk>,
}

/// The engine of the composer's run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnginePick {
    /// An id of `ham_core::engines` that a person can choose.
    pub id: &'static str,
    /// The person picked it in this composer, or chose it with `/engine`, or named it on a command line: it goes into the start as
    /// `--engine`. A pick that is only the default is not sent, so the file's `default_engine` decides at the start.
    pub chosen: bool,
}

impl EnginePick {
    fn default_pick() -> Self {
        Self {
            id: ham_core::engines::default_engine().id,
            chosen: false,
        }
    }
}

/// How the model on the composer's screen came to be chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickSource {
    /// The user's default: where it came from (`HAMH_MODEL`, the config file, the built-in default).
    Default(ModelSource),
    /// Picked with the arrow keys in this composer.
    Here,
}

/// The composer's Model field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectPick {
    /// Index into the model list of the chosen row (always a selectable one).
    pub index: usize,
    pub source: PickSource,
    /// Said when the user's default names no selectable model and the composer fell back to the list's default.
    pub note: Option<String>,
    /// The composer is on that fallback and the user has not confirmed it: Submit refuses until a key on the Model field
    /// (↑↓, Home, End or Enter) confirms or changes the model, so a missed note cannot send a model nobody chose.
    pub fallback: bool,
    /// The answer to the last save (shown under the list).
    pub saved: Option<String>,
}

/// What a direct composer needs beyond the key presses: the user's stored model choices and the rules the run will send.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectSettings {
    /// `HAMH_MODEL` and the config file's `default_model`, read when the TUI started (a save updates `config`).
    pub prefs: ModelPrefs,
    /// A rules directory or file instead of `~/.config/ham/rules/`.
    pub rules: Option<String>,
    /// Send no rules.
    pub no_rules: bool,
}

/// The direct line's state of an [`App`].
#[derive(Clone, Debug)]
struct Direct {
    models: &'static Models,
    settings: DirectSettings,
    /// Runs this session started: the Idempotency-Key nonce moves on after each, so the same prompt sent twice is two runs.
    started: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadState {
    None,
    Pending,
    Done(Pulled),
    /// `d` while a zip is on the card: the zip is being fetched again, and the card keeps showing the earlier one.
    Refresh(Pulled),
    Failed(String),
}

/// What the session did with the mouse: the terminal reports it now, or not; and `HAMH_NO_MOUSE` keeps it off whatever `/mouse` says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseState {
    pub on: bool,
    pub forced_off: bool,
}

/// The delivery card's clone: `C` turns the downloaded zip into a local git repository.
#[derive(Clone, Debug, PartialEq)]
pub enum CloneState {
    None,
    Pending,
    Done(Cloned),
    Failed(String),
}

/// How long an answer waits for the run's events to show that Cline took it, before the keys come back (the CLI resends at 12 s).
const ACK_WAIT: Duration = Duration::from_secs(15);

/// The typing screen's state: what is being written, for which question and which action. It outlives Esc, so a text is not lost
/// by looking at the card again; it goes when its question closes (and the user has left the screen).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub action: InstructAction,
    pub request_id: String,
    pub text: String,
    /// `try_model`: the route id and its display name.
    pub model: Option<(String, String)>,
    /// The question's title and last words, kept so the screen still shows them after the question closes.
    pub title: String,
    pub last_words: Option<String>,
    pub error: Option<String>,
    /// Ctrl-S went out and no answer has come back yet.
    pub sending: bool,
    /// The question this text was written for has closed (why, in words): the text stays and nothing more is sent.
    pub closed: Option<String>,
}

/// An answer the server queued: the card says it waits for Cline until the question closes or [`ACK_WAIT`] passes.
#[derive(Clone, Debug)]
struct Sent {
    request_id: String,
    at: Duration,
}

/// Which of `o` and `d` act on the focused run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinkKeys {
    /// The run has a pull request, and `o` opens it.
    pub pr: bool,
    /// A zip is delivered: `d` downloads it, and `o` opens its link when there is no pull request.
    pub zip: bool,
}

impl LinkKeys {
    /// `o` has something to open.
    pub fn open(self) -> bool {
        self.pr || self.zip
    }
}

/// What a command sink did with the line the user sent from the message box.
#[derive(Clone, Debug, PartialEq)]
pub enum Submit {
    /// The sink took the line: the box clears and the effects run.
    Handled(Vec<Effect>),
    /// The sink refuses the line: the box keeps its text and the reason is shown.
    Refused(String),
    /// The sink leaves the line to the default route: an open question is answered with it; otherwise the text stays and the screen says
    /// that Cline is not waiting.
    Pass,
}

/// The first look at a line sent from the message box: the trimmed, non-empty text. The default sink passes every line on.
pub type CommandSink = fn(&mut App, &str) -> Submit;

fn pass_sink(_: &mut App, _: &str) -> Submit {
    Submit::Pass
}

/// The config revision of an app that was not given one: always the same, so a preview never sees the settings move.
fn no_revision() -> u64 {
    0
}

/// A plain phrase waiting for the user's answer: what the chip asks, and the argv that Enter prepares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chip {
    /// The phrase in its normal form (lower case, single blanks).
    pub(crate) phrase: String,
    /// The command the phrase proposes, with the focused run when it takes one.
    pub(crate) argv: Vec<String>,
}

#[derive(Debug)]
pub struct App {
    screen: Screen,
    nonce: String,
    default_repo: Option<String>,
    pub(crate) runs: Vec<RunSummary>,
    /// The rows of [`App::runs`] as the Runs screen words them (`ham_core::runfilter`), rebuilt whenever the list changes.
    pub(crate) runs_rows: Vec<RunRow>,
    /// What the run index holds about the runs read, by run id: the first line of the prompt and the model.
    run_index: std::collections::HashMap<String, IndexEntry>,
    /// The words and the scope that narrow the Runs list.
    pub(crate) runs_filter: RunsFilter,
    /// The filter line at the bottom of the Runs screen is open: every character is the filter's.
    pub(crate) runs_editing: bool,
    /// Ticks since the Runs list was last read by the timer.
    runs_ticks: u8,
    /// A timer read of the Runs list is in flight: the next one waits for its answer.
    runs_inflight: bool,
    /// The Runs table's window onto the shown list (`runs_view.rs`): the first row it drew and the rows it had, as positions in the list the
    /// filter shows. A draw sets them (it takes `&self`, hence the cells); the page keys read them.
    runs_first: std::cell::Cell<usize>,
    runs_window: std::cell::Cell<usize>,
    /// The regions of the last draw: every box, run row, fold, key chip and scrolling pane, each with the key that does the same.
    pub(crate) hits: RefCell<HitMap>,
    /// The stored conversation of the focused run and how it is read (`history.rs`).
    pub(crate) hist: history::History,
    /// The prompts of earlier runs in the box's history: the full prompts being fetched and the wait for one (`earlier.rs`).
    pub(crate) earlier: earlier::EarlierRuns,
    /// The model to show for a run whose session has not named one (A1 proof: the title never depends on one event).
    pub(crate) titled: model_title::ModelTitle,
    /// The names of runs on the run screen: the seed of a prompt this app sent, what a read of the run's row said, the reads' schedule
    /// (`run_name.rs`).
    pub(crate) names: run_name::RunNames,
    /// The messages of the focused run and what Enter does (`send.rs`).
    pub(crate) send: send::SendState,
    /// The pull request link the last `o` asked the opener to open: if no browser opens it, the link is copied.
    open_fallback: Option<String>,
    /// Words that lead the answer to the copy that is on its way, once (the reason the link is being copied).
    copy_prefix: Option<String>,
    /// The open-anything box (`openbox.rs`).
    pub(crate) find: openbox::OpenBox,
    /// The tag explorer and the `/tag` preview.
    pub(crate) tags: tags::TagExplorer,
    /// The capability record of the focused run's engine (`engine_ui.rs`).
    pub(crate) caps: ham_core::engines::Capabilities,
    /// `default_engine` of the config file as the process read it at the start; `None` when the file sets none.
    pub(crate) file_engine: Option<String>,
    /// The engine `/engine <name>` chose for the runs this session starts; `None` until the person chooses.
    pub(crate) session_engine: Option<&'static str>,
    /// The Settings screen (`settings.rs`).
    pub(crate) settings: settings::SettingsState,
    /// The reconnect state of the run on screen: a failed poll shows the overlay only after the grace, a good read clears it.
    link: Link,
    /// The Unix time of the run's first event and of its completion, read from the events (the header counts between them).
    run_started: Option<i64>,
    run_ended: Option<i64>,
    /// The ids of the `turn.ended` events read, and how many of them the detail was asked for (the totals are as of that turn).
    turn_ids: HashSet<String>,
    turns_asked: usize,
    /// The first page of a run is read together with its detail; only turns that end after it ask for the totals again.
    first_pages: bool,
    /// The alerts for the Runs list: which runs waited at the last read. The first read only seeds it.
    alerts: Alerts,
    alerts_seeded: bool,
    /// The terminal notification option, the bell (`ui.bell`: on unless the file says false), whether the window is focused (a terminal
    /// that never says stays focused) and whether it is tmux.
    notify_on: bool,
    bell_on: bool,
    /// The ham mascot (`head.rs`): whether the title band may be drawn, whether the empty Runs table shows its picture, and the look. The
    /// constructor's value is neutral (no band); `cli::tui_app` gives the user's.
    pub(crate) head: Head,
    /// The look the environment asks for (`mascot::look_from_env`), kept so that a write of `ui.mascot` from the Settings screen can give
    /// `auto` its look back.
    env_look: super::mascot::Look,
    /// The smoke under the mascot's feet moves, in the title band and in the empty Runs table (`ui.mascot_motion`, on unless the file says `false`).
    mascot_motion: bool,
    /// The most bytes one copy to the clipboard carries (`ui.copy_limit_kib`, 64 KiB unless the file says otherwise).
    copy_limit: usize,
    /// The ticks of the timer counted while the mascot is on the screen and moves: the frame of the smoke follows from it
    /// ([`App::mascot_frame`]). With the band on (`Head.band`) it counts on every screen; without the band it counts while the picture is in
    /// the empty Runs table and is 0 otherwise, so that the picture starts at the still picture whenever it appears.
    mascot_ticks: u32,
    focused: bool,
    tmux: bool,
    /// The picture on the terminal may no longer be what was drawn (the window has focus again, Ctrl-L, a continue): the event loop clears
    /// the screen before its next draw, so that draw writes every cell and not only the cells that changed. Taken by [`App::take_repaint`].
    repaint: bool,
    /// The question that was being answered closed while the box held text: how it ended, for the box's title.
    pub(crate) closed_banner: Option<String>,
    /// A plain phrase that was typed and waits for Enter (prepare the command) or Esc (dismiss, keep the draft).
    pub(crate) chip: Option<Chip>,
    /// The phrases the user dismissed since the box was last emptied: they are not offered again, and Enter sends them as chat.
    chip_dismissed: Vec<String>,
    /// The marked row of the command popup.
    pub(crate) popup_sel: usize,
    /// The draft text the popup was closed at with Esc; a changed text opens it again.
    popup_closed: Option<String>,
    /// The prompt history of the box: the lines that were sent, then the run's earlier messages. In memory only.
    history: HistoryRing,
    /// Ctrl-R is open: the search over the history, and the box keeps the draft until a match is taken.
    pub(crate) hsearch: Option<ReverseSearch>,
    /// The box text was just set by the history (a recall), not typed.
    recalled: bool,
    /// The pastes behind the `[Pasted n lines]` tokens of each run's draft. In memory only.
    pastes: HashMap<String, PasteStore>,
    /// The mouse state machine: presses, the double click, the wheel.
    pointer: Pointer,
    /// Whether the terminal reports the mouse now, and whether `HAMH_NO_MOUSE` keeps it off (`/mouse`).
    mouse: MouseState,
    /// The one-line hint on selecting text with the terminal itself, shown on the first screen of a session that asks for the mouse until
    /// the first key or click.
    pub(crate) mouse_hint: Option<String>,
    /// The directory `C` clones into (`ham clone <run>` clones into the working directory); `None`: next to the zip.
    clone_dir: Option<std::path::PathBuf>,
    pub(crate) runs_error: Option<String>,
    /// The backends and how each answered last; `None` for a client with one backend, which draws no column, banner or field.
    pub(crate) backend_view: Option<ham_core::backends::BackendView>,
    /// Who holds a run, for the BACKEND column.
    backend_owner: Option<OwnerLookup>,
    /// The backend the composer starts on (`--backend`, `default_backend`, or the person's pick).
    backend_pick: Option<String>,
    /// Each backend with whether it is declared safe for customer content, for a client with one backend too.
    declared: Vec<(String, bool)>,
    /// A line for the Runs screen's status row, such as the paste it ignored; the next key clears it.
    pub(crate) runs_notice: Option<String>,
    /// The whole text of a Runs notice that its row cannot hold: the panel above the note row shows it while `runs_notice` is still
    /// its `row`, so a later notice or the next key takes it away.
    pub(crate) runs_detail: Option<RunsDetail>,
    /// The question `R` asked on the Runs screen: the plan it named and when it was shown. Enter or `R` again, 300 ms or more later,
    /// starts it; Esc or any other key withdraws it.
    pub(crate) resume_ask: Option<ResumeAsk>,
    /// The paused and stalled runs the plan left out when the question was confirmed, kept for the report.
    resume_skipped: Vec<ham_core::resume::Skipped>,
    /// The service has answered a read of the Runs list at least once: before that an empty list means "not read yet", not "no runs"
    /// (the ham mascot of `mascot.rs` waits for it, so it never shows while the list is still loading).
    pub(crate) runs_answered: bool,
    /// The dashboard's mode: typing in the message box, or browsing with the hotkeys.
    mode: Mode,
    /// A run opens in typing mode (the chat view's default) rather than browse mode.
    opens_typing: bool,
    /// The message box of each run opened in this process, keyed by run id.
    drafts: HashMap<String, ChatBox>,
    /// The line of provider data sharing of each run this process started, keyed by run id: the switch of the account's Bedrock data
    /// retention mode, or why ham left it alone. The run screen keeps it as a card, because the notice row holds one row and cuts it.
    pub(crate) sharing_notes: HashMap<String, String>,
    /// Why a retry, a follow-up or a resume did not start, keyed by the run it was started from. The run screen keeps it as a card of that
    /// run, wrapped to the width, because the notice row holds one row; a later start from the run that succeeds takes it away.
    pub(crate) start_refusals: HashMap<String, Refusal>,
    /// The first look at a sent line.
    sink: CommandSink,
    /// The preview waiting for its second Enter.
    pub(crate) armed: Option<Armed>,
    /// The result panel above the message box.
    pub(crate) panel: Option<Panel>,
    /// The runs whose message box holds text that was pasted: every command in it shows a preview.
    tainted: HashSet<String>,
    /// A preview was just armed: the event loop drops the key events it has queued.
    flush: bool,
    /// The start of the app's clock for key times, and the milliseconds of the key being handled on it. A key that carries no instant
    /// is read at the real time: it is calm (the tests' `handle_key`), but it cannot confirm a preview before 300 ms have really passed.
    epoch: Instant,
    key_ms: u64,
    /// The number that changes when the config file does (see [`commands::config_revision`]).
    config_rev: fn() -> u64,
    /// Tells a paste that arrives as keys from typing.
    burst: Burst,
    /// The key being handled may send or confirm: it is not part of a burst.
    calm: bool,
    /// A quit was asked for while text was unsent and the warning was shown: the very next key may quit for real.
    quit_armed: bool,
    /// The value `quit_armed` had when the current key arrived.
    was_armed: bool,
    pub(crate) selected: usize,
    pub(crate) composer: Composer,
    pub(crate) run: Option<RunState>,
    pub(crate) dashboard_error: Option<String>,
    pub(crate) download: DownloadState,
    /// The words of the `Tests` line of the delivered zip's clean-checkout test report, read once when the download ends; `None` while the
    /// zip is not there or holds no report.
    pub(crate) tests: Option<String>,
    /// The word of the `Flow` row of the delivered zip, read once with the tests line; `None` while the zip is
    /// not there or records no flow.
    pub(crate) flow: Option<String>,
    pub(crate) clone: CloneState,
    pub(crate) detail: Option<RunDetail>,
    polling: bool,
    /// Which shards the focused run's next poll reads and how long it waits (client polish B2). The clock counts timer ticks.
    plan: PollPlan,
    clock: Duration,
    next_poll_at: Duration,
    direct: Direct,
    /// The focused run's view and agent rows as of the last poll.
    pub(crate) cached: Option<RunView>,
    pub(crate) agent_rows: Vec<AgentRow>,
    pub(crate) list: Vec<ListLine>,
    /// The run screen's conversation and its scroll state. A draw sets the cards on it, hence the cell.
    pub(crate) chat: RefCell<ChatView>,
    /// Search inside the conversation: the bar, the matches and the folded block it holds open.
    pub(crate) search: ChatSearchUi,
    /// The answer to the last cancel, or the finish confirmation prompt.
    pub(crate) notice: Option<String>,
    pub(crate) artifacts: ArtifactList,
    pub(crate) artifact_cursor: usize,
    /// The Artifacts screen's download: the file name and its state.
    pub(crate) artifact_download: Option<(String, DownloadState)>,
    /// The typing screen's text for the open question (see [`Reply`]).
    pub(crate) reply: Option<Reply>,
    /// The highlighted line of the model list.
    pub(crate) model_cursor: usize,
    confirm_finish: bool,
    sent: Option<Sent>,
    /// The action of the answer on its way (Ctrl-S or the second `f` went out, no reply yet).
    inflight: Option<InstructAction>,
    /// The question the last poll showed, to see it close or be replaced.
    open_id: Option<String>,
    /// The questions whose bell has rung, on a run's screen or on the Runs list.
    rung: Rung,
    /// The wall clock in Unix seconds, set by the event loop; `None` (tests, before the first tick) shows no minutes left.
    pub(crate) now: Option<i64>,
    /// When the link made by `o` stops working (RFC 3339); it replaces the expiry of the link the download used, which is older.
    pub(crate) link_expires: Option<String>,
    /// `HAMH_OPEN_COMMAND`: the program `o` runs with the link as its one argument; empty or unset means the platform's opener.
    open_command: Option<String>,
    /// Writes the link's expiry in the reader's zone; UTC until the process sets the local one.
    time_text: ClockText,
    /// The last expiry written: Unix seconds and its text, so a draw does not ask the clock again.
    clock_memo: std::cell::RefCell<Option<(i64, String)>>,
    /// The same memo for the time of the last saved copy, so the two times never push each other out of one slot.
    saved_clock_memo: std::cell::RefCell<Option<(i64, String)>>,
}

impl App {
    /// The TUI of a direct run: one model from `engine/catalog/models.json`, `POST /v1/direct/runs`. `nonce` makes this
    /// session's Idempotency-Keys; `default_repo` prefills the composer.
    pub fn new_direct(
        nonce: String,
        default_repo: Option<String>,
        settings: DirectSettings,
    ) -> Self {
        Self::new_direct_with(builtin(), nonce, default_repo, settings)
    }

    /// [`App::new_direct`] over another model list (tests).
    pub fn new_direct_with(
        models: &'static Models,
        nonce: String,
        default_repo: Option<String>,
        settings: DirectSettings,
    ) -> Self {
        let direct = Direct {
            models,
            settings,
            started: 0,
        };
        Self {
            screen: Screen::Runs,
            composer: Composer::new(default_repo.as_deref(), &direct),
            nonce,
            default_repo,
            runs: Vec::new(),
            runs_rows: Vec::new(),
            run_index: std::collections::HashMap::new(),
            runs_filter: RunsFilter::new(),
            runs_editing: false,
            runs_ticks: 0,
            runs_inflight: false,
            runs_first: std::cell::Cell::new(0),
            runs_window: std::cell::Cell::new(0),
            hits: RefCell::new(HitMap::new()),
            hist: history::History::default(),
            earlier: earlier::EarlierRuns::default(),
            titled: model_title::ModelTitle::default(),
            names: run_name::RunNames::default(),
            send: send::SendState::default(),
            open_fallback: None,
            copy_prefix: None,
            find: openbox::OpenBox::default(),
            tags: tags::TagExplorer::default(),
            caps: ham_core::engines::Capabilities::unknown(),
            file_engine: None,
            session_engine: None,
            settings: settings::SettingsState::default(),
            link: Link::new(),
            run_started: None,
            run_ended: None,
            turn_ids: HashSet::new(),
            turns_asked: 0,
            first_pages: true,
            alerts: Alerts::new(),
            alerts_seeded: false,
            notify_on: false,
            bell_on: true,
            head: Head::default(),
            env_look: super::mascot::Look::Blocks,
            mascot_motion: true,
            copy_limit: copy::COPY_LIMIT,
            mascot_ticks: 0,
            focused: true,
            tmux: false,
            repaint: false,
            closed_banner: None,
            chip: None,
            chip_dismissed: Vec::new(),
            popup_sel: 0,
            popup_closed: None,
            history: HistoryRing::new(),
            hsearch: None,
            recalled: false,
            pastes: HashMap::new(),
            pointer: Pointer::new(WheelSettings::default()),
            mouse: MouseState::default(),
            mouse_hint: None,
            clone_dir: None,
            runs_error: None,
            backend_view: None,
            backend_owner: None,
            backend_pick: None,
            declared: Vec::new(),
            runs_notice: None,
            runs_detail: None,
            resume_ask: None,
            resume_skipped: Vec::new(),
            runs_answered: false,
            mode: Mode::Browse,
            opens_typing: false,
            drafts: HashMap::new(),
            sharing_notes: HashMap::new(),
            start_refusals: HashMap::new(),
            sink: pass_sink,
            armed: None,
            panel: None,
            tainted: HashSet::new(),
            flush: false,
            epoch: Instant::now(),
            key_ms: 0,
            config_rev: no_revision,
            burst: Burst::new(),
            calm: true,
            quit_armed: false,
            was_armed: false,
            selected: 0,
            run: None,
            dashboard_error: None,
            download: DownloadState::None,
            tests: None,
            flow: None,
            clone: CloneState::None,
            detail: None,
            polling: false,
            plan: PollPlan::new(TICK),
            clock: Duration::ZERO,
            next_poll_at: Duration::ZERO,
            direct,
            cached: None,
            agent_rows: Vec::new(),
            list: Vec::new(),
            chat: RefCell::new(ChatView::new()),
            search: ChatSearchUi::new(),
            notice: None,
            artifacts: ArtifactList::NotLoaded,
            artifact_cursor: 0,
            artifact_download: None,
            reply: None,
            model_cursor: 0,
            confirm_finish: false,
            sent: None,
            inflight: None,
            open_id: None,
            rung: Rung::default(),
            now: None,
            link_expires: None,
            open_command: None,
            time_text: ham_core::render::utc_clock,
            clock_memo: std::cell::RefCell::new(None),
            saved_clock_memo: std::cell::RefCell::new(None),
        }
    }

    /// Sets the program `o` opens links with (`HAMH_OPEN_COMMAND`); `None` or an empty string means the platform's opener.
    pub fn set_open_command(&mut self, program: Option<String>) {
        self.open_command = program;
    }

    /// Sets how the link's expiry is written (`06:12 MDT`). The default is UTC; the process passes `crate::link::local_clock`.
    pub fn set_clock(&mut self, clock: ClockText) {
        self.time_text = clock;
        *self.clock_memo.borrow_mut() = None;
        *self.saved_clock_memo.borrow_mut() = None;
    }

    /// The time of the last saved copy as the reader's clock, asking the clock once per distinct time.
    pub(crate) fn saved_clock_text(&self, epoch: i64) -> String {
        if let Some((at, text)) = &*self.saved_clock_memo.borrow()
            && *at == epoch
        {
            return text.clone();
        }
        let text = (self.time_text)(epoch);
        *self.saved_clock_memo.borrow_mut() = Some((epoch, text.clone()));
        text
    }

    /// `epoch` as the reader's clock, asking it once per distinct time.
    pub(crate) fn clock_text(&self, epoch: i64) -> String {
        if let Some((at, text)) = &*self.clock_memo.borrow()
            && *at == epoch
        {
            return text.clone();
        }
        let text = (self.time_text)(epoch);
        *self.clock_memo.borrow_mut() = Some((epoch, text.clone()));
        text
    }

    /// Tells the app the wall-clock time (Unix seconds): the card counts the minutes left from it. The app does no I/O, so the event
    /// loop calls this before each draw.
    pub fn set_now(&mut self, unix_secs: i64) {
        self.now = Some(unix_secs);
    }

    /// The model id on the composer's screen.
    pub fn chosen_model(&self) -> Option<&str> {
        self.direct
            .models
            .models()
            .get(self.composer.direct.index)
            .map(|m| m.id.as_str())
    }

    /// What the composer draws: the model rows, the chosen one, and the texts around them.
    pub(crate) fn direct_view(&self) -> Option<DirectView> {
        let d = &self.direct;
        let p = &self.composer.direct;
        let effective = resolve(d.models, None, &d.settings.prefs).ok();
        let rows = d
            .models
            .models()
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut tag = if !m.selectable() {
                    "pending: no passing smoke record, not selectable".to_string()
                } else {
                    match &effective {
                        Some(r) if r.model.id == m.id => match r.source {
                            ModelSource::Config => "your default".to_string(),
                            ModelSource::Env => "default (HAMH_MODEL)".to_string(),
                            _ => "default".to_string(),
                        },
                        _ => String::new(),
                    }
                };
                if m.requires_provider_sharing {
                    if !tag.is_empty() {
                        tag.push_str("; ");
                    }
                    tag.push_str("needs provider data sharing");
                }
                DirectRow {
                    alias: m.alias.clone().unwrap_or_default(),
                    id: m.id.clone(),
                    name: m.name.clone(),
                    window: m.context_tokens,
                    selectable: m.selectable(),
                    chosen: i == p.index,
                    tag,
                }
            })
            .collect::<Vec<_>>();
        let chosen = rows.iter().find(|r| r.chosen)?;
        Some(DirectView {
            title: format!(
                "{} ({})",
                chosen.name,
                match p.source {
                    PickSource::Default(s) => s.label(),
                    PickSource::Here => "chosen here",
                }
            ),
            rows,
            // A chosen model that needs provider data sharing says what Submit does.
            note: p.note.clone().or_else(|| {
                d.models
                    .models()
                    .get(p.index)
                    .filter(|m| m.requires_provider_sharing)
                    .map(|m| {
                        format!(
                            "{} needs provider data sharing: with run.provider_sharing off, Submit asks before ham turns it on in the AWS account of the stage.",
                            m.name
                        )
                    })
            }),
            saved: p.saved.clone(),
        })
    }

    /// The dashboard's mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether a run opens in typing mode (the chat view's default) or in browse mode (the default until that view lands, so the
    /// dashboard's hotkeys work as they always did).
    pub fn set_run_opens_typing(&mut self, typing: bool) {
        self.opens_typing = typing;
    }

    /// Sets the first look at a line sent from the message box (see [`Submit`]).
    pub fn set_command_sink(&mut self, sink: CommandSink) {
        self.sink = sink;
    }

    /// Puts a line where the current screen shows one (the notice row of the dashboard, the status row of Runs, the message line of New
    /// run).
    pub fn say(&mut self, text: &str) {
        match self.screen {
            Screen::Runs => self.runs_notice = Some(text.to_string()),
            Screen::Composer => self.composer.notice = Some(text.to_string()),
            Screen::Reply => {
                if let Some(r) = self.reply.as_mut() {
                    r.error = Some(text.to_string());
                }
            }
            Screen::Dashboard | Screen::Artifacts | Screen::Models => {
                self.notice = Some(text.to_string());
            }
            Screen::Settings => self.settings.say(text),
        }
    }

    /// The focused run's message box.
    pub(crate) fn chat_box(&self) -> Option<&ChatBox> {
        self.drafts.get(self.focused_run()?)
    }

    fn chat_mut(&mut self) -> Option<&mut ChatBox> {
        let id = self.run.as_ref()?.run_id().to_string();
        Some(
            self.drafts
                .entry(id)
                .or_insert_with(|| ChatBox::new(Kind::Multi)),
        )
    }

    /// The text in the focused run's message box (empty when the run has none yet); `None` with no run in focus.
    pub fn chat_text(&self) -> Option<String> {
        let id = self.focused_run()?;
        Some(self.drafts.get(id).map(ChatBox::text).unwrap_or_default())
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn focused_run(&self) -> Option<&str> {
        self.run.as_ref().map(RunState::run_id)
    }

    pub fn view(&self) -> Option<RunView> {
        self.cached
            .clone()
            .or_else(|| self.run.as_ref().map(RunState::view))
    }

    fn scroll_chat(&mut self, action: ScrollAction) -> Vec<Effect> {
        self.chat.get_mut().scroll(action);
        self.hist_scrolled()
    }

    /// The conversation again from the events read so far and the prompt: the progress feed of the chat model. Its own `status` and
    /// `delivery` blocks are left out, because the run screen draws those as cards that carry the keys and the link line.
    fn rebuild_chat(&mut self) {
        let Some(state) = &self.run else {
            return;
        };
        let prompt = self.detail.as_ref().and_then(|d| d.objective.as_deref());
        let inputs = ChatInputs {
            prompt,
            ..ChatInputs::default()
        };
        let mut model = ChatModel::from_progress_feed(state, &inputs);
        model
            .blocks
            .retain(|b| b.id != "status" && b.id != "delivery");
        // The stored conversation, when the store has the run, in place of the progress feed.
        let model = self.hist_model(model);
        // What the person wrote in this run (its prompt and the messages sent to Cline) is in the history, oldest first.
        self.history.set_run_messages(
            model
                .blocks
                .iter()
                .filter(|b| b.kind == BlockKind::Message && b.effective_role() == Role::You)
                .map(|b| b.body.clone())
                .collect(),
        );
        let chat = self.chat.get_mut();
        chat.set_model(model);
        // The matches are searched again in the new conversation; the view stays where it is.
        let search = &mut self.search;
        chat.with_state(|state, model| search.model_changed(state, model));
    }

    /// Recomputes the cached view, the rows and the list after new events.
    fn refresh(&mut self) {
        let Some(state) = &self.run else {
            return;
        };
        let v = state.view();
        self.agent_rows = rows(&v);
        self.list = Vec::with_capacity(self.agent_rows.len() + 2);
        let mut i = 0;
        for (g, n) in group_counts(&self.agent_rows) {
            self.list.push(ListLine::Group(g, n));
            for k in i..i + n {
                self.list.push(ListLine::Agent(k));
            }
            i += n;
        }
        self.cached = Some(v);
        self.caps_refresh();
        self.send_context_changed();
        self.rebuild_chat();
    }

    /// The first effects of a session on the Runs screen.
    pub fn start(&mut self) -> Vec<Effect> {
        vec![Effect::LoadRuns]
    }

    fn poll_effect(&mut self) -> Vec<Effect> {
        match &self.run {
            Some(state) if !self.polling => {
                self.polling = true;
                let cursors = self.plan.cursors(state, self.clock);
                vec![Effect::Poll {
                    run_id: state.run_id().to_string(),
                    cursors,
                }]
            }
            _ => vec![],
        }
    }

    /// Opens the dashboard on `run_id`, rehydrated from the API.
    pub fn open_run(&mut self, run_id: &str) -> Vec<Effect> {
        self.screen = Screen::Dashboard;
        self.mode = if self.opens_typing {
            Mode::Typing
        } else {
            Mode::Browse
        };
        let changed = self.focused_run() != Some(run_id);
        if changed {
            self.run = Some(RunState::new(run_id));
            self.download = DownloadState::None;
            self.tests = None;
            self.flow = None;
            self.clone = CloneState::None;
            self.link_expires = None;
            self.detail = None;
            self.dashboard_error = None;
            self.polling = false;
            self.plan = PollPlan::new(TICK);
            self.next_poll_at = self.clock;
            self.cached = None;
            self.agent_rows.clear();
            self.list.clear();
            self.chat = RefCell::new(ChatView::new());
            self.search = ChatSearchUi::new();
            self.notice = None;
            self.artifacts = ArtifactList::NotLoaded;
            self.artifact_download = None;
            self.reply = None;
            self.model_cursor = 0;
            self.confirm_finish = false;
            self.sent = None;
            self.inflight = None;
            self.open_id = None;
            // A preview and a result belong to the run that was on screen; so do the chip and the popup.
            self.armed = None;
            self.panel = None;
            self.chip = None;
            self.chip_dismissed.clear();
            self.popup_closed = None;
            self.popup_sel = 0;
            self.closed_banner = None;
            self.link = Link::new();
            self.run_started = None;
            self.run_ended = None;
            self.turn_ids.clear();
            self.turns_asked = 0;
            self.first_pages = true;
            self.refresh();
            // The wait for a full prompt belonged to the run that was on screen; the run now on screen leaves the earlier runs of the
            // history, and the one that was on screen joins them.
            self.earlier_open();
        }
        let mut effects = self.poll_effect();
        if changed {
            // The prompt of the run and its totals come from the detail.
            effects.push(Effect::LoadDetail {
                run_id: run_id.to_string(),
            });
            // Its name: the held list or the seed may have it already; unless a model's name is known one read asks for the row.
            effects.extend(self.name_open(run_id));
            // The newest chunks of the stored conversation and the run's messages.
            effects.extend(self.hist_open(run_id));
            effects.extend(self.send_open(run_id));
        }
        effects
    }

    /// The 1 s timer: the focused run is polled when its plan says a poll is due. While events flow that is every tick; a
    /// quiet run backs off to every fifth tick, and reads only the shards that matter (`ham_core::poll`).
    pub fn tick(&mut self) -> Vec<Effect> {
        self.clock += TICK;
        self.mascot_tick();
        if self.screen == Screen::Runs {
            self.runs_ticks = self.runs_ticks.saturating_add(1);
            let mut effects = Vec::new();
            if self.runs_ticks >= RUNS_REFRESH_TICKS && !self.runs_inflight {
                self.runs_ticks = 0;
                self.runs_inflight = true;
                effects.push(Effect::RefreshRuns(Refresh::Timer));
            }
            effects.extend(self.openbox_tick());
            return effects;
        }
        self.runs_ticks = 0;
        let mut effects = if matches!(self.screen, Screen::Dashboard | Screen::Artifacts)
            && self.clock >= self.next_poll_at
        {
            self.poll_effect()
        } else {
            vec![]
        };
        self.earlier_tick();
        effects.extend(self.hist_tick());
        effects.extend(self.model_probe());
        effects.extend(self.run_name_tick());
        effects.extend(self.send_tick());
        effects.extend(self.openbox_tick());
        effects
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.key_at(key, None)
    }

    /// A mouse event read at `at`: the pointer turns it into actions against the regions of the last draw, and each action does what
    /// its key does. Text selection and copy are the next step; their actions change nothing here.
    pub fn handle_mouse_at(&mut self, event: MouseEvent, at: Instant) -> Vec<Effect> {
        self.mouse_hint = None;
        let actions = {
            let map = self.hits.borrow();
            self.pointer.handle(&map, event, at)
        };
        let mut effects = Vec::new();
        for action in actions {
            effects.extend(self.mouse_action(action));
        }
        effects
    }

    /// When the event loop must wake to repeat the autoscroll of a drag that is held outside the conversation: `Some` while one is,
    /// `None` otherwise (then the loop has no timer for it).
    pub fn autoscroll_due(&self, now: Instant) -> Option<Instant> {
        self.pointer.autoscroll_due(&self.hits.borrow(), now)
    }

    /// The repeat of the autoscroll of a held drag, at `now`: the pointer scrolls the conversation by a step and moves the end of the selection
    /// to the edge cell of the pane, once per `mouse::AUTOSCROLL_EVERY`, through the same actions a mouse report gives. Nothing happens
    /// before the interval has passed, after the button came up, after a key, a resize, or while the pointer is inside the pane.
    pub fn autoscroll_at(&mut self, now: Instant) -> Vec<Effect> {
        let actions = {
            let map = self.hits.borrow();
            self.pointer.tick(&map, now)
        };
        let mut effects = Vec::new();
        for action in actions {
            effects.extend(self.mouse_action(action));
        }
        effects
    }

    fn mouse_action(&mut self, action: MouseAction) -> Vec<Effect> {
        match action {
            MouseAction::Focus(BoxId(n)) => self.focus_box(n),
            MouseAction::Select { list, index, .. } => {
                self.select_row(list, index);
                vec![]
            }
            MouseAction::Open { list, index, .. } => {
                self.select_row(list, index);
                if list == RUNS_LIST {
                    self.open_selected_run()
                } else if list == TAGS_LIST {
                    self.tags_apply();
                    vec![]
                } else {
                    vec![]
                }
            }
            MouseAction::ToggleFold { block } => {
                self.chat.get_mut().toggle_fold(&block);
                vec![]
            }
            // A chip is the key it names: the key table and every guard that goes with it apply.
            MouseAction::Press(code) => self.key_at(KeyEvent::new(code, KeyModifiers::NONE), None),
            MouseAction::Wheel { lines, .. } => self.wheel_lines(lines),
            // Drag to select in the conversation: the text under the pointer is reversed while it is held, and the release copies it.
            MouseAction::SelectionStart { x, y, .. } => {
                self.chat.get_mut().select_start(x, y);
                vec![]
            }
            MouseAction::SelectionExtend { x, y, .. } => {
                self.chat.get_mut().select_extend(x, y);
                vec![]
            }
            MouseAction::SelectionEnd { x, y, .. } => {
                let chat = self.chat.get_mut();
                chat.select_extend(x, y);
                match chat.selection_text() {
                    Some(text) => self.copy_text(&text),
                    None => {
                        self.say("The selected text is no longer on the screen.");
                        vec![]
                    }
                }
            }
            MouseAction::Autoscroll { lines, .. } => self.wheel_lines(lines),
            MouseAction::ClearSelection => {
                self.chat.get_mut().clear_selection();
                vec![]
            }
            _ => vec![],
        }
    }

    /// A press on a text box: the composer's box gets the focus; on the run screen the message box starts typing, as `i` does.
    fn focus_box(&mut self, n: u16) -> Vec<Effect> {
        match self.screen {
            Screen::Composer => {
                let field = match n {
                    BOX_PROMPT => Field::Prompt,
                    BOX_TARGET => Field::Target,
                    BOX_REPO => Field::Repo,
                    BOX_BASE => Field::Base,
                    BOX_OPTIONS => Field::Options,
                    BOX_MODEL => Field::Model,
                    BOX_BACKEND => Field::Backend,
                    // The Submit line is a button: it starts the run (a missing prompt is said on the form) and moves no focus. A second
                    // press before the form is drawn again (a double click) starts nothing more.
                    BOX_SUBMIT if self.composer.submitting => return vec![],
                    BOX_SUBMIT => return self.submit(),
                    _ => return vec![],
                };
                self.composer.focus = field;
            }
            // The search bar owns every key while it is open.
            Screen::Dashboard if self.mode == Mode::Browse && !self.search.is_active() => {
                self.mode = Mode::Typing;
            }
            _ => {}
        }
        vec![]
    }

    /// A press on a row of a list. The Runs list selects the run shown at `index`; the composer's list picks that model.
    fn select_row(&mut self, list: ListId, index: usize) {
        if list == TAGS_LIST && self.tags.open {
            self.tags_select(index);
        } else if list == RUNS_LIST && self.screen == Screen::Runs {
            if let Some(&i) = self.runs_visible().get(index) {
                self.selected = i;
            }
        } else if list == MODEL_LIST && self.screen == Screen::Composer {
            let selectable = self
                .direct
                .models
                .models()
                .get(index)
                .is_some_and(|m| m.selectable());
            if selectable {
                let p = &mut self.composer.direct;
                if p.index != index {
                    p.source = PickSource::Here;
                }
                p.index = index;
                p.saved = None;
                p.fallback = false;
                self.composer.focus = Field::Model;
            }
        }
    }

    /// The wheel: `lines` toward newer text (positive) or older. A list moves its selection one row a notch; the conversation scrolls.
    fn wheel_lines(&mut self, lines: i32) -> Vec<Effect> {
        if self.tags.open {
            self.tags_wheel(lines);
            return vec![];
        }
        match self.screen {
            // A notch carries its lines (the step of the wheel, faster for a quick run of notches): a list of hundreds of runs would take
            // a day to cross at one run a notch.
            Screen::Runs => self.runs_move(isize::try_from(lines).unwrap_or(0)),
            Screen::Artifacts => {
                self.artifacts_key(KeyEvent::new(
                    if lines > 0 {
                        KeyCode::Down
                    } else {
                        KeyCode::Up
                    },
                    KeyModifiers::NONE,
                ));
            }
            Screen::Dashboard => self.chat.get_mut().scroll_lines(lines),
            Screen::Settings => self
                .settings
                .step(isize::try_from(lines.signum()).unwrap_or(0)),
            Screen::Composer | Screen::Models | Screen::Reply => {}
        }
        vec![]
    }

    /// The pointer lost its place: the window lost focus.
    pub fn focus_lost(&mut self) {
        self.pointer.reset();
        self.set_focus(false);
    }

    /// The mouse of this session: whether the terminal reports it, and `hint`, the one line on selecting text with the terminal itself, shown on the
    /// first screen until the first key or click (a session with the mouse off has none).
    pub fn set_mouse(&mut self, state: MouseState, hint: Option<String>) {
        self.mouse = state;
        self.mouse_hint = hint.filter(|_| state.on);
    }

    /// The directory `C` clones into, as `ham clone <run>` does: the working directory `ham` was started in.
    /// Tells the app who holds a run (the plane's run map) and which backend the composer starts on.
    pub fn set_backends(&mut self, owner: OwnerLookup, pick: Option<String>) {
        self.backend_owner = Some(owner);
        self.backend_pick = pick;
    }

    /// Tells the app which backends the client holds and whether each is declared safe for customer content.
    pub fn set_declared(&mut self, declared: Vec<(String, bool)>) {
        self.declared = declared;
    }

    /// The backend of a run, for the BACKEND column.
    pub(crate) fn backend_of(&self, run_id: &str) -> Option<String> {
        self.backend_owner.as_ref().and_then(|o| (o.0)(run_id))
    }

    /// The backends the composer offers, with whether each is declared safe for customer content: empty for a client with one backend.
    pub(crate) fn backend_choices(&self) -> Vec<(String, bool)> {
        self.backend_view
            .as_ref()
            .filter(|v| v.shows_backend_column())
            .map(|v| {
                v.backends
                    .iter()
                    .map(|b| (b.alias.clone(), b.customer_content))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The backend the composer starts on, if it is one of the choices.
    pub(crate) fn backend_picked(&self) -> Option<String> {
        let pick = self.backend_pick.as_ref()?;
        self.backend_choices()
            .iter()
            .any(|(a, _)| a == pick)
            .then(|| pick.clone())
    }

    /// One plain line for each backend that did not answer last.
    pub(crate) fn backend_banners(&self) -> Vec<String> {
        self.backend_view
            .as_ref()
            .map(ham_core::backends::BackendView::banners)
            .unwrap_or_default()
    }

    /// Moves the composer's backend pick one place; it goes round.
    fn pick_backend(&mut self, forward: bool) {
        let choices = self.backend_choices();
        if choices.is_empty() {
            return;
        }
        let at = self
            .backend_picked()
            .and_then(|p| choices.iter().position(|(a, _)| *a == p));
        let n = choices.len();
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.backend_pick = Some(choices[next].0.clone());
        self.composer.error = None;
    }

    pub fn set_clone_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.clone_dir = dir;
    }

    /// The terminal notification option (`HAMH_NOTIFY`): a run that starts to wait while the window is not focused is announced with OSC 9.
    pub fn set_notify(&mut self, on: bool) {
        self.notify_on = on;
        self.alerts.set_notify(on);
    }

    /// The bell option (`ui.bell`): whether a question on the run screen, or a run that starts to wait in the Runs list, rings the terminal
    /// bell. The notification does not depend on it.
    pub fn set_bell(&mut self, on: bool) {
        self.bell_on = on;
        self.alerts.set_bell(on);
    }

    /// The ham mascot (`ui.mascot`, `HAMH_NO_MASCOT`): whether the title band may be drawn (on a terminal more than 30 rows tall and at least 72
    /// columns wide), whether the empty Runs table shows its picture, and the look. It takes effect at the next draw.
    pub fn set_head(&mut self, head: Head) {
        self.head = head;
    }

    /// The mascot as `ui.mascot` and `HAMH_NO_MASCOT` ask for it, with `env_look` the look the environment asks for.
    pub fn set_mascot(
        &mut self,
        mode: ham_core::config::MascotMode,
        env_look: super::mascot::Look,
    ) {
        self.env_look = env_look;
        self.head = Head::for_mode(mode, env_look);
    }

    /// The most bytes one copy to the clipboard carries (`ui.copy_limit_kib`).
    pub fn copy_limit(&self) -> usize {
        self.copy_limit
    }

    /// The copy limit in bytes (`ui.copy_limit_kib`); it applies to the next copy.
    pub fn set_copy_limit(&mut self, bytes: usize) {
        self.copy_limit = bytes;
    }

    /// Whether the smoke under the mascot's feet moves (`ui.mascot_motion`).
    pub fn mascot_motion(&self) -> bool {
        self.mascot_motion
    }

    /// The smoke switch (`ui.mascot_motion`). It takes effect at the next draw, and the smoke starts again from the still picture.
    pub fn set_mascot_motion(&mut self, on: bool) {
        self.mascot_motion = on;
        self.mascot_ticks = 0;
    }

    /// The frame of the mascot that the screens draw, in the title band and in the empty Runs table: 0, the still picture, unless the smoke
    /// moves; then the number of ticks counted since the mascot appeared, one frame for each [`MASCOT_TICKS_PER_FRAME`] ticks, going round the
    /// frames ([`super::mascot::FRAMES`]).
    pub fn mascot_frame(&self) -> usize {
        if !self.mascot_motion {
            return 0;
        }
        usize::try_from(self.mascot_ticks / MASCOT_TICKS_PER_FRAME).unwrap_or(0)
            % super::mascot::FRAMES
    }

    /// One tick of the timer for the smoke. With the title band on (`Head.band`: `ui.mascot` is not `off`) every tick counts, on every screen,
    /// so the smoke of the band and the smoke of the picture move in step; the terminal's size is not known here, and a terminal too small for
    /// the band or the picture draws no smoke, which costs nothing. Without the band a tick counts while the mascot's picture is in the empty
    /// table of the Runs screen and moves, else the count goes back to 0. The picture is there when the service has answered with no runs and
    /// no error, the Runs screen is up, and `ui.mascot` has not hidden it. This adds no timer and no redraw: the loop redraws on every tick
    /// already.
    fn mascot_tick(&mut self) {
        let picture = self.screen == Screen::Runs
            && self.head.empty_table
            && self.runs_answered
            && self.runs.is_empty()
            && self.runs_error.is_none();
        let showing = self.mascot_motion && (self.head.band || picture);
        self.mascot_ticks = if showing {
            self.mascot_ticks.wrapping_add(1)
        } else {
            0
        };
    }

    /// What the app was told about the mascot.
    pub fn head(&self) -> Head {
        self.head
    }

    /// The terminal reported focus (`FocusGained`, `FocusLost`).
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.alerts.set_focus(focused);
    }

    /// The terminal reported that the window has focus again (`FocusGained`), or the process was continued after a stop. A window that was
    /// covered or switched away from can have lost cells that `ham` still believes are on the screen, and ratatui writes only the cells
    /// that changed, so the screen is repainted whole at the next draw. Losing focus repaints nothing.
    pub fn focus_gained(&mut self) {
        self.set_focus(true);
        self.request_repaint();
    }

    /// Asks the event loop for a full repaint at the next draw: the screen is cleared and every cell is written again.
    pub fn request_repaint(&mut self) {
        self.repaint = true;
    }

    /// Whether a full repaint was asked for since the last call; asking again is needed for the next one.
    pub fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.repaint)
    }

    /// The terminal is tmux: the notification is wrapped to pass through.
    pub fn set_tmux(&mut self, on: bool) {
        self.tmux = on;
        self.alerts.set_tmux(on);
    }

    /// The app's clock in milliseconds: it moves one second at each tick.
    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.as_millis()).unwrap_or(u64::MAX)
    }

    /// Seconds of the run so far, or all of it once it ended: from the first event to `run.completed` (or now).
    pub(crate) fn elapsed(&self) -> Option<u64> {
        let started = self.run_started.or_else(|| {
            self.detail
                .as_ref()
                .and_then(|d| parse_utc_unix(&d.summary.created_at))
        });
        elapsed_s(started, self.run_ended, self.now?)
    }

    /// Seconds since the last good read of the run on screen.
    pub(crate) fn updated_ago(&self) -> Option<u64> {
        self.link.age_s(self.now_ms())
    }

    /// The overlay's words while polling fails past the grace.
    pub(crate) fn link_overlay(&self) -> Option<String> {
        self.link.overlay(self.now_ms())
    }

    /// The `turn.ended` events read: the totals of the header are as of this turn.
    pub(crate) fn turns(&self) -> usize {
        self.turn_ids.len()
    }

    /// The alerts for runs that began to wait: one bell for the batch, and the notification for each when the option is on and the window
    /// is not focused. The first call only remembers which runs wait. A run counts as waiting for one question, so it rings again when it asks
    /// a new one (a new request id), and a question that rang on the run's own screen rings nothing here: the bell of a batch goes
    /// to the first run that has not rung yet.
    fn alert_effects(&mut self) -> Vec<Effect> {
        // `Alerts` is given `run|request` for each run that waits, so a run that asks again with a new request id is a new wait.
        let waiting: Vec<String> = self
            .runs
            .iter()
            .filter(|r| !r.status.is_terminal())
            .filter_map(|r| {
                r.needs_input
                    .as_ref()
                    .map(|n| format!("{}|{}", r.run_id, n.request_id))
            })
            .collect();
        let refs: Vec<&str> = waiting.iter().map(String::as_str).collect();
        if !self.alerts_seeded {
            self.alerts_seeded = true;
            self.alerts.seed(&refs);
            return vec![];
        }
        let mut effects = Vec::new();
        let alerts = self.alerts.observe(&refs);
        let mut bell_due = alerts.iter().any(|a| a.bell);
        for alert in alerts {
            let (run, request) = alert
                .run_id
                .split_once('|')
                .unwrap_or((alert.run_id.as_str(), ""));
            if !self.rung.list_rings(run, request) {
                continue;
            }
            let run_id = run.to_string();
            if std::mem::take(&mut bell_due) {
                effects.push(Effect::Bell);
            }
            if alert.notify {
                effects.push(Effect::Notify(Alert {
                    run_id,
                    bell: false,
                    ..alert
                }));
            }
        }
        effects
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> Vec<Effect> {
        self.handle_mouse_at(event, Instant::now())
    }

    /// The regions of the last draw.
    pub fn hit_map(&self) -> std::cell::Ref<'_, HitMap> {
        self.hits.borrow()
    }

    /// The terminal changed size: a click before the next draw hits nothing.
    pub fn resized(&mut self) {
        self.hits.get_mut().resized();
    }

    /// The wheel step and acceleration (the `wheel` setting).
    pub fn set_wheel(&mut self, wheel: WheelSettings) {
        self.pointer = Pointer::new(wheel);
    }

    /// [`App::handle_key`] for a key that arrived at `at`: keys that follow each other within the burst gap, or soon after a paste, are
    /// not calm and cannot send, confirm or run a command (see `burst.rs`).
    pub fn handle_key_at(&mut self, key: KeyEvent, at: Instant) -> Vec<Effect> {
        self.key_at(key, Some(at))
    }

    fn key_at(&mut self, key: KeyEvent, at: Option<Instant>) -> Vec<Effect> {
        // Two clicks with a key between them are not a double click.
        self.pointer.note_key();
        if key.kind == KeyEventKind::Release {
            return vec![];
        }
        // Ctrl-L repaints the screen on every screen and in every mode, whatever else the key does there (in the message box of a prepared
        // follow-up it also adds the engine's last message): the person presses it when the terminal shows damage.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
            self.request_repaint();
        }
        // The first key takes the hint on selecting text away.
        self.mouse_hint = None;
        // A quit warning holds for the next key only.
        self.was_armed = std::mem::take(&mut self.quit_armed);
        self.calm = at.is_none_or(|t| self.burst.key(t));
        self.key_ms = u64::try_from(
            at.unwrap_or_else(Instant::now)
                .saturating_duration_since(self.epoch)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        // A preview lives on the run screen only.
        if self.screen != Screen::Dashboard {
            self.armed = None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.ctrl_c();
        }
        if let Some(effects) = self.ext_key(&key) {
            return effects;
        }
        match self.screen {
            Screen::Runs => self.runs_key(key),
            Screen::Composer => self.composer_key(key),
            Screen::Dashboard => match self.mode {
                Mode::Browse => self.browse_key(key),
                Mode::Typing => self.typing_key(key),
            },
            Screen::Artifacts => self.artifacts_key(key),
            Screen::Models => self.models_key(key),
            Screen::Reply => self.reply_key(key),
            Screen::Settings => self.settings_key(key),
        }
    }

    /// [`App::handle_paste`] for a paste that arrived at `at`: the keys right after it cannot send or confirm.
    pub fn handle_paste_at(&mut self, text: String, at: Instant) -> Vec<Effect> {
        self.burst.paste(at);
        self.handle_paste(text)
    }

    /// Text pasted into the terminal, delivered whole by bracketed paste (`Event::Paste`). It is text, never keys: on a screen with no
    /// text box it does nothing but say so (a paste of `cc` used to cancel the run, `q` to quit, `ff` to finish it), and a pending
    /// `c` or `f` is dropped so a paste cannot stand between two typed keys. In a text box it inserts the cleaned text: line breaks
    /// kept as `\n`, escape sequences and control characters removed, tabs written as four spaces.
    pub fn handle_paste(&mut self, text: String) -> Vec<Effect> {
        let text = clean_paste(&text);
        self.quit_armed = false;
        // A preview that waits for an argument takes the paste into its slot (a URL is pasted more often than typed).
        if self.screen == Screen::Dashboard
            && self.mode == Mode::Typing
            && let Some(armed) = self.armed.as_mut()
            && armed.slot_paste(&text)
        {
            return vec![];
        }
        // The text changes under a preview: it is cancelled.
        self.armed = None;
        if self.tags.open {
            self.tags_paste(&text);
            return vec![];
        }
        match self.screen {
            Screen::Composer => self.composer_paste(&text),
            Screen::Reply => self.reply_paste(&text),
            Screen::Runs if self.runs_editing => {
                // The filter line is one line: breaks become blanks.
                let one_line = text.replace(['\n', '\r'], " ");
                let joined = format!("{}{one_line}", self.runs_filter.text());
                self.runs_set_text(&joined);
            }
            Screen::Runs => {
                self.runs_notice = Some(PASTE_IGNORED.to_string());
            }
            Screen::Dashboard => {
                self.confirm_finish = false;
                if self.mode == Mode::Typing {
                    // A long paste goes in as a `[Pasted n lines]` token and its text is kept aside.
                    let shown = match self.pastes_mut().map(|p| p.offer(&text)) {
                        Some(Pasted::Placeholder(token)) => token,
                        Some(Pasted::Inline(t)) => t,
                        None => text.clone(),
                    };
                    if let Some(b) = self.chat_mut() {
                        b.insert_str(&shown);
                        // Text that was pasted never runs as a command without its preview. A paste is an edit: it ends the walk
                        // through the history and the wait for a full prompt, as typing does.
                        if !text.is_empty() {
                            self.taint();
                            self.history.cancel();
                        }
                    }
                } else {
                    self.notice = Some(PASTE_IGNORED_BROWSING.to_string());
                }
            }
            Screen::Artifacts | Screen::Models => {
                self.confirm_finish = false;
            }
            Screen::Settings => {
                if !self.settings.paste(&text) {
                    self.settings.say(PASTE_IGNORED);
                }
            }
        }
        vec![]
    }

    fn composer_paste(&mut self, text: &str) {
        let c = &mut self.composer;
        c.notice = None;
        match c.focus {
            Field::Prompt => c.prompt.insert_str(text),
            // The one-line boxes take no line breaks and the repository and base ref no white space, so those are dropped.
            Field::Repo => c.repo.insert_str(text),
            Field::Base => c.base.insert_str(text),
            Field::Options => c.options.insert_str(text),
            Field::Target | Field::Backend | Field::Model | Field::Submit => {
                c.notice = Some(
                    "paste ignored: this field takes keys; paste into the Prompt, Repository or Base ref box"
                        .to_string(),
                );
            }
        }
    }

    fn reply_paste(&mut self, text: &str) {
        if let Some(r) = self.reply.as_mut()
            && !r.sending
            && r.closed.is_none()
        {
            r.text.push_str(text);
            r.error = None;
        }
    }

    /// Ctrl-C: with text in the focused box it clears the box and stays; otherwise it is a request to quit.
    fn ctrl_c(&mut self) -> Vec<Effect> {
        if self.clear_focused_box() {
            return vec![];
        }
        self.quit_request()
    }

    /// Empties the text box that has the keyboard, when it holds text; true when it did.
    fn clear_focused_box(&mut self) -> bool {
        match self.screen {
            Screen::Composer => {
                let c = &mut self.composer;
                let b = match c.focus {
                    Field::Prompt => &mut c.prompt,
                    Field::Repo => &mut c.repo,
                    Field::Base => &mut c.base,
                    Field::Options => &mut c.options,
                    Field::Target | Field::Backend | Field::Model | Field::Submit => return false,
                };
                if b.is_empty() {
                    return false;
                }
                b.clear();
                true
            }
            Screen::Reply => match self.reply.as_mut() {
                Some(r) if !r.text.is_empty() && !r.sending && r.closed.is_none() => {
                    r.text.clear();
                    r.error = None;
                    true
                }
                _ => false,
            },
            Screen::Settings => self.settings.clear_box(),
            Screen::Dashboard if self.mode == Mode::Typing => {
                let cleared = match self.chat_mut() {
                    Some(b) if !b.is_empty() => {
                        b.clear();
                        true
                    }
                    _ => false,
                };
                if cleared {
                    self.closed_banner = None;
                }
                if cleared {
                    self.armed = None;
                    self.untaint();
                }
                cleared
            }
            _ => false,
        }
    }

    /// Text typed and not sent anywhere: the composer's boxes (the repository only when it differs from the prefilled one) and an open
    /// answer.
    fn has_unsent_text(&self) -> bool {
        let c = &self.composer;
        let prefilled = self.default_repo.as_deref().unwrap_or_default();
        let repo = c.repo.text();
        !c.prompt.is_empty()
            || !c.base.is_empty()
            || !c.options.is_empty()
            || (!repo.is_empty() && repo != prefilled)
            || self.drafts.values().any(|b| !b.text().trim().is_empty())
            || self
                .reply
                .as_ref()
                .is_some_and(|r| !r.text.trim().is_empty() && !r.sending && r.closed.is_none())
    }

    /// Quit, unless text is unsent: then the first request only says so, and the next key may quit.
    fn quit_request(&mut self) -> Vec<Effect> {
        if self.has_unsent_text() && !self.was_armed {
            self.quit_armed = true;
            self.say(UNSENT_NOTICE);
            return vec![];
        }
        vec![Effect::Quit]
    }

    /// One notch of the mouse wheel. A terminal that sends mouse reports no longer turns the wheel into arrow keys, so the app does: on
    /// the Runs and Artifacts lists it moves the selection one row, on the dashboard it scrolls three lines, and on any other screen it
    /// does nothing (an arrow key there would edit or choose).
    pub fn handle_wheel(&mut self, down: bool) -> Vec<Effect> {
        if self.tags.open {
            self.tags_wheel(if down { 1 } else { -1 });
            return vec![];
        }
        match self.screen {
            Screen::Runs => {
                self.runs_key(KeyEvent::new(
                    if down { KeyCode::Down } else { KeyCode::Up },
                    KeyModifiers::NONE,
                ));
            }
            Screen::Artifacts => {
                self.artifacts_key(KeyEvent::new(
                    if down { KeyCode::Down } else { KeyCode::Up },
                    KeyModifiers::NONE,
                ));
            }
            Screen::Dashboard => {
                for _ in 0..3 {
                    self.chat.get_mut().scroll(if down {
                        ScrollAction::LineDown
                    } else {
                        ScrollAction::LineUp
                    });
                }
            }
            Screen::Settings => self.settings.step(if down { 1 } else { -1 }),
            Screen::Composer | Screen::Models | Screen::Reply => {}
        }
        vec![]
    }

    /// Opens the composer. What was typed stays (Esc leaves the draft in place for the next `n`; a started run empties it), and the
    /// model goes back to the user's default: a pick never persists by itself.
    fn open_composer(&mut self) -> Vec<Effect> {
        self.screen = Screen::Composer;
        // The composer lists the compiled-in models: nothing to load.
        let c = &mut self.composer;
        c.direct = initial_pick(&self.direct);
        c.focus = Field::Prompt;
        c.error = None;
        c.notice = None;
        self.composer.engine = self.initial_engine_pick();
        vec![]
    }

    /// Tells the screen the `default_engine` of the config file, read at the start. A word that is no engine is ignored here; the start refuses it
    /// with the layer named.
    pub fn set_default_engine(&mut self, word: Option<&str>) {
        self.file_engine = word.map(str::to_string);
    }

    /// The engine a new run would start on: the session's choice, else `default_engine` of the file, else the default engine.
    pub(crate) fn new_run_engine(&self) -> &'static ham_core::engines::Engine {
        self.session_engine
            .and_then(ham_core::engines::find)
            .or_else(|| {
                ham_core::engines::resolve(None, self.file_engine.as_deref())
                    .ok()
                    .map(|(e, _)| e)
            })
            .unwrap_or_else(ham_core::engines::default_engine)
    }

    fn initial_engine_pick(&self) -> EnginePick {
        EnginePick {
            id: self.new_run_engine().id,
            chosen: self.session_engine.is_some(),
        }
    }

    /// Moves the composer's engine to the next (or previous) engine a person can choose, wrapping at the ends.
    fn pick_engine(&mut self, forward: bool) {
        let all = ham_core::engines::available();
        let n = all.len();
        if n < 2 {
            return;
        }
        let i = all
            .iter()
            .position(|e| e.id == self.composer.engine.id)
            .unwrap_or(0);
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.composer.engine = EnginePick {
            id: all[j].id,
            chosen: true,
        };
    }

    /// `/engine [<name>]`: with no name, the engines a person can choose and the one new runs start on; with a name, the engine the runs this
    /// session starts use (the config file is not touched; `default_engine` there sets it for good).
    fn engine_word(&mut self, args: &[String]) -> Vec<Effect> {
        self.clear_chat();
        let wanted = match args {
            [] => None,
            [word] => match ham_core::engines::choose(word) {
                Ok(e) => Some(e),
                Err(e) => {
                    self.panel = Some(Panel::not_run(
                        "ham /engine",
                        &format!("{e}\nusage: /engine [<name>]"),
                    ));
                    return vec![];
                }
            },
            _ => {
                self.panel = Some(Panel::not_run(
                    "ham /engine",
                    &format!(
                        "usage: /engine [<name>]\n/engine chooses the engine for the runs you start in this session: {}.",
                        ham_core::engines::choices_text()
                    ),
                ));
                return vec![];
            }
        };
        if let Some(e) = wanted {
            self.session_engine = Some(e.id);
        }
        let now = self.new_run_engine();
        let mut text = format!(
            "New runs start on {} ({}).\n/engine <name> chooses another for this session: {}.\ndefault_engine in the config file sets it for good.",
            now.name,
            if self.session_engine.is_some() {
                "your choice for this session"
            } else {
                "the default"
            },
            ham_core::engines::choices_text()
        );
        if wanted.is_some() && self.screen == Screen::Composer {
            self.composer.engine = self.initial_engine_pick();
            text.push_str("\nThe New run screen shows it now.");
        }
        self.panel = Some(Panel::info("Engine", &text));
        vec![]
    }

    /// The indices into [`App::runs`] of the runs the filter shows, in the list's order.
    pub(crate) fn runs_visible(&self) -> Vec<usize> {
        self.runs_filter.visible(&self.runs_rows)
    }

    /// Lays a new list over the held one: probe runs are left out, the rows are made again, and the selection stays on its run when it is
    /// still shown, else on the nearest place of the narrowed list.
    fn set_runs(&mut self, runs: Vec<RunSummary>) {
        self.runs_answered = true;
        let before = self.runs_visible();
        let prev_pos = before.iter().position(|&i| i == self.selected).unwrap_or(0);
        let prev_id = self.runs.get(self.selected).map(|r| r.run_id.clone());
        self.runs = runs.into_iter().filter(|r| !r.is_probe()).collect();
        self.rebuild_rows();
        self.reselect(prev_id.as_deref(), prev_pos);
        // The titles of the runs are the prompts of earlier runs in the box's history.
        self.earlier_feed();
    }

    /// The rows of the Runs screen made again from the runs held and what the run index holds about them (the title, the model, the name and
    /// its source, the effective tags). The tag explorer follows the new rows when it is open.
    pub(crate) fn rebuild_rows(&mut self) {
        self.runs_rows = self
            .runs
            .iter()
            .map(|s| {
                let held = self.run_index.get(&s.run_id);
                RunRow::from_summary(s).with_index(
                    held.and_then(|e| e.title.as_deref()),
                    held.and_then(|e| e.model.as_deref()),
                    held.and_then(|e| e.name.as_deref()),
                    held.and_then(|e| e.name_source.as_deref()),
                    held.and_then(|e| e.tags.as_deref()),
                )
            })
            .collect();
        self.tags_rows_changed();
    }

    /// Puts the selection on `prev_id` if it is shown, else at `prev_pos` (kept inside the shown list); on the first run when none is shown.
    fn reselect(&mut self, prev_id: Option<&str>, prev_pos: usize) {
        let visible = self.runs_visible();
        self.selected = match select_after(&self.runs_rows, &visible, prev_id, prev_pos) {
            Some(pos) => visible[pos],
            None => 0,
        };
    }

    /// Changes the filter's words or scope: the selection stays on its run when that run is still shown, else it keeps its place in the
    /// new list.
    fn change_runs_filter(&mut self, change: impl FnOnce(&mut RunsFilter)) {
        let before = self.runs_visible();
        let prev_pos = before.iter().position(|&i| i == self.selected).unwrap_or(0);
        let prev_id = self.runs.get(self.selected).map(|r| r.run_id.clone());
        change(&mut self.runs_filter);
        self.reselect(prev_id.as_deref(), prev_pos);
    }

    /// Moves the selection by `by` shown runs (negative goes up); the first and last stop it.
    fn runs_move(&mut self, by: isize) {
        let visible = self.runs_visible();
        if visible.is_empty() {
            return;
        }
        let at = visible
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        let to = at.saturating_add_signed(by).min(visible.len() - 1);
        self.selected = visible[to];
    }

    /// The first row the Runs table draws for a window of `rows` rows over `visible`, the list the filter shows: the offset of the last draw,
    /// moved the least that brings the selected run into the window and kept inside the list (`runs_view::follow`). The draw calls it, and
    /// so leaves the offset and the height of its window for the page keys.
    pub(crate) fn runs_window_first(&self, visible: &[usize], rows: usize) -> usize {
        let at = visible
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        let first = runs_view::follow(self.runs_first.get(), at, visible.len(), rows);
        self.runs_first.set(first);
        self.runs_window.set(rows);
        first
    }

    /// PageDown and PageUp: the selection and the first row of the window move together by a window less one row, and stop at the ends.
    fn runs_page(&mut self, down: bool) {
        let visible = self.runs_visible();
        if visible.is_empty() {
            return;
        }
        let at = visible
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        let (first, to) = runs_view::page(
            self.runs_first.get(),
            at,
            visible.len(),
            self.runs_window.get(),
            down,
        );
        self.runs_first.set(first);
        self.selected = visible[to];
    }

    /// Home and `g` select the first shown run, End and `G` the last; the next draw brings it into view.
    fn runs_edge(&mut self, last: bool) {
        let visible = self.runs_visible();
        let edge = if last {
            visible.last()
        } else {
            visible.first()
        };
        if let Some(&i) = edge {
            self.selected = i;
        }
    }

    fn runs_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.runs_notice = None;
        self.runs_detail = None;
        if self.runs_editing {
            return self.runs_find_key(key);
        }
        if let Some(ask) = self.resume_ask.take() {
            return self.resume_answer(ask, &key);
        }
        let held = KeyModifiers::CONTROL | KeyModifiers::ALT;
        match key.code {
            KeyCode::Char('q') => self.quit_request(),
            // A letter with Ctrl or Alt held is not the letter. Shift is how a terminal reports a capital.
            KeyCode::Char('R') if !key.modifiers.intersects(held) => self.resume_all_ask(),
            KeyCode::Char('n') => self.open_composer(),
            KeyCode::Char('r') => vec![Effect::LoadRuns],
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('t') => self.tags_open(),
            KeyCode::Char('/') => {
                self.runs_editing = true;
                vec![]
            }
            KeyCode::Char('p') => {
                let focused = self
                    .runs_visible()
                    .contains(&self.selected)
                    .then(|| self.runs_rows.get(self.selected))
                    .flatten()
                    .cloned();
                self.change_runs_filter(|f| f.toggle_scope(focused.as_ref()));
                vec![]
            }
            KeyCode::Esc if self.runs_filter.is_narrowed() => {
                self.runs_clear_filter();
                vec![]
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.runs_move(-1);
                vec![]
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.runs_move(1);
                vec![]
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                self.runs_page(key.code == KeyCode::PageDown);
                vec![]
            }
            // A letter with Ctrl or Alt held is not the letter: Ctrl-G stays what it was on this screen, nothing.
            KeyCode::Home | KeyCode::Char('g') if !key.modifiers.intersects(held) => {
                self.runs_edge(false);
                vec![]
            }
            KeyCode::End | KeyCode::Char('G') if !key.modifiers.intersects(held) => {
                self.runs_edge(true);
                vec![]
            }
            KeyCode::Enter => self.open_selected_run(),
            _ => vec![],
        }
    }

    /// `R` on the Runs screen: the plan for the runs read, and the one line that asks. Nothing starts until Enter or `R` again.
    fn resume_all_ask(&mut self) -> Vec<Effect> {
        let plan = ham_core::resume::plan(&self.runs);
        match ham_core::resume::confirm_line(&plan) {
            None => {
                self.runs_notice = Some("no paused or stalled run to resume".to_string());
            }
            Some(line) => {
                self.runs_notice = Some(line);
                if !plan.resume.is_empty() {
                    self.resume_ask = Some(ResumeAsk {
                        plan,
                        shown_ms: self.key_ms,
                    });
                }
            }
        }
        vec![]
    }

    /// A key while the question of `R` waits. Enter or `R`, 300 ms or more after the question was shown, starts the follow-up runs (an earlier
    /// one is ignored and the question stays); Esc withdraws it; any other key withdraws it and is the screen's own.
    fn resume_answer(&mut self, ask: ResumeAsk, key: &KeyEvent) -> Vec<Effect> {
        let chord = keys::chord(key);
        let confirms =
            chord.mods.is_empty() && matches!(chord.code, KeyCode::Enter | KeyCode::Char('R'));
        if confirms {
            if !self.calm
                || self.key_ms.saturating_sub(ask.shown_ms) < ham_core::router::CONFIRM_DELAY_MS
            {
                self.runs_notice = ham_core::resume::confirm_line(&ask.plan);
                self.resume_ask = Some(ask);
                return vec![];
            }
            self.resume_skipped = ask.plan.skipped.clone();
            self.runs_notice = Some(format!(
                "starting {} follow-up run{}",
                ask.plan.resume.len(),
                if ask.plan.resume.len() == 1 { "" } else { "s" }
            ));
            return vec![Effect::Ext(Job::ResumeRuns {
                run_ids: ask.plan.resume,
            })];
        }
        if key.code == KeyCode::Esc {
            return vec![];
        }
        self.runs_key(*key)
    }

    /// The answer to [`Job::ResumeRuns`]: one short line on the Runs screen that says what each run did, and a fresh read of the list, which shows
    /// the new runs with the mark of a run that continues another. The lines of provider data sharing (`notes`, the new run and its line) and
    /// the reason each run did not start go whole in the panel above it: the lines first, because the account may have changed, then
    /// each reason once with the runs it stopped. A run that did not start keeps its reason as a card; one that started loses that card.
    pub(crate) fn resume_done(
        &mut self,
        outcomes: &[ham_core::resume::Outcome],
        notes: &[(String, String)],
    ) -> Vec<Effect> {
        let skipped = std::mem::take(&mut self.resume_skipped);
        let started = outcomes.iter().filter(|o| o.result.is_ok()).count();
        let mut parts: Vec<String> = Vec::new();
        let mut reasons: Vec<(String, Vec<String>)> = Vec::new();
        for o in outcomes {
            let id = short_id(&o.run_id);
            match &o.result {
                Ok(new) => {
                    self.start_refusals.remove(&o.run_id);
                    parts.push(format!("{id} -> {}", short_id(new)));
                }
                Err(why) => {
                    let why = sanitize_line(why);
                    self.start_refusals.insert(
                        o.run_id.clone(),
                        Refusal {
                            what: Refused::Resume,
                            paragraphs: vec![why.clone()],
                        },
                    );
                    parts.push(format!("{id} not started"));
                    match reasons.iter_mut().find(|(w, _)| *w == why) {
                        Some((_, ids)) => ids.push(id),
                        None => reasons.push((why, vec![id])),
                    }
                }
            }
        }
        if !skipped.is_empty() {
            parts.push(format!("{} skipped", skipped.len()));
        }
        let row = format!(
            "resumed {started} of {}: {}",
            outcomes.len(),
            parts.join(" · ")
        );
        let mut paragraphs: Vec<String> = Vec::new();
        for (_, note) in notes {
            if !paragraphs.contains(note) {
                paragraphs.push(note.clone());
            }
        }
        let lines = paragraphs.len();
        paragraphs.extend(
            reasons
                .into_iter()
                .map(|(why, ids)| format!("{} not started: {why}", ids.join(", "))),
        );
        let title = match (lines, paragraphs.len() - lines) {
            (0, 0) => None,
            (_, 0) => Some(SHARING_TITLE),
            (0, _) => Some("not started"),
            _ => Some("resume details"),
        };
        match title {
            Some(title) => self.runs_say_detailed(row, title, paragraphs),
            None => {
                self.runs_detail = None;
                self.runs_notice = Some(row);
            }
        }
        vec![Effect::LoadRuns]
    }

    /// The panel of the Runs notice, while the notice it belongs to is the one shown.
    pub(crate) fn runs_detail_shown(&self) -> Option<&RunsDetail> {
        self.runs_detail
            .as_ref()
            .filter(|d| self.runs_notice.as_deref() == Some(d.row.as_str()))
    }

    /// The Runs notice `row` with its whole text in the panel above it.
    pub(crate) fn runs_say_detailed(&mut self, row: String, title: &str, paragraphs: Vec<String>) {
        self.runs_detail = Some(RunsDetail {
            row: row.clone(),
            title: title.to_string(),
            paragraphs,
        });
        self.runs_notice = Some(row);
    }

    /// The Runs notice shown now keeps `paragraph` whole in the panel above it; on any other screen nothing changes.
    pub(crate) fn runs_detail_add(&mut self, title: &str, paragraph: String) {
        if self.screen != Screen::Runs {
            return;
        }
        if let Some(row) = self.runs_notice.clone() {
            self.runs_say_detailed(row, title, vec![paragraph]);
        }
    }

    /// A retry or a follow-up from `source` that did not start: the run keeps `card` (the line of provider data sharing when the account
    /// was switched first, then the reason) as a card. On the Runs screen the note row says what did not start and the panel above it shows
    /// `card`; elsewhere, when `say` holds, the notice row has the sentence with `why` when it fits [`NOTICE_COLUMNS`], else a short one that
    /// points to the card.
    pub(crate) fn start_refused(
        &mut self,
        source: &str,
        what: Refused,
        card: Vec<String>,
        why: &str,
        say: bool,
    ) {
        self.start_refusals.insert(
            source.to_string(),
            Refusal {
                what,
                paragraphs: card.clone(),
            },
        );
        if self.screen == Screen::Runs {
            self.runs_say_detailed(what.runs_row(source), what.title(), card);
        } else if say {
            let here = self.focused_run() == Some(source);
            self.say(&what.notice(source, why, here));
        }
    }

    /// Opens the selected run, if the filter shows it.
    fn open_selected_run(&mut self) -> Vec<Effect> {
        if !self.runs_visible().contains(&self.selected) {
            return vec![];
        }
        match self.runs.get(self.selected).map(|r| r.run_id.clone()) {
            Some(id) => self.open_run(&id),
            None => vec![],
        }
    }

    /// Esc: the words and the scope go, the list is whole again, and the filter line is closed.
    fn runs_clear_filter(&mut self) {
        self.change_runs_filter(|f| *f = RunsFilter::new());
        self.runs_editing = false;
    }

    /// A key while the filter line is open: every character is its text.
    fn runs_find_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.runs_clear_filter(),
            KeyCode::Enter => {
                self.runs_editing = false;
                if ham_core::router::is_command_attempt(self.runs_filter.text()) {
                    // A command filters nothing; it runs from a run's message box.
                    self.change_runs_filter(|f| *f = RunsFilter::new());
                    self.runs_notice = Some(RUNS_COMMAND_NOTE.to_string());
                }
            }
            KeyCode::Up => self.runs_move(-1),
            KeyCode::Down => self.runs_move(1),
            KeyCode::Backspace => {
                let mut text = self.runs_filter.text().to_string();
                text.pop();
                self.runs_set_text(&text);
            }
            KeyCode::Char('u') if ctrl => self.runs_set_text(""),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let mut text = self.runs_filter.text().to_string();
                text.push(c);
                self.runs_set_text(&text);
            }
            _ => {}
        }
        vec![]
    }

    fn runs_set_text(&mut self, text: &str) {
        let text = sanitize_line(text);
        self.change_runs_filter(|f| f.set_text(&text));
    }

    fn run_effect(&self, make: impl FnOnce(String) -> Effect) -> Vec<Effect> {
        self.focused_run()
            .map(|r| vec![make(r.to_string())])
            .unwrap_or_default()
    }

    /// Browse mode: the dashboard's hotkeys, read from the key table. The burst guard does not hold them back: the live drivers press
    /// `f` twice in two writes, and a paste cannot act as hotkeys because a bracketed paste is text and the run screen's default mode
    /// (the chat view's) is typing, in which no letter is a command.
    fn browse_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(effects) = self.search_key(&key) {
            return effects;
        }
        // The preview of `c` waits for its second key before anything else reads the key.
        if let Some(effects) = self.hotkey_preview_key(&key) {
            return effects;
        }
        if let Some(effects) = self.panel_key(&key) {
            return effects;
        }
        let finishing = std::mem::take(&mut self.confirm_finish);
        let action = keys::lookup(Mode::Browse, &key);
        // A key other than the one that was asked for withdraws the question, and the notice that asked it goes too.
        if finishing && action != Some(Action::Finish) {
            self.clear_notice_starting(ASK_FINISH);
        }
        match action {
            Some(action) => self.browse_action(action, finishing),
            None => vec![],
        }
    }

    /// A key while the preview of a hotkey is up: the same key again confirms it
    /// once the preview is 300 ms old (an earlier one is ignored and the preview stays); Esc withdraws the preview and stays on the run
    /// screen; any other key withdraws it and then acts as itself. In browse mode Alt and a letter is the letter (the key table's rule).
    /// `None` when no hotkey preview is up, or for the key that only withdrew it.
    fn hotkey_preview_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        let gesture = self.armed.as_ref()?.gesture_key()?;
        let same = matches!(key.code, KeyCode::Char(c) if c == gesture)
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::ALT);
        if same {
            let now = self.key_ms;
            return Some(match self.armed.as_mut()?.press(ConfirmKey::Enter, now) {
                ConfirmStep::Run => self.run_hotkey(),
                ConfirmStep::Waiting(_) => vec![],
                ConfirmStep::Cancelled | ConfirmStep::Spent => {
                    self.armed = None;
                    vec![]
                }
            });
        }
        self.armed = None;
        (key.code == KeyCode::Esc).then(Vec::new)
    }

    /// The second key of a hotkey preview was accepted: the command that was previewed runs, for the run it named.
    fn run_hotkey(&mut self) -> Vec<Effect> {
        let Some(armed) = self.armed.take() else {
            return vec![];
        };
        if (self.config_rev)() != armed.rev() {
            self.panel = Some(Panel::not_run(
                &commands::command_title(armed.argv()),
                "The settings changed while the preview was open, so nothing ran. Start it again to see a fresh preview.",
            ));
            return vec![];
        }
        match armed.invocation().map(|i| &i.command) {
            Some(Command::Cancel { run_id }) => vec![Effect::Cancel {
                run_id: run_id.clone(),
            }],
            // `R`: the same command as `/resume --all`; its report is the result panel's lines.
            Some(Command::Resume { .. }) => {
                let (Some(invocation), argv) = (armed.invocation().cloned(), armed.argv().to_vec())
                else {
                    return vec![];
                };
                self.panel = Some(Panel::running(&argv));
                vec![Effect::RunCommand { invocation, argv }]
            }
            _ => vec![],
        }
    }

    /// `R` on a run's screen: the preview of `ham resume --all`, with the count of the runs read; the same key again, 300 ms or more
    /// later, starts the follow-up runs and the panel reports what each did.
    fn resume_key(&mut self) -> Vec<Effect> {
        let plan = ham_core::resume::plan(&self.runs);
        if plan.resume.is_empty() && !self.runs.is_empty() {
            self.say("no paused or stalled run to resume");
            return vec![];
        }
        let outcome = prepare_argv(&["resume".to_string(), "--all".to_string()], false);
        let line = if self.runs.is_empty() {
            "press R again to resume every paused and stalled run".to_string()
        } else {
            format!(
                "press R again to resume {} paused or stalled run{}",
                plan.resume.len(),
                if plan.resume.len() == 1 { "" } else { "s" }
            )
        };
        let gesture = Gesture { key: 'R', line };
        self.armed = Armed::hotkey(&outcome, self.key_ms, (self.config_rev)(), gesture);
        vec![]
    }

    /// `c`: the same prepared command as `/cancel` for the focused run, shown as the preview of that command; the same key again, 300 ms or
    /// more later, cancels the run the preview named.
    fn cancel_key(&mut self) -> Vec<Effect> {
        let Some(run) = self.focused_run().map(str::to_string) else {
            return vec![];
        };
        let outcome = prepare_argv(&["cancel".to_string(), run.clone()], false);
        let gesture = Gesture {
            key: 'c',
            line: format!("{ASK_CANCEL}{run}"),
        };
        self.armed = Armed::hotkey(&outcome, self.key_ms, (self.config_rev)(), gesture);
        vec![]
    }

    /// Drops the notice when it is the one that starts with `prefix`; a notice that something else wrote since stays.
    fn clear_notice_starting(&mut self, prefix: &str) {
        if self
            .notice
            .as_deref()
            .is_some_and(|n| n.starts_with(prefix))
        {
            self.notice = None;
        }
    }

    fn browse_action(&mut self, action: Action, finishing: bool) -> Vec<Effect> {
        match action {
            Action::Quit => self.quit_request(),
            Action::Runs => {
                self.screen = Screen::Runs;
                vec![Effect::LoadRuns]
            }
            Action::NewRun => self.open_composer(),
            Action::ScrollDown => self.scroll_chat(ScrollAction::LineDown),
            Action::ScrollUp => self.scroll_chat(ScrollAction::LineUp),
            Action::PageDown => self.scroll_chat(ScrollAction::PageDown),
            Action::PageUp => self.scroll_chat(ScrollAction::PageUp),
            Action::Top => self.scroll_chat(ScrollAction::Top),
            Action::Bottom => self.scroll_chat(ScrollAction::Bottom),
            Action::SelectNext => {
                self.chat.get_mut().select(Step::Next);
                vec![]
            }
            Action::SelectPrev => {
                self.chat.get_mut().select(Step::Prev);
                vec![]
            }
            Action::ToggleFold => {
                self.chat.get_mut().toggle_selected_fold();
                vec![]
            }
            Action::Artifacts => {
                self.screen = Screen::Artifacts;
                self.artifacts = ArtifactList::Loading;
                self.artifact_download = None;
                self.run_effect(|run_id| Effect::LoadArtifacts { run_id })
            }
            Action::Retry => self.retry_action(),
            Action::ResumeAll => self.resume_key(),
            // A run that ended blocked has nothing to stop; `r` retries it.
            Action::Cancel if self.run_is_blocked() => {
                self.say(ham_core::retry::ENDED);
                vec![]
            }
            Action::Cancel => self.cancel_key(),
            Action::Clone => self.clone_key(),
            Action::Open => self.open_key(),
            Action::Download => self.download_key(),
            Action::Message => self.open_reply(InstructAction::ContinueWithPrompt),
            Action::FreshSession => self.open_reply(InstructAction::NewSessionWithPrompt),
            Action::Model => self.open_models(),
            Action::Finish if finishing => self.finish_run(),
            Action::Finish => self.ask_finish(),
            Action::Copy => self.copy_key(),
            Action::Withdraw => self.send_withdraw_action(),
            Action::ConfirmHeld => self.send_confirm_action(),
            Action::ChooseHeld => self.send_choose_action(),
            Action::OpenAnything => {
                self.openbox_open("");
                vec![]
            }
            Action::StartTyping => {
                self.mode = Mode::Typing;
                vec![]
            }
            _ => vec![],
        }
    }

    /// Typing mode: letters are text. The key table comes first (Enter sends, Ctrl-J and Shift-Enter add a line, Esc leaves for browse
    /// mode); then Alt and a hotkey letter, which crossterm reads when Esc and the letter arrive in one chunk, acts as Esc then the
    /// letter; every other key goes to the box.
    fn typing_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // Ctrl-R is open: the search takes every key; the box keeps its draft until a match is taken.
        if self.hsearch.is_some() {
            return self.hsearch_key(&key);
        }
        // A long result panel is open: PgUp and PgDn scroll it, as its border says. The arrow keys stay with the box (the caret, the history).
        if key.modifiers.is_empty()
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            && self.panel.as_ref().is_some_and(Panel::overflows)
            && let Some(effects) = self.panel_key(&key)
        {
            return effects;
        }
        let before = self.chat_text();
        let effects = if let Some(effects) = self.armed_key(&key) {
            effects
        } else if let Some(effects) = self.chip_key(&key) {
            effects
        } else if let Some(effects) = self.popup_key(&key) {
            effects
        } else {
            self.typing_key_in_box(key)
        };
        // A changed draft starts the popup's marker at the top again, and an edit of a recalled text ends the walk through the history.
        if self.chat_text() != before {
            self.popup_sel = 0;
            if !std::mem::take(&mut self.recalled) {
                self.history.cancel();
            }
        }
        self.recalled = false;
        // Text that arrives as a burst of keys is a paste from a terminal without bracketed paste: the draft is tainted as a paste's is.
        let inserts = match key.code {
            KeyCode::Char(_) => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
            KeyCode::Enter => true,
            _ => false,
        };
        if inserts && !self.calm {
            self.taint();
        }
        // An emptied box ends the taint, and with it the phrases the user dismissed.
        if self.chat_box().is_some_and(ChatBox::is_empty) {
            self.untaint();
            self.chip_dismissed.clear();
        }
        effects
    }

    /// The commands the popup lists for the draft: the built commands whose name or alias starts with the word typed after the slash. The
    /// popup shows only while the person types in the run's box, on one line that is a bare `/word`, and not after Esc closed it at this text.
    pub(crate) fn popup_entries(&self) -> Vec<&'static registry::Entry> {
        if self.screen != Screen::Dashboard
            || self.mode != Mode::Typing
            || self.armed.is_some()
            || self.chip.is_some()
            // A recalled text is not being typed: its Up and Down walk the history and its Esc gives the draft back.
            || self.history.is_browsing()
        {
            return Vec::new();
        }
        let Some(text) = self.chat_text() else {
            return Vec::new();
        };
        let Some(prefix) = text.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace)
            || self.popup_closed.as_deref() == Some(text.as_str())
        {
            return Vec::new();
        }
        self.popup_for(prefix)
    }

    /// Keys while the popup is open: Up and Down move the marker, Tab completes the marked word, Enter completes a command
    /// with arguments and sends one without through the router, Esc closes the popup. Any other key goes to the box.
    fn popup_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        if !key.modifiers.is_empty() {
            return None;
        }
        let entries = self.popup_entries();
        let last = entries.len().checked_sub(1)?;
        self.popup_sel = self.popup_sel.min(last);
        match key.code {
            KeyCode::Up => {
                self.popup_sel = self.popup_sel.saturating_sub(1);
                Some(vec![])
            }
            KeyCode::Down => {
                self.popup_sel = (self.popup_sel + 1).min(last);
                Some(vec![])
            }
            KeyCode::Tab => {
                self.popup_complete(entries[self.popup_sel]);
                Some(vec![])
            }
            // An Enter inside a burst is a line break, never a send: the box handles it. A word that is already whole (`/cancel`, or an
            // alias) has nothing left to complete: Enter sends it, as it did before the popup.
            KeyCode::Enter if self.calm && !self.popup_word_is_whole(&entries) => {
                let entry = entries[self.popup_sel];
                if entry.has_args() {
                    self.popup_complete(entry);
                    Some(vec![])
                } else {
                    self.set_chat_text(&format!("/{}", entry.name));
                    Some(self.send_box())
                }
            }
            KeyCode::Esc => {
                self.popup_closed = self.chat_text();
                Some(vec![])
            }
            _ => None,
        }
    }

    /// The draft is `/word` and `word` is the name or an alias of an entry in the list: the word is complete.
    fn popup_word_is_whole(&self, entries: &[&registry::Entry]) -> bool {
        let text = self.chat_text().unwrap_or_default();
        let word = text.trim_start_matches('/').to_ascii_lowercase();
        entries
            .iter()
            .any(|e| e.name == word || e.aliases.iter().any(|a| *a == word))
    }

    /// Writes `/word` into the box, with a blank after it when the command takes arguments (the caret is then ready for them).
    fn popup_complete(&mut self, entry: &registry::Entry) {
        let blank = if entry.has_args() { " " } else { "" };
        self.set_chat_text(&format!("/{}{blank}", entry.name));
        self.popup_sel = 0;
    }

    fn set_chat_text(&mut self, text: &str) {
        if let Some(b) = self.chat_mut() {
            b.set_text(text);
        }
    }

    /// A typed submission that is exactly a fixed phrase is not sent: the chip asks first. Only the user's own typing gets
    /// one: not a tainted draft (a paste, a recall, an editor's answer), not a command attempt, not a phrase the user dismissed.
    fn offer_chip(&mut self, line: &str) -> Option<Vec<Effect>> {
        if self.draft_is_tainted() || is_command_attempt(line) {
            return None;
        }
        let word = registry::phrase_command(line)?;
        let phrase = registry::normalize_phrase(line)?;
        if self.chip_dismissed.contains(&phrase) {
            return None;
        }
        let run = self.focused_run()?.to_string();
        let entry = registry::subcommand(word)?;
        let mut argv = vec![word.to_string()];
        if entry.run_arg != registry::RunArg::None {
            argv.push(run);
        }
        self.chip = Some(Chip { phrase, argv });
        Some(vec![])
    }

    /// Keys while the chip waits: Enter prepares the command through the router (its preview and second Enter follow), Esc dismisses the
    /// chip and keeps the draft, and any other key edits the draft, which drops the chip.
    fn chip_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        self.chip.as_ref()?;
        match key.code {
            KeyCode::Enter if self.calm => {
                let chip = self.chip.take()?;
                let outcome = self.prepare_chip(&chip.argv);
                Some(self.outcome_effects(outcome))
            }
            KeyCode::Esc => {
                let chip = self.chip.take()?;
                self.chip_dismissed.push(chip.phrase);
                Some(vec![])
            }
            _ => {
                self.chip = None;
                None
            }
        }
    }

    /// The router's reading of the command a chip stands for. `push` lacks its URL, so it is read as the line `ham push` is, and the URL
    /// is asked for in the slot of the preview; every other command goes through [`prepare_argv`] as before.
    fn prepare_chip(&self, argv: &[String]) -> RouteOutcome {
        if argv.first().map(String::as_str) == Some("push") {
            let ctx = RouteContext {
                focused_run: self.focused_run().map(str::to_string),
                tainted: false,
            };
            return route("ham push", &ctx);
        }
        prepare_argv(argv, false)
    }

    fn typing_key_in_box(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(action) = keys::alt_as_browse(&key) {
            self.mode = Mode::Browse;
            return self.browse_action(action, false);
        }
        match keys::lookup(Mode::Typing, &key) {
            Some(Action::Send) => self.enter_in_box(),
            Some(Action::SendLiteral) => self.send_literal(),
            Some(Action::NewLine) => {
                if let Some(b) = self.chat_mut() {
                    b.newline();
                }
                vec![]
            }
            Some(Action::LeaveTyping) => {
                // The first Esc on a recalled text gives the draft back; the next one leaves the box.
                if let Some(draft) = self.history.cancel() {
                    self.recalled = true;
                    if let Some(p) = self.pastes_mut() {
                        p.clear();
                    }
                    self.set_chat_text(&draft.text);
                    if draft.tainted {
                        self.taint();
                    } else {
                        self.untaint();
                    }
                    return vec![];
                }
                self.mode = Mode::Browse;
                vec![]
            }
            Some(Action::ClearOrQuit) => self.ctrl_c(),
            Some(Action::HistorySearch) => {
                let text = self.expanded_chat_text().unwrap_or_default();
                self.hsearch = Some(self.history.search(&text, self.draft_is_tainted()));
                vec![]
            }
            Some(Action::Expand) => self.expand_placeholder(),
            Some(Action::Edit) => self.start_edit(),
            Some(Action::ClineMessage) => self.add_cline_message(),
            Some(Action::Undo) => {
                if let Some(b) = self.chat_mut() {
                    b.undo();
                }
                vec![]
            }
            Some(Action::Redo) => {
                if let Some(b) = self.chat_mut() {
                    b.redo();
                }
                vec![]
            }
            Some(_) => vec![],
            None => {
                // Up on the first row and Down on the last row of the box walk the history; elsewhere they move the caret.
                let plain = key.modifiers.is_empty();
                if plain && key.code == KeyCode::Backspace && self.delete_token(true) {
                    return vec![];
                }
                if plain && key.code == KeyCode::Delete && self.delete_token(false) {
                    return vec![];
                }
                if plain && key.code == KeyCode::Up && self.caret_on_first_row() {
                    let text = self.expanded_chat_text().unwrap_or_default();
                    if let Some(recall) = self.history.prev(&text, self.draft_is_tainted()) {
                        return self.apply_recall(recall);
                    }
                }
                if plain
                    && key.code == KeyCode::Down
                    && self.caret_on_last_row()
                    && let Some(recall) = self.history.next()
                {
                    return self.apply_recall(recall);
                }
                if let Some(b) = self.chat_mut() {
                    b.handle_key(key);
                }
                vec![]
            }
        }
    }

    /// A box that was never typed in has one empty row, and the caret is on it.
    fn caret_on_first_row(&self) -> bool {
        self.chat_box().is_none_or(|b| b.cursor().0 == 0)
    }

    fn caret_on_last_row(&self) -> bool {
        self.chat_box()
            .is_none_or(|b| b.cursor().0 + 1 == b.lines().len())
    }

    /// The history puts `recall` in the box, and asks for the full prompt when the recall holds only the short form of an earlier run's
    /// (`earlier.rs`).
    fn apply_recall(&mut self, recall: Recall) -> Vec<Effect> {
        self.recalled = true;
        self.put_recall(&recall);
        self.earlier_wait(&recall)
    }

    /// `recall` is now the text of the box: tainted as it says (a recalled text never runs a command at once), and the popup, the chip
    /// and the preview of the old text are gone.
    fn put_recall(&mut self, recall: &Recall) {
        // The whole text of the box is replaced by text that holds no token.
        if let Some(p) = self.pastes_mut() {
            p.clear();
        }
        self.set_chat_text(&recall.text);
        self.armed = None;
        self.chip = None;
        self.popup_closed = None;
        if recall.tainted {
            self.taint();
        } else {
            self.untaint();
        }
    }

    /// Keys while Ctrl-R is open: letters and Backspace edit the query, Ctrl-R finds an older match, Enter takes the match into the box
    /// (tainted), Esc gives the draft back.
    fn hsearch_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let held = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                if let Some(search) = self.hsearch.take()
                    && let Some(recall) = self.history.accept_search(search)
                {
                    return self.apply_recall(recall);
                }
            }
            KeyCode::Esc => {
                // The box was not changed while the search was open: the draft is still there.
                self.hsearch = None;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = self.hsearch.as_mut() {
                    s.older();
                }
            }
            KeyCode::Backspace if !held => {
                if let Some(s) = self.hsearch.as_mut() {
                    s.backspace();
                }
            }
            KeyCode::Char(c) if !held => {
                if let Some(s) = self.hsearch.as_mut() {
                    s.type_char(c);
                }
            }
            _ => {}
        }
        vec![]
    }

    /// Enter in the message box. A line that ends in a backslash continues (the backslash goes, a line starts); an Enter inside a burst
    /// is a line break, never a send; otherwise the line is sent.
    fn enter_in_box(&mut self) -> Vec<Effect> {
        let calm = self.calm;
        let Some(b) = self.chat_mut() else {
            return vec![];
        };
        let ends_in_backslash = b.char_before_cursor() == Some('\\');
        if ends_in_backslash {
            b.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        if ends_in_backslash || !calm {
            b.newline();
            return vec![];
        }
        self.send_box()
    }

    /// Alt-Enter: the whole box is sent as chat exactly as typed. No command is read from it, no chip is
    /// offered, the command sink does not see it, and nothing is trimmed: a leading space, a `//` or a second line stay. A pasted block is
    /// put back, as for every send. Inside a burst it is a line break, like Enter, and a box with only blanks sends nothing.
    fn send_literal(&mut self) -> Vec<Effect> {
        if !self.calm {
            if let Some(b) = self.chat_mut() {
                b.newline();
            }
            return vec![];
        }
        let shown = self.chat_text().unwrap_or_default();
        if self
            .pastes_ref()
            .is_some_and(|p| !p.dangling(&shown).is_empty())
        {
            self.say("A pasted block in this message is gone: paste it again before sending.");
            return vec![];
        }
        let raw = self.expanded_chat_text().unwrap_or_default();
        if raw.trim().is_empty() {
            return vec![];
        }
        self.answer_with(&raw)
    }

    /// Sends the message box: the command sink looks at the trimmed line first; what it passes on goes to the router, which
    /// reads the text as typed.
    fn send_box(&mut self) -> Vec<Effect> {
        // The box holds the short form of an earlier run's prompt and the full one is on its way: nothing is sent (`earlier.rs`).
        if self.earlier_blocks_send() {
            return vec![];
        }
        let shown = self.chat_text().unwrap_or_default();
        // A token whose paste is gone (an edit brought its label back) is refused, not sent as plain text.
        if self
            .pastes_ref()
            .is_some_and(|p| !p.dangling(&shown).is_empty())
        {
            self.say("A pasted block in this message is gone: paste it again before sending.");
            return vec![];
        }
        let raw = self.expanded_chat_text().unwrap_or_default();
        let line = raw.trim().to_string();
        if line.is_empty() {
            // The title of a blocked run's box says that Enter retries it, so an empty box is answered too.
            return if self.run_is_blocked() {
                self.send_enter("").unwrap_or_default()
            } else {
                vec![]
            };
        }
        let sink = self.sink;
        match sink(self, &line) {
            Submit::Handled(effects) => {
                self.clear_chat();
                effects
            }
            Submit::Refused(reason) => {
                self.say(&reason);
                vec![]
            }
            Submit::Pass => match self.offer_chip(&line) {
                Some(effects) => effects,
                None => self.route_line(raw.trim_end()),
            },
        }
    }

    /// The router's reading of a sent line, acted on: chat answers the open question; a refusal goes to the result panel and the text
    /// stays; a start form fills the New run form; a word of the screen is handled here; a command runs at once or waits for its
    /// second Enter.
    fn route_line(&mut self, text: &str) -> Vec<Effect> {
        // A `//word` the engine advertises goes to the engine.
        if let Some(effects) = self.forward_engine_command(text) {
            return effects;
        }
        let ctx = RouteContext {
            focused_run: self.focused_run().map(str::to_string),
            tainted: self.draft_is_tainted(),
        };
        let outcome = route(text, &ctx);
        self.outcome_effects(outcome)
    }

    /// What the router's reading of a line (or of a chip's argv) does.
    fn outcome_effects(&mut self, outcome: RouteOutcome) -> Vec<Effect> {
        match outcome {
            RouteOutcome::Empty => vec![],
            RouteOutcome::Message { text, .. } => self.answer_with(&text),
            RouteOutcome::Refused { reason, .. } => {
                self.panel = Some(Panel::not_run("Nothing was sent", &reason));
                vec![]
            }
            RouteOutcome::StartForm { args } => {
                self.clear_chat();
                self.open_composer_with(&args)
            }
            RouteOutcome::Ui { word, args } => self.ui_word(word, &args),
            RouteOutcome::Incomplete { argv, missing, .. } => {
                // The command lacks one argument: its preview has a slot to type it in. Keys queued behind the Enter that
                // opened it are dropped, as for any preview.
                self.armed = Some(Armed::ask(argv, missing, (self.config_rev)()));
                self.flush = true;
                vec![]
            }
            command @ RouteOutcome::Command { .. } => self.command_outcome(&command),
        }
    }

    /// A parsed command: one that changes something waits for its second Enter, the rest run at once. A command that needs the terminal
    /// (`push`, `rules edit`) has a preview as well and, after its second Enter, hands the terminal to a child `ham` ([`Effect::RunTerminal`]).
    /// `update` is the exception: it runs from a shell, so every form of it goes to the runner at once, which answers with the line that says so.
    fn command_outcome(&mut self, outcome: &RouteOutcome) -> Vec<Effect> {
        // The first `config set` of a table key waits for its second Enter with the one-way-door note on the preview.
        let door = commands::door_preview(outcome, &ham_core::envnames::os_env);
        let outcome = door.as_ref().unwrap_or(outcome);
        // A typed follow-up names the target the screen already knows for its run; the line itself is untouched.
        let named = commands::name_continue_target(outcome, &|id| self.known_target(id));
        let outcome = named.as_ref().unwrap_or(outcome);
        let RouteOutcome::Command {
            argv,
            invocation,
            preview,
            ..
        } = outcome
        else {
            return vec![];
        };
        // `ham update` runs from a shell: the box asks for no second Enter before the runner's answer, the line that says so.
        if preview.is_some() && !matches!(invocation.command, Command::Update { .. }) {
            self.armed = Armed::new(outcome, self.key_ms, (self.config_rev)());
            // Keys that were queued behind the Enter that armed the preview must not answer it.
            self.flush = self.armed.is_some();
            return vec![];
        }
        self.execute(invocation, argv)
    }

    /// The target of run `id` when the screen holds its summary: the run on screen, else a row of the run list. A run's target never changes.
    fn known_target(&self, id: &str) -> Option<harness_protocol::target::RunTarget> {
        self.detail
            .as_ref()
            .filter(|d| d.summary.run_id == id)
            .map(|d| d.summary.target.clone())
            .or_else(|| {
                self.runs
                    .iter()
                    .find(|r| r.run_id == id)
                    .map(|r| r.target.clone())
            })
    }

    /// Runs a command that is allowed to run: `watch` opens the run on this screen (the runner refuses it), the rest become
    /// [`Effect::RunCommand`] with a panel that says it is running. The box is emptied: the line was sent.
    fn execute(&mut self, invocation: &Invocation, argv: &[String]) -> Vec<Effect> {
        match &invocation.command {
            // `/engine <name>` chooses the engine of the runs this session starts; nothing is run.
            Command::Engine(EngineCommand {
                engine,
                action: EngineAction::Choose,
            }) => self.engine_word(std::slice::from_ref(engine)),
            // The router put the focused run into the argv when the line named none, so a confirmed `watch` never reads the live focus.
            Command::Watch { run_id } => {
                self.clear_chat();
                self.panel = None;
                match run_id {
                    Some(id) => self.open_from_box(id),
                    None => vec![],
                }
            }
            // The editor and git's prompts need the terminal: the event loop hands it to a child `ham` (the in-process runner has none).
            // `ham update` is never handed over: it waits for runs and gives the terminal to setup for up to an hour, so it runs
            // from a shell, and the in-process runner answers with the line that says so.
            command
                if registry::effects_of(command).contains(registry::Effects::TERMINAL_HANDOFF)
                    && !matches!(command, Command::Update { .. }) =>
            {
                self.clear_chat();
                self.panel = Some(Panel::running(argv));
                vec![Effect::RunTerminal {
                    argv: argv.to_vec(),
                }]
            }
            _ => {
                self.clear_chat();
                self.panel = Some(Panel::running(argv));
                vec![Effect::RunCommand {
                    invocation: invocation.clone(),
                    argv: argv.to_vec(),
                }]
            }
        }
    }

    /// Opens a run from a word typed in the box and stays in typing mode, so the person can go on typing.
    fn open_from_box(&mut self, run_id: &str) -> Vec<Effect> {
        let effects = self.open_run(run_id);
        self.mode = Mode::Typing;
        effects
    }

    /// The words that belong to the screen itself.
    fn ui_word(&mut self, word: &str, args: &[String]) -> Vec<Effect> {
        match word {
            "help" => {
                let text = match args.first() {
                    Some(w) => registry::command_help(w)
                        .unwrap_or_else(|| registry::no_command_text(registry::entries(), w)),
                    None => format!("{}{}", self.engine_help(), registry::help_text()),
                };
                self.clear_chat();
                self.panel = Some(Panel::info("Commands", &text));
                vec![]
            }
            "new" => {
                self.clear_chat();
                self.open_composer()
            }
            // The router put the focused run into `args` when the line named none. A run id opens the run; other words open the search box.
            "open" if args.len() != 1 || !harness_protocol::common::is_ulid(&args[0]) => {
                self.clear_chat();
                self.panel = None;
                self.openbox_open(&args.join(" "));
                vec![]
            }
            "open" => args.first().map_or_else(Vec::new, |id| {
                self.clear_chat();
                self.panel = None;
                self.open_from_box(id)
            }),
            "engines" => {
                self.engines_panel();
                vec![]
            }

            "sessions" => {
                self.clear_chat();
                self.screen = Screen::Runs;
                vec![Effect::LoadRuns]
            }
            "now" => args
                .first()
                .map_or_else(Vec::new, |text| self.send_now(text)),
            "tag" => self.tag_word(args),
            "retry" => self.retry_word(args),
            "copy" => {
                self.clear_chat();
                self.copy_last_answer()
            }
            "export" => self.export_chat(args),
            "mouse" => self.mouse_word(args),
            "settings" => {
                self.clear_chat();
                self.panel = None;
                self.open_settings()
            }
            // A planned word never gets here: the router refuses it.
            _ => vec![],
        }
    }

    /// `/mouse [on|off]`: the mouse reports go on or off for this session, and nothing is written to the config file. The word
    /// alone says where the mouse stands. `HAMH_NO_MOUSE=1` keeps it off whatever is asked.
    fn mouse_word(&mut self, args: &[String]) -> Vec<Effect> {
        let want = match args {
            [] => None,
            [word] if word.eq_ignore_ascii_case("on") => Some(true),
            [word] if word.eq_ignore_ascii_case("off") => Some(false),
            _ => {
                self.panel = Some(Panel::not_run(
                    "ham /mouse",
                    "usage: /mouse [on|off]\n/mouse turns the mouse reports on or off for this session.\nIt does not change the config file; mouse = false there turns them off for good.",
                ));
                return vec![];
            }
        };
        self.clear_chat();
        let state = self.mouse;
        let stands = |on: bool| if on { "on" } else { "off" };
        let Some(want) = want else {
            let mut text = format!(
                "The mouse is {} for this session.\n/mouse on|off changes it for this session only; the config file is not touched.",
                stands(state.on)
            );
            if state.forced_off {
                text.push_str("\nHAMH_NO_MOUSE is set, so the mouse stays off until ham is started without it.");
            }
            self.panel = Some(Panel::info("Mouse", &text));
            return vec![];
        };
        if want && state.forced_off {
            self.panel = Some(Panel::info(
                "Mouse",
                "HAMH_NO_MOUSE is set, so the mouse stays off for this run of ham.\nUnset it and start ham again to use the mouse.",
            ));
            return vec![];
        }
        if want == state.on {
            self.panel = Some(Panel::info(
                "Mouse",
                &format!("The mouse is already {}.", stands(want)),
            ));
            return vec![];
        }
        self.mouse.on = want;
        // A drag that was under way, and a first click that waits for its second, belong to the mouse that is gone.
        self.pointer.reset();
        let text = if want {
            "The mouse is on for this session.\nClick a box to type in it, a run to select it, a fold to open it; the wheel scrolls.\nEvery click has a key. The config file is not touched."
        } else {
            "The mouse is off for this session.\nEvery click has a key (the footer lists them), and your terminal selects text again.\nThe config file is not touched; mouse = false there turns it off for good."
        };
        self.panel = Some(Panel::info("Mouse", text));
        vec![Effect::SetMouse { on: want }]
    }

    /// Copies `text` as the person chose it: cleaned and masked by [`copy::prepare`], then one [`Effect::Copy`]; text that cannot be
    /// copied says why in the notice row.
    pub(crate) fn copy_text(&mut self, text: &str) -> Vec<Effect> {
        match copy::prepare_within(text, self.copy_limit) {
            Ok(prepared) => vec![Effect::Copy(prepared)],
            Err(e) => {
                self.say(&e.to_string());
                vec![]
            }
        }
    }

    /// `/copy`: the last answer Cline wrote, as it stands now.
    fn copy_last_answer(&mut self) -> Vec<Effect> {
        let text = {
            let chat = self.chat.borrow();
            copy::last_answer(chat.model()).map(copy::block_text)
        };
        match text {
            Some(t) => self.copy_text(&t),
            None => self.copy_text(""),
        }
    }

    /// `y`: the selected block of the conversation; with none selected, the pull request link when the delivery card shows one (the card
    /// names the key); else the last answer. The link of the zip is a bearer link and is never copied.
    fn copy_key(&mut self) -> Vec<Effect> {
        let selected = {
            let chat = self.chat.borrow();
            chat.selected_id()
                .and_then(|id| chat.model().blocks.iter().find(|b| b.id == id))
                .map(copy::block_text)
        };
        if let Some(text) = selected {
            return self.copy_text(&text);
        }
        if let Some(url) = self.view().and_then(|v| v.pr_url) {
            return self.copy_text(&url);
        }
        let text = {
            let chat = self.chat.borrow();
            copy::last_answer(chat.model()).map(copy::block_text)
        };
        self.copy_text(&text.unwrap_or_default())
    }

    /// `/export [file]`: the conversation (not the cards of the screen) as Markdown, built here from the model and written by
    /// [`Effect::Export`]. A line that was pasted or recalled never writes a file.
    fn export_chat(&mut self, args: &[String]) -> Vec<Effect> {
        if self.draft_is_tainted() {
            self.panel = Some(Panel::not_run(
                "Export",
                "A pasted line does not write files. Type /export yourself.",
            ));
            return vec![];
        }
        let Some(run_id) = self.focused_run().map(str::to_string) else {
            self.say("There is no chat to export here.");
            return vec![];
        };
        let model = self.chat.borrow().conversation();
        if model.blocks.is_empty() {
            self.say("Nothing to export yet.");
            return vec![];
        }
        let now = self.now.unwrap_or(0);
        let (y, mo, d, h, mi, s) = utc_civil(now);
        let exported_at = format!(
            "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC ({})",
            self.clock_text(now)
        );
        let stamp = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
        let model_name = self
            .view()
            .and_then(|v| v.direct_model_id().map(ham_core::render::model_name))
            .filter(|m| !m.is_empty());
        let content = export::to_markdown(
            &model,
            &export::Meta {
                run_id: &run_id,
                model: model_name.as_deref(),
                exported_at: &exported_at,
            },
        );
        self.clear_chat();
        vec![Effect::Export {
            run_id,
            arg: args.first().cloned(),
            stamp,
            content,
            blocks: model.blocks.len(),
        }]
    }

    /// The New run form with the values of a start line typed in the box. Nothing starts: the form's own Ctrl-S does that.
    fn open_composer_with(&mut self, args: &DirectArgs) -> Vec<Effect> {
        let effects = self.open_composer();
        let models = self.direct.models;
        let c = &mut self.composer;
        c.prompt.set_text(&args.prompt);
        match &args.target {
            TargetArg::Repo(t) => {
                c.kind = TargetKind::Repo;
                c.repo.set_text(&t.to_string());
            }
            TargetArg::NewRepo(t) => {
                c.kind = TargetKind::NewRepo;
                c.repo.set_text(&t.to_string());
            }
            TargetArg::S3Only => c.kind = TargetKind::S3Only,
        }
        c.base.set_text(args.base.as_deref().unwrap_or(""));
        c.options.set_text(&option_text(args));
        if let Some(e) = args
            .engine
            .as_deref()
            .and_then(|w| ham_core::engines::choose(w).ok())
        {
            c.engine = EnginePick {
                id: e.id,
                chosen: true,
            };
        }
        if let Some(m) = &args.model {
            match ham_core::models::resolve_given(models, m) {
                Ok(r) => {
                    if let Some(i) = models.models().iter().position(|x| x.id == r.model.id) {
                        c.direct.index = i;
                        c.direct.source = PickSource::Here;
                        c.direct.fallback = false;
                    }
                }
                Err(e) => c.notice = Some(format!("--model {m}: {e}")),
            }
        }
        effects
    }

    /// Asks the event loop (once) to drop the key events it has queued: a preview was just armed.
    pub fn take_flush(&mut self) -> bool {
        std::mem::take(&mut self.flush)
    }

    /// Sets the function that reads the revision of the config file; a preview freezes it and a command whose preview was made under
    /// another revision does not run. The process passes [`commands::config_revision`].
    pub fn set_config_revision(&mut self, revision: fn() -> u64) {
        self.config_rev = revision;
    }

    /// A key while a preview is up. `Some` when the key was the preview's (it ran the command, waited, or cancelled); `None` when it
    /// cancelled the preview and goes on to the box like any other key.
    fn armed_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        let armed = self.armed.as_ref()?;
        // A preview that waits for an argument takes every key into its slot.
        if armed.is_ask() {
            return Some(self.ask_key(key));
        }
        // The preview of a hotkey belongs to browse mode (its own key confirms it there): a key in the box withdraws it.
        if armed.gesture_key().is_some() {
            self.armed = None;
            return None;
        }
        let expects = armed.expected().is_some();
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let confirm = match key.code {
            KeyCode::Esc => ConfirmKey::Esc,
            KeyCode::Enter if key.modifiers.is_empty() => ConfirmKey::Enter,
            KeyCode::Backspace if expects && plain => ConfirmKey::Backspace,
            KeyCode::Char(c) if expects && plain => ConfirmKey::Char(c),
            // Alt and a letter would act as Esc then the letter, and Alt-Enter would send the box as chat; with a preview up either only
            // cancels the preview.
            KeyCode::Char(_) | KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                self.armed = None;
                return Some(vec![]);
            }
            _ => {
                self.armed = None;
                return None;
            }
        };
        // A key inside a burst, or soon after a paste, cannot answer a preview; Esc always can.
        if !self.calm && confirm != ConfirmKey::Esc {
            return Some(vec![]);
        }
        let armed = self.armed.as_mut()?;
        let now = self.key_ms;
        let want = armed.expected().map(str::to_string);
        match armed.press(confirm, now) {
            ConfirmStep::Run => Some(self.run_armed()),
            ConfirmStep::Cancelled | ConfirmStep::Spent => {
                self.armed = None;
                Some(vec![])
            }
            ConfirmStep::Waiting(Wait::TypeAnswer) => {
                self.say(&format!(
                    "Type {}, then press Enter",
                    want.unwrap_or_default()
                ));
                Some(vec![])
            }
            ConfirmStep::Waiting(Wait::Mismatch) => {
                self.say(&format!(
                    "That is not {}: change what you typed, or Esc cancels",
                    want.unwrap_or_default()
                ));
                Some(vec![])
            }
            ConfirmStep::Waiting(_) => Some(vec![]),
        }
    }

    /// The second Enter was accepted: run what was previewed, unless the config file changed under it.
    fn run_armed(&mut self) -> Vec<Effect> {
        let Some(armed) = self.armed.take() else {
            return vec![];
        };
        if (self.config_rev)() != armed.rev() {
            self.panel = Some(Panel::not_run(
                &commands::command_title(armed.argv()),
                "The settings changed while the preview was open, so nothing ran. Press Enter to preview it again.",
            ));
            return vec![];
        }
        let Some(invocation) = armed.invocation().cloned() else {
            return vec![];
        };
        let argv = armed.argv().to_vec();
        self.execute(&invocation, &argv)
    }

    /// A key while the preview of a command waits for its missing argument: the slot takes the
    /// characters; Esc closes it and keeps the box; Enter, when calm, goes through the same parser as a typed line. An empty answer or a URL
    /// that carries a credential is refused in the slot and nothing runs; a good one makes the confirmation that asks for the last four
    /// characters of the run id.
    fn ask_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => {
                self.armed = None;
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                // An Enter inside a burst, or soon after a paste, is not an answer.
                if self.calm {
                    return self.answer_ask();
                }
            }
            KeyCode::Enter => {
                self.armed = None;
            }
            _ => {
                if let Some(armed) = self.armed.as_mut() {
                    armed.slot_key(*key);
                }
            }
        }
        vec![]
    }

    /// The slot's Enter: the known words and what was typed go to the router's `complete`.
    fn answer_ask(&mut self) -> Vec<Effect> {
        let Some((argv, value)) = self
            .armed
            .as_ref()
            .and_then(Armed::answer)
            .map(|(argv, value)| (argv.to_vec(), value))
        else {
            return vec![];
        };
        match complete(&argv, &value) {
            RouteOutcome::Refused { reason, .. } => {
                if let Some(armed) = self.armed.as_mut() {
                    armed.refuse(&reason);
                }
                vec![]
            }
            outcome => {
                self.armed = None;
                self.outcome_effects(outcome)
            }
        }
    }

    /// A key of the search inside the chat: `/` opens the bar; while it is open every character is the query's (an `n` or a `q`
    /// too); Enter accepts, so that `n` and `N` walk the matches; Esc comes to the search first, and cancels or clears it. `None` for any
    /// other key, which is the screen's.
    fn search_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        if self.search.takes_text() {
            match key.code {
                KeyCode::Down => {
                    self.search_step(true);
                }
                KeyCode::Up => {
                    self.search_step(false);
                }
                KeyCode::Char('n') if ctrl => {
                    self.search_step(true);
                }
                KeyCode::Char('p') if ctrl => {
                    self.search_step(false);
                }
                KeyCode::Char('w') if ctrl => self.search_text(SearchKey::DeleteWord),
                KeyCode::Char('u') if ctrl => self.search_text(SearchKey::Clear),
                KeyCode::Char(c) if plain => self.search_text(SearchKey::Char(c)),
                KeyCode::Backspace => self.search_text(SearchKey::Backspace),
                KeyCode::Enter => self.search_text(SearchKey::Enter),
                KeyCode::Esc => self.search_text(SearchKey::Esc),
                // Any other key does nothing while the bar is open.
                _ => {}
            }
            return Some(vec![]);
        }
        if key.code == KeyCode::Esc && self.search.is_active() {
            self.search_text(SearchKey::Esc);
            return Some(vec![]);
        }
        if self.search.walks_matches() && plain {
            match key.code {
                KeyCode::Char('n') => return Some(self.search_step(true)),
                KeyCode::Char('N') => return Some(self.search_step(false)),
                _ => {}
            }
        }
        if key.code == KeyCode::Char('/') && plain {
            let chat = self.chat.get_mut();
            let area = chat.transcript_area();
            let search = &mut self.search;
            chat.with_state(|state, model| search.open(state, model, area));
            return Some(vec![]);
        }
        None
    }

    fn search_text(&mut self, key: SearchKey) {
        let chat = self.chat.get_mut();
        let search = &mut self.search;
        chat.with_state(|state, model| search.key(key, state, model));
    }

    fn search_step(&mut self, next: bool) -> Vec<Effect> {
        let chat = self.chat.get_mut();
        let search = &mut self.search;
        chat.with_state(|state, model| {
            if next {
                search.next(state, model);
            } else {
                search.prev(state, model);
            }
        });
        vec![]
    }

    /// A key in browse mode while a result panel is open: Esc closes it, the arrow keys scroll a long one, `o` opens the bearer link
    /// the command returned. `None` for any other key.
    fn panel_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        let panel = self.panel.as_mut()?;
        let overflows = panel.overflows();
        let page = panel.page();
        let has_link = panel.bearer().is_some();
        match key.code {
            KeyCode::Esc => {
                self.panel = None;
                Some(vec![])
            }
            KeyCode::Up if overflows => {
                panel.scroll_by(-1);
                Some(vec![])
            }
            KeyCode::Down if overflows => {
                panel.scroll_by(1);
                Some(vec![])
            }
            KeyCode::PageUp if overflows => {
                panel.scroll_by(-page);
                Some(vec![])
            }
            KeyCode::PageDown if overflows => {
                panel.scroll_by(page);
                Some(vec![])
            }
            // The link opens on the user's own key: unmodified, and not part of a burst.
            KeyCode::Char('o') if has_link && key.modifiers.is_empty() && self.calm => {
                let bearer = panel.bearer()?.clone();
                let program = crate::open::program(self.open_command.as_deref());
                self.notice = Some("opening the download link…".to_string());
                Some(vec![Effect::OpenBearer {
                    program,
                    bearer,
                    what: "download link",
                }])
            }
            KeyCode::Char('u') if has_link && key.modifiers.is_empty() && self.calm => {
                self.notice = Some(
                    "copying the link is not built yet: press o to open it in the browser"
                        .to_string(),
                );
                Some(vec![])
            }
            _ => None,
        }
    }

    /// The default route of a sent line: it answers the open question in the same Cline session (what `p` and Ctrl-S do), else the
    /// text stays in the box and the screen says that Cline is not waiting.
    fn answer_with(&mut self, line: &str) -> Vec<Effect> {
        // An open question is answered through the input route below; otherwise the run's state decides.
        if let Some(effects) = self.send_enter(line) {
            return effects;
        }
        let Some(q) = self.prelude(InstructAction::ContinueWithPrompt) else {
            return vec![];
        };
        if let Err(why) = check_text(line) {
            self.say(&why);
            return vec![];
        }
        let run_id = self.focused_run().unwrap_or_default().to_string();
        self.inflight = Some(InstructAction::ContinueWithPrompt);
        // The answer is a sent message: a pending row shows it, and the conversation holds it once the service has taken it.
        self.send_answer_started(&q.request_id, line);
        self.clear_chat();
        vec![Effect::Answer {
            run_id,
            answer: InputAnswer {
                request_id: q.request_id,
                action: InstructAction::ContinueWithPrompt,
                text: Some(line.to_string()),
                model: None,
            },
        }]
    }

    /// The pastes of the focused run's draft, made on first use.
    fn pastes_mut(&mut self) -> Option<&mut PasteStore> {
        let id = self.focused_run()?.to_string();
        Some(self.pastes.entry(id).or_default())
    }

    fn pastes_ref(&self) -> Option<&PasteStore> {
        self.pastes.get(self.focused_run()?)
    }

    /// The text of the box as it will be read: every `[Pasted n lines]` token replaced by the text that was pasted.
    pub fn expanded_chat_text(&self) -> Option<String> {
        let text = self.chat_text()?;
        Some(match self.pastes_ref() {
            Some(p) => p.expand_all(&text),
            None => text,
        })
    }

    /// The character ranges of the tokens in the box, for drawing them apart.
    pub(crate) fn paste_marks(&self) -> Vec<std::ops::Range<usize>> {
        match (self.pastes_ref(), self.chat_text()) {
            (Some(p), Some(text)) => p.placeholders_in(&text),
            _ => Vec::new(),
        }
    }

    /// Ctrl-G: the draft, with every pasted block put back, goes to the user's editor. The loop runs it and answers with [`Msg::Edited`].
    fn start_edit(&mut self) -> Vec<Effect> {
        let text = self.expanded_chat_text().unwrap_or_default();
        vec![Effect::EditDraft { text }]
    }

    /// What the editor came to: another text replaces the box (its pastes are gone with the old text, and it is tainted as the editor's answer
    /// says); an unchanged draft or a cancelled edit leaves the box, with the editor's hint when it returned at once; an error goes to
    /// the notice row.
    fn edited(&mut self, result: Result<ham_core::editor::Report, String>) -> Vec<Effect> {
        use ham_core::editor::Outcome;
        match result {
            Err(why) => self.say(&why),
            Ok(report) => match &report.outcome {
                Outcome::Edited { text, tainted } => {
                    self.history.cancel();
                    if let Some(p) = self.pastes_mut() {
                        p.clear();
                    }
                    self.set_chat_text(text);
                    self.armed = None;
                    self.chip = None;
                    self.popup_closed = None;
                    if *tainted {
                        self.taint();
                    } else if text.is_empty() {
                        self.untaint();
                    }
                }
                Outcome::Unchanged => {
                    if let Some(hint) = report.hint() {
                        self.say(hint);
                    }
                }
                Outcome::Cancelled => {}
            },
        }
        vec![]
    }

    /// Tab: the placeholder under the caret becomes the text that was pasted, and the caret goes after it.
    fn expand_placeholder(&mut self) -> Vec<Effect> {
        let (Some(text), Some(caret)) =
            (self.chat_text(), self.chat_box().map(ChatBox::caret_index))
        else {
            return vec![];
        };
        if let Some(done) = self.pastes_ref().and_then(|p| p.expand_at(&text, caret))
            && let Some(b) = self.chat_mut()
        {
            b.set_text_caret(&done.text, done.caret);
        }
        vec![]
    }

    /// Backspace at the end of a token and Delete at the start of one take the whole token. `true` when one was taken.
    fn delete_token(&mut self, backward: bool) -> bool {
        let (Some(text), Some(caret)) =
            (self.chat_text(), self.chat_box().map(ChatBox::caret_index))
        else {
            return false;
        };
        let range = self.pastes_ref().and_then(|p| {
            if backward {
                p.token_ending_at(&text, caret)
            } else {
                p.token_starting_at(&text, caret)
            }
        });
        match (range, self.chat_mut()) {
            (Some(r), Some(b)) => {
                b.remove_chars(r);
                true
            }
            _ => false,
        }
    }

    fn clear_chat(&mut self) {
        // A line the box gave up because it was sent (a message or a command) is the newest entry of the history, with the text of its pastes.
        if let Some(text) = self.expanded_chat_text() {
            self.history.record(&text);
        }
        if let Some(p) = self.pastes_mut() {
            p.clear();
        }
        if let Some(b) = self.chat_mut() {
            b.clear();
        }
        self.armed = None;
        self.chip = None;
        self.chip_dismissed.clear();
        self.untaint();
    }

    /// The draft of the focused run holds pasted text.
    fn taint(&mut self) {
        if let Some(id) = self.focused_run().map(str::to_string) {
            self.tainted.insert(id);
        }
    }

    /// The draft of the focused run is clean again (the box was emptied).
    fn untaint(&mut self) {
        self.closed_banner = None;
        if let Some(id) = self.focused_run().map(str::to_string) {
            self.tainted.remove(&id);
        }
    }

    /// The message box of the focused run holds text that was pasted (or arrived as a burst of keys): every command in it shows its
    /// preview. A custom command sink that runs commands must apply the same rule.
    pub fn draft_is_tainted(&self) -> bool {
        self.focused_run()
            .is_some_and(|id| self.tainted.contains(id))
    }

    fn artifacts_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let n = match &self.artifacts {
            ArtifactList::Loaded(list) => list.len(),
            _ => 0,
        };
        match key.code {
            KeyCode::Char('q') => self.quit_request(),
            KeyCode::Esc => {
                self.screen = Screen::Dashboard;
                self.poll_effect()
            }
            KeyCode::Char('r') => {
                self.artifacts = ArtifactList::Loading;
                self.run_effect(|run_id| Effect::LoadArtifacts { run_id })
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.artifact_cursor = self.artifact_cursor.saturating_sub(1);
                vec![]
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.artifact_cursor + 1 < n {
                    self.artifact_cursor += 1;
                }
                vec![]
            }
            KeyCode::Enter => {
                let (Some(run_id), ArtifactList::Loaded(list)) =
                    (self.focused_run().map(str::to_string), &self.artifacts)
                else {
                    return vec![];
                };
                let Some(a) = list.get(self.artifact_cursor) else {
                    return vec![];
                };
                self.artifact_download = Some((local_name(&run_id, a), DownloadState::Pending));
                vec![Effect::DownloadArtifact {
                    run_id,
                    artifact_id: a.artifact_id.clone(),
                }]
            }
            _ => vec![],
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let fields: &[Field] = if self.backend_choices().len() >= 2 {
            &FIELDS_MULTI
        } else {
            &FIELDS
        };
        let i = fields
            .iter()
            .position(|f| *f == self.composer.focus)
            .unwrap_or(0);
        let n = fields.len();
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.composer.focus = fields[j];
    }

    /// A key while the composer asks about provider data sharing. `y` sends the start again with the agreement for this run (the same nonce,
    /// so the same start) and with what the question showed: the start switches the account's mode first, and only while the account is in the
    /// state the person saw; `n` and Esc drop it with one line and send nothing; any other key leaves the question up.
    fn sharing_answer(&mut self, ask: SharingAsk, key: &KeyEvent) -> Vec<Effect> {
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match key.code {
            KeyCode::Char('y' | 'Y') if plain && self.calm => {
                let SharingAsk {
                    mut args,
                    nonce,
                    backend,
                    shown,
                    ..
                } = ask;
                args.provider_sharing = Some(true);
                self.composer.submitting = true;
                vec![Effect::SubmitAgreed {
                    args,
                    nonce,
                    backend,
                    shown,
                }]
            }
            KeyCode::Char('n' | 'N') if plain => {
                self.composer.error = Some(ham_core::sharing::declined(&ask.model, &ask.id));
                vec![]
            }
            KeyCode::Esc => {
                self.composer.error = Some(ham_core::sharing::declined(&ask.model, &ask.id));
                vec![]
            }
            _ => {
                self.composer.sharing = Some(ask);
                vec![]
            }
        }
    }

    /// Moves the composer's pick `by` selectable rows (clamped at the ends); `None` goes to the first or last.
    fn pick_model(&mut self, to: PickMove) {
        let p = &mut self.composer.direct;
        let rows: Vec<usize> = self
            .direct
            .models
            .models()
            .iter()
            .enumerate()
            .filter(|(_, m)| m.selectable())
            .map(|(i, _)| i)
            .collect();
        let at = rows.iter().position(|i| *i == p.index).unwrap_or(0);
        let next = match to {
            PickMove::Up => at.saturating_sub(1),
            PickMove::Down => (at + 1).min(rows.len().saturating_sub(1)),
            PickMove::First => 0,
            PickMove::Last => rows.len().saturating_sub(1),
        };
        if let Some(i) = rows.get(next) {
            if *i != p.index {
                p.source = PickSource::Here;
            }
            p.index = *i;
        }
        p.saved = None;
        p.fallback = false;
    }

    fn composer_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.composer.notice = None;
        if let Some(ask) = self.composer.sharing.take() {
            return self.sharing_answer(ask, &key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Runs;
                return vec![];
            }
            KeyCode::Tab => {
                self.move_focus(true);
                return vec![];
            }
            KeyCode::BackTab => {
                self.move_focus(false);
                return vec![];
            }
            KeyCode::Char('s') if ctrl => return self.submit(),
            _ => {}
        }
        // The text boxes take control and alt keys (word moves, delete a word, undo and redo, line start and end); the other fields
        // ignore them.
        let text_field = matches!(
            self.composer.focus,
            Field::Prompt | Field::Repo | Field::Base | Field::Options
        );
        if ctrl && !text_field {
            return vec![];
        }
        let c = &mut self.composer;
        match (c.focus, key.code) {
            (Field::Prompt, KeyCode::Enter) => c.prompt.newline(),
            (Field::Prompt, _) => {
                c.prompt.handle_key(key);
            }
            (Field::Repo, _) => {
                c.repo.handle_key(key);
            }
            (Field::Base, _) => {
                c.base.handle_key(key);
            }
            (Field::Options, _) => {
                c.options.handle_key(key);
            }
            (Field::Target, KeyCode::Right | KeyCode::Char(' ')) => c.kind = c.kind.next(),
            (Field::Target, KeyCode::Left) => c.kind = c.kind.prev(),
            (Field::Target, KeyCode::Down) => self.pick_engine(true),
            (Field::Target, KeyCode::Up) => self.pick_engine(false),
            (Field::Backend, KeyCode::Right | KeyCode::Down | KeyCode::Char(' ')) => {
                self.pick_backend(true);
            }
            (Field::Backend, KeyCode::Left | KeyCode::Up) => self.pick_backend(false),
            (Field::Model, KeyCode::Up | KeyCode::Left) => self.pick_model(PickMove::Up),
            (Field::Model, KeyCode::Down | KeyCode::Right) => self.pick_model(PickMove::Down),
            (Field::Model, KeyCode::Home | KeyCode::PageUp) => self.pick_model(PickMove::First),
            (Field::Model, KeyCode::End | KeyCode::PageDown) => self.pick_model(PickMove::Last),
            (Field::Model, KeyCode::Enter) => {
                // Confirms the model on the screen (see `DirectPick::fallback`).
                self.composer.direct.fallback = false;
            }
            (Field::Model, KeyCode::Char('d')) => {
                if let Some(id) = self.chosen_model().map(str::to_string) {
                    self.composer.error = None;
                    return vec![Effect::SaveDefaultModel { model_id: id }];
                }
            }
            (Field::Submit, KeyCode::Enter) => return self.submit(),
            _ => {}
        }
        vec![]
    }

    /// The argv the composer stands for, parsed by `ham_core::command::parse`. The Options box adds the words of the options it takes.
    fn argv(&self) -> Result<Vec<String>, String> {
        let c = &self.composer;
        let mut v = vec!["--prompt".to_string(), c.prompt.text()];
        let (repo, base) = (c.repo.text(), c.base.text());
        match c.kind {
            TargetKind::Repo => v.extend(["--repo".to_string(), repo.trim().to_string()]),
            TargetKind::NewRepo => v.extend(["--new-repo".to_string(), repo.trim().to_string()]),
            TargetKind::S3Only => v.push("--s3-only".to_string()),
        }
        if !base.trim().is_empty() {
            v.extend(["--base".to_string(), base.trim().to_string()]);
        }
        v.extend(
            option_words(&c.options.text())?
                .into_iter()
                .filter(|w| w != CUSTOMER_CONTENT_FLAG),
        );
        Ok(v)
    }

    /// Whether the Options box marks the run as customer content.
    fn customer_content_marked(&self) -> bool {
        option_words(&self.composer.options.text())
            .is_ok_and(|words| words.iter().any(|w| w == CUSTOMER_CONTENT_FLAG))
    }

    /// The backend a start goes to and the refusal that stops it before any call: `Ok(None)` for a client with one backend.
    fn start_backend(&self) -> Result<Option<String>, String> {
        let choices = self.backend_choices();
        if choices.len() < 2 {
            // One backend leaves nothing to choose, but its declaration still decides whether a run marked as customer content may start; a
            // client that was told nothing about it counts as not declared, as on the command line.
            if self.customer_content_marked() {
                let (alias, safe) = self
                    .declared
                    .first()
                    .cloned()
                    .unwrap_or_else(|| (ham_core::backends::DEFAULT_ALIAS.to_string(), false));
                if !safe {
                    return Err(ham_core::backends::Local::CustomerContent { alias }.to_string());
                }
            }
            return Ok(None);
        }
        let Some(alias) = self.backend_picked() else {
            return Err(format!(
                "choose the backend for this run: Tab to the Backend field and pick one ({})",
                choices
                    .iter()
                    .map(|(a, _)| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        if self.customer_content_marked() && choices.iter().any(|(a, safe)| *a == alias && !*safe) {
            return Err(ham_core::backends::Local::CustomerContent { alias }.to_string());
        }
        Ok(Some(alias))
    }

    /// The composer's Submit: the argv form of the same words (`--direct --model <id>`) goes through the parser, so the
    /// repository is checked as `ham` checks it; the chosen model travels as an explicit choice.
    fn submit_direct(&mut self) -> Vec<Effect> {
        let Some(model) = self.chosen_model().map(str::to_string) else {
            return vec![];
        };
        if self.composer.direct.fallback {
            let name = self.direct_view().map_or_else(String::new, |v| v.title);
            self.composer.error = Some(format!(
                "the model setting you made names no supported model, so the composer is on {name}; press Enter on the Model field to confirm it, or pick another, then send"
            ));
            return vec![];
        }
        let backend = match self.start_backend() {
            Ok(b) => b,
            Err(why) => {
                self.composer.error = Some(why);
                return vec![];
            }
        };
        let mut argv = match self.argv() {
            Ok(argv) => argv,
            Err(why) => {
                self.composer.error = Some(why);
                return vec![];
            }
        };
        argv.extend(["--direct".to_string(), "--model".to_string(), model]);
        // The engine goes into the start only when the person chose one: the file's `default_engine` decides otherwise, at the start.
        if self.composer.engine.chosen {
            argv.extend(["--engine".to_string(), self.composer.engine.id.to_string()]);
        }
        match parse(&argv) {
            Ok(inv) => match inv.command {
                Command::StartDirect(mut args) => {
                    let d = &self.direct;
                    // The Options box decides when it names rules; the settings the process started with fill in otherwise.
                    args.rules = args.rules.or_else(|| d.settings.rules.clone());
                    args.no_rules |= d.settings.no_rules;
                    let nonce = format!("{}-{}", self.nonce, d.started);
                    self.composer.error = None;
                    self.composer.submitting = true;
                    self.model_asked(args.model.clone());
                    self.name_asked(&args.prompt);
                    match backend {
                        Some(backend) => vec![Effect::SubmitDirectOn {
                            args,
                            nonce,
                            backend,
                        }],
                        None => vec![Effect::SubmitDirect { args, nonce }],
                    }
                }
                _ => vec![],
            },
            Err(e) => {
                let text = e.to_string();
                self.composer.error =
                    Some(text.strip_prefix("usage: ").unwrap_or(&text).to_string());
                vec![]
            }
        }
    }

    fn submit(&mut self) -> Vec<Effect> {
        if self.composer.prompt.text().trim().is_empty() {
            self.composer.error = Some("a prompt is required".into());
            return vec![];
        }
        self.submit_direct()
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Runs(Ok(runs)) => {
                // A probe run is not the user's; `ham runs --probes` lists it.
                self.runs_error = None;
                self.tags_note_full_read();
                self.set_runs(runs);
                self.alert_effects()
            }
            Msg::Runs(Err(e)) => {
                self.runs_error = Some(e);
                vec![]
            }
            Msg::RunsRead(Ok(read)) => {
                // Applied against the list held now, never a copy from when the request began: a timer read and a manual read can be in
                // flight together.
                self.runs_inflight = false;
                self.runs_error = None;
                self.tags_note_read(&read);
                let merged = read.apply(&self.runs);
                self.set_runs(merged);
                self.alert_effects()
            }
            Msg::RunsIndexed(Ok(IndexedRead { read, mut index })) => {
                // The tags of an edit that was answered while this read was out are newer than the read (`tags.rs`).
                self.tags_unpin(&mut index);
                self.runs_inflight = false;
                self.runs_error = None;
                self.tags_note_read(&read);
                // A read of every page is the whole list: it replaces what was known. A read of the newest page lays over it.
                if matches!(read, RunsRead::All(_)) {
                    self.run_index.clear();
                }
                for e in index {
                    self.run_index.insert(e.run_id.clone(), e);
                }
                let merged = read.apply(&self.runs);
                self.set_runs(merged);
                // This is the answer the compiled app gets for every read of the list (`Effect::LoadRuns`, the 5 s timer), so the bell and the
                // notification for a run that starts to wait come from here, by the same rule as the other two reads: the first read only seeds.
                self.alert_effects()
            }
            Msg::RunsIndexed(Err(e)) => {
                self.runs_inflight = false;
                self.runs_error = Some(e);
                vec![]
            }
            Msg::Backends(view) => {
                self.backend_view = view;
                vec![]
            }
            Msg::RunsRead(Err(e)) => {
                // A failed read keeps the held list and frees the timer for the next interval.
                self.runs_inflight = false;
                self.runs_error = Some(e);
                vec![]
            }
            Msg::Started(Ok(r)) => {
                // The prompt went out: the next `n` starts from an empty composer.
                self.composer = Composer::new(self.default_repo.as_deref(), &self.direct);
                self.direct.started += 1;
                self.model_started(&r.run_id);
                self.name_started(&r.run_id);
                self.open_run(&r.run_id)
            }
            Msg::RunName { run_id, name } => {
                self.name_read(&run_id, name);
                vec![]
            }
            Msg::TagsEdited { origin, results } => self.tags_edited(origin, results),
            Msg::DefaultSaved(result) => {
                let (d, p) = (&mut self.direct, &mut self.composer.direct);
                match result {
                    Ok(id) => {
                        let name = d
                            .models
                            .models()
                            .iter()
                            .find(|m| m.id == id)
                            .map_or(id.clone(), |m| m.name.clone());
                        let shadowed = d
                            .settings
                            .prefs
                            .env
                            .as_ref()
                            .is_some_and(|e| !e.trim().is_empty());
                        d.settings.prefs.config = Some(id);
                        p.saved = Some(if shadowed {
                            format!(
                                "Saved {name} as your default (config default_model). HAMH_MODEL is set and still wins until you unset it."
                            )
                        } else {
                            format!(
                                "Saved {name} as your default (config default_model); new composers open on it."
                            )
                        });
                    }
                    Err(e) => {
                        self.composer.error = Some(format!("could not save the default: {e}"));
                    }
                }
                vec![]
            }
            Msg::Started(Err(e)) => {
                self.composer.submitting = false;
                self.composer.error = Some(e);
                vec![]
            }
            Msg::SharingAsk(ask) => {
                self.composer.submitting = false;
                self.composer.error = None;
                self.composer.sharing = Some(*ask);
                vec![]
            }
            Msg::StartedWith { response, note } => {
                self.sharing_notes
                    .insert(response.run_id.clone(), note.clone());
                let fx = self.update(Msg::Started(Ok(response)));
                self.say(&note);
                fx
            }
            Msg::Pages(result) => {
                self.polling = false;
                match result {
                    Ok(pages) => {
                        self.plan.record(self.clock, &pages);
                        self.next_poll_at = self.clock + self.plan.interval();
                        self.dashboard_error = None;
                        self.link.on_ok(self.now_ms());
                        if let Some(state) = &mut self.run {
                            for (shard, page) in pages {
                                if page.events.iter().all(|e| e.run_id == state.run_id()) {
                                    for e in &page.events {
                                        match e.event_type {
                                            EventType::RunCreated => {
                                                self.run_started = self
                                                    .run_started
                                                    .or_else(|| parse_utc_unix(&e.ts));
                                            }
                                            EventType::RunCompleted => {
                                                self.run_ended = parse_utc_unix(&e.ts);
                                            }
                                            EventType::TurnEnded => {
                                                self.turn_ids.insert(e.event_id.clone());
                                            }
                                            _ => {}
                                        }
                                    }
                                    state.apply_page(shard, page);
                                }
                            }
                        }
                        self.refresh();
                        let mut effects = self.track_input();
                        effects.extend(self.maybe_download());
                        // The totals are as of the last turn that ended: a turn that ends after the first read asks for them again.
                        let turns = self.turn_ids.len();
                        if self.first_pages {
                            self.first_pages = false;
                            self.turns_asked = turns;
                        } else if turns > self.turns_asked {
                            self.turns_asked = turns;
                            if let Some(run_id) = self.focused_run() {
                                effects.push(Effect::LoadDetail {
                                    run_id: run_id.to_string(),
                                });
                            }
                        }
                        effects
                    }
                    Err(e) => {
                        // A failed poll is tried again at the next tick.
                        self.next_poll_at = self.clock + TICK;
                        self.link.on_err(self.now_ms());
                        self.dashboard_error = Some(e);
                        vec![]
                    }
                }
            }
            Msg::Downloaded { run_id, result: r } => {
                // The zip of a run that is no longer on screen is on the disk and nowhere else: this card is another run's, and it asks for its own.
                if self.focused_run() != Some(run_id.as_str()) {
                    return vec![];
                }
                // A download that `d` started on a card that already had a zip: the earlier zip stays when this one fails.
                let earlier = match std::mem::replace(&mut self.download, DownloadState::None) {
                    DownloadState::Refresh(p) => Some(p),
                    _ => None,
                };
                self.download = match (r, earlier) {
                    (Ok(p), earlier) => {
                        self.link_expires = None;
                        // The report in the zip is read once, here, and not on every redraw of the card.
                        self.tests = (p.artifact.kind
                            == harness_protocol::store::ArtifactKind::Zip)
                            .then(|| crate::report::tests_of_zip(&p.path))
                            .flatten();
                        // The flow the run declared is read with it, once.
                        self.flow = (p.artifact.kind == harness_protocol::store::ArtifactKind::Zip)
                            .then(|| crate::flow::flow_word_of_zip(&p.path))
                            .flatten();
                        if earlier.is_some() {
                            self.notice =
                                Some(format!("saved {} (sha256 matched)", p.path.display()));
                        }
                        DownloadState::Done(p)
                    }
                    (Err(e), Some(earlier)) => {
                        self.notice = Some(format!("download failed: {e}"));
                        DownloadState::Done(earlier)
                    }
                    (Err(e), None) => DownloadState::Failed(e),
                };
                vec![]
            }
            Msg::Opened(r) => {
                let fallback = self.open_fallback.take();
                match r {
                    Ok(o) => {
                        if o.expires_at.is_some() {
                            self.link_expires = o.expires_at;
                        }
                        self.notice = Some(format!("opened the {} in your browser", o.what));
                        vec![]
                    }
                    // No browser opened the pull request link: it is copied on the same key press, so it can be pasted where a browser is.
                    Err(_) if fallback.is_some() => {
                        self.notice = Some(
                            "could not open a browser: copying the PR link instead".to_string(),
                        );
                        self.copy_prefix =
                            Some("could not open a browser: the PR link is copied. ".to_string());
                        self.copy_text(&fallback.unwrap_or_default())
                    }
                    Err(e) => {
                        self.notice = Some(format!("could not open the link: {e}"));
                        vec![]
                    }
                }
            }
            Msg::Cloned { run_id, result } => {
                if self.focused_run() == Some(run_id.as_str()) {
                    // The card shows the clone once the zip is on it; the notice row says how it ended either way.
                    self.clear_notice_starting(CLONING);
                    self.clone = match result {
                        Ok(c) => {
                            self.notice = Some(format!("cloned into {}", c.dir.display()));
                            CloneState::Done(c)
                        }
                        Err(e) => {
                            self.notice = Some(format!("clone failed: {e}"));
                            CloneState::Failed(e)
                        }
                    };
                }
                vec![]
            }
            Msg::Detail(r) => {
                if let Ok(d) = r
                    && self.focused_run() == Some(d.summary.run_id.as_str())
                {
                    self.detail = Some(d);
                    // The prompt comes from the detail.
                    self.rebuild_chat();
                }
                vec![]
            }
            Msg::Artifacts(r) => {
                self.artifact_download = None;
                self.artifacts = match r {
                    Ok(list) => {
                        if self.artifact_cursor >= list.len() {
                            self.artifact_cursor = 0;
                        }
                        ArtifactList::Loaded(list)
                    }
                    Err(e) => ArtifactList::Failed(e),
                };
                vec![]
            }
            Msg::ArtifactPulled(r) => {
                let name = self
                    .artifact_download
                    .take()
                    .map(|(n, _)| n)
                    .unwrap_or_default();
                self.artifact_download = Some(match r {
                    Ok(p) => (name, DownloadState::Done(p)),
                    Err(e) => (name, DownloadState::Failed(e)),
                });
                vec![]
            }
            Msg::Cancelled(r) => {
                self.notice = Some(match r {
                    Ok(c) => format!("run {} {}", c.run_id, status_word(c.status)),
                    Err(e) => e,
                });
                self.poll_effect()
            }
            Msg::Answered(result) => self.answered(result),
            Msg::CommandDone { argv, out } => {
                // A resume that started by the command (`/resume <id>`, `/resume --all`, `R` on a run's screen) takes the card of a resume of
                // that run that did not start away, as one that started from the Runs screen does.
                if argv.first().is_some_and(|w| w == "resume") {
                    for id in ham_core::resume::started_run_ids(&out.lines()) {
                        self.start_refusals.remove(&id);
                    }
                }
                // The editor `e` opened on the Settings screen came back: the screen reads the file again; its panel is not for this screen.
                if let Some(effects) = self.settings.edit_done(&argv, &out) {
                    return effects;
                }
                self.panel = Some(Panel::done(&argv, &out));
                vec![]
            }
            Msg::Ext(done) => self.ext_done(done),
            Msg::Settings(done) => self.settings_done(done),
            Msg::Edited(result) => self.edited(result),
            Msg::Copied(result) => {
                let lead = self.copy_prefix.take();
                match (result, lead) {
                    (Ok(text), Some(lead)) => self.say(&format!("{lead}{text}")),
                    (Err(text), Some(_)) => self.say(&format!(
                        "could not open a browser, and the link could not be copied: {text}"
                    )),
                    (Ok(text) | Err(text), None) => self.say(&text),
                }
                vec![]
            }
            Msg::Exported(result) => {
                match result {
                    Ok(text) | Err(text) => self.say(&text),
                }
                vec![]
            }
        }
    }

    /// `C`: open the shared clone flow of `ham clone <run>` for the run on screen: the default directory, the path on the card when it is
    /// done, and the zip fetched by the flow itself when it is not on this machine yet. Nothing happens while a clone runs or after one succeeded
    /// (the card says where), and a failed clone may be tried again.
    fn clone_key(&mut self) -> Vec<Effect> {
        let run_id = self.focused_run().unwrap_or_default().to_string();
        let delivered = self.view().is_some_and(|v| v.zip.is_some())
            || matches!(self.download, DownloadState::Done(_));
        if !delivered {
            // A run that ended without a zip is told where its work is.
            self.notice = Some(
                ham_core::saved_copy::no_delivery(self.view().and_then(|v| v.status))
                    .unwrap_or_else(|| {
                        "nothing is delivered yet, so there is nothing to clone".to_string()
                    }),
            );
            return vec![];
        }
        match self.clone {
            CloneState::None | CloneState::Failed(_) => {
                self.clone = CloneState::Pending;
                self.notice = Some(CLONING.to_string());
                vec![Effect::Clone {
                    run_id,
                    cwd: self.clone_dir.clone(),
                }]
            }
            CloneState::Pending | CloneState::Done(_) => vec![],
        }
    }

    /// `o`: open the pull request in the browser; without one, a download link made now. The link is never put in a notice.
    fn open_key(&mut self) -> Vec<Effect> {
        let program = crate::open::program(self.open_command.as_deref());
        let Some(view) = self.view() else {
            return vec![];
        };
        self.open_fallback = None;
        if let Some(url) = view.pr_url.clone() {
            self.notice = Some("opening the PR link…".to_string());
            // If no browser opens it, the link is copied (the zip's bearer link is never copied).
            self.open_fallback = Some(url.clone());
            return vec![Effect::OpenUrl {
                program,
                url,
                what: "PR link",
            }];
        }
        match view.zip {
            Some(zip) => {
                self.notice = Some("opening the download link…".to_string());
                vec![Effect::OpenDownload {
                    program,
                    run_id: view.run_id.clone(),
                    artifact_id: zip.artifact_id,
                }]
            }
            None => {
                self.notice = Some(
                    ham_core::saved_copy::no_delivery(view.status).unwrap_or_else(|| {
                        "there is no link yet: nothing is delivered".to_string()
                    }),
                );
                vec![]
            }
        }
    }

    /// `d`: download the delivered zip again, through the code path of `ham pull` (a fresh link, ranged GETs, the SHA-256 check, a file
    /// that already verifies is kept). A zip on the card stays there while the new one comes.
    fn download_key(&mut self) -> Vec<Effect> {
        let Some(view) = self.view() else {
            return vec![];
        };
        let Some(zip) = view.zip.clone() else {
            self.notice = Some(
                ham_core::saved_copy::no_delivery(view.status).unwrap_or_else(|| {
                    "nothing is delivered yet, so there is nothing to download".to_string()
                }),
            );
            return vec![];
        };
        let next = match std::mem::replace(&mut self.download, DownloadState::None) {
            DownloadState::Done(p) => DownloadState::Refresh(p),
            busy @ (DownloadState::Pending | DownloadState::Refresh(_)) => {
                self.download = busy;
                self.notice = Some("the zip is already downloading".to_string());
                return vec![];
            }
            DownloadState::None | DownloadState::Failed(_) => DownloadState::Pending,
        };
        self.download = next;
        self.notice = None;
        vec![Effect::Download {
            run_id: view.run_id.clone(),
            artifact_id: zip.artifact_id,
        }]
    }

    /// What the link keys can do now: `o` opens the pull request when the run has one, else the download link of a delivered zip; `d`
    /// needs a delivered zip.
    pub(crate) fn link_keys(&self) -> LinkKeys {
        let (pr, zip) = self
            .cached
            .as_ref()
            .map_or((false, false), |v| (v.pr_url.is_some(), v.zip.is_some()));
        LinkKeys { pr, zip }
    }

    /// On `delivery.ready` the zip downloads once.
    fn maybe_download(&mut self) -> Vec<Effect> {
        let Some(view) = self.view() else {
            return vec![];
        };
        match (&view.zip, &self.download) {
            (Some(zip), DownloadState::None) if finished(&view) => {
                self.download = DownloadState::Pending;
                vec![
                    Effect::Download {
                        run_id: view.run_id.clone(),
                        artifact_id: zip.artifact_id.clone(),
                    },
                    Effect::LoadDetail {
                        run_id: view.run_id.clone(),
                    },
                ]
            }
            _ => vec![],
        }
    }
}

/// The notice that asks for a second `c` (the run id follows) and the one that asks for a second `f`.
const ASK_CANCEL: &str = "press c again to cancel run ";

/// The composer's question before a run on a model that needs provider data sharing: the start it holds, and its words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharingAsk {
    /// The start as it was submitted. `y` sends it again with `--provider-sharing on` and the same nonce, so it is the same start.
    pub args: DirectArgs,
    pub nonce: String,
    /// The backend the start was aimed at, when the client holds several.
    pub backend: Option<String>,
    /// The model's display name and id, for the words of the answer `n`.
    pub model: String,
    pub id: String,
    /// The model, what sharing means, the account and the Region, and what ham changes there (`ham_core::sharing::ask_text`).
    pub question: String,
    /// What the question showed: `y` agrees to this, and the start that follows changes the account only while it is in this state.
    pub shown: ham_core::sharing::Shown,
}

/// The question `R` asks on the Runs screen: the plan it named and the time it was shown.
#[derive(Clone, Debug)]
pub(crate) struct ResumeAsk {
    pub plan: ham_core::resume::ResumePlan,
    pub shown_ms: u64,
}

/// The whole text of a Runs notice: `row` is the short notice on the note row; the panel above it shows `title` and each paragraph wrapped
/// to the width.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunsDetail {
    pub(crate) row: String,
    pub(crate) title: String,
    pub(crate) paragraphs: Vec<String>,
}

/// The title of the Runs panel and of the card that hold a line of provider data sharing.
pub(crate) const SHARING_TITLE: &str = "provider data sharing";

/// What did not start from a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Retry,
    FollowUp,
    Resume,
}

impl Refused {
    /// The title of the card.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Refused::Retry => "retry did not start",
            Refused::FollowUp => "follow-up did not start",
            Refused::Resume => "resume did not start",
        }
    }

    /// What did not start, in the words of a notice.
    fn word(self) -> &'static str {
        match self {
            Refused::FollowUp => "follow-up",
            Refused::Retry => "retry",
            Refused::Resume => "resume",
        }
    }

    /// The notice off the Runs screen: the whole sentence when it fits [`NOTICE_COLUMNS`]. A longer one is cut to a short sentence, which
    /// points to the card above when `here` holds (the screen shows the run the start came from, which keeps the card) and names the run
    /// otherwise, because the card is on that run's screen only.
    fn notice(self, source: &str, why: &str, here: bool) -> String {
        let (whole, short) = match self {
            Refused::FollowUp => (
                format!("the follow-up run did not start: {why}; your text is kept"),
                "the follow-up run did not start (the card above says why); your text is kept",
            ),
            Refused::Retry => (
                format!("the retry did not start: {why}; your text is kept"),
                "the retry did not start (the card above says why); your text is kept",
            ),
            Refused::Resume => (
                format!("the resume did not start: {why}"),
                "the resume did not start (the card above says why)",
            ),
        };
        if ratatui::text::Span::raw(whole.as_str()).width() <= NOTICE_COLUMNS {
            whole
        } else if here {
            short.to_string()
        } else {
            format!(
                "the {} of run {} did not start: open it to see why",
                self.word(),
                short_id(source)
            )
        }
    }

    /// The short notice on the Runs screen, above which the panel shows why.
    fn runs_row(self, source: &str) -> String {
        let source = sanitize_line(source);
        match self {
            Refused::Retry => format!("the retry of run {source} did not start"),
            Refused::FollowUp => format!("the follow-up of run {source} did not start"),
            Refused::Resume => format!("the resume of run {source} did not start"),
        }
    }
}

/// Why a start from a run did not start: the paragraphs of the run's card (the line of provider data sharing, when the account was
/// switched first, then the reason).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) what: Refused,
    pub(crate) paragraphs: Vec<String>,
}

/// The widest notice the run screen's one notice row shows whole on the narrowest screen ham is made for.
pub(crate) const NOTICE_COLUMNS: usize = 80;

/// The first eight characters of a run id, made safe to print.
fn short_id(id: &str) -> String {
    sanitize_line(id).chars().take(8).collect()
}
/// The notice while `C` clones: the shared flow fetches the zip first when it is not on this machine.
const CLONING: &str = "cloning… (the zip is fetched first when it is not on this machine)";
const ASK_FINISH: &str = "press f again to finish run ";

/// The notice when nothing waits for the user.
const NOT_WAITING: &str = "{engine} is not waiting for an answer from you.";

/// Why `action` cannot be chosen for `q`, in one line; `None` when it can.
pub(crate) fn refusal(q: &InputRequest, action: InstructAction) -> Option<String> {
    use InstructAction::{ContinueWithPrompt, TryModel};
    if q.offers(action) && !(action == TryModel && q.models.is_empty()) {
        return None;
    }
    Some(match action {
        ContinueWithPrompt if !q.can_continue => format!(
            "{} cannot take a message now: s starts a fresh session, f f finishes.",
            q.engine
        ),
        TryModel if !q.can_continue => format!(
            "Another model needs a live {} session: s starts a fresh one, f f finishes.",
            q.engine
        ),
        TryModel => "There is no other model to try for this run.".to_string(),
        _ => "That choice is not offered for this question.".to_string(),
    })
}

/// What the notice says once the server has queued an answer. Queued is not applied: Cline takes it at its next step.
fn sent_notice(engine: &str, action: Option<InstructAction>) -> String {
    let what = match action {
        Some(InstructAction::ContinueWithPrompt) => "Your message",
        Some(InstructAction::NewSessionWithPrompt) => "Your request for a fresh session",
        Some(InstructAction::TryModel) => "Your model choice",
        Some(InstructAction::Finish) => "Your request to finish",
        None => "Your answer",
    };
    format!("{what} was sent to the run; waiting for {engine} to take it.")
}

/// The notice that keeps the end of a question on the dashboard after its card is gone.
fn outcome_words(how: &str) -> Option<&'static str> {
    Some(match how {
        "answered:continue_with_prompt" => "answered: message sent to Cline",
        "answered:new_session_with_prompt" => "answered: fresh session",
        "answered:try_model" => "answered: other model",
        "finish" | "answered:finish" => "answered: finish",
        "expired" => "window closed: nobody answered",
        "cancelled" => "the run was cancelled",
        "deadline" => "the run reached its time limit",
        "superseded" => "the question was replaced by a newer one",
        "recovered" => "model routes answer again",
        _ => return None,
    })
}

/// The answer keys and the typing screen.
impl App {
    /// The open question that wants an answer. The outage notice (`blocking: false`) is not one.
    pub(crate) fn question(&self) -> Option<&InputRequest> {
        let v = self.cached.as_ref()?;
        // A blocked run has no session left to take an answer: a question it asked before it ended is not waiting.
        if v.status == Some(harness_protocol::goal::GoalStatus::Blocked) {
            return None;
        }
        v.input.waiting()
    }

    /// True (with a notice) while an answer is on its way: the server has it, and the run's events do not show Cline took it yet.
    fn answer_pending(&mut self) -> bool {
        if self.inflight.is_some() {
            self.notice = Some("Your answer is being sent.".into());
            return true;
        }
        let open = self.question().map(|q| q.request_id.clone());
        let waiting = self.sent.as_ref().is_some_and(|s| {
            Some(&s.request_id) == open.as_ref() && self.clock.saturating_sub(s.at) < ACK_WAIT
        });
        if waiting {
            self.notice = Some("Your answer is on its way: wait for Cline to take it.".into());
        }
        waiting
    }

    /// The seconds left in the open question's window, from the app's clock; `None` before the clock is set.
    pub(crate) fn secs_left(&self, q: &InputRequest) -> Option<i64> {
        q.secs_left_at(self.now?)
    }

    /// True while an answer was queued and Cline has not shown it took it.
    pub(crate) fn answer_waiting(&self) -> bool {
        let open = self.question().map(|q| &q.request_id);
        self.sent.as_ref().is_some_and(|s| {
            Some(&s.request_id) == open && self.clock.saturating_sub(s.at) < ACK_WAIT
        })
    }

    fn prelude(&mut self, action: InstructAction) -> Option<InputRequest> {
        let Some(q) = self.question().cloned() else {
            self.notice = Some(self.engine_say(NOT_WAITING));
            return None;
        };
        if let Some(why) = refusal(&q, action) {
            self.notice = Some(why);
            return None;
        }
        if self.answer_pending() {
            return None;
        }
        Some(q)
    }

    fn open_reply(&mut self, action: InstructAction) -> Vec<Effect> {
        if let Some(q) = self.prelude(action) {
            self.begin_reply(&q, action, None);
        }
        vec![]
    }

    /// Opens the typing screen for `request_id`; the text of the same question and action is kept.
    fn begin_reply(
        &mut self,
        q: &InputRequest,
        action: InstructAction,
        model: Option<(String, String)>,
    ) {
        let request_id = q.request_id.as_str();
        let keep = self.reply.as_ref().is_some_and(|r| {
            r.request_id == request_id
                && r.action == action
                && r.model == model
                && r.closed.is_none()
        });
        if keep {
            if let Some(r) = &mut self.reply {
                r.error = None;
            }
        } else {
            self.reply = Some(Reply {
                action,
                request_id: request_id.to_string(),
                text: String::new(),
                model,
                title: q.title.clone(),
                last_words: q.last_message.clone(),
                error: None,
                sending: false,
                closed: None,
            });
        }
        self.notice = None;
        self.screen = Screen::Reply;
    }

    /// `s` on the Runs list and `/settings`: the Settings screen opens over this one and asks for the file; Esc comes back.
    fn open_settings(&mut self) -> Vec<Effect> {
        self.settings.open(self.screen, self.direct.models);
        self.screen = Screen::Settings;
        vec![Effect::Settings(settings::Job::Load)]
    }

    fn settings_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let cx = settings::Cx {
            now_ms: self.key_ms,
            calm: self.calm,
        };
        let (effects, exit) = self.settings.key(key, &cx);
        match exit {
            settings::Exit::Stay => effects,
            settings::Exit::Back => {
                self.screen = self.settings.back();
                effects
            }
            settings::Exit::Quit => self.quit_request(),
        }
    }

    /// The answer of the Settings screen's work. A key it wrote that the running app reads takes effect now: the bell and the notification,
    /// the model and the repository the next composer opens with. The rest apply to the next run or after a restart, as their rows say.
    fn settings_done(&mut self, done: settings::Done) -> Vec<Effect> {
        let Some(key) = self.settings.done(done) else {
            return vec![];
        };
        let Some(snap) = self.settings.snapshot() else {
            return vec![];
        };
        let (notify, bell, mascot) = (snap.notify, snap.bell, snap.mascot);
        let snap_motion = snap.mascot_motion;
        let (model, repo) = (snap.default_model.clone(), snap.default_repo.clone());
        let engine = snap.default_engine.clone();
        match key {
            SettingKey::UiNotify => self.set_notify(notify),
            SettingKey::UiBell => self.set_bell(bell),
            SettingKey::UiMascot => self.head = Head::for_mode(mascot, self.env_look),
            SettingKey::UiMascotMotion => self.set_mascot_motion(snap_motion),
            SettingKey::DefaultModel => self.direct.settings.prefs.config = model,
            SettingKey::DefaultEngine => {
                self.file_engine = engine;
                if !self.composer.engine.chosen {
                    self.composer.engine = self.initial_engine_pick();
                }
            }
            SettingKey::DefaultRepo => {
                // The box of the New run form follows while it still holds the old prefill; text the person typed there stays.
                let was = self.default_repo.clone().unwrap_or_default();
                if self.composer.repo.text() == was {
                    self.composer
                        .repo
                        .set_text(repo.as_deref().unwrap_or_default());
                }
                self.default_repo = repo;
            }
            _ => {}
        }
        vec![]
    }

    fn open_models(&mut self) -> Vec<Effect> {
        if self.prelude(InstructAction::TryModel).is_some() {
            self.model_cursor = 0;
            self.notice = None;
            self.screen = Screen::Models;
        }
        vec![]
    }

    fn models_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(q) = self.question().cloned() else {
            self.screen = Screen::Dashboard;
            return vec![];
        };
        let last = q.models.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => self.screen = Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => {
                self.model_cursor = self.model_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.model_cursor = (self.model_cursor + 1).min(last);
            }
            KeyCode::Enter => {
                if let Some(m) = q.models.get(self.model_cursor) {
                    let pick = Some((m.id.clone(), m.name.clone()));
                    self.begin_reply(&q, InstructAction::TryModel, pick);
                }
            }
            _ => {}
        }
        vec![]
    }

    fn reply_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(r) = self.reply.as_mut() else {
            self.screen = Screen::Dashboard;
            return vec![];
        };
        match key.code {
            KeyCode::Esc => {
                // The question stays open and the text stays with it; a text whose question closed has no use left.
                if r.closed.is_some() && !r.sending {
                    self.reply = None;
                }
                self.screen = Screen::Dashboard;
                return vec![];
            }
            KeyCode::Char('s') if ctrl => return self.send_reply(),
            _ => {}
        }
        if ctrl || r.sending || r.closed.is_some() {
            return vec![];
        }
        match key.code {
            KeyCode::Enter => r.text.push('\n'),
            KeyCode::Backspace => {
                r.text.pop();
            }
            KeyCode::Char(c) => r.text.push(c),
            _ => return vec![],
        }
        r.error = None;
        vec![]
    }

    /// Ctrl-S on the typing screen: checks the text the way the server will and sends it.
    fn send_reply(&mut self) -> Vec<Effect> {
        let open = self.question().map(|q| q.request_id.clone());
        let run_id = self.focused_run().unwrap_or_default().to_string();
        let Some(r) = self.reply.as_mut() else {
            return vec![];
        };
        if r.sending || r.closed.is_some() {
            return vec![];
        }
        if open.as_deref() != Some(r.request_id.as_str()) {
            r.closed = Some("it is no longer open".into());
            return vec![];
        }
        let text = r.text.trim().to_string();
        // The server's rule for a text, checked before anything is sent (ham_core::input::check_text): a message needs some text of at most
        // 262,144 bytes; the text of a fresh session or a model switch is optional but has the same limit.
        if (r.action == InstructAction::ContinueWithPrompt || !text.is_empty())
            && let Err(why) = check_text(&text)
        {
            r.error = Some(why);
            return vec![];
        }
        r.sending = true;
        r.error = None;
        let answer = InputAnswer {
            request_id: r.request_id.clone(),
            action: r.action,
            text: (!text.is_empty()).then_some(text),
            model: r.model.as_ref().map(|(id, _)| id.clone()),
        };
        self.inflight = Some(answer.action);
        vec![Effect::Answer { run_id, answer }]
    }

    /// The first `f`: asks for the second, like `c` before a cancel.
    fn ask_finish(&mut self) -> Vec<Effect> {
        if self.prelude(InstructAction::Finish).is_some() {
            self.confirm_finish = true;
            self.notice = self
                .focused_run()
                .map(|r| format!("{ASK_FINISH}{r} with what exists"));
        }
        vec![]
    }

    /// The second `f`: finish the run with what exists.
    fn finish_run(&mut self) -> Vec<Effect> {
        let Some(q) = self.prelude(InstructAction::Finish) else {
            return vec![];
        };
        let run_id = self.focused_run().unwrap_or_default().to_string();
        self.inflight = Some(InstructAction::Finish);
        self.notice = None;
        vec![Effect::Answer {
            run_id,
            answer: InputAnswer {
                request_id: q.request_id,
                action: InstructAction::Finish,
                text: None,
                model: None,
            },
        }]
    }

    /// The server's answer to [`Effect::Answer`]. A refusal keeps the typed text and shows the server's plain message; both outcomes
    /// ask for a poll at once, so the card shows what really happened.
    fn answered(&mut self, result: Result<InputAccepted, String>) -> Vec<Effect> {
        self.send_answer_done(&result);
        let inflight = self.inflight.take();
        match result {
            Ok(a) => {
                self.sent = Some(Sent {
                    request_id: a.request_id,
                    at: self.clock,
                });
                if let Some(r) = &mut self.reply {
                    r.sending = false;
                    r.error = None;
                }
                self.notice = Some(sent_notice(self.run_engine().name, inflight));
                if self.screen == Screen::Reply {
                    self.screen = Screen::Dashboard;
                }
            }
            Err(e) => {
                if let Some(r) = &mut self.reply {
                    r.sending = false;
                    r.error = Some(e.clone());
                }
                self.notice = Some(e);
            }
        }
        self.next_poll_at = self.clock;
        self.poll_effect()
    }

    /// After a poll: the bell for a question the screen had not shown, and the end of the one it was showing.
    fn track_input(&mut self) -> Vec<Effect> {
        let open = self.cached.as_ref().and_then(|v| v.input.open.clone());
        let open_id = open.as_ref().map(|o| o.request_id.clone());
        if self.open_id != open_id
            && let Some(prev) = self.open_id.take()
        {
            self.close_input(&prev);
        }
        self.open_id = open_id;
        let mut effects = vec![];
        let run = self.focused_run().unwrap_or_default().to_string();
        if let Some(o) = open
            && self.rung.screen_rings(&run, &o.request_id)
        {
            if self.bell_on {
                effects.push(Effect::Bell);
            }
            // Away from the screen: a terminal notification too, when the person asked for it.
            if self.notify_on
                && !self.focused
                && let Some(run_id) = self.focused_run()
            {
                effects.push(Effect::Notify(Alert {
                    run_id: run_id.to_string(),
                    bell: false,
                    notify: true,
                    tmux: self.tmux,
                }));
            }
        }
        effects
    }

    /// The question `prev` is no longer the open one: it was answered, expired, replaced or ended.
    fn close_input(&mut self, prev: &str) {
        self.sent = None;
        self.inflight = None;
        self.confirm_finish = false;
        let how = self.cached.as_ref().and_then(|v| {
            v.input
                .history
                .iter()
                .rev()
                .find(|h| h.request_id == prev)
                .map(|h| h.how.clone())
        });
        if let Some(words) = how.as_deref().and_then(outcome_words) {
            self.notice = Some(words.to_string());
        }
        // A half-written answer stays in the box; the box says that the question it was for has closed.
        if self.screen == Screen::Dashboard && self.chat_box().is_some_and(|b| !b.is_empty()) {
            self.closed_banner = Some(
                how.as_deref()
                    .and_then(outcome_words)
                    .unwrap_or("it ended")
                    .to_string(),
            );
        }
        if self.screen == Screen::Models {
            self.screen = Screen::Dashboard;
        }
        if let Some(r) = &mut self.reply
            && r.request_id == prev
        {
            if self.screen == Screen::Reply {
                r.closed = Some(
                    how.as_deref()
                        .and_then(outcome_words)
                        .unwrap_or("it ended")
                        .to_string(),
                );
                r.sending = false;
            } else {
                self.reply = None;
            }
        }
    }
}

impl Composer {
    /// A fresh composer: the repository prefilled from `default_repo`, the model on the user's default. A stored choice that
    /// names nothing selectable falls back to the list's default with a note.
    fn new(default_repo: Option<&str>, d: &Direct) -> Self {
        Self {
            prompt: ChatBox::new(Kind::Multi),
            kind: TargetKind::Repo,
            repo: ChatBox::with_text(Kind::Token, default_repo.unwrap_or_default()),
            base: ChatBox::new(Kind::Token),
            options: ChatBox::new(Kind::Line),
            focus: Field::Prompt,
            error: None,
            notice: None,
            submitting: false,
            direct: initial_pick(d),
            engine: EnginePick::default_pick(),
            sharing: None,
        }
    }
}

/// The composer's pick at its opening: the user's default; a stored choice that names nothing selectable falls back to the list's
/// default with a note.
fn initial_pick(d: &Direct) -> DirectPick {
    let index_of = |id: &str| d.models.models().iter().position(|m| m.id == id);
    let first = d
        .models
        .models()
        .iter()
        .position(|m| m.selectable())
        .unwrap_or(0);
    match resolve(d.models, None, &d.settings.prefs) {
        Ok(r) => DirectPick {
            index: index_of(&r.model.id).unwrap_or(first),
            source: PickSource::Default(r.source),
            note: None,
            fallback: false,
            saved: None,
        },
        Err(e) => {
            let fallback = d
                .models
                .default_model()
                .and_then(|m| index_of(&m.id))
                .unwrap_or(first);
            let name = d
                .models
                .models()
                .get(fallback)
                .map_or("", |m| m.name.as_str());
            DirectPick {
                index: fallback,
                source: PickSource::Default(ModelSource::BuiltIn),
                fallback: true,
                note: Some(match &e {
                    ModelError::Unknown { input, from, .. } => format!(
                        "{} {input:?} is not a supported model; this run starts on {name}. Pick another here; ham models lists them.",
                        from.label()
                    ),
                    ModelError::NotSmoked { input, from, .. } => format!(
                        "{} {input:?} has no passing smoke record yet; this run starts on {name}. Pick another here; ham models lists them.",
                        from.label()
                    ),
                    other => format!("{other}; this run starts on {name}."),
                }),
                saved: None,
            }
        }
    }
}

/// What the Options box takes, for its error lines and its empty-box hint.
const OPTIONS_TAKE: &str = "--rules <dir>, --no-rules, --continue <file>, --input-wait <minutes>, --goal on|off, --research, --no-research, --allow-third-party, --compute auto|microvm|instances, --max-run-hours <hours>, --provider-sharing on|off, --attach <file> and --customer-content";

/// The words of the Options box, split as a shell would split them for quotes (single and double quotes keep a word with a space whole;
/// there are no escapes). An option the box does not take, a word that belongs to no option, and an option that is missing its value
/// are refused with a line that names what the box takes.
/// The Options box text for the start options of `args`: the words of the command line, a word with a space in quotes.
fn option_text(args: &DirectArgs) -> String {
    let quote = |w: &str| {
        if !w.contains(char::is_whitespace) && !w.contains(['"', '\'']) {
            w.to_string()
        } else if !w.contains('"') {
            format!("\"{w}\"")
        } else if !w.contains('\'') {
            format!("'{w}'")
        } else {
            w.to_string()
        }
    };
    let mut words: Vec<String> = Vec::new();
    if let Some(r) = &args.rules {
        words.extend(["--rules".to_string(), quote(r)]);
    }
    if args.no_rules {
        words.push("--no-rules".to_string());
    }
    if let Some(c) = &args.continue_file {
        words.extend(["--continue".to_string(), quote(c)]);
    }
    if let Some(m) = args.input_wait {
        words.extend(["--input-wait".to_string(), m.to_string()]);
    }
    if let Some(g) = args.goal {
        words.extend([
            "--goal".to_string(),
            if g { "on" } else { "off" }.to_string(),
        ]);
    }
    match args.research {
        Some(true) => words.push("--research".to_string()),
        Some(false) => words.push("--no-research".to_string()),
        None => {}
    }
    if args.allow_third_party {
        words.push("--allow-third-party".to_string());
    }
    // Every word, `auto` too: an explicit `--compute auto` wins over `run.compute` of the file, so the form keeps it.
    if let Some(c) = args.compute {
        words.extend(["--compute".to_string(), c.word().to_string()]);
    }
    if let Some(h) = args.max_run_hours {
        words.extend(["--max-run-hours".to_string(), h.to_string()]);
    }
    if let Some(p) = args.provider_sharing {
        words.extend([
            "--provider-sharing".to_string(),
            if p { "on" } else { "off" }.to_string(),
        ]);
    }
    for f in &args.attach {
        words.extend(["--attach".to_string(), quote(f)]);
    }
    words.join(" ")
}

/// The word in the Options box that marks a run as customer content; the parser never sees it.
const CUSTOMER_CONTENT_FLAG: &str = "--customer-content";

fn option_words(text: &str) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_word = false;
    for ch in text.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                has_word = true;
            }
            None if ch.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            None => {
                cur.push(ch);
                has_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("Options has an unclosed quote".to_string());
    }
    if has_word {
        words.push(cur);
    }
    let takes = |why: &str| format!("Options takes only {OPTIONS_TAKE}: {why}");
    let mut wants_value: Option<&str> = None;
    for w in &words {
        if let Some(flag) = wants_value.take() {
            if w.starts_with("--") {
                return Err(format!("{flag} needs a value: {}", value_hint(flag)));
            }
            continue;
        }
        match w.as_str() {
            "--no-rules"
            | "--research"
            | "--no-research"
            | "--allow-third-party"
            | CUSTOMER_CONTENT_FLAG => {}
            "--rules" => wants_value = Some("--rules"),
            "--continue" => wants_value = Some("--continue"),
            "--input-wait" => wants_value = Some("--input-wait"),
            "--goal" => wants_value = Some("--goal"),
            "--compute" => wants_value = Some("--compute"),
            "--max-run-hours" => wants_value = Some("--max-run-hours"),
            "--provider-sharing" => wants_value = Some("--provider-sharing"),
            "--attach" => wants_value = Some("--attach"),
            "--engine" => {
                return Err(takes(
                    "choose the engine in the Engine box: the up and down keys on the Target field",
                ));
            }
            other => return Err(takes(&format!("not {other}"))),
        }
    }
    if let Some(flag) = wants_value {
        return Err(format!("{flag} needs a value: {}", value_hint(flag)));
    }
    Ok(words)
}

fn value_hint(flag: &str) -> &'static str {
    match flag {
        "--rules" => "a directory or a .md file",
        "--continue" | "--attach" => "a file",
        "--goal" | "--provider-sharing" => "on or off",
        "--compute" => "auto, microvm or instances",
        "--max-run-hours" => "whole hours from 1 to 720",
        _ => "whole minutes from 0 to 43200",
    }
}

fn status_word(s: harness_protocol::goal::GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Unix seconds as UTC civil time: year, month, day, hour, minute, second (days to a date by the civil-from-days algorithm).
fn utc_civil(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86_400);
    let secs = u32::try_from(epoch.rem_euclid(86_400)).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, secs / 3600, (secs % 3600) / 60, secs % 60)
}

#[cfg(test)]
mod refused_tests {
    use super::*;

    /// The longest run id the service makes.
    const ID: &str = "01K6A0000000000000000000R1";

    /// Each sentence a start that did not start can say on the notice row, for each of the three starts, fits the one row of the
    /// narrowest screen with the longest run id; a long reason is cut to a short sentence that points to the card on the run's own screen and
    /// names the run on every other screen. The resume's wording is not reached through `start_refused` (the Runs screen's answer writes it
    /// whole), so this test is the only thing that holds it.
    #[test]
    fn b832_every_notice_of_a_start_that_did_not_start_fits_one_row() {
        for what in [Refused::Retry, Refused::FollowUp, Refused::Resume] {
            for here in [true, false] {
                let short = what.notice(ID, "the service did not answer", here);
                assert!(short.contains("the service did not answer"), "{short}");
                let long = what.notice(ID, &"x".repeat(600), here);
                assert!(long.chars().count() <= NOTICE_COLUMNS, "{long}");
                assert_eq!(long.contains("the card above says why"), here, "{long}");
                assert_eq!(long.contains(&short_id(ID)), !here, "{long}");
                assert!(long.contains(what.word()), "{long}");
            }
            let row = what.runs_row(ID);
            assert!(row.chars().count() <= NOTICE_COLUMNS, "{row}");
            assert!(row.contains(ID) && row.contains(what.word()), "{row}");
            assert!(!what.title().is_empty());
        }
    }
}
