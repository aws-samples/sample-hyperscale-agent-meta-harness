# Engine parity

The harness builds each protection and each tool once, for every engine. An engine adds code only where it differs, and that code is small,
lives in `crates/agentd/src/engine/` and is named below with its reason. This page lists every protection and tool and says how it applies to
each engine that runs.

`crates/agentd/tests/engine_parity.rs` holds the table. For each row and each engine it either runs an assertion (asserted), carries a named
reason for a difference (exception), or says which lane of the OpenCode program still owes the OpenCode half (owed). The table below is the
output of that file: the test fails when the two differ, and `PARITY_WRITE=1` rewrites the table. The test `b789_parity_no_row_is_owed` fails
while any row is owed, so no row is owed on a merged tree.

## Adding a row or an engine

- A new protection or tool adds a row to `rows()` in the test, with an assertion for each engine or an exception that says why.
- A new engine profile fails `b789_parity_the_engines_of_the_table_are_the_engines_the_build_has_profiles_for` until the table has a column for it.
- A lane that finishes an owed row replaces `Owed` with an assertion and rewrites the table.

## The table

<!-- parity:begin -->
| Row | What it covers | Cline | OpenCode |
|---|---|---|---|
| rules placement | the user's rules are placed read-only under the root of the layout and checked before any model call | asserted | asserted |
| skills placement | the run's skills are placed read-only under the root of the layout and checked before any model call | asserted | asserted |
| rules reach the engine | the engine is told to load the placed rules | asserted | asserted |
| config protection | the engine user cannot rewrite what the harness placed | asserted | asserted |
| locked settings | settings the engine user cannot rewrite | asserted | asserted |
| config root variable | HAMH_ENGINE_CONFIG_ROOT names the root of the harness's files in every launch | asserted | asserted |
| model value on the ACP wire | the catalog id is mapped to the engine's own value at the ACP boundary only | asserted | asserted |
| tool and title tables | each engine's tool names and title style live in one table; none is judged by another's | asserted | asserted |
| permission answering | every permission request is answered at once, allow once | asserted | asserted |
| idle watchdog | an engine silent past the limit is cancelled and given up on | asserted | asserted |
| tool-idle limit | a tool call that runs while the engine sends nothing is cancelled after the limit; each engine starts with its own default, one setting replaces it | asserted | asserted |
| command time limit | the shell command time limit of the engine is one setting, placed in the launch, and is ten minutes when the user sets none | asserted | asserted |
| task text that starts with a slash | a task that starts with / reaches the model as a task, and an engine that reads no prompt as a command gets its prompts byte for byte | asserted | asserted |
| telemetry kill switches | telemetry and self-update are off in every launch, and a launch without them is refused | asserted | asserted |
| credentials binding | the engine reaches AWS through the agent profile and agentd creds, with no raw key | asserted | asserted |
| telemetry proof by effect | the engine's profile gates it on an egress capture of a full turn: no name outside the allowlist | asserted | asserted |
| credential proof by effect | the engine's profile gates it on credential binding: every call is signed by the session profile and a stopped broker fails closed | asserted | asserted |
| egress allowlist | the network source names the hosts the firewall drops, and no engine profile points at one | asserted | asserted |
| hidden paths | the engine's own config files are hidden from the work (the hiddenPaths of its profile) | asserted | asserted |
| delivery refusal of the config file | the delivery validator refuses every config file of the engine | asserted | asserted |
| harness tool server (code_search) | the harness's tool server is registered with the engine | asserted | asserted |
| ripgrep | the engine is told to search with rg | asserted | asserted |
| provider data sharing agreement | a model that needs provider data sharing carries the mark on the rows of every engine that lists it, so a run that did not agree is refused before any call | asserted | asserted |
| AgentCore MCP gateway tools | tools of the AgentCore MCP gateway | exception | exception |
| semantic-engine access | semantic search for the engine | exception | exception |
| AWS API MCP access | an AWS API MCP for the engine | exception | exception |

### Exceptions

- AgentCore MCP gateway tools (both engines): The harness gives no AgentCore MCP gateway tool to any engine today. A gateway tool added later reaches every engine through the harness tool server of the row above.
- semantic-engine access (both engines): The harness gives no semantic search to any engine today; the code_search tool of the harness's tool server is a ripgrep search of the work tree, the same for every engine.
- AWS API MCP access (both engines): The harness gives no AWS API MCP to any engine today; an engine reaches AWS with the session credentials of the credentials row, through its own tools.

### Owed by another lane of the OpenCode program

None.
<!-- parity:end -->

## Engine-specific code

| Where | What differs | Why |
|---|---|---|
| `src/engine/config.rs` | Built-in config files and the value of the ACP `model` option | Engines read settings from different files and name the same model differently. |
| `src/engine/tools.rs` | Tool names and the title style | Engines name their tools differently, and no engine is judged by another's names. |
| `src/engine/layout.rs` | Where rules, skills and locked settings go, their protection strategy, the kill switches, the repository folders that shadow | An engine that keeps state in its config directory cannot have it owned by root. |
| `src/engine/direct.rs` | Launch arguments, the command time limit variable, rules and skills listing commands, the locked settings file | Only the engine that has these commands uses them; the other engines are checked on the disk. |
| `src/engine/goal.rs` | The goal plugin and the files of the goal loop | Engines carry their goal loop in different ways. |
| `src/engine/direct_words.rs`, `faults.rs`, `user_settings.rs` | Words, idle-timeout text and user settings that name an engine | The words a person reads name the engine they run on. |
