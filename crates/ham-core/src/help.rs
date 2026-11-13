//! The help of the command line: `ham --help`, `ham -h`, `ham help`, `ham help <command>` and `ham <command> --help`.
//!
//! [`request`] decides from the argv alone whether a line asks for help and for which text. It runs before the parser and before anything
//! is read from the config file, the credentials or the network, so a person with no setup at all gets the usage. A help flag that stands
//! as the value of an option (`--prompt --help`) is the value (`crate::command::value_positions`), and `help` followed by anything but one
//! command word is a new run with the prompt `help`. The usage forms of `rules`, `lifecycle`, `config` and `engine` are the constants the
//! parser's own errors print (`crate::command::RULES_FORMS` and the others); the one-line summary of each command is the registry's.
//! The option rows of a text are checked against the parser by `tests/help.rs`.

use crate::command::{
    CONFIG_FORMS, ENGINE_FORMS, LIFECYCLE_FORMS, RULES_FORMS, SUBCOMMANDS, value_positions,
};
use crate::exit::{ERROR, USAGE, exit_code};
use crate::registry::{self, Entry, Kind, subcommand};
use crate::{engines, models, render};
use harness_protocol::goal::GoalStatus;
use std::sync::OnceLock;

/// The flags that ask for help.
pub const HELP_FLAGS: [&str; 2] = ["--help", "-h"];

/// What a help request asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelpRequest {
    /// The usage screen: the forms, the options of a run, the commands, the settings and the exit codes.
    Top,
    /// One command, by its name in `command::SUBCOMMANDS`.
    Command(&'static str),
    /// `ham help <word>` for a word that is no command: a usage error ([`unknown_text`]).
    Unknown(String),
}

/// Whether `args` (without the program name) ask for help, and for what. `None` means the line is not a help request and goes to the parser.
pub fn request<S: AsRef<str>>(args: &[S]) -> Option<HelpRequest> {
    let all: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    // The words without the `--json` flags, each with whether it stands as the value of the option before it.
    let words: Vec<(&str, bool)> = all
        .iter()
        .copied()
        .zip(value_positions(&all))
        .filter(|(w, value)| !(*w == "--json" && !*value))
        .collect();
    let (first, _) = *words.first()?;
    if first == "help" && words.len() <= 2 {
        return match words.get(1).map(|(w, _)| *w) {
            None | Some("help") => Some(HelpRequest::Top),
            Some(w) if HELP_FLAGS.contains(&w) => Some(HelpRequest::Top),
            Some(w) if w.starts_with('-') => None,
            Some(w) => Some(match subcommand(w) {
                Some(e) => HelpRequest::Command(e.name),
                None => HelpRequest::Unknown(w.to_string()),
            }),
        };
    }
    if !words
        .iter()
        .any(|(w, value)| !*value && HELP_FLAGS.contains(w))
    {
        return None;
    }
    Some(match subcommand(first) {
        Some(e) => HelpRequest::Command(e.name),
        None => HelpRequest::Top,
    })
}

/// The refusal for `ham help <word>` when the word is no command: `no command "word"`, the nearest commands when there are any, and where the list is.
pub fn unknown_text(word: &str) -> String {
    let table: Vec<Entry> = registry::entries()
        .iter()
        .filter(|e| e.kind == Kind::Subcommand)
        .copied()
        .collect();
    format!(
        "{}. ham help lists the commands.",
        registry::no_command_text(&table, word)
    )
}

// ---- the layout ---------------------------------------------------------------------------------------------------------------

/// The width the text is wrapped to.
const WIDTH: usize = 78;
/// The longest option label that keeps its description on its row; a longer label stands alone and the description follows indented.
const LABEL_MAX: usize = 34;

/// `text` with the words the client takes from its own tables put in: `{engine}` and `{engine_id}` (the engine this release runs, as
/// `engines::say` does), `{aliases}` (the short names of the shipped model list) and `{default_model}` (the name of the built-in default).
fn fill(text: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }
    static ALIASES: OnceLock<String> = OnceLock::new();
    let aliases = ALIASES.get_or_init(|| {
        models::builtin()
            .models()
            .iter()
            .filter_map(|m| m.alias.as_deref())
            .collect::<Vec<_>>()
            .join(", ")
    });
    engines::say(text)
        .replace("{engine_id}", engines::default_engine().id)
        .replace("{engines}", &engines::choices_text())
        .replace("{aliases}", aliases)
        .replace(
            "{default_model}",
            &render::model_name(models::DEFAULT_MODEL_ID),
        )
}

/// `text` broken at spaces into lines of at most `width` characters (a longer word stands alone).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let text = fill(text);
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

/// A paragraph, wrapped, every line indented by `indent` spaces.
fn paragraph(out: &mut String, indent: usize, text: &str) {
    for line in wrap(text, WIDTH - indent) {
        out.push_str(&" ".repeat(indent));
        out.push_str(&line);
        out.push('\n');
    }
}

/// Rows of a label and a description: the labels are padded to the longest one that fits, the descriptions wrap under each other.
fn rows(out: &mut String, indent: usize, items: &[(String, &str)]) {
    let lw = items
        .iter()
        .map(|(l, _)| l.chars().count())
        .filter(|n| *n <= LABEL_MAX)
        .max()
        .unwrap_or(0);
    for (label, text) in items {
        let n = label.chars().count();
        if n > LABEL_MAX {
            out.push_str(&format!("{}{label}\n", " ".repeat(indent)));
            paragraph(out, indent + lw + 2, text);
            continue;
        }
        let hang = indent + lw + 2;
        let lines = wrap(text, WIDTH.saturating_sub(hang).max(20));
        for (i, line) in lines.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("{}{label:<lw$}  {line}\n", " ".repeat(indent)));
            } else {
                out.push_str(&format!("{}{line}\n", " ".repeat(hang)));
            }
        }
    }
}

fn labelled(items: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
    items.iter().map(|(l, t)| ((*l).to_string(), *t)).collect()
}

// ---- the words ----------------------------------------------------------------------------------------------------------------

const INTRO: &str = "Use ham to run an AI coding agent in your AWS account while your laptop is closed. Your prompt's result returns as a zip, a local git repository, or an AWS CodeCommit pull request.";
const TARGET_REPO: &str =
    "Deliver the run's code as a pull request in the CodeCommit repository <name>.";
const TARGET_NEW_REPO: &str = "Create the CodeCommit repository <name> first, then deliver the run's code there as a pull request.";
const TARGET_S3_ONLY: &str = "Receive code only as a zip in your AWS account, without a repository. Fetch it with ham pull <runId> or ham clone <runId>.";
const OPT_DIRECT: &str =
    "Start the run with the coding agent directly; --direct is required on every start line.";
const OPT_PROMPT: &str =
    "Give your prompt here when it starts with a dash or equals a command name.";
const OPT_BASE: &str = "Start --repo from this repository branch instead of its default branch.";
const OPT_MODEL: &str = "Choose an id or alias ({aliases}) listed by ham models. The default is default_model when set, otherwise {default_model}.";
const OPT_RULES: &str = "Send a rule folder or one .md file instead of ~/.config/ham/rules/.";
const OPT_NO_RULES: &str = "Send no rules; starting without rules requires this option.";
const OPT_SKILLS: &str = "Send a folder of skill folders instead of ~/.config/ham/skills/.";
const OPT_NO_SKILLS: &str = "Send no skills.";
const OPT_CONTINUE: &str = "Send this nudge file, text that keeps the coding agent going, instead of ~/.config/ham/continue.md.";
const OPT_INPUT_WAIT: &str =
    "Wait 0 to 43200 whole minutes for your answer to the coding agent; 0 skips waiting.";
const OPT_GOAL: &str = "Keep the coding agent working until it reports completion with on (default); use off for a plain run.";
const OPT_RESEARCH: &str = "Request web searches and public page or repository downloads; enabled wherever your service supports search.";
const OPT_NO_RESEARCH: &str = "Turn off web research for this run.";
const OPT_ALLOW_THIRD_PARTY: &str = "Allow identical downloaded third-party files, reported as findings; otherwise, matching files prevent zip delivery.";
const OPT_ATTACH: &str = "Send UTF-8 context files for the coding agent; repeat --attach for more. The files, the prompt and the rules together must fit in one request, which carries about 6 MiB.";
const OPT_COMPUTE: &str = "Choose auto for service selection, microvm for the standard 2 vCPU session, or instances for EC2. The instances option uses one larger machine where your AWS service offers it.";
const OPT_ENGINE: &str = "Choose the coding engine for this run: one of {engines}. The default is default_engine when set, otherwise {engine_id}. A run on the default engine sends no engine member.";
const OPT_MAX_RUN_HOURS: &str = "Set the most hours this run may take in all, from 1 to 720; the default is 24. A run that is not finished when its 8-hour session ends continues in a new session until this time is used up, then stops as paused and ham resume continues it.";
const OPT_PROVIDER_SHARING: &str = "Agree with on, or refuse with off, that the prompts and answers of this run go to the provider of a model that needs provider data sharing; with on, ham also switches the account's Bedrock data retention mode for it when needed. The default is off.";
const OPT_JSON: &str =
    "Print one JSON object per line instead of text; accepted with every command.";
const SETTINGS: &str = "List settings and values with ham config, and trace their sources with ham config show. Change one with ham config set <key> <value>, or open ~/.config/ham/config.toml with ham config edit. Read the reference with ham config docs, and view the settings of a coding engine with ham engine <name> show.";
const EXIT_0: &str = "The command completes its job; a run completes.";
const EXIT_1: &str = "An error occurs before a run state is known.";
const EXIT_2: &str = "The command line is invalid, and nothing starts.";
const EXIT_ACTIVE: &str = "The run is still active.";
const EXIT_PAUSED: &str = "The run is paused: it used the total time it was given and its work was delivered. ham resume continues it.";
const EXIT_BLOCKED: &str = "The run is blocked and waits for you.";
const EXIT_USAGE_LIMITED: &str = "The run stops at a usage limit.";
const EXIT_BUDGET_LIMITED: &str = "The run stops at a budget limit.";
const EXIT_CLEARED: &str = "The run is cleared.";
const EXIT_STALLED: &str = "The run is stalled: it lost its progress (the session stopped answering or several sessions changed nothing) and its work was delivered. ham resume continues it.";
const EXIT_NOTE: &str =
    "Exit codes 10 to 16 describe the run states returned by ham watch and a start line.";
const MORE: &str = "Read command help with ham help <command> or ham <command> --help.\nPrint the ham version with ham --version.\nOpen the terminal app with ham and no arguments.";
const ABOUT_RUNS: &str = "Each row shows the run id, state and phase, target, start time, and any run name and tags. Runs started by automated checks are hidden unless you request them.";
const OPT_RUNS_PROBES: &str = "Include runs started by automated checks.";
const OPT_RESUME_ALL: &str =
    "Resume every paused and stalled run in your list that has not been resumed already.";
const OPT_RESUME_DRY_RUN: &str =
    "Print which runs would be resumed and which would be left, and start nothing.";
const ABOUT_WATCH: &str = "Omit the run id to follow your newest run. The command downloads the zip on completion and returns the run's exit code.";
const ABOUT_CANCEL: &str = "Give the run id.";
const ABOUT_MODELS: &str =
    "The built-in model list works offline. The list marks the default model.";
const OPT_MODELS_SERVER: &str =
    "Ask the service in your AWS account for its model list instead of the built-in list.";
const ABOUT_PR: &str = "Use this for runs that target a CodeCommit repository.";
const ABOUT_PULL: &str = "The default download is the zip. Files go to download_dir when set, otherwise your current folder.";
const OPT_PULL_ARTIFACT: &str = "Choose zip (default), evidence (a zip without source), or partial. The partial option finds nothing for runs started with --direct.";
const OPT_PULL_SNAPSHOT: &str = "Download the latest work snapshot from an active run; this option cannot accompany --artifact.";
const ABOUT_CLONE: &str = "Use ham clone <runId> [dir], with ./<repository name> as the default directory. The local repository comes from repo.bundle inside the zip.";
const ABOUT_PUSH: &str = "Use ham push <runId> <git-url> [--branch <name>] to rebuild the repository from the zip and push the run's branch with your git credentials. Give a URL without a password.";
const OPT_PUSH_BRANCH: &str =
    "Choose the destination branch; the default is the run's branch, harness/<runId>.";
const ABOUT_LINK: &str = "Anyone with the link can download the zip until the link expires.";
const ABOUT_DOCTOR: &str =
    "Each check prints a PASS, WARN or FAIL line. The exit code is 0 unless a check fails.";
const OPT_DOCTOR_CATALOG_MAX_AGE: &str =
    "Warn when the newest model test record exceeds this age in hours; use 1 or more.";
const ABOUT_RULES: &str = "Rules are text files in ~/.config/ham/rules/ that every run receives unless you override them. Use ham rules init to write default rules, the nudge file, and a development-lifecycle pack containing a rule file and seven skills, which are reusable agent instructions.";
const OPT_RULES_PACK: &str = "Choose lifecycle (default) for the development-lifecycle pack, or none to write only default rules.";
const OPT_RULES_SKILLS_AS_RULES: &str =
    "Write each pack skill as an always-on rule file instead of a skill.";
const OPT_RULES_SKILLS: &str = "Also install the pack's skills in the skills folder.";
const OPT_RULES_NO_SKILLS: &str = "Install the pack's rule file without its skills.";
const OPT_RULES_SHOW_RULES: &str =
    "Show rules from this folder or file instead of the default folder.";
const OPT_RULES_SHOW_CONTINUE: &str = "Show this nudge file instead of the default one.";
const ABOUT_CONFIG: &str = "The settings file is ~/.config/ham/config.toml. Settings take effect at your next run unless the reference table says otherwise. Use ham config migrate to copy the files you made with the old command name into ~/.config/ham.";
const OPT_CONFIG_STRICT: &str = "Treat a warning as an error.";
const OPT_CONFIG_DRY_RUN: &str = "List the files that would be copied and write nothing.";
const ABOUT_ENGINE: &str = "Each coding engine keeps its settings in ~/.config/ham/engines/<name>.toml. Use ham engine <name> init for a template with every key commented out; ham engine <name> check validates the file without sending anything.";
const OPT_ENGINE_REPLACE: &str =
    "Replace an existing coding-agent settings file with the imported file.";
const ABOUT_SAY: &str = "Use ham say <runId> <text>, or give - as the text to read standard input. The command waits up to 20 seconds for the coding agent to take your answer.";
const OPT_REQUEST: &str =
    "Choose the question id to answer; by default, ham finds the open question.";
const OPT_NO_WAIT: &str = "Return as soon as your answer is queued.";
const OPT_TEXT: &str = "Give your text here when it starts with a dash.";
const ABOUT_NEW_SESSION: &str = "Give optional text for the fresh session.";
const ABOUT_TRY_MODEL: &str = "Use ham try-model <runId> <model> [<text>]. Choose a model id or alias listed in the open question.";
const ABOUT_FINISH: &str = "Give only the run id.";
const ABOUT_RESUME: &str = "A run that used its total time is paused, and a run that lost its progress is stalled; both keep the code they delivered. ham resume starts a new billable run from that code for each of them, with the request the run was started with and a note that it continues. Pressing it twice never starts a second run for the same run. The terminal app does the same with R.";
const ABOUT_CONTINUE: &str = "Use ham continue <runId> [<text>] [options] to start a billable run; omitting text opens your editor. The run uses the original target unless you give --repo or --s3-only, and accepts run options except --new-repo, --base and --direct because it always runs the coding agent directly.";
const ABOUT_UPDATE: &str = "ham update reads the newest version from the main branch of the update source, the setting update_source; the word off turns ham update off, and it then makes no network call. With --check it shows your ham and backend versions, the newest version with its commit, and the changes between. The update asks once, waits until your active runs end, then runs the setup.sh --update of that commit, which updates the backend and then ham. It never cancels a run. It waits only for your own runs and does not stop anyone from starting a run; setup stops with exit 7 while any run on the stage is active. Afterwards ham and the backend must report the new version; a new ham that reports another version is replaced by the one it replaced. It runs from a shell, not from the chat box.";
const OPT_UPDATE_CHECK: &str = "Show the versions and the changes, and change nothing.";
const OPT_UPDATE_YES: &str = "Do not ask before the update.";
const OPT_UPDATE_NO_WAIT: &str = "When runs are active, stop at once instead of waiting.";
const OPT_UPDATE_WAIT: &str =
    "Wait at most this many minutes for your active runs to end: 1 to 1440, 60 by default.";
const ABOUT_LIFECYCLE: &str =
    "Use ham lifecycle init <dir> to write AGENTS.md and .agents/skills/ into <dir>.";
const OPT_LIFECYCLE_KIRO: &str = "Also link .kiro/skills to .agents/skills.";

/// The targets of a start line, as option rows.
const TARGETS: [(&str, &str); 3] = [
    ("--repo cc:<name>", TARGET_REPO),
    ("--new-repo cc:<name>", TARGET_NEW_REPO),
    ("--s3-only", TARGET_S3_ONLY),
];

/// The options of a run, as option rows. `ham continue` takes them except `--new-repo`, `--base` and `--direct`.
const RUN_OPTIONS: [(&str, &str); 19] = [
    ("--direct", OPT_DIRECT),
    ("--prompt <text>", OPT_PROMPT),
    ("--base <ref>", OPT_BASE),
    ("--model <id|alias>", OPT_MODEL),
    ("--rules <path>", OPT_RULES),
    ("--no-rules", OPT_NO_RULES),
    ("--skills <dir>", OPT_SKILLS),
    ("--no-skills", OPT_NO_SKILLS),
    ("--continue <file>", OPT_CONTINUE),
    ("--input-wait <minutes>", OPT_INPUT_WAIT),
    ("--goal on|off", OPT_GOAL),
    ("--research", OPT_RESEARCH),
    ("--no-research", OPT_NO_RESEARCH),
    ("--allow-third-party", OPT_ALLOW_THIRD_PARTY),
    ("--attach <file>", OPT_ATTACH),
    ("--compute auto|microvm|instances", OPT_COMPUTE),
    ("--max-run-hours <hours>", OPT_MAX_RUN_HOURS),
    ("--engine <name>", OPT_ENGINE),
    ("--provider-sharing on|off", OPT_PROVIDER_SHARING),
];

const JSON_OPTION: (&str, &str) = ("--json", OPT_JSON);
const REQUEST_OPTIONS: [(&str, &str); 2] =
    [("--request <id>", OPT_REQUEST), ("--no-wait", OPT_NO_WAIT)];

/// The exit codes: the two of the client and the run states, in the numbers `exit::exit_code` returns.
fn exit_rows() -> Vec<(String, &'static str)> {
    let status = |s: GoalStatus| exit_code(Some(s)).to_string();
    vec![
        (exit_code(Some(GoalStatus::Complete)).to_string(), EXIT_0),
        (ERROR.to_string(), EXIT_1),
        (USAGE.to_string(), EXIT_2),
        (status(GoalStatus::Active), EXIT_ACTIVE),
        (status(GoalStatus::Paused), EXIT_PAUSED),
        (status(GoalStatus::Blocked), EXIT_BLOCKED),
        (status(GoalStatus::UsageLimited), EXIT_USAGE_LIMITED),
        (status(GoalStatus::BudgetLimited), EXIT_BUDGET_LIMITED),
        (status(GoalStatus::Cleared), EXIT_CLEARED),
        (status(GoalStatus::Stalled), EXIT_STALLED),
    ]
}

/// The usage screen of `ham --help`.
pub fn top_text() -> String {
    let mut o = String::from("Usage:\n");
    o.push_str("  ham\n");
    o.push_str("  ham \"<prompt>\" <target> --direct [options]\n");
    o.push_str("  ham <command> [options]\n");
    o.push_str("  ham help [<command>]\n\n");
    paragraph(&mut o, 0, INTRO);
    o.push_str("\nTargets of a run (give one):\n");
    rows(&mut o, 2, &labelled(&TARGETS));
    o.push_str("\nOptions of a run:\n");
    let mut run = labelled(&RUN_OPTIONS);
    run.push((JSON_OPTION.0.to_string(), JSON_OPTION.1));
    rows(&mut o, 2, &run);
    o.push_str("\nCommands:\n");
    let commands: Vec<(String, String)> = SUBCOMMANDS
        .iter()
        .filter_map(|n| subcommand(n))
        .map(|e| (e.name.to_string(), format!("{}.", e.help)))
        .collect();
    let lw = commands.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, help) in &commands {
        let hang = 2 + lw + 2;
        for (i, line) in wrap(help, WIDTH - hang).iter().enumerate() {
            if i == 0 {
                o.push_str(&format!("  {name:<lw$}  {line}\n"));
            } else {
                o.push_str(&format!("{}{line}\n", " ".repeat(hang)));
            }
        }
    }
    o.push_str("\nSettings:\n");
    paragraph(&mut o, 2, SETTINGS);
    o.push_str("\nExit codes:\n");
    rows(&mut o, 2, &exit_rows());
    paragraph(&mut o, 2, EXIT_NOTE);
    o.push_str("\nMore help:\n");
    for line in MORE.lines() {
        paragraph(&mut o, 2, line);
    }
    o
}

/// The forms of a command, one per line, each starting with `ham `.
fn forms(name: &str) -> Vec<String> {
    let split = |forms: &str| forms.split(" | ").map(str::to_string).collect::<Vec<_>>();
    let fixed = |forms: &[&str]| forms.iter().map(|f| (*f).to_string()).collect::<Vec<_>>();
    match name {
        "rules" => split(RULES_FORMS),
        "lifecycle" => split(LIFECYCLE_FORMS),
        "config" => split(CONFIG_FORMS),
        "engine" => split(ENGINE_FORMS),
        "runs" => fixed(&["ham runs [--probes]"]),
        "watch" => fixed(&["ham watch [<runId>]"]),
        "cancel" => fixed(&["ham cancel <runId>"]),
        "models" => fixed(&["ham models [--server]"]),
        "pr" => fixed(&["ham pr <runId>"]),
        "pull" => fixed(&["ham pull <runId> [--artifact zip|partial|evidence | --snapshot]"]),
        "clone" => fixed(&["ham clone <runId> [dir]"]),
        "push" => fixed(&["ham push <runId> <git-url> [--branch <name>]"]),
        "link" => fixed(&["ham link <runId>"]),
        "doctor" => fixed(&["ham doctor [--catalog-max-age <hours>]"]),
        "say" => fixed(&[
            "ham say <runId> <text> [--request <id>] [--no-wait]",
            "ham say <runId> --text <text> [--request <id>] [--no-wait]",
        ]),
        "new-session" => fixed(&["ham new-session <runId> [<text>] [--request <id>] [--no-wait]"]),
        "try-model" => {
            fixed(&["ham try-model <runId> <model> [<text>] [--request <id>] [--no-wait]"])
        }
        "finish" => fixed(&["ham finish <runId> [--request <id>] [--no-wait]"]),
        "continue" => {
            fixed(&["ham continue <runId> [<text>] [--repo cc:<name> | --s3-only] [options]"])
        }
        "resume" => fixed(&[
            "ham resume <runId> [--dry-run]",
            "ham resume --all [--dry-run]",
        ]),
        "update" => fixed(&[
            "ham update --check [--json]",
            "ham update [--yes] [--no-wait | --wait <minutes>]",
        ]),
        _ => Vec::new(),
    }
}

/// What a command says besides its summary.
fn about(name: &str) -> &'static str {
    match name {
        "runs" => ABOUT_RUNS,
        "watch" => ABOUT_WATCH,
        "cancel" => ABOUT_CANCEL,
        "models" => ABOUT_MODELS,
        "pr" => ABOUT_PR,
        "pull" => ABOUT_PULL,
        "clone" => ABOUT_CLONE,
        "push" => ABOUT_PUSH,
        "link" => ABOUT_LINK,
        "doctor" => ABOUT_DOCTOR,
        "rules" => ABOUT_RULES,
        "config" => ABOUT_CONFIG,
        "engine" => ABOUT_ENGINE,
        "say" => ABOUT_SAY,
        "new-session" => ABOUT_NEW_SESSION,
        "try-model" => ABOUT_TRY_MODEL,
        "finish" => ABOUT_FINISH,
        "continue" => ABOUT_CONTINUE,
        "resume" => ABOUT_RESUME,
        "lifecycle" => ABOUT_LIFECYCLE,
        "update" => ABOUT_UPDATE,
        _ => "",
    }
}

type Section = (String, Vec<(String, &'static str)>);

/// The option sections of a command: its own options, and `--json` last.
fn sections(name: &str) -> Vec<Section> {
    let one = |heading: &str, items: &[(&str, &'static str)]| -> Section {
        (heading.to_string(), labelled(items))
    };
    let json = || one("Options of every command:", &[JSON_OPTION]);
    let with_json = |items: &[(&str, &'static str)]| -> Vec<Section> {
        let mut all = labelled(items);
        all.push((JSON_OPTION.0.to_string(), JSON_OPTION.1));
        vec![("Options:".to_string(), all)]
    };
    match name {
        "runs" => with_json(&[("--probes", OPT_RUNS_PROBES)]),
        "resume" => with_json(&[("--all", OPT_RESUME_ALL), ("--dry-run", OPT_RESUME_DRY_RUN)]),
        "update" => with_json(&[
            ("--check", OPT_UPDATE_CHECK),
            ("--yes, -y", OPT_UPDATE_YES),
            ("--no-wait", OPT_UPDATE_NO_WAIT),
            ("--wait <minutes>", OPT_UPDATE_WAIT),
        ]),
        "models" => with_json(&[("--server", OPT_MODELS_SERVER)]),
        "pull" => with_json(&[
            ("--artifact zip|partial|evidence", OPT_PULL_ARTIFACT),
            ("--snapshot", OPT_PULL_SNAPSHOT),
        ]),
        "push" => with_json(&[("--branch <name>", OPT_PUSH_BRANCH)]),
        "doctor" => with_json(&[("--catalog-max-age <hours>", OPT_DOCTOR_CATALOG_MAX_AGE)]),
        "say" | "new-session" | "try-model" => {
            let mut items: Vec<(&str, &'static str)> = REQUEST_OPTIONS.to_vec();
            items.push(("--text <text>", OPT_TEXT));
            with_json(&items)
        }
        "finish" => with_json(&REQUEST_OPTIONS),
        "continue" => {
            let mut items: Vec<(&str, &'static str)> = vec![
                ("--repo cc:<name>", TARGET_REPO),
                ("--s3-only", TARGET_S3_ONLY),
                ("--text <text>", OPT_TEXT),
            ];
            items.extend(
                RUN_OPTIONS
                    .iter()
                    .filter(|(l, _)| !["--direct", "--base <ref>"].contains(l)),
            );
            with_json(&items)
        }
        "rules" => vec![
            one(
                "Options of ham rules init:",
                &[
                    ("--pack lifecycle|none", OPT_RULES_PACK),
                    ("--skills-as-rules", OPT_RULES_SKILLS_AS_RULES),
                    ("--skills", OPT_RULES_SKILLS),
                    ("--no-skills", OPT_RULES_NO_SKILLS),
                ],
            ),
            one(
                "Options of ham rules show:",
                &[
                    ("--rules <path>", OPT_RULES_SHOW_RULES),
                    ("--continue <file>", OPT_RULES_SHOW_CONTINUE),
                ],
            ),
            json(),
        ],
        "config" => vec![
            one(
                "Options of ham config check:",
                &[("--strict", OPT_CONFIG_STRICT)],
            ),
            one(
                "Options of ham config migrate:",
                &[("--dry-run", OPT_CONFIG_DRY_RUN)],
            ),
            json(),
        ],
        "engine" => vec![
            one(
                "Options of ham engine <name> import:",
                &[("--replace", OPT_ENGINE_REPLACE)],
            ),
            json(),
        ],
        "lifecycle" => vec![
            one(
                "Options of ham lifecycle init:",
                &[("--kiro", OPT_LIFECYCLE_KIRO)],
            ),
            json(),
        ],
        _ => with_json(&[]),
    }
}

/// The help of one command: its forms, what it does and its options. `None` for a word that is no subcommand.
pub fn command_text(name: &str) -> Option<String> {
    let entry = subcommand(name)?;
    let mut o = String::from("Usage:\n");
    for form in forms(entry.name) {
        o.push_str(&format!("  {form}\n"));
    }
    o.push('\n');
    let summary = format!("{}. {}", entry.help, about(entry.name));
    paragraph(&mut o, 0, summary.trim());
    for (heading, items) in sections(entry.name) {
        o.push_str(&format!("\n{heading}\n"));
        rows(&mut o, 2, &items);
    }
    Some(o)
}
