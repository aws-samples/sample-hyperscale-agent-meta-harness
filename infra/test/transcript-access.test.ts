import { describe, expect, test } from "vitest";
import { ACCOUNT } from "../lib/config.ts";
import { phase2Images, synth } from "./helpers.ts";

const { templates } = synth("direct", phase2Images("direct"));

type Stmt = Record<string, any>;
const roles = (stackName: string): any[] =>
  Object.values((templates.get(stackName)!.toJSON().Resources ?? {}) as Record<string, any>).filter((r) => r.Type === "AWS::IAM::Role");
const statements = (stackName: string, roleName: string): Stmt[] => {
  const role = roles(stackName).find((r) => r.Properties.RoleName === roleName);
  expect(role, roleName).toBeDefined();
  return role.Properties.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
};
const actions = (s: Stmt): string[] => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;

describe("the finalize-time indexer", () => {
  test("run-session already reads and lists the transcript prefix and writes the owner's search item, so N46 adds no grant", () => {
    const rs = statements("hamh-direct-orchestration", "hamh-direct-run-session");
    const outputs = rs.find((s) => s.Sid === "AgentOutputs")!;
    expect(actions(outputs)).toEqual(["s3:GetObject"]);
    expect(outputs.Resource).toBe(`${bucket}/runs/*/agents/direct/turns/*`);
    const list = rs.find((s) => s.Sid === "ListForAbsentKeys")!;
    expect(actions(list)).toEqual(["s3:ListBucket"]);
    expect(list.Resource).toBe(bucket);
    const table = rs.find((s) => s.Sid === "HarnessTable")!;
    expect(actions(table)).toEqual(expect.arrayContaining(["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem"]));
    // It cannot write or delete anything in the transcript prefix: the only object writes are the evidence file, the checked test report (also under evidence/), the session nonce and the records of a run's slices (runs/<run>/slices/).
    const writes = rs.filter((s) => actions(s).some((a) => /^s3:(Put|Delete)/.test(a)));
    expect(writes.map((s) => s.Sid).sort()).toEqual(["Evidence", "SessionNonce", "SliceRecords", "TestReportEvidence"]);
  });

  test("the worker's session role writes its own prefix but can neither delete nor list anything else", () => {
    const worker = statements("hamh-direct-security", "hamh-direct-harness-worker-session");
    const own = worker.find((s) => s.Sid === "ReadWriteOwnAgentPrefix")!;
    expect(actions(own).sort()).toEqual(["s3:AbortMultipartUpload", "s3:GetObject", "s3:PutObject"]);
    expect(JSON.stringify(worker.filter((s) => s.Effect === "Allow"))).not.toMatch(/s3:Delete|s3:PutObjectVersion/);
  });
});

describe("the table's stream", () => {
  test("no function reads it and no role may: a search body that enters it has no consumer", () => {
    const streamActions = /dynamodb:(GetRecords|GetShardIterator|DescribeStream|ListStreams)|dynamodb:\*|^\*$/;
    for (const [name, t] of templates) {
      const resources = (t.toJSON().Resources ?? {}) as Record<string, any>;
      expect(Object.values(resources).filter((r) => r.Type === "AWS::Lambda::EventSourceMapping"), name).toEqual([]);
      const docs: Stmt[] = [];
      for (const r of Object.values(resources)) {
        if (r.Type === "AWS::IAM::Role") docs.push(...(r.Properties.Policies ?? []).flatMap((p: any) => p.PolicyDocument.Statement));
        if (r.Type === "AWS::IAM::Policy" || r.Type === "AWS::IAM::ManagedPolicy") docs.push(...r.Properties.PolicyDocument.Statement);
      }
      for (const s of docs.filter((d) => d.Effect === "Allow")) {
        const bad = actions(s).filter((a) => streamActions.test(a));
        expect(bad, `${name} ${s.Sid ?? ""}`).toEqual([]);
      }
    }
  });
});
