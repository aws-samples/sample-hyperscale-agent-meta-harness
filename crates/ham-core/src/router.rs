//! The router of the chat box: a pure function from the text of the box to what happens next, as settles it.
//!
//! It holds no terminal, network or clock; the chat box (TC1) and the offline scenario tests call the same code. The grammar:
//!
//! The confirmation owns the argv that was previewed, so nothing that happens on the screen afterwards (the focus moving, a poll)
//! changes what runs. The router decides nothing about the engine: a [`RouteOutcome::Message`] is sent as typed by the caller.

use crate::command::{
    Command, ConfigCommand, DirectArgs, EngineAction, EngineCommand, Invocation, RulesCommand,
    TargetArg, parse,
};
use crate::followup::continue_preview_tail;
use crate::push::{default_branch, mask_userinfo};
use crate::registry::{self, Availability, EffectClass, Entry, Kind, RunArg, TextArg};
use crate::sanitize::sanitize_line;
use harness_protocol::common::is_ulid;

/// A confirming key that arrives sooner than this after the preview was drawn is ignored (the Enter that sent the line, a repeat
/// of it, or a burst of pasted characters).
pub const CONFIRM_DELAY_MS: u64 = 300;

/// What the box knows beyond the text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteContext {
    /// The run on screen; a command without a run id acts on it.
    pub focused_run: Option<String>,
    /// True when the draft is tainted: a paste put text into it, or it was recalled from history, restored, or
    /// returned from an editor. It stays tainted until the box is emptied.
    pub tainted: bool,
}

/// The line shown before a second Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// One line: what will happen, to which run, and where it goes.
    pub text: String,
    /// What the person must type before Enter: `push` the last four characters of the run id shown above, a security setting its key
    /// name. `None` for Enter alone.
    pub confirm_with: Option<String>,
}

/// What the text of the box means.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteOutcome {
    /// Nothing to send.
    Empty,
    /// A command the shell parser read. `argv` is what was parsed (`parse(&argv) == Ok(invocation)`).
    Command {
        argv: Vec<String>,
        invocation: Invocation,
        class: EffectClass,
        /// Present when `class.needs_confirm()` and for every command of a tainted draft.
        preview: Option<Preview>,
    },
    /// A command that lacks the one argument the box can ask for on the preview line (`push` without its URL). `argv` holds what is
    /// known; [`complete`] adds the answer and runs it through the same parser.
    Incomplete {
        argv: Vec<String>,
        missing: &'static str,
        class: EffectClass,
    },
    /// `ham "<prompt>" --repo ... --direct` typed in the box: open the New run form with these values. Nothing starts.
    StartForm { args: DirectArgs },
    /// A word of the terminal app itself; `args` has the focused run filled in where the word takes one.
    Ui {
        word: &'static str,
        args: Vec<String>,
    },
    /// Chat for the engine, exactly as typed. `literal` is true for `/send`, so the footer can say the text was sent as chat.
    Message { text: String, literal: bool },
    /// Not sent and not run. `suggestions` are the closest command names.
    Refused {
        reason: String,
        suggestions: Vec<String>,
    },
}

fn refuse(reason: impl Into<String>) -> RouteOutcome {
    RouteOutcome::Refused {
        reason: reason.into(),
        suggestions: Vec::new(),
    }
}

/// An unclosed quote in a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitError;

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("usage: the quote in this command is not closed")
    }
}

impl std::error::Error for SplitError {}

/// Splits text into words like a shell: white space (a line break too) separates; `'...'` is literal; `"..."` allows `\"` and `\\`;
/// a backslash outside quotes takes the next character literally; an empty quoted word is a word. Nothing is expanded.
pub fn split_words(text: &str) -> Result<Vec<String>, SplitError> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if SEPARATORS.contains(&c) => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err(SplitError),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\')) => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => return Err(SplitError),
                        },
                        Some(c) => cur.push(c),
                        None => return Err(SplitError),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next().unwrap_or('\\'));
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// The characters that end a word on a command line.
const SEPARATORS: [char; 4] = [' ', '\t', '\r', '\n'];

const CONTROL_REFUSAL: &str = "A command cannot contain control characters.";

/// True when `s` holds a control character other than a line break or a tab. The words of a command may not hide an escape
/// sequence or a bell; a payload is kept exactly as typed.
fn has_control(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
}

/// A lower-case letter, then lower-case letters, digits or hyphens.
fn is_word(token: &str) -> bool {
    token.starts_with(|c: char| c.is_ascii_lowercase())
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The text after the single separating character (a space, a tab, a line break) that ends a word; the rest, which can start with
/// more spaces, is untouched.
fn after_separator(rest: &str) -> &str {
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix(SEPARATORS))
        .unwrap_or(rest)
}

/// What the text starts like.
enum Attempt<'a> {
    /// A first line that starts with `//`.
    DoubleSlash,
    /// `/` with nothing after it on its line.
    LoneSlash,
    /// `/word...` or `ham word...`; `rest` is the text from the character after the word; `body` is the text from the word on.
    Word {
        word: &'a str,
        rest: &'a str,
        body: &'a str,
        slash: bool,
    },
    /// `ham -...` or `ham "...`; `body` is the text after `ham`.
    StartForm { body: &'a str },
}

fn attempt_of(text: &str) -> Option<Attempt<'_>> {
    if let Some(after) = text.strip_prefix('/') {
        if after.starts_with('/') {
            return Some(Attempt::DoubleSlash);
        }
        let first_line = after.split(['\n', '\r']).next().unwrap_or("");
        if first_line.trim_matches([' ', '\t']).is_empty() {
            return Some(Attempt::LoneSlash);
        }
        // A slash, a space and text is not `/word`: the word below is empty, so it is chat.
        let end = after.find(SEPARATORS).unwrap_or(after.len());
        return is_word(&after[..end]).then(|| Attempt::Word {
            word: &after[..end],
            rest: &after[end..],
            body: after,
            slash: true,
        });
    }
    // COMPAT-CAH: for the window of the earlier command name the word `cah` starts a command attempt like `ham`; the rule for `ham` is unchanged.
    let after = text
        .strip_prefix("ham")
        .or_else(|| text.strip_prefix("cah")) // COMPAT-CAH
        .filter(|a| a.starts_with([' ', '\t']))?;
    let second = after.trim_start_matches([' ', '\t']);
    if second.starts_with(['-', '"', '\'']) {
        return Some(Attempt::StartForm { body: after });
    }
    let end = second.find(SEPARATORS).unwrap_or(second.len());
    is_word(&second[..end]).then(|| Attempt::Word {
        word: &second[..end],
        rest: &second[end..],
        body: second,
        slash: false,
    })
}

/// True when `text` starts like a command attempt: `/word`, a lone `/`, `//`, or `ham` followed by a word or a start form. The Runs
/// screen uses it to leave its list unfiltered while a command is typed, so the two never disagree about what is a command.
pub fn is_command_attempt(text: &str) -> bool {
    attempt_of(text).is_some()
}

/// What the box does with `text`. See the module documentation for the grammar.
pub fn route(text: &str, ctx: &RouteContext) -> RouteOutcome {
    route_with(registry::entries(), text, ctx)
}

/// [`route`] over another table (tests use one with aliases).
pub fn route_with(table: &[Entry], text: &str, ctx: &RouteContext) -> RouteOutcome {
    if text.trim().is_empty() {
        return RouteOutcome::Empty;
    }
    let Some(attempt) = attempt_of(text) else {
        return RouteOutcome::Message {
            text: text.to_string(),
            literal: false,
        };
    };
    if matches!(attempt, Attempt::DoubleSlash) {
        return refuse(
            "A line that starts with // is not sent. To send text that starts with a slash as chat, use /send <text>.",
        );
    }
    if matches!(attempt, Attempt::LoneSlash) {
        return refuse("Type a command name after the slash; /help lists them.");
    }
    match attempt {
        Attempt::StartForm { body } if has_control(body) => refuse(CONTROL_REFUSAL),
        Attempt::StartForm { body } => match split_words(body) {
            Ok(words) => start_form(&words),
            Err(e) => refuse(e.to_string()),
        },
        Attempt::Word {
            word,
            rest,
            body,
            slash,
        } => word_outcome(table, word, rest, body, slash, ctx),
        // Answered above.
        Attempt::DoubleSlash | Attempt::LoneSlash => {
            refuse("Type a command name after the slash; /help lists them.")
        }
    }
}

/// `ham "<prompt>" ... --direct` as the shell parses it; only a new run opens the form.
fn start_form(words: &[String]) -> RouteOutcome {
    match parse(words) {
        Ok(Invocation {
            command: Command::StartDirect(args),
            ..
        }) => RouteOutcome::StartForm { args },
        Ok(_) => refuse(format!(
            "\"{}\" is a command name, so it is read as that command, not as a prompt. Put the prompt after --prompt to start a run with it.",
            sanitize_line(words.first().map_or("", String::as_str))
        )),
        Err(e) => refuse(e.to_string()),
    }
}

fn unknown(table: &[Entry], word: &str) -> RouteOutcome {
    let suggestions: Vec<String> = registry::closest_in(table, word, 3)
        .into_iter()
        .map(str::to_string)
        .collect();
    let base = registry::no_command_text(table, word);
    let tail = "To send it as chat, use /send <text>.";
    let reason = if suggestions.is_empty() {
        format!("{base}. /help lists the commands. {tail}")
    } else {
        format!("{base}. {tail}")
    };
    RouteOutcome::Refused {
        reason,
        suggestions,
    }
}

/// The first line of `rest` (a bare carriage return ends a line too), when nothing but blank lines follows it.
fn one_line(rest: &str) -> Result<&str, &'static str> {
    match rest.find(['\n', '\r']) {
        None => Ok(rest),
        Some(i) => {
            let (head, tail) = rest.split_at(i);
            if tail.trim().is_empty() {
                Ok(head)
            } else {
                Err("commands are one line; use /send to send this as chat")
            }
        }
    }
}

/// The shell options that take a value; their value is not a positional word.
const VALUED_OPTIONS: [&str; 14] = [
    "--artifact",
    "--branch",
    "--request",
    "--text",
    "--prompt",
    "--repo",
    "--new-repo",
    "--base",
    "--rules",
    "--model",
    "--continue",
    "--input-wait",
    "--goal",
    "--attach",
];

/// How many positional words `args` holds: not an option and not the value of one. A lone `-` is a word.
fn positionals(args: &[String]) -> usize {
    let mut n = 0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if VALUED_OPTIONS.contains(&a.as_str()) {
            it.next();
        } else if a == "-" || !a.starts_with('-') {
            n += 1;
        }
    }
    n
}

/// The focused run goes in as the run id when fewer positional words are given than the form needs.
fn with_run(entry: &Entry, argv: &mut Vec<String>, focus: Option<&str>) {
    let Some(run) = focus else { return };
    let need = match entry.run_arg {
        RunArg::None => return,
        RunArg::Optional => 1,
        RunArg::Required => usize::from(entry.min_args),
    };
    if positionals(&argv[1..]) < need {
        argv.insert(1, run.to_string());
    }
}

/// The count of repository options on a start line.
fn repository_options(words: &[String]) -> usize {
    words
        .iter()
        .filter(|w| matches!(w.as_str(), "--repo" | "--new-repo" | "--s3-only"))
        .count()
}

fn word_outcome(
    table: &[Entry],
    word: &str,
    rest: &str,
    body: &str,
    slash: bool,
    ctx: &RouteContext,
) -> RouteOutcome {
    let focus = ctx.focused_run.as_deref();
    let Some(entry) = registry::find_in(table, word) else {
        if !slash
            && let Ok(words) = split_words(body)
            && repository_options(&words) == 1
            && words.iter().any(|w| w == "--direct")
        {
            return start_form(&words);
        }
        return unknown(table, word);
    };
    if let Availability::Planned(_) = entry.status {
        return refuse(format!("{} is not available in this release", entry.name));
    }
    if registry::is_send(entry) {
        return send_message(rest);
    }
    if registry::is_now(entry) {
        return now_message(entry, rest);
    }
    if entry.kind == Kind::Subcommand && entry.text != TextArg::None && slash {
        return text_command(entry, rest, focus, ctx.tainted);
    }
    // The shell form: the words of the line, the first line only unless the command takes a text word.
    let line = if entry.text == TextArg::None {
        match one_line(body) {
            Ok(line) => line,
            Err(reason) => return refuse(reason),
        }
    } else {
        body
    };
    if has_control(line) {
        return refuse(CONTROL_REFUSAL);
    }
    let mut argv = match split_words(line) {
        Ok(words) => words,
        Err(e) => return refuse(e.to_string()),
    };
    argv[0] = entry.name.to_string();
    // Only a command without a text payload may omit its run id: `ham say <runId>` must not become a message to the focused
    // run whose text is the run id.
    if entry.text == TextArg::None {
        with_run(entry, &mut argv, focus);
    }
    match entry.kind {
        Kind::TuiOnly => RouteOutcome::Ui {
            word: entry.name,
            args: argv.split_off(1),
        },
        Kind::Subcommand => command_outcome(entry, argv, true, ctx.tainted),
    }
}

/// `/send <text>`: the payload after the single separating space, exactly as typed.
fn send_message(rest: &str) -> RouteOutcome {
    let payload = after_separator(rest);
    if payload.trim().is_empty() {
        return refuse("/send needs the text to send as chat.");
    }
    RouteOutcome::Message {
        text: payload.to_string(),
        literal: true,
    }
}

/// `/now <text>`: the payload after the single separating space, exactly as typed, handed to the screen as the one argument of the word
/// `now`. The screen asks for a second Enter before it interrupts the engine's turn.
fn now_message(entry: &Entry, rest: &str) -> RouteOutcome {
    let payload = after_separator(rest);
    if payload.trim().is_empty() {
        return refuse(
            "/now needs the text to send; it interrupts the current turn and delivers it at once.",
        );
    }
    RouteOutcome::Ui {
        word: entry.name,
        args: vec![payload.to_string()],
    }
}

/// One word of a structured prefix, split like a shell word (quotes and backslashes decoded), and the text after the single
/// separating character that ends it. `None` when no word starts here.
fn take_word(s: &str) -> Result<Option<(String, &str)>, SplitError> {
    let ahead = s.trim_start_matches([' ', '\t']);
    let mut end = 0;
    let (mut single, mut double, mut escaped) = (false, false, false);
    for (i, c) in ahead.char_indices() {
        if escaped {
            escaped = false;
        } else if single {
            single = c != '\'';
        } else if double {
            match c {
                '\\' => escaped = true,
                '"' => double = false,
                _ => {}
            }
        } else {
            match c {
                '\\' => escaped = true,
                '\'' => single = true,
                '"' => double = true,
                c if SEPARATORS.contains(&c) => break,
                _ => {}
            }
        }
        end = i + c.len_utf8();
    }
    if single || double {
        return Err(SplitError);
    }
    if end == 0 {
        return Ok(None);
    }
    let word = split_words(&ahead[..end])?
        .into_iter()
        .next()
        .unwrap_or_default();
    Ok(Some((word, after_separator(&ahead[end..]))))
}

/// `/say`, `/new-session` and `/try-model`: a structured prefix (`--run <id>`, a model) and then the raw payload.
fn text_command(entry: &Entry, rest: &str, focus: Option<&str>, tainted: bool) -> RouteOutcome {
    let mut s = after_separator(rest);
    let mut run: Option<String> = None;
    let ahead = s.trim_start_matches([' ', '\t']);
    if let Some(after) = ahead
        .strip_prefix("--run")
        .filter(|a| a.is_empty() || a.starts_with(SEPARATORS))
    {
        let value = match take_word(after_separator(after)) {
            Ok(v) => v,
            Err(e) => return refuse(e.to_string()),
        };
        let Some((id, next)) = value.filter(|(id, _)| is_ulid(id)) else {
            return refuse("--run takes a run id, the 26 characters `ham runs` prints.");
        };
        run = Some(id);
        s = next;
    }
    // A follow-up bills: a run id typed after `/continue` (the shell form's first word) would be the first word of the prompt, or all of it.
    if run.is_none()
        && registry::is_continue(entry)
        && let Ok(Some((word, after))) = take_word(s)
        && is_ulid(&word)
    {
        return refuse(leading_run_id(&word, after));
    }
    let Some(run) = run.or_else(|| focus.map(str::to_string)) else {
        return refuse("No run is open. Give one with --run <id>.");
    };
    let mut lead: Vec<String> = Vec::new();
    for _ in 0..entry.lead {
        match take_word(s) {
            Ok(Some((word, next))) => {
                if has_control(&word) {
                    return refuse(CONTROL_REFUSAL);
                }
                lead.push(word);
                s = next;
            }
            Ok(None) => break,
            Err(e) => return refuse(e.to_string()),
        }
    }
    prepare_text(entry.name, &run, &lead, s, tainted)
}

/// The refusal for `/continue <runId>...`: the slash word takes the run on screen, or `--run <id>`, so the id is not read as the first
/// word of the text. The reply shows the line to type. `after` is the text that follows the id.
fn leading_run_id(id: &str, after: &str) -> String {
    let rest = after.trim_end();
    let line = if rest.is_empty() {
        format!("/continue --run {id}")
    } else {
        format!("/continue --run {id} {}", clip(rest, 80))
    };
    format!(
        "/continue uses the run on screen, so {id} would be the first word of your text. To follow that run, type: {line}"
    )
}

/// A command with a text payload, prepared the way a typed `/say`, `/new-session` or `/try-model` line is: the run, the structured
/// words (`lead`: the model of `try-model`) and the payload exactly as typed (a payload that starts with `-` goes in as `--text`).
/// The hotkeys `p`, `s` and `m` give it the text of the Reply screen.
pub fn prepare_text(
    name: &str,
    run: &str,
    lead: &[String],
    payload: &str,
    tainted: bool,
) -> RouteOutcome {
    let Some(entry) = registry::subcommand(name).filter(|e| e.text != TextArg::None) else {
        return refuse(format!("{name} takes no text"));
    };
    let mut argv = vec![entry.name.to_string(), run.to_string()];
    argv.extend(lead.iter().cloned());
    // A missing model is the parser's to report: the payload must not be taken for it.
    if lead.len() >= usize::from(entry.lead)
        && !(entry.text == TextArg::Optional && payload.trim().is_empty())
    {
        if payload == "-" {
            return refuse(
                "A message of only - means standard input in the shell; to send a dash as chat, use /send -.",
            );
        }
        if payload.starts_with('-') {
            argv.push("--text".to_string());
        }
        argv.push(payload.to_string());
    }
    command_outcome(entry, argv, false, tainted)
}

fn command_outcome(entry: &Entry, argv: Vec<String>, may_ask: bool, tainted: bool) -> RouteOutcome {
    // One argument the box can ask for: `push` without its URL, when the run id is known.
    if may_ask
        && let Some(missing) = entry.ask
        && argv.get(1).is_some_and(|t| is_ulid(t))
        && positionals(&argv[1..]) + 1 == usize::from(entry.min_args)
    {
        return RouteOutcome::Incomplete {
            argv,
            missing,
            class: entry.class(),
        };
    }
    match parse(&argv) {
        Err(e) => refuse(e.to_string()),
        Ok(invocation) => {
            // The run id is the word the shell would send to the service: a plain id, never a stray character from the line.
            if let Some(id) = run_id_of(&invocation.command).filter(|id| !plain_id(id)) {
                return refuse(format!(
                    "{} is not a run id; use the id `ham runs` prints.",
                    clip(id, 40)
                ));
            }
            let class = registry::class_of(&invocation.command);
            let preview = if class.needs_confirm() || tainted {
                Some(preview_of(&invocation))
            } else {
                None
            };
            RouteOutcome::Command {
                argv,
                invocation,
                class,
                preview,
            }
        }
    }
}

/// A run id as `ham runs` prints it: letters, digits, `-` and `_`, at most 64 (the shape `ham clone` and `ham push` accept).
fn plain_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The run id a parsed command names, if it takes one.
fn run_id_of(command: &Command) -> Option<&str> {
    match command {
        Command::Watch { run_id } => run_id.as_deref(),
        Command::Resume { run_id, .. } => run_id.as_deref(),
        Command::Cancel { run_id }
        | Command::Pr { run_id }
        | Command::Pull { run_id, .. }
        | Command::Clone { run_id, .. }
        | Command::Push { run_id, .. }
        | Command::Link { run_id }
        | Command::Say { run_id, .. }
        | Command::NewSession { run_id, .. }
        | Command::TryModel { run_id, .. }
        | Command::Finish { run_id, .. } => Some(run_id),
        _ => None,
    }
}

/// The prepared invocation for an argv that is already resolved: the hotkeys (`c c`, `f f`), the popup and the plain-phrase chip
/// use it, so every path to a command gets the same parse, class and preview as a typed line.
pub fn prepare_argv(argv: &[String], tainted: bool) -> RouteOutcome {
    let Some(first) = argv.first() else {
        return refuse("There is no command to run.");
    };
    match registry::subcommand(first) {
        Some(entry) => command_outcome(entry, argv.to_vec(), false, tainted),
        None => refuse(registry::no_command_text(registry::entries(), first)),
    }
}

/// The answer to an [`RouteOutcome::Incomplete`]: `value` joins the argv and the whole goes through the same parser as a typed line.
pub fn complete(argv: &[String], value: &str) -> RouteOutcome {
    let value = value.trim();
    let Some(entry) = argv.first().and_then(|w| registry::subcommand(w)) else {
        return refuse("There is no command to finish.");
    };
    if value.is_empty() {
        return refuse(format!(
            "Enter the {} to continue.",
            entry.ask.unwrap_or("missing argument")
        ));
    }
    let mut full = argv.to_vec();
    full.push(value.to_string());
    command_outcome(entry, full, false, false)
}

/// `text` cut to `max` characters with an ellipsis, on one line.
fn clip(text: &str, max: usize) -> String {
    let flat = sanitize_line(text);
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The last four characters of a run id (all of it when shorter): the answer `push` asks for.
fn last_four(run_id: &str) -> String {
    let n = run_id.chars().count();
    run_id.chars().skip(n.saturating_sub(4)).collect()
}

/// The preview text of a `ham continue` line, or `None` for another command: the command, then what starts, the target and that ham waits
/// for the new run. `known` is the target the screen already knows the source run to have, for a line that names none (the line itself is
/// never changed, so the child resolves the target as the shell does).
pub fn continue_preview_text(inv: &Invocation, known: Option<&TargetArg>) -> Option<String> {
    let Command::Continue(d, explicit) = &inv.command else {
        return None;
    };
    Some(format!(
        "Run ham {}.{} {}",
        clip(&inv.to_argv().join(" "), 160),
        if d.prompt.is_empty() {
            " Your editor opens for the text."
        } else {
            ""
        },
        continue_preview_tail(
            d.continue_from.as_deref().unwrap_or_default(),
            explicit.as_ref(),
            known
        )
    ))
}

fn preview_of(inv: &Invocation) -> Preview {
    let plain = |text: String| Preview {
        text,
        confirm_with: None,
    };
    match &inv.command {
        Command::Cancel { run_id } => plain(format!(
            "Cancel run {}: Cline is asked to stop and the work so far is kept.",
            clip(run_id, 40)
        )),
        Command::Finish { run_id, .. } => plain(format!(
            "Finish run {} with what exists. This answers the question Cline asked.",
            clip(run_id, 40)
        )),
        Command::NewSession { run_id, text, .. } => plain(format!(
            "Start a fresh Cline session on the files of run {}{}.",
            clip(run_id, 40),
            if text.is_some() {
                " with your message"
            } else {
                ""
            }
        )),
        Command::TryModel {
            run_id,
            model,
            text,
            ..
        } => plain(format!(
            "Switch run {} to {}{}.",
            clip(run_id, 40),
            clip(model, 40),
            if text.is_some() {
                " and send your message"
            } else {
                ""
            }
        )),
        Command::Say { run_id, text, .. } => plain(format!(
            "Send your message to run {}: \"{}\"",
            clip(run_id, 40),
            clip(text, 60)
        )),
        Command::Push {
            run_id,
            url,
            branch,
        } => Preview {
            text: format!(
                "Push run {} to {} as branch {}: your own git login sends the run's code to that repository.",
                clip(run_id, 40),
                clip(&mask_userinfo(url), 80),
                clip(
                    &branch.clone().unwrap_or_else(|| default_branch(run_id)),
                    80
                )
            ),
            confirm_with: Some(last_four(run_id)),
        },
        Command::Config(ConfigCommand::Set { key, value }) if registry::is_security_key(key) => {
            Preview {
                text: format!(
                    "Change {} to {}: this decides where ham sends requests, who signs them or where downloads land.",
                    key_name(key),
                    clip(value, 80)
                ),
                confirm_with: Some(key_name(key)),
            }
        }
        Command::Config(ConfigCommand::Unset { key }) if registry::is_security_key(key) => {
            Preview {
                text: format!(
                    "Clear {}: this decides where ham sends requests, who signs them or where downloads land.",
                    key_name(key)
                ),
                confirm_with: Some(key_name(key)),
            }
        }
        Command::Config(ConfigCommand::Edit) => plain(
            "Open your editor on config.toml: ham hands the terminal to it until you quit, then checks the file.".to_string(),
        ),
        Command::Engine(EngineCommand {
            engine,
            action: EngineAction::Edit,
        }) => plain(format!(
            "Open your editor on engines/{}.toml: ham hands the terminal to it until you quit, then checks the file.",
            clip(engine, 40)
        )),
        Command::Rules(RulesCommand::Edit { which }) => plain(format!(
            "Open your editor on {}: ham hands the terminal to it until you quit.",
            clip(which.as_deref().unwrap_or("your rules file"), 60)
        )),
        Command::Continue(..) => plain(continue_preview_text(inv, None).unwrap_or_default()),
        _ => plain(format!("Run ham {}.", clip(&inv.to_argv().join(" "), 160))),
    }
}

/// The name `ham config` prints for a key given in either spelling.
fn key_name(key: &str) -> String {
    crate::config::KEYS
        .iter()
        .find(|(file, snake)| *file == key || *snake == key)
        .map_or_else(|| clip(key, 40), |(_, snake)| (*snake).to_string())
}

/// A key the confirmation line reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmKey {
    Enter,
    Esc,
    Backspace,
    Char(char),
}

/// Why the command has not run yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Nothing to report; the key changed nothing.
    Idle,
    /// The key came less than [`CONFIRM_DELAY_MS`] after the preview was drawn.
    TooSoon,
    /// Enter before anything was typed: the preview asks for [`Confirmation::expected`].
    TypeAnswer,
    /// Enter with the wrong characters.
    Mismatch,
}

/// What a key did to the confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmStep {
    Waiting(Wait),
    /// Run the command ([`Confirmation::argv`]).
    Run,
    Cancelled,
    /// The confirmation already ran or was cancelled; nothing a key does can run it again.
    Spent,
}

/// The second Enter of a command that changes a run, sends data, changes a security setting or came from tainted text. It owns the
/// argv that was previewed, so nothing that happens on the screen afterwards (the focused run changing, a poll) can change what
/// runs. Characters of a paste are never fed to it.
#[derive(Clone, Debug)]
pub struct Confirmation {
    argv: Vec<String>,
    invocation: Invocation,
    preview: Preview,
    shown_at_ms: u64,
    typed: String,
    spent: bool,
}

impl Confirmation {
    /// A confirmation for a [`RouteOutcome::Command`] that has a preview; `None` for anything else.
    pub fn new(outcome: &RouteOutcome, shown_at_ms: u64) -> Option<Self> {
        match outcome {
            RouteOutcome::Command {
                argv,
                invocation,
                preview: Some(preview),
                ..
            } => Some(Self {
                argv: argv.clone(),
                invocation: invocation.clone(),
                preview: preview.clone(),
                shown_at_ms,
                typed: String::new(),
                spent: false,
            }),
            _ => None,
        }
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    pub fn preview(&self) -> &Preview {
        &self.preview
    }

    /// What must be typed before Enter, if anything.
    pub fn expected(&self) -> Option<&str> {
        self.preview.confirm_with.as_deref()
    }

    /// What has been typed so far.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// One key, at `now_ms` on the same clock as `shown_at_ms`. After [`ConfirmStep::Run`] or [`ConfirmStep::Cancelled`] the
    /// confirmation is spent and every key answers [`ConfirmStep::Spent`].
    pub fn press(&mut self, key: ConfirmKey, now_ms: u64) -> ConfirmStep {
        let step = self.step(key, now_ms);
        if matches!(step, ConfirmStep::Run | ConfirmStep::Cancelled) {
            self.spent = true;
        }
        step
    }

    fn step(&mut self, key: ConfirmKey, now_ms: u64) -> ConfirmStep {
        if self.spent {
            return ConfirmStep::Spent;
        }
        if key == ConfirmKey::Esc {
            return ConfirmStep::Cancelled;
        }
        let ready = now_ms.saturating_sub(self.shown_at_ms) >= CONFIRM_DELAY_MS;
        let want = self.preview.confirm_with.as_deref();
        match key {
            ConfirmKey::Enter if !ready => ConfirmStep::Waiting(Wait::TooSoon),
            ConfirmKey::Enter => match want {
                None => ConfirmStep::Run,
                Some(_) if self.typed.is_empty() => ConfirmStep::Waiting(Wait::TypeAnswer),
                Some(w) if self.typed.eq_ignore_ascii_case(w) => ConfirmStep::Run,
                Some(_) => ConfirmStep::Waiting(Wait::Mismatch),
            },
            ConfirmKey::Char(c) => {
                // The answer is exactly the characters asked for: more than that is kept (up to a bound) so Enter can refuse it.
                if ready
                    && !c.is_control()
                    && want.is_some_and(|w| {
                        self.typed.chars().count() < w.chars().count() + TYPED_SLACK
                    })
                {
                    self.typed.push(c);
                }
                ConfirmStep::Waiting(Wait::Idle)
            }
            ConfirmKey::Backspace => {
                if ready {
                    self.typed.pop();
                }
                ConfirmStep::Waiting(Wait::Idle)
            }
            ConfirmKey::Esc => ConfirmStep::Cancelled,
        }
    }
}

/// How many characters beyond the expected answer a confirmation keeps, so that a longer answer shows and is refused.
const TYPED_SLACK: usize = 16;
