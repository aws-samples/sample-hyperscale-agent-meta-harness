// Drives the harness-rounds observer by hand (no engine): its two hooks write one line each, a failed write never throws.
// usage: node observer-unit.mjs <observer module>    (works in a new directory under the system temp directory)
import { readFileSync, existsSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
const [, , modPath] = process.argv;
const work = mkdtempSync(join(tmpdir(), "observer-unit-"));
const dataDir = join(work, "data");
// a regular file where the data directory should be: every write under it fails
const blocker = join(work, "blocker");
writeFileSync(blocker, "x");
process.env.CLINE_DATA_DIR = dataDir;
const out = [];
const check = (name, ok, detail) => out.push({ name, ok: !!ok, detail });
const mod = await import(modPath);
const plugin = mod.default;
check("name and capability", plugin.name === "harness-rounds" && plugin.manifest.capabilities.length === 1 && plugin.manifest.capabilities[0] === "hooks", JSON.stringify(plugin.manifest));
check("setup does nothing", plugin.setup() === undefined, "");
check("beforeRun returns nothing", plugin.hooks.beforeRun() === undefined, "");
plugin.hooks.afterRun({ result: { runId: "r1", status: "completed", iterations: 3, usage: { inputTokens: 7, outputTokens: 2 }, outputText: "x".repeat(3000) + "\nBLOCKED: need the key" } });
plugin.hooks.beforeRun();
plugin.hooks.afterRun({ result: { status: "aborted", error: new Error("boom") } });
plugin.hooks.afterRun({});
plugin.hooks.afterRun(undefined);
const path = join(dataDir, "plugins", "harness-rounds", "rounds.jsonl");
const rows = existsSync(path) ? readFileSync(path, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l)) : [];
check("start and end lines are written in order", rows.map((r) => `${r.ev}${r.n}`).join(",") === "start1,end1,start2,end2,end3,end4", rows.map((r) => `${r.ev}${r.n}`).join(","));
check("end line carries status, iterations, tokens and the tail of the text", rows[1].status === "completed" && rows[1].iterations === 3 && rows[1].inputTokens === 7 && rows[1].outputTokens === 2 && rows[1].text.endsWith("BLOCKED: need the key") && rows[1].text.length === 1500, JSON.stringify(rows[1]).slice(0, 200));
check("an error is one short line", rows[3].status === "aborted" && rows[3].error === "boom", JSON.stringify(rows[3]));
check("every line has a time", rows.every((r) => typeof r.t === "number" && r.t > 0), "");
// a data dir that cannot be written: no throw
let threw = false;
try {
  process.env.CLINE_DATA_DIR = blocker;
  const p2 = (await import(modPath + "?second")).default;
  p2.hooks.beforeRun();
  p2.hooks.afterRun({ result: { status: "completed" } });
} catch {
  threw = true;
}
check("a failed write never throws", !threw, "");
console.log(JSON.stringify({ results: out, allOk: out.every((x) => x.ok) }, null, 1));
