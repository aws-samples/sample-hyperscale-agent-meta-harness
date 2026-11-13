// PLG1: drive the goal plugin's setup, hooks and tool by hand (no Cline) to pin the state-file contract.
// usage: node plugin-unit.mjs <plugin module> [data dir]    (no data dir: a new directory under the system temp directory)
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
const [, , modPath, givenDir] = process.argv;
const dataDir = givenDir || mkdtempSync(join(tmpdir(), "goal-unit-"));
process.env.CLINE_DATA_DIR = dataDir;
const queued = [];
globalThis.__clinePluginHost = { emitEvent: (name, payload) => queued.push({ name, payload }) };
const mod = await import(modPath);
const plugin = mod.default;
const reg = { tools: [], commands: [], rules: [] };
const api = {
  registerTool: (t) => reg.tools.push(t),
  registerCommand: (c) => reg.commands.push(c),
  registerRule: (r) => reg.rules.push(r),
};
const SESSION = "sess-1";
const state = () => (existsSync(join(dataDir, "plugins/goal/goals.json")) ? JSON.parse(readFileSync(join(dataDir, "plugins/goal/goals.json"), "utf8")) : null);
const out = [];
const check = (name, ok, detail) => { out.push({ name, ok: !!ok, detail }); };
// agentd's side: write pendingGoal before the first prompt.
mkdirSync(join(dataDir, "plugins/goal"), { recursive: true });
writeFileSync(join(dataDir, "plugins/goal/goals.json"), JSON.stringify({ version: 1, pendingGoal: { goal: "make hello.txt contain 42", createdAt: new Date().toISOString() }, activeGoals: {} }, null, 2) + "\n");
plugin.setup(api, { session: { sessionId: SESSION } });
check("plugin name and capabilities", plugin.name === "goal" && plugin.manifest.capabilities.includes("tools"), JSON.stringify(plugin.manifest));
check("tool registered", reg.tools.length === 1 && reg.tools[0].name === "mark_goal_complete", reg.tools.map((t) => t.name).join(","));
check("tool defaults", reg.tools[0].timeoutMs === 30000 && reg.tools[0].retryable === false && reg.tools[0].inputSchema.type === "object", JSON.stringify({ t: reg.tools[0].timeoutMs, r: reg.tools[0].retryable, s: reg.tools[0].inputSchema.type }));
// before the first run: the rule is empty until the goal is claimed? (rule content is a function read at each request)
const ruleBefore = typeof reg.rules[0].content === "function" ? reg.rules[0].content() : reg.rules[0].content;
check("rule mentions the pending goal before the claim", /Goal is active/.test(ruleBefore), ruleBefore.split("\n")[0]);
plugin.hooks.beforeRun();
const s1 = state();
check("beforeRun claims the pending goal for the session", s1.pendingGoal === undefined && s1.activeGoals[SESSION]?.goal === "make hello.txt contain 42", JSON.stringify(s1));
// the model calls the tool during the initial run: refused
const early = await reg.tools[0].execute({ summary: "too early" }, { sessionId: SESSION });
check("mark_goal_complete before the verification prompt is refused", early.completed === false, JSON.stringify(early));
// first run ends: afterRun queues the verification prompt and flags the goal
plugin.hooks.afterRun({ result: { status: "completed" } });
check("afterRun queues one verification prompt", queued.length === 1 && queued[0].name === "queue_message" && /Are you sure you've completed the goal: make hello.txt contain 42/.test(queued[0].payload.prompt), JSON.stringify(queued[0]).slice(0, 200));
check("goal is flagged awaitingVerification", state().activeGoals[SESSION].awaitingVerification === true, JSON.stringify(state().activeGoals[SESSION]));
// the model answers without calling the tool: the next afterRun queues again (no cap)
for (let i = 0; i < 5; i++) plugin.hooks.afterRun({ result: { status: "completed" } });
check("the loop has no cap (6 queued after 6 runs)", queued.length === 6, String(queued.length));
// a run that did not complete (cancelled, error) queues nothing
plugin.hooks.afterRun({ result: { status: "cancelled" } });
check("a cancelled run queues nothing", queued.length === 6, String(queued.length));
// the tool call that finishes the goal
const done = await reg.tools[0].execute({ summary: "all done" }, { sessionId: SESSION });
check("mark_goal_complete after the verification prompt completes the goal", done.completed === true && done.summary === "all done", JSON.stringify(done));
check("goal leaves the state file", Object.keys(state().activeGoals).length === 0 && state().pendingGoal === undefined, JSON.stringify(state()));
plugin.hooks.afterRun({ result: { status: "completed" } });
check("no further prompt after completion", queued.length === 6, String(queued.length));
// slash command forms
const cmd = reg.commands[0];
const r = cmd.handler("status");
check("/goal status text", /No goal is active/.test(String(r)), String(r));
console.log(JSON.stringify({ results: out, allOk: out.every((x) => x.ok) }, null, 1));
