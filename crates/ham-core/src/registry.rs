//! The command registry: one table that says what `ham` can be asked to do, by name.
//!
//! The same table feeds the parser's idea of a subcommand ([`subcommand`], [`subcommand_names`]), the `/help` text ([`help_text`],
//! [`command_help`]), the popup of the chat box ([`popup`]) and the router (`crate::router`), so a word the popup shows is a word
//! the parser reads. Per entry: the name, aliases, the shape of the arguments (how many positional words the shell
//! form needs, whether the first is a run id, whether a text payload follows a structured prefix), one line of help, the
//! [`Effects`] it declares, whether it is built, and (reserved) the engine capability it needs.
//!
//! Two kinds of entry exist. A [`Kind::Subcommand`] entry is a subcommand of [`crate::command::parse`]: the tests fail when the
//! parser has a subcommand with no entry, and when an entry claims to be one and the parser does not know it. A [`Kind::TuiOnly`]
//! entry (`/help`, `/new`, `/open`, `/send`, `/mouse` and the other words of the terminal app) has no argv form. A word marked
//! [`Availability::Planned`] (none is, since `/mouse` was built) is refused with a stable text ("<word> is not available in this release") and is left out of the
//! popup, `/help` and the suggestions, so nothing is advertised that does nothing. A command joins by adding an entry: `push`
//! did, and so did `continue` and `lifecycle init`.
//!
//! The registry owns no state: the focused run, the confirmation and the handlers live elsewhere (`crate::router`, the `ham` crate).

use crate::command::{
    Command, ConfigCommand, EngineAction, EngineCommand, LifecycleCommand, RulesCommand,
};
use crate::config::KEYS;

/// What a command does to the world, as a set. A command declares the union over all its forms; a parsed invocation
/// declares its own ([`effects_of`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects(u8);

impl Effects {
    /// Reads and prints; changes nothing.
    pub const NONE: Effects = Effects(0);
    /// Changes a run on the server: cancel, finish, a new session, another model, a message.
    pub const REMOTE_MUTATION: Effects = Effects(1);
    /// Sends data off this machine: a push; the copy of a bearer link.
    pub const DATA_LEAVES: Effects = Effects(2);
    /// Writes a file on this machine: a download, a clone, a rules file, a config value.
    pub const LOCAL_WRITE: Effects = Effects(4);
    /// Changes a setting that decides where requests go, who signs them or where downloads land.
    pub const CONFIG_SECURITY: Effects = Effects(8);
    /// Starts something that costs money (a new run).
    pub const BILLABLE: Effects = Effects(16);
    /// Needs the terminal for a child process: an editor, or the git and ssh prompts of a push.
    pub const TERMINAL_HANDOFF: Effects = Effects(32);

    const NAMES: [(Effects, &str); 6] = [
        (Effects::REMOTE_MUTATION, "changes a run"),
        (Effects::DATA_LEAVES, "sends data off this machine"),
        (Effects::LOCAL_WRITE, "writes a file on this machine"),
        (
            Effects::CONFIG_SECURITY,
            "changes where requests go or who signs them",
        ),
        (Effects::BILLABLE, "starts a billable run"),
        (Effects::TERMINAL_HANDOFF, "needs your terminal"),
    ];

    pub const fn union(self, other: Effects) -> Effects {
        Effects(self.0 | other.0)
    }

    pub const fn contains(self, other: Effects) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The class a person sees: no effect is read-only, only a local write is a local write, any other effect is run-changing or
    /// off-machine.
    pub fn class(self) -> EffectClass {
        if self.is_empty() {
            EffectClass::ReadOnly
        } else if self == Effects::LOCAL_WRITE {
            EffectClass::LocalWrite
        } else {
            EffectClass::ChangesRun
        }
    }

    /// One short phrase per effect in the set.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(fx, _)| self.contains(*fx))
            .map(|(_, n)| *n)
            .collect()
    }
}

/// The class a person sees, from the effects (see [`Effects::class`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectClass {
    /// Reads and prints; runs at once.
    ReadOnly,
    /// Only writes a file on this machine; runs at once into a fresh destination.
    LocalWrite,
    /// Changes a run, sends data off the machine, changes a security setting, hands the terminal over or starts a billable run.
    /// Shows a preview and needs a second Enter.
    ChangesRun,
}

impl EffectClass {
    /// Order of strength.
    pub fn rank(self) -> u8 {
        match self {
            EffectClass::ReadOnly => 0,
            EffectClass::LocalWrite => 1,
            EffectClass::ChangesRun => 2,
        }
    }

    /// True when the chat box shows a preview line and waits for a second Enter.
    pub fn needs_confirm(self) -> bool {
        self == EffectClass::ChangesRun
    }

    /// Words for the popup and `/help`.
    pub fn label(self) -> &'static str {
        match self {
            EffectClass::ReadOnly => "reads only",
            EffectClass::LocalWrite => "writes a file on this machine",
            EffectClass::ChangesRun => {
                "changes a run, sends data or changes a setting: asks for a second Enter"
            }
        }
    }
}

/// Where an entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A subcommand of `ham_core::command::parse`.
    Subcommand,
    /// A word of the terminal app only; it has no argv form.
    TuiOnly,
}

/// Whether the first positional word of a command is a run id (the router fills the focused run in when it is missing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunArg {
    /// The command takes no run id.
    None,
    /// A run id may be given; without one the command acts on its default (`watch`: the newest run) or the focused run.
    Optional,
    /// A run id is required; the chat fills in the focused run when [`Entry::min_args`] words are not given.
    Required,
}

/// Whether a command takes a free-text payload after its structured prefix. The payload is never tokenised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextArg {
    None,
    Optional,
    Required,
}

/// Whether the word works in this release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Built,
    /// Not built yet; the string is the plan key of the issue that builds it. Typing it gets a stable refusal.
    Planned(&'static str),
}

/// One command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: &'static str,
    /// Other words that mean the same; the router rewrites them to `name`.
    pub aliases: &'static [&'static str],
    pub kind: Kind,
    /// The arguments after the name as the popup and `/help` print them (the slash form).
    pub usage: &'static str,
    /// One line, at most 80 characters.
    pub help: &'static str,
    /// The effects of all forms of the command; [`class_of`] and [`effects_of`] give those of a parsed one.
    pub effects: Effects,
    pub run_arg: RunArg,
    /// How many positional words the shell form needs, the run id included. A run id given with fewer words than this is taken from
    /// the focused run.
    pub min_args: u8,
    /// The payload after the prefix. `say`, `new-session`, `try-model` and `send` have one.
    pub text: TextArg,
    /// How many structured words (a model id) stand between the run id and the payload.
    pub lead: u8,
    /// An argument the box can ask for on the preview line when it is missing (`push` asks for its `git-url`).
    pub ask: Option<&'static str>,
    /// The engine capability the command needs, a control or feature word of the session's capability record (`message.now` for `now`).
    /// The popup leaves the command out when the record lacks the word.
    pub needs: Option<&'static str>,
    pub status: Availability,
}

impl Entry {
    /// The class a person sees for the strongest form of the command.
    pub fn class(&self) -> EffectClass {
        self.effects.class()
    }

    /// True when the slash form takes arguments: the popup completes `/word ` instead of running it.
    pub fn has_args(&self) -> bool {
        !self.usage.is_empty()
    }
}

const fn sub(
    name: &'static str,
    usage: &'static str,
    help: &'static str,
    effects: Effects,
    run_arg: RunArg,
    min_args: u8,
) -> Entry {
    Entry {
        name,
        aliases: &[],
        kind: Kind::Subcommand,
        usage,
        help,
        effects,
        run_arg,
        min_args,
        text: TextArg::None,
        lead: 0,
        ask: None,
        needs: None,
        status: Availability::Built,
    }
}

const fn tui(
    name: &'static str,
    usage: &'static str,
    help: &'static str,
    run_arg: RunArg,
    status: Availability,
) -> Entry {
    Entry {
        kind: Kind::TuiOnly,
        status,
        ..sub(name, usage, help, Effects::NONE, run_arg, 0)
    }
}

use Availability::Built;
use Effects as Fx;
use RunArg::{None as NoRun, Optional, Required};

const SEND_NAME: &str = "send";
const NOW_NAME: &str = "now";
const CONTINUE_NAME: &str = "continue";

/// The registry. The subcommands come first, in the order of `command::SUBCOMMANDS`; the terminal app's own words follow.
pub const ENTRIES: &[Entry] = &[
    sub(
        "runs",
        "[--probes]",
        "List your runs, newest first",
        Fx::NONE,
        NoRun,
        0,
    ),
    sub(
        "watch",
        "[<runId>]",
        "Follow a run to its end (in the app: open the run)",
        Fx::NONE,
        Optional,
        0,
    ),
    sub(
        "cancel",
        "<runId>",
        "Ask Cline to stop; the work so far is kept",
        Fx::REMOTE_MUTATION,
        Required,
        1,
    ),
    sub(
        "models",
        "[--server]",
        "List the models you can pick (--server: the stage's list)",
        Fx::NONE,
        NoRun,
        0,
    ),
    sub(
        "pr",
        "<runId>",
        "Print the pull request link of a run",
        Fx::NONE,
        Required,
        1,
    ),
    sub(
        "pull",
        "<runId> [--artifact zip|partial|evidence | --snapshot]",
        "Download a run's result to this machine; a stopped download resumes",
        Fx::LOCAL_WRITE,
        Required,
        1,
    ),
    sub(
        "clone",
        "<runId> [dir]",
        "Make a local git repository from a run's zip",
        Fx::LOCAL_WRITE,
        Required,
        1,
    ),
    Entry {
        ask: Some("git-url"),
        ..sub(
            "push",
            "<runId> <git-url> [--branch <name>]",
            "Push a run's code to another git repository with your own git login",
            Fx::DATA_LEAVES
                .union(Fx::LOCAL_WRITE)
                .union(Fx::TERMINAL_HANDOFF),
            Required,
            2,
        )
    },
    sub(
        "link",
        "<runId>",
        "Make a fresh download link for a run's zip (15 minutes)",
        Fx::NONE,
        Required,
        1,
    ),
    sub(
        "doctor",
        "",
        "Check your identity, the API, the model list and the allowlist",
        Fx::NONE,
        NoRun,
        0,
    ),
    sub(
        "rules",
        "init | edit [<file.md>|continue] | show",
        "Your rules files: write the defaults, edit, show",
        Fx::LOCAL_WRITE.union(Fx::TERMINAL_HANDOFF),
        NoRun,
        0,
    ),
    sub(
        "config",
        "[get [<key>] | set <key> <value> | unset <key> | show | check | edit | docs | migrate]",
        "Show or change ham's settings",
        Fx::LOCAL_WRITE
            .union(Fx::CONFIG_SECURITY)
            .union(Fx::TERMINAL_HANDOFF),
        NoRun,
        0,
    ),
    sub(
        "engine",
        "<name> init|show|check|edit|set <key> <value>|unset <key>|import <file> [--replace]",
        "Show or change the settings file of an engine",
        Fx::LOCAL_WRITE.union(Fx::TERMINAL_HANDOFF),
        NoRun,
        2,
    ),
    Entry {
        text: TextArg::Required,
        ..sub(
            "say",
            "[--run <id>] <text>",
            "Answer Cline's open question with a message",
            Fx::REMOTE_MUTATION,
            Required,
            2,
        )
    },
    Entry {
        text: TextArg::Optional,
        ..sub(
            "new-session",
            "[--run <id>] [<text>]",
            "Answer with a fresh Cline session on the same files",
            Fx::REMOTE_MUTATION,
            Required,
            1,
        )
    },
    Entry {
        text: TextArg::Optional,
        lead: 1,
        ..sub(
            "try-model",
            "[--run <id>] <model> [<text>]",
            "Answer by switching to another model the question offers",
            Fx::REMOTE_MUTATION,
            Required,
            2,
        )
    },
    sub(
        "finish",
        "<runId>",
        "Answer by finishing the run with what exists",
        Fx::REMOTE_MUTATION,
        Required,
        1,
    ),
    Entry {
        text: TextArg::Optional,
        ..sub(
            "continue",
            "[--run <id>] [<text>]",
            "Start a new run from the code a finished run delivered",
            Fx::BILLABLE.union(Fx::TERMINAL_HANDOFF),
            Required,
            1,
        )
    },
    sub(
        "resume",
        "<runId> | --all [--dry-run]",
        "Continue paused and stalled runs from the code they delivered",
        Fx::BILLABLE.union(Fx::REMOTE_MUTATION),
        NoRun,
        0,
    ),
    sub(
        "lifecycle",
        "init <dir> [--kiro]",
        "Copy the lifecycle pack (AGENTS.md and skills) into a repository",
        Fx::LOCAL_WRITE,
        NoRun,
        0,
    ),
    tui(
        "help",
        "[<command>]",
        "List the commands, or explain one",
        NoRun,
        Built,
    ),
    tui("new", "", "Open the New run form", NoRun, Built),
    tui(
        "open",
        "[<runId>]",
        "Open a run's chat view",
        Optional,
        Built,
    ),
    Entry {
        text: TextArg::Required,
        ..tui(
            SEND_NAME,
            "<text>",
            "Send text to the engine as chat, exactly as typed",
            NoRun,
            Built,
        )
    },
    Entry {
        text: TextArg::Required,
        effects: Fx::REMOTE_MUTATION,
        needs: Some("message.now"),
        ..tui(
            "now",
            "<text>",
            "Send a message that interrupts Cline's current turn",
            NoRun,
            Built,
        )
    },
    // The tags of the run on screen. It writes the service (a tag edit), so it asks for a second Enter like every word that changes a run.
    Entry {
        text: TextArg::Required,
        effects: Fx::REMOTE_MUTATION,
        ..tui(
            "tag",
            "<+kind:value | -kind:value | word>...",
            "Add or remove tags on this run",
            NoRun,
            Built,
        )
    },
    // The blocked run on screen is started again with the request it was started with. It starts a billable run, so the screen asks for a
    // second Enter (or a second `r`). It has no argv form: `ham continue` is the command line's follow-up.
    Entry {
        effects: Fx::BILLABLE,
        ..tui(
            "retry",
            "",
            "Start the blocked run on screen again with the same request",
            NoRun,
            Built,
        )
    },
    tui(
        "sessions",
        "",
        "Pick one of your earlier runs from a list",
        NoRun,
        Built,
    ),
    tui(
        "engines",
        "",
        "List the engines and which one is selected",
        NoRun,
        Built,
    ),
    Entry {
        effects: Fx::LOCAL_WRITE,
        ..tui(
            "export",
            "[<file>]",
            "Write the chat to a file",
            NoRun,
            Built,
        )
    },
    tui("copy", "", "Copy the last answer of Cline", NoRun, Built),
    tui(
        "mouse",
        "[on|off]",
        "Turn the mouse on or off for this session",
        NoRun,
        Built,
    ),
    tui(
        "settings",
        "",
        "Open the Settings screen: every config.toml key, changed in place",
        NoRun,
        Built,
    ),
];

/// The registry.
pub fn entries() -> &'static [Entry] {
    ENTRIES
}

/// The entry for `word` (a name or an alias, ASCII case ignored) in `table`.
pub fn find_in<'a>(table: &'a [Entry], word: &str) -> Option<&'a Entry> {
    if word.is_empty() {
        return None;
    }
    table.iter().find(|e| {
        e.name.eq_ignore_ascii_case(word) || e.aliases.iter().any(|a| a.eq_ignore_ascii_case(word))
    })
}

/// The entry for `word` in the registry.
pub fn find(word: &str) -> Option<&'static Entry> {
    find_in(ENTRIES, word)
}

/// The subcommand entry whose name is exactly `word`. This is the parser's test for "is the first argument a subcommand": the shell
/// is case-sensitive and knows no aliases.
pub fn subcommand(word: &str) -> Option<&'static Entry> {
    ENTRIES
        .iter()
        .find(|e| e.kind == Kind::Subcommand && e.name == word)
}

/// The subcommand names, in the parser's order.
pub fn subcommand_names() -> Vec<&'static str> {
    ENTRIES
        .iter()
        .filter(|e| e.kind == Kind::Subcommand)
        .map(|e| e.name)
        .collect()
}

/// True for the entry that sends a message as an interrupt (`/now`).
pub fn is_now(entry: &Entry) -> bool {
    entry.kind == Kind::TuiOnly && entry.name == NOW_NAME
}

/// True for the entry that sends literal chat (`/send`).
pub fn is_send(entry: &Entry) -> bool {
    entry.kind == Kind::TuiOnly && entry.name == SEND_NAME
}

/// True for the entry that starts a follow-up run (`/continue`): the one text command that bills, so its payload is checked for a run id.
pub fn is_continue(entry: &Entry) -> bool {
    entry.kind == Kind::Subcommand && entry.name == CONTINUE_NAME
}

/// The number of single-character edits (insert, delete, replace, or swap of two neighbours) between two words.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let m = b.len();
    let mut before_prev = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(before_prev[j - 2] + 1);
            }
            cur[j] = best;
        }
        std::mem::swap(&mut before_prev, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// The most edits a suggestion may be away from the typed word.
pub const SUGGEST_MAX_EDITS: usize = 2;

/// The built entries of `table` nearest to `word`, as their own names, at most `limit`: a name that starts with the typed word, or a
/// name at most [`SUGGEST_MAX_EDITS`] edits away. Prefix matches come first, then the fewest edits. An alias leads to its entry's
/// name, once. A planned entry is never suggested.
pub fn closest_in(table: &[Entry], word: &str, limit: usize) -> Vec<&'static str> {
    let word = word.to_ascii_lowercase();
    if word.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(bool, usize, &'static str)> = Vec::new();
    for e in table.iter().filter(|e| e.status == Availability::Built) {
        let best = std::iter::once(e.name)
            .chain(e.aliases.iter().copied())
            .map(|w| (!w.starts_with(word.as_str()), edit_distance(&word, w)))
            .filter(|(not_prefix, d)| !*not_prefix || *d <= SUGGEST_MAX_EDITS)
            .min();
        if let Some((not_prefix, d)) = best {
            scored.push((not_prefix, d, e.name));
        }
    }
    scored.sort();
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, name)| name)
        .collect()
}

/// [`closest_in`] over the registry.
pub fn closest(word: &str, limit: usize) -> Vec<&'static str> {
    closest_in(ENTRIES, word, limit)
}

/// The fixed plain phrases: an exact whole submission and the command word it proposes. A bare single word is never in
/// it (a one-word instruction to Cline must stay chat), and the words are lower case.
pub const PHRASES: &[(&str, &str)] = &[
    ("push my code", "push"),
    ("push this run", "push"),
    ("pull the zip", "pull"),
    ("show my runs", "runs"),
    ("cancel this run", "cancel"),
];

/// The phrase `text` is, in the form [`PHRASES`] keeps: trimmed, blanks between words as one, lower case. `None` for text with a line
/// break inside, because a phrase is a whole one-line submission.
pub fn normalize_phrase(text: &str) -> Option<String> {
    let t = text.trim();
    if t.contains('\n') || t.contains('\r') {
        return None;
    }
    Some(
        t.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
    )
}

/// The command word the whole submission `text` proposes, if it is one of [`PHRASES`] (ignoring case and surrounding blanks).
pub fn phrase_command(text: &str) -> Option<&'static str> {
    let flat = normalize_phrase(text)?;
    PHRASES
        .iter()
        .find(|(phrase, _)| *phrase == flat)
        .map(|(_, word)| *word)
}

/// The built entries whose name or alias starts with `prefix` (ASCII case ignored), in registry order: the popup's list as the user
/// types after the slash.
pub fn popup(prefix: &str) -> Vec<&'static Entry> {
    popup_with(prefix, &|_| true)
}

/// [`popup`] for a session that offers only some capabilities: an entry whose [`Entry::needs`] word `offers` refuses is left out.
/// The caller decides what a missing capability record means; `ham_core::engines` offers everything then.
pub fn popup_with(prefix: &str, offers: &dyn Fn(&str) -> bool) -> Vec<&'static Entry> {
    let p = prefix.to_ascii_lowercase();
    ENTRIES
        .iter()
        .filter(|e| e.status == Availability::Built)
        .filter(|e| e.needs.is_none_or(offers))
        .filter(|e| e.name.starts_with(&p) || e.aliases.iter().any(|a| a.starts_with(&p)))
        .collect()
}

/// The config keys that decide where requests go, who signs them, where downloads land, which backend takes a start that names none and
/// which backend may take customer content. Changing one is a security change: it needs a second Enter and the key typed out.
pub const SECURITY_KEYS: [&str; 6] = [
    "api_endpoint",
    "aws_profile",
    "region",
    "download_dir",
    "default_backend",
    "customer_content",
];

/// True when `key` (either spelling) is one of [`SECURITY_KEYS`].
pub fn is_security_key(key: &str) -> bool {
    KEYS.iter()
        .find(|(file, snake)| *file == key || *snake == key)
        .is_some_and(|(_, snake)| SECURITY_KEYS.contains(snake))
}

/// The effects of one parsed command.
pub fn effects_of(command: &Command) -> Effects {
    match command {
        Command::StartDirect(_) => Effects::BILLABLE,
        Command::Rules(
            RulesCommand::Init | RulesCommand::InitNoSkills | RulesCommand::InitPack { .. },
        ) => Effects::LOCAL_WRITE,
        Command::Lifecycle(LifecycleCommand::Init { .. }) => Effects::LOCAL_WRITE,
        // A follow-up costs money and runs as a child `ham continue` that has the terminal: with no text the editor needs it, and with
        // text the in-process runner of the box could not follow the new run, which the child does until it ends.
        Command::Continue(..) => Effects::BILLABLE.union(Effects::TERMINAL_HANDOFF),
        Command::Resume { dry_run, .. } => {
            if *dry_run {
                Effects::NONE
            } else {
                Effects::BILLABLE.union(Effects::REMOTE_MUTATION)
            }
        }
        Command::Rules(RulesCommand::Edit { .. }) => {
            Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF)
        }
        Command::Rules(RulesCommand::Show { .. }) => Effects::NONE,
        Command::Config(
            ConfigCommand::List
            | ConfigCommand::Get { .. }
            | ConfigCommand::Show
            | ConfigCommand::Check { .. }
            | ConfigCommand::Docs
            | ConfigCommand::Migrate { dry_run: true },
        ) => Effects::NONE,
        Command::Config(ConfigCommand::Migrate { dry_run: false }) => Effects::LOCAL_WRITE,
        // The editor needs the terminal, so it is a hand-off as well as a write.
        Command::Config(ConfigCommand::Edit) => {
            Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF)
        }
        Command::Config(ConfigCommand::Set { key, .. } | ConfigCommand::Unset { key }) => {
            if is_security_key(key) {
                Effects::LOCAL_WRITE.union(Effects::CONFIG_SECURITY)
            } else {
                Effects::LOCAL_WRITE
            }
        }
        Command::Engine(EngineCommand { action, .. }) => match action {
            EngineAction::Show | EngineAction::Check => Effects::NONE,
            // The editor needs the terminal, so it is a hand-off as well as a write.
            EngineAction::Edit => Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF),
            EngineAction::Init
            | EngineAction::Set { .. }
            | EngineAction::Unset { .. }
            | EngineAction::Import { .. } => Effects::LOCAL_WRITE,
        },
        Command::Runs { .. }
        | Command::Watch { .. }
        | Command::Models
        | Command::ModelsServer
        | Command::Pr { .. }
        | Command::Link { .. }
        | Command::Doctor { .. } => Effects::NONE,
        Command::Pull { .. } | Command::Clone { .. } => Effects::LOCAL_WRITE,
        Command::Push { .. } => Effects::DATA_LEAVES
            .union(Effects::LOCAL_WRITE)
            .union(Effects::TERMINAL_HANDOFF),
        Command::Cancel { .. }
        | Command::Say { .. }
        | Command::NewSession { .. }
        | Command::TryModel { .. }
        | Command::Finish { .. } => Effects::REMOTE_MUTATION,
    }
}

/// The exact class of a parsed command.
pub fn class_of(command: &Command) -> EffectClass {
    effects_of(command).class()
}

fn usage_line(e: &Entry) -> String {
    if e.usage.is_empty() {
        format!("/{}", e.name)
    } else {
        format!("/{} {}", e.name, e.usage)
    }
}

/// The text of `/help`: every built command with its arguments and one line of help, then the rules of the box.
pub fn help_text() -> String {
    let mut out = String::from("Commands (type / for the list; `ham <word>` works too):\n");
    for e in ENTRIES.iter().filter(|e| e.status == Availability::Built) {
        out.push_str(&format!("  {}\n      {}\n", usage_line(e), e.help));
    }
    out.push_str(
        "\nA command that changes a run, sends data or changes a setting shows a preview\n",
    );
    out.push_str("and waits for a second Enter. Pasted text always shows the preview first.\n");
    out.push_str("To send text that starts with / or ham as chat, use /send <text>.\n");
    out
}

/// The help of one command (`/help push`): usage, help line, aliases and what it does to the world. `None` for an unknown word.
pub fn command_help(word: &str) -> Option<String> {
    let e = find(word)?;
    let mut out = format!("{}\n  {}\n", usage_line(e), e.help);
    if !e.aliases.is_empty() {
        let list: Vec<String> = e.aliases.iter().map(|a| format!("/{a}")).collect();
        out.push_str(&format!("  also: {}\n", list.join(", ")));
    }
    match e.status {
        Availability::Built => out.push_str(&format!("  {}\n", e.class().label())),
        Availability::Planned(_) => out.push_str("  not available in this release\n"),
    }
    Some(out)
}

/// `no command "word"`, with `; closest: a, b` when some built command is near (the nearest first, at most three). Both the shell's
/// and the chat box's refusals start with it.
pub fn no_command_text(table: &[Entry], word: &str) -> String {
    let near = closest_in(table, word, 3);
    if near.is_empty() {
        format!("no command \"{word}\"")
    } else {
        format!("no command \"{word}\"; closest: {}", near.join(", "))
    }
}

/// The text of `/help [<command>]`: the whole list, or one command, or the refusal for a word that is none.
pub fn help_for(args: &[String]) -> String {
    match args.first() {
        None => help_text(),
        Some(word) => command_help(word).unwrap_or_else(|| {
            format!(
                "{}. /help lists the commands.\n",
                no_command_text(ENTRIES, word)
            )
        }),
    }
}
