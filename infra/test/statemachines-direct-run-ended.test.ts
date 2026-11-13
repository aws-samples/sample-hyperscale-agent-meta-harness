import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";

// Direct run (defect F1/2): statemachines/direct-run-ended.asl.json, the machine the run-ended rule starts. A run whose execution
// ended ABORTED, FAILED or TIMED_OUT while run-session could not answer (the live scenario: its init failed on a missing parameter)
// was left `active`, because EventBridge's asynchronous invocation retried twice within minutes and gave up. This machine calls
// run-session and keeps calling it every five minutes for days, so the run is cleared as soon as the function answers. The behavior
// is tested in crates/run-session/tests/reaper_machine.rs, which walks this same file.
const ASL = new URL("../../statemachines/direct-run-ended.asl.json", import.meta.url);
const text = readFileSync(ASL, "utf8");
const def = JSON.parse(text) as { StartAt: string; TimeoutSeconds: number; States: Record<string, any> };
const reap = def.States.Reap;

describe("direct-run-ended.asl.json", () => {
  test("three states: Reap calls run-session with the event as it came; Dropped ends an event that can never be reaped; ReaperGaveUp makes exhaustion visible", () => {
    expect(Object.keys(def.States).sort()).toEqual(["Dropped", "Reap", "ReaperGaveUp"]);
    expect(def.StartAt).toBe("Reap");
    expect(reap.Resource).toBe("arn:aws:states:::lambda:invoke");
    expect(reap.Parameters).toEqual({ FunctionName: "${RunSessionArn}", "Payload.$": "$" });
    expect(reap.ResultPath).toBeNull();
    expect(reap.End).toBe(true);
    expect(def.States.Dropped.Type).toBe("Succeed");
    expect(def.States.ReaperGaveUp).toMatchObject({ Type: "Fail", Error: "RunNotReaped", CausePath: "$.failure.Cause" });
  });

  test("a refusal (RunSessionInvalid) is dropped, not retried: no later attempt can make a foreign or finished event reapable", () => {
    expect(reap.Retry[0]).toEqual({ ErrorEquals: ["RunSessionInvalid"], MaxAttempts: 0 });
    expect(reap.Catch).toEqual([
      { ErrorEquals: ["RunSessionInvalid"], ResultPath: "$.dropped", Next: "Dropped" },
      { ErrorEquals: ["States.ALL"], ResultPath: "$.failure", Next: "ReaperGaveUp" },
    ]);
  });

  test("every other failure is retried every five minutes for days, whatever its name: a function that is down for a deploy does not lose the run", () => {
    expect(reap.Retry).toHaveLength(2);
    const r = reap.Retry[1];
    // States.ALL is last and alone (the service requires it), so an error nobody listed (a conflict during a deploy, a timeout
    // reported as Sandbox.Timedout, a permission fault) is retried too.
    expect(r.ErrorEquals).toEqual(["States.ALL"]);
    expect(r.IntervalSeconds).toBe(300);
    expect(r.BackoffRate).toBe(1);
    expect(r.MaxAttempts * r.IntervalSeconds).toBeGreaterThanOrEqual(3 * 24 * 3600);
    // The attempts, not the clock, bound the machine; its timeout only has to be longer than 1001 calls and 1000 waits.
    expect(def.TimeoutSeconds).toBeGreaterThan((r.MaxAttempts + 1) * reap.TimeoutSeconds + r.MaxAttempts * r.IntervalSeconds);
    expect(reap.TimeoutSeconds).toBeGreaterThanOrEqual(300);
  });

  test("the only substitution is run-session, and no state comment claims the harness checked the work", () => {
    expect([...new Set(text.match(/\$\{[A-Za-z]+\}/g) ?? [])]).toEqual(["${RunSessionArn}"]);
    const comments = Object.values(def.States).map((s: any) => s.Comment ?? "").join(" ") + (JSON.parse(text).Comment as string);
    for (const banned of ["verified", "tested", "gated", "evaluated"]) {
      expect(new RegExp(`\\b${banned}\\b`, "i").test(comments), banned).toBe(false);
    }
  });
});
