// harness-rounds: the harness's observer plugin. It appends one JSON line to
// <data dir>/plugins/harness-rounds/rounds.jsonl when an engine run starts and one when it ends.
// The goal plugin queues its follow-up prompts itself, so those runs have no session/prompt response:
// these two hooks are the only place that sees when each run starts and how it ends (status, final text, error).
// It writes a file and calls nothing else. Every write is inside try/catch: a failed write must never fail a run, and
// the supervisor in agentd falls back to the ACP stream, the silence watchdog and the deadline when the file is missing or damaged.
// The done claim never rests on this file: the model can write it, the ACP stream it cannot.
import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

const DATA = process.env.CLINE_DATA_DIR || join(homedir(), ".cline", "data");
const PATH = join(DATA, "plugins", "harness-rounds", "rounds.jsonl");
const TEXT_TAIL = 1500;
let started = 0;
let ended = 0;

function line(obj) {
  try {
    mkdirSync(dirname(PATH), { recursive: true });
    appendFileSync(PATH, `${JSON.stringify(obj)}\n`);
  } catch {
    // no file, no observation
  }
}

export default {
  name: "harness-rounds",
  manifest: { capabilities: ["hooks"] },
  setup() {},
  hooks: {
    beforeRun() {
      line({ ev: "start", n: ++started, t: Date.now() });
      return undefined;
    },
    afterRun(arg) {
      try {
        const result = (arg && arg.result) || {};
        const err = result.error ? String(result.error.message ?? result.error).slice(0, 300) : undefined;
        line({
          ev: "end",
          n: ++ended,
          t: Date.now(),
          runId: result.runId,
          status: result.status,
          iterations: result.iterations,
          inputTokens: result.usage?.inputTokens,
          outputTokens: result.usage?.outputTokens,
          error: err,
          text: String(result.outputText ?? "").slice(-TEXT_TAIL),
        });
      } catch {
        // see line()
      }
    },
  },
};
