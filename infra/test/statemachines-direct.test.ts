import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";

// Direct run: the structure of statemachines/direct.asl.json, the run machine of stage `direct`.
// The machine is 23 states and 12 Tasks (the last a Fail state); run-session decides every step and the machine only routes. After each session
// `NextSlice` asks run-session whether another follows, and the one cycle of the graph (NextSlice, SliceChoice, ResetSession, StartSession) is that loop.
// The behavior of the paths is tested in crates/run-session/tests/machine.rs and slice_loop.rs, which walk this same file over fakes of its four Lambdas.
const ASL = new URL("../../statemachines/direct.asl.json", import.meta.url);
const text = readFileSync(ASL, "utf8");
const def = JSON.parse(text) as { StartAt: string; TimeoutSeconds?: number; States: Record<string, any> };
const states = def.States;
const names = Object.keys(states);

/** Every state a state names: Next, Default, each Choice's Next, each Catch's Next. */
function targets(st: any): string[] {
  return [
    ...(st.Next ? [st.Next] : []),
    ...(st.Default ? [st.Default] : []),
    ...(st.Choices ?? []).map((c: any) => c.Next),
    ...(st.Catch ?? []).map((c: any) => c.Next),
  ];
}

describe("direct.asl.json: shape", () => {
  test("23 states, 12 of them Tasks, and the machine has a timeout of the longest total a run may ask for plus the delivery", () => {
    expect(names.length).toBe(23);
    expect(names.filter((n) => states[n].Type === "Task").length).toBe(12);
    expect(def.StartAt).toBe("Name");
    // 30 days (MAX_MAX_RUN_S) and ten hours for the stage preparation and the delivery
    expect(def.TimeoutSeconds).toBe(30 * 24 * 3600 + 36_000);
  });

  test("every Next, Default and Catch target names a state, and every state is reachable from the first one", () => {
    for (const n of names) for (const t of targets(states[n])) expect(names, `${n} -> ${t}`).toContain(t);
    expect(names).toContain(def.StartAt);
    const seen = new Set<string>();
    const walk = (n: string) => {
      if (seen.has(n)) return;
      seen.add(n);
      targets(states[n]).forEach(walk);
    };
    walk(def.StartAt);
    expect([...seen].sort()).toEqual([...names].sort());
  });

  test("the only cycle is the slice loop, and the machine has no Wait state: a lost session is the heartbeat's to find, not a poll loop", () => {
    expect(names.filter((n) => states[n].Type === "Wait")).toEqual([]);
    const color = new Map<string, number>();
    const visit = (n: string) => {
      if (color.get(n) === 1) throw new Error(`cycle through ${n}`);
      if (color.get(n) === 2) return;
      color.set(n, 1);
      // the edge that closes the loop is the one from ResetSession back to StartSession
      targets(states[n]).filter((t) => !(n === "ResetSession" && t === "StartSession")).forEach(visit);
      color.set(n, 2);
    };
    expect(() => visit(def.StartAt)).not.toThrow();
    expect(states.ResetSession.Next).toBe("StartSession");
    expect(states.ResetSession.Type).toBe("Pass");
  });

  test("a state ends the execution only when it has End or no successor: Finalize and Blocked, and the Fail state after a Blocked that could not run", () => {
    const ends = names.filter((n) => states[n].End === true || states[n].Type === "Succeed" || states[n].Type === "Fail");
    expect(ends.sort()).toEqual(["Blocked", "Finalize", "RunMachineFailed"]);
    for (const n of names) {
      if (ends.includes(n)) continue;
      expect(targets(states[n]).length, n).toBeGreaterThan(0);
    }
  });
});

describe("direct.asl.json: what it does not hold", () => {
  test("no state names a function of the multi-agent line", () => {
    for (const forbidden of ["RunTasksArn", "DispatchArn", "GateRunnerArn", "EvaluateArn", "HiddenAdmissionArn", "GoalStepArn", "AdmissionArn"]) {
      expect(text).not.toContain(forbidden);
    }
    expect(text).not.toContain("worker-goal-loop");
  });

  test("the only substitutions are the four functions", () => {
    const subs = [...new Set(text.match(/\$\{[A-Za-z]+\}/g) ?? [])].sort();
    expect(subs).toEqual(["${BundleValidatorArn}", "${PackagerArn}", "${RepoGatewayArn}", "${RunSessionArn}"]);
  });

  test("the only intrinsic function is States.JsonMerge", () => {
    const used = [...new Set([...text.matchAll(/States\.([A-Za-z]+)\(/g)].map((m) => m[1]))];
    expect(used).toEqual(["JsonMerge"]);
  });

  test("no word of the harness checking the work in any state's comment", () => {
    const comments = names.map((n) => states[n].Comment ?? "").join(" ") + (JSON.parse(text).Comment as string);
    for (const banned of ["verified", "tested", "gated", "evaluated"]) {
      expect(new RegExp(`\\b${banned}\\b`, "i").test(comments), banned).toBe(false);
    }
  });
});

describe("direct.asl.json: the Name state", () => {
  const name = states.Name;

  test("it is the first state, asks run-session to name the run and does not wait for the answer", () => {
    expect(def.StartAt).toBe("Name");
    expect(name.Type).toBe("Task");
    expect(name.Resource).toBe("arn:aws:states:::lambda:invoke");
    expect(name.Parameters).toEqual({ FunctionName: "${RunSessionArn}", InvocationType: "Event", Payload: { op: "name-run", "runId.$": "$.runId" } });
    expect(name.Next).toBe("Prepare");
  });

  test("its result is dropped, so Prepare gets the input of the execution", () => {
    expect(name.ResultPath).toBeNull();
    expect(name.ResultSelector).toBeUndefined();
    expect(name.Catch).toEqual([{ ErrorEquals: ["States.ALL"], ResultPath: null, Next: "Prepare" }]);
  });

  test("a call that cannot be made is tried three times in all, with a short wait, and then ignored: a label never blocks a run", () => {
    expect(name.TimeoutSeconds).toBe(15);
    expect(name.Retry).toEqual([
      {
        ErrorEquals: ["Lambda.ServiceException", "Lambda.TooManyRequestsException", "Lambda.SdkClientException"],
        IntervalSeconds: 1,
        MaxAttempts: 2,
        BackoffRate: 2,
        JitterStrategy: "FULL",
      },
    ]);
    expect(JSON.stringify(name.Retry)).not.toContain("RunSession");
  });

  test("no other state sends an asynchronous call: every other Task waits for its Lambda's answer", () => {
    const events = names.filter((n) => states[n].Parameters?.InvocationType !== undefined);
    expect(events).toEqual(["Name"]);
  });
});

describe("direct.asl.json: the session state", () => {
  const start = states.StartSession;
  test("waits for the task token with a heartbeat of 600 s and a timeout of 28,500 s", () => {
    expect(start.Resource).toBe("arn:aws:states:::lambda:invoke.waitForTaskToken");
    expect(start.HeartbeatSeconds).toBe(600);
    expect(start.TimeoutSeconds).toBe(28_500);
    expect(start.TimeoutSeconds).toBeGreaterThan(start.HeartbeatSeconds);
    expect(start.ResultPath).toBe("$.session.turn");
    expect(start.Parameters.Payload["taskToken.$"]).toBe("$$.Task.Token");
    expect(start.Parameters.Payload.op).toBe("start-session");
  });

  test("a lost session, a timeout and every other failure go to NextSlice with the failure kept", () => {
    expect(start.Catch).toEqual([{ ErrorEquals: ["States.ALL"], ResultPath: "$.session.failure", Next: "NextSlice" }]);
    expect(start.Next).toBe("NextSlice");
  });

  test("the slice to start is the loop's, and Init starts the loop at slice 1 with nothing counted", () => {
    expect(start.Parameters.Payload["slice.$"]).toBe("$.loop.slice");
    expect(states.Init.Parameters.loop).toEqual({ slice: 1, lost: 0, empty: 0, continue: false });
  });

  test("a retry is for run-session's own transient errors and Lambda's, never for the state's heartbeat or timeout", () => {
    const retried = start.Retry.flatMap((r: any) => r.ErrorEquals);
    expect(retried).toEqual([
      "RunSessionConflict", "RunSessionUnavailable", "Lambda.ServiceException", "Lambda.TooManyRequestsException", "Lambda.SdkClientException",
    ]);
    expect(retried).not.toContain("States.ALL");
    expect(retried).not.toContain("States.HeartbeatTimeout");
    expect(retried).not.toContain("States.Timeout");
  });
});

describe("direct.asl.json: the slice loop", () => {
  const next = states.NextSlice;

  test("NextSlice asks run-session how the slice ended and keeps the answer as the loop", () => {
    expect(next.Resource).toBe("arn:aws:states:::lambda:invoke");
    expect(next.Parameters.Payload).toEqual({
      op: "next-slice", "runId.$": "$.runId", "staged.$": "$.staged.result", "session.$": "$.session", "loop.$": "$.loop",
    });
    expect(next.ResultPath).toBe("$.loop");
    expect(Object.keys(next.ResultSelector).sort()).toEqual(["continue.$", "empty.$", "endReason.$", "lost.$", "slice.$"]);
    expect(next.Next).toBe("SliceChoice");
  });

  test("a failure of NextSlice delivers what the loop left instead of blocking the run, which would lose the snapshot", () => {
    expect(next.Catch).toEqual([{ ErrorEquals: ["States.ALL"], ResultPath: "$.nextFailure", Next: "Plan" }]);
  });

  test("SliceChoice starts the next slice on continue and delivers otherwise", () => {
    expect(states.SliceChoice.Choices).toEqual([{ Variable: "$.loop.continue", BooleanEquals: true, Next: "ResetSession" }]);
    expect(states.SliceChoice.Default).toBe("Plan");
  });

  test("ResetSession clears the session result and keeps the loop, the run and what Prepare and StageRepo made", () => {
    expect(states.ResetSession.Parameters).toEqual({
      "runId.$": "$.runId", "prep.$": "$.prep", "staged.$": "$.staged", "loop.$": "$.loop",
      session: { turn: {}, failure: {} }, prOut: {}, prFailure: {},
    });
  });

  test("Plan is told where the loop ended", () => {
    expect(states.Plan.Parameters.Payload["loop.$"]).toBe("$.loop");
  });
});

describe("direct.asl.json: retries, catches and timeouts", () => {
  const lambdaTasks = names.filter((n) => states[n].Type === "Task");

  test("every Task sets a timeout, below the machine's", () => {
    for (const n of lambdaTasks) {
      const t = states[n].TimeoutSeconds;
      expect(t, n).toBeGreaterThan(0);
      expect(t, n).toBeLessThan(def.TimeoutSeconds!);
    }
    expect(states.Package.TimeoutSeconds).toBe(900);
  });

  test("every Task catches States.ALL, so no failure ends the execution inside a state that has nowhere to go", () => {
    for (const n of lambdaTasks) {
      const all = (states[n].Catch ?? []).filter((c: any) => c.ErrorEquals.includes("States.ALL"));
      expect(all.length, n).toBe(1);
      expect((states[n].Catch as any[]).at(-1).ErrorEquals, `${n}: States.ALL last`).toContain("States.ALL");
    }
  });

  test("Blocked, the last resort, catches into a Fail state that carries the cause: a function that cannot answer fails the execution with a reason", () => {
    expect(states.Blocked.Catch).toEqual([{ ErrorEquals: ["States.ALL"], ResultPath: "$.blockedFailure", Next: "RunMachineFailed" }]);
    expect(states.RunMachineFailed).toMatchObject({ Type: "Fail", Error: "RunSessionUnreachable", CausePath: "$.blockedFailure.Cause" });
    expect(states.RunMachineFailed.Next).toBeUndefined();
  });

  test("a failed pull request push does not block; every other catch lands in Blocked, Plan or the slice decision, except the one from Name into Prepare", () => {
    expect(states.PushPullRequest.Catch[0].Next).toBe("PrFailed");
    expect(states.PushPullRequest.Catch[0].ResultPath).toBe("$.prFailure");
    for (const n of lambdaTasks) {
      if (n === "PushPullRequest" || n === "StartSession" || n === "NextSlice" || n === "Blocked" || n === "Name") continue;
      for (const c of states[n].Catch ?? []) expect(c.Next, n).toBe("Blocked");
    }
    // The one exception: a label is not worth a blocked run, so a Name that fails goes on to Prepare with the input it had.
    const exceptions = lambdaTasks.flatMap((n) => (states[n].Catch ?? []).filter((c: any) => !["Blocked", "Plan", "NextSlice", "PrFailed", "RunMachineFailed"].includes(c.Next)).map((c: any) => `${n} -> ${c.Next}`));
    expect(exceptions).toEqual(["Name -> Prepare"]);
  });

  test("the run-session ops retry its conflict, its unavailability and Lambda's errors; the delivery Lambdas their Unavailable", () => {
    const rs = ["RunSessionConflict", "RunSessionUnavailable", "Lambda.ServiceException", "Lambda.TooManyRequestsException", "Lambda.SdkClientException"];
    const rd = ["Unavailable", "Lambda.ServiceException", "Lambda.TooManyRequestsException", "Lambda.SdkClientException"];
    for (const n of ["Prepare", "NextSlice", "Plan", "Finalize", "Blocked"]) {
      expect(states[n].Retry, n).toEqual([expect.objectContaining({ ErrorEquals: rs, IntervalSeconds: 2, MaxAttempts: 6, BackoffRate: 2 })]);
    }
    for (const n of ["CreateRepo", "StageRepo", "ValidateBundle", "PushPullRequest"]) {
      expect(states[n].Retry, n).toEqual([expect.objectContaining({ ErrorEquals: rd, IntervalSeconds: 5, MaxAttempts: 5, BackoffRate: 2 })]);
    }
    expect(states.Package.Retry[0]).toMatchObject({ ErrorEquals: rd, IntervalSeconds: 5, MaxAttempts: 4, BackoffRate: 2 });
  });

  test("which function each Task calls", () => {
    const fn = (n: string) => states[n].Parameters.FunctionName;
    expect(["Name", "Prepare", "StartSession", "NextSlice", "Plan", "Finalize", "Blocked"].map(fn)).toEqual(Array(7).fill("${RunSessionArn}"));
    expect(["CreateRepo", "StageRepo", "PushPullRequest"].map(fn)).toEqual(Array(3).fill("${RepoGatewayArn}"));
    expect(fn("ValidateBundle")).toBe("${BundleValidatorArn}");
    expect(fn("Package")).toBe("${PackagerArn}");
    const ops = Object.fromEntries(["Name", "Prepare", "StartSession", "NextSlice", "Plan", "Finalize", "Blocked"].map((n) => [n, states[n].Parameters.Payload.op]));
    expect(ops).toEqual({ Name: "name-run", Prepare: "prepare", StartSession: "start-session", NextSlice: "next-slice", Plan: "plan-delivery", Finalize: "finalize", Blocked: "mark-blocked" });
  });
});

/** The paths a state's Parameters read with `.$`, including the arguments of States.JsonMerge. */
function reads(params: unknown): string[] {
  const out: string[] = [];
  const walk = (v: any) => {
    if (Array.isArray(v)) return v.forEach(walk);
    if (v && typeof v === "object") {
      for (const [k, x] of Object.entries(v)) {
        if (k.endsWith(".$") && typeof x === "string") {
          if (x.startsWith("States.")) out.push(...[...x.matchAll(/\$[A-Za-z0-9_.]*/g)].map((m) => m[0]));
          else out.push(x);
        } else walk(x);
      }
    }
  };
  walk(params);
  return out.filter((p) => p !== "$" && !p.startsWith("$$"));
}

/** The paths a Pass state's Parameters (or an object result) define, nested constants included. */
function defined(params: any, base = "$"): string[] {
  const out: string[] = [];
  for (const [k, v] of Object.entries(params ?? {})) {
    const key = k.replace(/\.\$$/, "");
    out.push(`${base}.${key}`);
    if (!k.endsWith(".$") && v && typeof v === "object" && !Array.isArray(v)) out.push(...defined(v, `${base}.${key}`));
  }
  return out;
}

describe("direct.asl.json: every path a Parameters block reads is written on every path that reaches the state", () => {
  type Defs = Set<string>;
  // Forward data flow: the paths certainly written when a state is entered. The graph has one cycle (the slice loop), so the walk repeats until
  // no entry set shrinks any more (a "must be written" analysis only ever loses paths).
  const entry = new Map<string, Defs>();
  const order: string[] = [];
  const seen = new Set<string>();
  const topo = (n: string) => {
    if (seen.has(n)) return;
    seen.add(n);
    targets(states[n]).forEach(topo);
    order.unshift(n);
  };
  topo(def.StartAt);

  const outgoing = (n: string, inDefs: Defs): { to: string; defs: Defs }[] => {
    const st = states[n];
    const ok = new Set(inDefs);
    if (st.Type === "Task") {
      const path = st.ResultPath as string | undefined;
      if (path) {
        ok.add(path);
        for (const k of Object.keys(st.ResultSelector ?? {})) ok.add(`${path}.${k.replace(/\.\$$/, "")}`);
      }
    } else if (st.Type === "Pass") {
      const written = defined(st.Parameters ?? (st.Result ? { x: 1 } : {}));
      if (st.ResultPath) {
        ok.add(st.ResultPath);
        for (const w of written.filter(() => st.Parameters)) ok.add(w.replace("$.", `${st.ResultPath}.`));
      } else if (st.Parameters) {
        return [{ to: st.Next, defs: new Set(written) }];
      }
    }
    const edges: { to: string; defs: Defs }[] = [];
    if (st.Next) edges.push({ to: st.Next, defs: ok });
    if (st.Default) edges.push({ to: st.Default, defs: ok });
    for (const c of st.Choices ?? []) edges.push({ to: c.Next, defs: ok });
    for (const c of st.Catch ?? []) {
      const caught = new Set(inDefs);
      if (c.ResultPath) caught.add(c.ResultPath);
      edges.push({ to: c.Next, defs: caught });
    }
    return edges;
  };

  entry.set(def.StartAt, new Set(["$.runId"]));
  for (let changed = true; changed; ) {
    changed = false;
    for (const n of order) {
      const here = entry.get(n);
      if (!here) continue;
      for (const { to, defs } of outgoing(n, here)) {
        const prior = entry.get(to);
        const next = prior ? new Set([...prior].filter((p) => defs.has(p))) : new Set(defs);
        if (!prior || next.size !== prior.size) changed = true;
        entry.set(to, next);
      }
    }
  }
  const covered = (path: string, defs: Defs) => [...defs].some((d) => path === d || path.startsWith(`${d}.`));

  test("every state is analysed", () => {
    expect([...entry.keys()].sort()).toEqual([...names].sort());
  });

  for (const n of names) {
    const st = states[n];
    if (st.Type === "Choice" || !st.Parameters) continue;
    test(`${n} reads only paths written before it`, () => {
      for (const path of reads(st.Parameters)) {
        expect(covered(path, entry.get(n)!), `${n} reads ${path}; written: ${[...entry.get(n)!].sort().join(", ")}`).toBe(true);
      }
    });
  }

  test("a Choice on a value reads a path written before it", () => {
    for (const n of names.filter((x) => states[x].Type === "Choice")) {
      for (const c of states[n].Choices) {
        if (c.IsPresent !== undefined) continue;
        expect(covered(c.Variable, entry.get(n)!), `${n} reads ${c.Variable}`).toBe(true);
      }
    }
  });

  test("the Fail state reads the cause its Catch wrote", () => {
    expect(covered(states.RunMachineFailed.CausePath, entry.get("RunMachineFailed")!)).toBe(true);
  });

  test("Init rebuilds the state with empty objects, never a null constant", () => {
    expect(states.Init.Parameters.session).toEqual({ turn: {}, failure: {} });
    expect(states.Init.Parameters.prFailure).toEqual({});
    expect(JSON.stringify(states.Init)).not.toContain("null");
  });
});
