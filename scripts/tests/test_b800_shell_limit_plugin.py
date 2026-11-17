"""The logic of the command time limit plugin of the second engine, driven by hand under Node.

`crates/agentd/src/engine/opencode_shell_limit.js` is the text agentd writes beside the session document of an OpenCode run. Its one hook gives a
foreground shell call that names no time limit the run's limit and leaves every other call as it is. This test loads that file under Node
(`scripts/node.sh`, the pinned container) with a stand-in for the engine's plugin context and feeds the hook one call per case: no timeout, null, 0,
a longer and a shorter limit, a number written as text, a background call, another tool, an input that is no object; options out of range; a hook
that fails.

What it proves and what it does not: it proves the hook's logic against a stand-in of the engine's plugin API.

The driver is passed to Node as its program text, so the repository holds no second JavaScript file for it. It uses no dollar sign and no backtick,
so that no shell on the way reads it. Without docker the case is skipped with the reason, as the observer's Node test is.
"""

from __future__ import annotations

import json
import shutil
import subprocess  # nosec B404: the test starts the repository's Node wrapper with an argument list, never a shell
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PLUGIN = ROOT / "crates" / "agentd" / "src" / "engine" / "opencode_shell_limit.js"
RANGE = "options.timeoutMs must be a whole number of milliseconds from 1000 to 1800000"

DRIVER = r"""
import { readFileSync } from "node:fs";
const path = process.argv[process.argv.length - 1];
const text = readFileSync(path, "utf8");
let serial = 0;
const load = async () => {
  serial += 1;
  const body = Buffer.from(text + "\n// load " + serial + "\n").toString("base64");
  return (await import("data:text/javascript;base64," + body)).default;
};
const out = [];
const check = (name, ok, detail) => out.push({ name, ok: !!ok, detail: detail === undefined ? "" : String(detail) });
const lines = [];
const realWrite = process.stderr.write.bind(process.stderr);
process.stderr.write = (chunk) => { lines.push(String(chunk)); return true; };
const setup = async (options) => {
  const plugin = await load();
  const hooks = [];
  const ctx = { options, tool: { hook: async (name, fn) => { hooks.push([name, fn]); } } };
  await plugin.setup(ctx);
  return { plugin, hooks };
};
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

const { plugin, hooks } = await setup({ timeoutMs: 600000 });
check("the plugin's id", plugin.id === "hamh.shell-limit", plugin.id);
check("one hook, before a tool runs", hooks.length === 1 && hooks[0][0] === "execute.before", JSON.stringify(hooks.map((h) => h[0])));
check("the ready line is the one thing setup writes, after the hook is in place", lines.length === 1 && lines[0] === "hamh.shell-limit: ready 600000\n", JSON.stringify(lines));
const run = (tool, input) => { const event = { tool, input }; hooks[0][1](event); return event.input; };
check("a call that names no timeout gets the run's limit", same(run("shell", { command: "x" }), { command: "x", timeout: 600000 }));
check("a null timeout gets the run's limit", same(run("shell", { command: "x", timeout: null }), { command: "x", timeout: 600000 }));
check("a timeout of 0 is kept", same(run("shell", { command: "x", timeout: 0 }), { command: "x", timeout: 0 }));
check("a longer limit is kept", same(run("shell", { command: "x", timeout: 960000 }), { command: "x", timeout: 960000 }));
check("a shorter limit is kept", same(run("shell", { command: "x", timeout: 3000 }), { command: "x", timeout: 3000 }));
check("a number written as text is kept as the model wrote it", same(run("shell", { command: "x", timeout: "5000" }), { command: "x", timeout: "5000" }));
check("a background call is left alone", same(run("shell", { command: "x", background: true }), { command: "x", background: true }));
check("background false is a foreground call", same(run("shell", { command: "x", background: false }), { command: "x", background: false, timeout: 600000 }));
check("another tool is left alone", same(run("read", { path: "a" }), { path: "a" }));
for (const odd of [null, "text", 7, ["a"]]) {
  check("an input that is no object is left alone: " + JSON.stringify(odd), same(run("shell", odd), odd));
}
check("an input that is absent is left alone", run("shell", undefined) === undefined);
const given = { command: "x" };
const event = { tool: "shell", input: given };
hooks[0][1](event);
check("the hook hands the engine a new input and leaves the given object as it was", event.input !== given && !("timeout" in given) && event.input.timeout === 600000);
check("the hook writes nothing for these calls", lines.length === 1, JSON.stringify(lines));

for (const bad of [undefined, {}, { timeoutMs: 999 }, { timeoutMs: 1800001 }, { timeoutMs: 1500.5 }, { timeoutMs: "abc" }, { timeoutMs: null }]) {
  const before = lines.length;
  let said = "";
  try { await setup(bad); } catch (e) { said = String(e && e.message); }
  check("options out of range end the setup and no ready line is written: " + JSON.stringify(bad), said.includes("RANGE_TEXT") && lines.length === before, said);
}
for (const [good, ms] of [[{ timeoutMs: 1000 }, 1000], [{ timeoutMs: 1800000 }, 1800000], [{ timeoutMs: "5000" }, 5000]]) {
  const before = lines.length;
  const made = await setup(good);
  check("options in range set the limit: " + JSON.stringify(good), lines.length === before + 1 && lines[before] === "hamh.shell-limit: ready " + ms + "\n"
    && same((() => { const e = { tool: "shell", input: { command: "x" } }; made.hooks[0][1](e); return e.input; })(), { command: "x", timeout: ms }), JSON.stringify(lines.slice(before)));
}

const failing = await setup({ timeoutMs: 5000 });
const before = lines.length;
const bomb = () => {
  const e = { tool: "shell" };
  Object.defineProperty(e, "input", { get() { throw new Error("boom"); }, set() {}, configurable: true });
  return e;
};
let threw = false;
try { failing.hooks[0][1](bomb()); failing.hooks[0][1](bomb()); failing.hooks[0][1](bomb()); } catch { threw = true; }
check("a hook that fails never throws: the call goes through", !threw);
check("a hook that fails writes one line, once", lines.length === before + 1 && lines[before] === "hamh.shell-limit: execute.before failed: boom\n", JSON.stringify(lines.slice(before)));
const after = { tool: "shell", input: { command: "y" } };
failing.hooks[0][1](after);
check("the hook goes on working after a failure", same(after.input, { command: "y", timeout: 5000 }));

process.stderr.write = realWrite;
console.log(JSON.stringify({ results: out, allOk: out.every((x) => x.ok) }));
""".replace("RANGE_TEXT", RANGE)


def node(*args: str) -> subprocess.CompletedProcess:
    """Runs Node in the pinned container (scripts/node.sh) from the repository root."""
    return subprocess.run(  # nosec B603: argument list, repository script, no shell
        [str(ROOT / "scripts" / "node.sh"), "node", *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=600,
        check=False,
    )


class DriverTextCase(unittest.TestCase):
    def test_b800_the_driver_holds_nothing_a_shell_would_read(self) -> None:
        for mark in ("$", "`"):
            self.assertNotIn(mark, DRIVER)

    def test_b800_the_plugin_names_the_range_the_driver_looks_for(self) -> None:
        self.assertIn(RANGE, PLUGIN.read_text(encoding="utf-8"))


class PluginUnderNodeCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if shutil.which("docker") is None:
            raise unittest.SkipTest("docker is not installed: scripts/node.sh cannot run Node")
        cls.out = node("--input-type=module", "-e", DRIVER, "--", str(PLUGIN))

    def results(self) -> dict:
        self.assertEqual(self.out.returncode, 0, self.out.stdout[-2000:] + self.out.stderr[-2000:])
        return json.loads(self.out.stdout.strip().splitlines()[-1])

    def test_b800_the_hook_gives_the_limit_only_to_a_foreground_shell_call_that_names_none(self) -> None:
        res = self.results()
        self.assertEqual([r for r in res["results"] if not r["ok"]], [])
        self.assertTrue(res["allOk"])
        self.assertEqual(len(res["results"]), 32, [r["name"] for r in res["results"]])

    def test_b800_the_cases_cover_every_input_the_brief_names(self) -> None:
        names = " | ".join(r["name"] for r in self.results()["results"])
        for needle in ("names no timeout", "null timeout", "timeout of 0", "longer limit", "shorter limit", "written as text", "background call",
                       "another tool", "no object", "out of range", "writes one line, once", "ready line"):
            with self.subTest(needle=needle):
                self.assertIn(needle, names)


if __name__ == "__main__":
    unittest.main()
