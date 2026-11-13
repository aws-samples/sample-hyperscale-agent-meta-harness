//! The tool names and the title convention of each engine: what the recorder, the goal watch and the notices that name a tool need to
//! know about an engine's tools. Outside `src/engine/` no module names a tool of an engine (`tests/engine_neutral.rs` fails on it); they ask
//! here, by engine name.
//!
//! An engine that has no table of its own gets the generic one: the common title style, no tool the harness checks and no helper-agent tool. An
//! engine with a table gets the parts it has measured, so no engine is judged by another engine's names. The table of an engine whose real titles are
//! not recorded yet says so (OpenCode, until a recorded session settles it).

/// The parameters of one tool of an engine, read from the tool specs of its model requests. A tool that is not listed is checked for blank keys and
/// for an input that is not an object only.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub allowed: &'static [&'static str],
    pub required: &'static [&'static str],
}

/// How an engine writes the title of a tool call in `session/update`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleStyle {
    /// `name` or `name: arguments`, the name in lower case letters, digits and underscores.
    NameColonArgs,
    /// OpenCode v2: a call is titled by its tool name while it is pending and a shell call by its command once it runs. The `tool_call` carries
    /// `rawInput: {}`: the input arrives with the first `in_progress` update ([`TitleStyle::input_is_late`]), and the answer is in `content`.
    NameThenCommand,
}

impl TitleStyle {
    /// The name of the tool from a title such as `name: args`; empty when the title has no such prefix.
    pub fn tool_of(self, title: &str) -> String {
        match self {
            // The title of a pending call is the tool's name; a command is not a name.
            Self::NameThenCommand => {
                if !title.is_empty()
                    && title.len() <= 40
                    && title.starts_with(|c: char| c.is_ascii_lowercase())
                    && title
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    title.to_string()
                } else {
                    String::new()
                }
            }
            Self::NameColonArgs => match title.split_once(':') {
                Some((n, _))
                    if !n.is_empty()
                        && n.len() <= 40
                        && n.starts_with(|c: char| c.is_ascii_lowercase())
                        && n.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') =>
                {
                    n.to_string()
                }
                _ => String::new(),
            },
        }
    }

    /// Whether the title of a tool call names `tool`: the name alone or the name before a colon. A title that only starts with the same letters
    /// is another tool.
    pub fn calls(self, title: &str, tool: &str) -> bool {
        match self {
            Self::NameThenCommand => title == tool,
            Self::NameColonArgs => {
                title == tool
                    || title
                        .strip_prefix(tool)
                        .is_some_and(|rest| rest.starts_with(':'))
            }
        }
    }
}

impl TitleStyle {
    /// Whether the input of a call reaches the wire after the `tool_call` (with the first `in_progress` update) and its answer is in `content`
    /// and not in `rawOutput`: the watch then reads the input and the answer from there.
    pub fn input_is_late(self) -> bool {
        matches!(self, Self::NameThenCommand)
    }
}

/// What the harness knows about the tools of one engine.
#[derive(Debug, PartialEq, Eq)]
pub struct EngineTools {
    pub title: TitleStyle,
    /// The tools whose input keys the recorder checks.
    pub known: &'static [ToolSpec],
    /// The tool the model uses to start a helper agent, when the engine has one.
    pub delegate: Option<&'static str>,
}

/// The helper-agent tool of the qualified engine.
pub const QUALIFIED_DELEGATE: &str = "spawn_agent";

/// The helper-agent tool of OpenCode v2 (`task` in v1): the model calls `subagent`, and the ACP layer titles the call `subagent`.
pub const OPENCODE_DELEGATE: &str = "subagent";

/// The tools of the qualified engine's pinned build, read from the tool specs of its Bedrock requests.
const QUALIFIED: EngineTools = EngineTools {
    title: TitleStyle::NameColonArgs,
    known: &[
        ToolSpec {
            name: "apply_patch",
            allowed: &["input"],
            required: &["input"],
        },
        ToolSpec {
            name: "editor",
            allowed: &["insert_line", "new_text", "old_text", "path"],
            required: &["path", "new_text"],
        },
        ToolSpec {
            name: "fetch_web_content",
            allowed: &["requests"],
            required: &["requests"],
        },
        ToolSpec {
            name: "read_files",
            allowed: &["files"],
            required: &["files"],
        },
        ToolSpec {
            name: "run_commands",
            allowed: &["commands"],
            required: &["commands"],
        },
        ToolSpec {
            name: "search_codebase",
            allowed: &["queries"],
            required: &["queries"],
        },
        ToolSpec {
            name: "spawn_agent",
            allowed: &["systemPrompt", "task"],
            required: &["systemPrompt", "task"],
        },
    ],
    delegate: Some(QUALIFIED_DELEGATE),
};

/// OpenCode v2.2.6, from recorded sessions: a call is titled by its tool name while it is pending (a shell call by its command once it runs), and
/// the helper-agent tool is `subagent`. The table checks the input keys of no tool yet.
const OPENCODE: EngineTools = EngineTools {
    title: TitleStyle::NameThenCommand,
    known: &[],
    delegate: Some(OPENCODE_DELEGATE),
};

/// An engine this build has no table for.
const GENERIC: EngineTools = EngineTools {
    title: TitleStyle::NameColonArgs,
    known: &[],
    delegate: None,
};

/// The tools of `engine` (the profile name of the request). An empty name means "no engine named": the qualified engine's table, which is what a
/// recorder that was never told its engine reads. The fake ACP engine replays recordings of the qualified engine.
pub fn engine_tools(engine: &str) -> &'static EngineTools {
    match engine {
        "cline" | "fake-acp" | "" => &QUALIFIED,
        "opencode" => &OPENCODE,
        _ => &GENERIC,
    }
}

/// The name of the tool from a title, in the title style of `engine`.
pub fn tool_of(engine: &str, title: &str) -> String {
    engine_tools(engine).title.tool_of(title)
}

/// Whether the title of a tool call names `tool`, in the title style of `engine`.
pub fn calls_tool(engine: &str, title: &str, tool: &str) -> bool {
    engine_tools(engine).title.calls(title, tool)
}

/// The spec of the tool `name` of `engine`, when the harness checks it.
pub fn known_tool(engine: &str, name: &str) -> Option<&'static ToolSpec> {
    engine_tools(engine).known.iter().find(|t| t.name == name)
}
