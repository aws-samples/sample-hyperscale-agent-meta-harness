// The operator identity of an accounts.json entry: the optional field `operator` names the one IAM role (or IAM Identity Center role) or IAM user
// that runs setup and later operates the stage. Without the field the stage trusts the IAM role `Admin` as before, byte for byte; with it the stage trusts
// exactly that identity wherever `Admin` was written: the operator principals of control-api, the deploying principal (the E2E second principal, the Go module
// store's filler role and its delete exception) and the minters of worker sessions. The rules are scripts/lib/aws_target.py's (scripts/tests/test_operator_identity.py)
// and the words are one table: test/fixtures/operator-identity.json.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { beforeAll, describe, expect, test, vi } from "vitest";
import * as config from "../lib/config.ts";
import * as operator from "../lib/operator.ts";

const TABLE = JSON.parse(readFileSync(new URL("./fixtures/operator-identity.json", import.meta.url), "utf8"));
const EXAMPLE = fileURLToPath(new URL("../../fixtures/setup/accounts-generated.json", import.meta.url));
const EXAMPLE_DOC = JSON.parse(readFileSync(EXAMPLE, "utf8"));
const ACCOUNT = "111122223333";
const OTHER = "444455556666";
// A table value with the account placeholders filled in (any string inside a JSON value).
const fill = (v: unknown): any => JSON.parse(JSON.stringify(v).replaceAll("{account}", ACCOUNT).replaceAll("{other}", OTHER));

describe("the operator field", () => {
  test("every accepted word parses to the identity it names and has the label, the control-api patterns and the aws:PrincipalArn patterns of the table", () => {
    expect(TABLE.accept.length).toBeGreaterThanOrEqual(5);
    for (const row of TABLE.accept) {
      const got = operator.parseOperator(row.operator, "accounts[0]");
      expect(got, row.label).toEqual(row.operator);
      expect(operator.operatorLabel(got), row.label).toBe(row.label);
      expect(operator.callerArnPatterns(ACCOUNT, got), row.label).toEqual(fill(row.caller));
      expect(operator.principalArnPatterns(ACCOUNT, got), row.label).toEqual(fill(row.principal));
    }
  });

  test("the longest accepted name has 64 characters and the shortest refused long one has 65", () => {
    expect(Math.max(...TABLE.accept.map((r: any) => r.operator.name.length))).toBe(64);
    const long = TABLE.refuse.find((r: any) => r.why === "65 characters");
    expect(long.operator.name.length).toBe(65);
  });

  test("every refused word is refused with the reason of the table and the place in the file", () => {
    expect(TABLE.refuse.length).toBeGreaterThanOrEqual(15);
    for (const row of TABLE.refuse) {
      expect(() => operator.parseOperator(fill(row.operator), "accounts.json accounts[2]"), row.why).toThrow(
        new RegExp(`^accounts\\.json accounts\\[2\\]: ${row.reason.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`));
    }
  });

  test("a caller is the operator when its identity is the one the field names, or the setup runner of the account", () => {
    expect(TABLE.callers.length).toBeGreaterThanOrEqual(15);
    for (const row of TABLE.callers) {
      const arn: string = fill(row.arn);
      expect(operator.operatorMayDeploy(ACCOUNT, fill(row.operator), arn), `${row.operator.kind}/${row.operator.name} ${arn}`).toBe(row.ok);
    }
  });

  test("the setup runner's role is the one name the template infra/setup-runner.yaml creates", () => {
    const template = readFileSync(new URL("../setup-runner.yaml", import.meta.url), "utf8");
    expect(operator.SETUP_RUNNER_ROLE).toBe("hamh-setup-runner-role");
    expect(template).toContain(`RoleName: ${operator.SETUP_RUNNER_ROLE}\n`);
    expect(operator.runnerRoleArn(ACCOUNT)).toBe(`arn:aws:iam::${ACCOUNT}:role/hamh-setup-runner-role`);
  });
});

describe("selectTarget and the operator field", () => {
  const doc = (op: unknown) => ({ ...EXAMPLE_DOC, accounts: [{ ...EXAMPLE_DOC.accounts[0], ...(op === undefined ? {} : { operator: op }) }] });

  test("an entry without the field has no operator", () => {
    expect(config.selectTarget(doc(undefined), {}).operator).toBeUndefined();
  });

  test("an entry with the field carries the identity, for each accepted word", () => {
    for (const row of TABLE.accept) expect(config.selectTarget(doc(row.operator), {}).operator, row.label).toEqual(row.operator);
  });

  test("a refused word makes the whole file a refusal that names the entry", () => {
    for (const row of TABLE.refuse) {
      expect(() => config.selectTarget(doc(fill(row.operator)), {}), row.why).toThrow(/^accounts\.json accounts\[0\]: operator/);
    }
  });

  test("an entry that is not selected is checked as well", () => {
    const second = { ...EXAMPLE_DOC.accounts[0], profile: "other", role: "test", account: OTHER, operator: { kind: "role", name: "bad name" } };
    expect(() => config.selectTarget({ ...EXAMPLE_DOC, accounts: [EXAMPLE_DOC.accounts[0], second] }, {})).toThrow(/accounts\[1\]: operator\.name must be/);
  });
});

// A fresh module graph per accounts file: config.ts reads HAMH_ACCOUNTS_FILE when it loads (the pattern of test/accounts-file.test.ts).
async function under(file: string | undefined) {
  const prev = process.env.HAMH_ACCOUNTS_FILE;
  if (file === undefined) delete process.env.HAMH_ACCOUNTS_FILE;
  else process.env.HAMH_ACCOUNTS_FILE = file;
  vi.resetModules();
  try {
    const cfg = await import("../lib/config.ts");
    const { buildApp } = await import("../lib/app.ts");
    const { Template } = await import("aws-cdk-lib/assertions");
    const stacks = (buildApp({ stage: "direct" }).node.children.filter((c) => "templateOptions" in c)) as any[];
    const tpl = (name: string) => Template.fromStack(stacks.find((s) => s.stackName === `hamh-direct-${name}`)).toJSON();
    return { cfg, tpl, all: () => stacks.map((s) => Template.fromStack(s).toJSON()) };
  } finally {
    if (prev === undefined) delete process.env.HAMH_ACCOUNTS_FILE;
    else process.env.HAMH_ACCOUNTS_FILE = prev;
  }
}

function accountsFile(op: unknown): string {
  const p = join(mkdtempSync(join(tmpdir(), "operator-")), "accounts.json");
  const entry = { ...EXAMPLE_DOC.accounts[0], ...(op === undefined ? {} : { operator: op }) };
  writeFileSync(p, JSON.stringify({ ...EXAMPLE_DOC, accounts: [entry] }));
  return p;
}

const rolesOf = (t: any) => Object.values(t.Resources).filter((r: any) => r.Type === "AWS::IAM::Role") as any[];
const roleNamed = (t: any, name: string) => rolesOf(t).find((r) => JSON.stringify(r.Properties.RoleName).includes(name));
const bucketPolicy = (t: any) => (Object.values(t.Resources) as any[]).find((r) => r.Type === "AWS::S3::BucketPolicy" && JSON.stringify(r).includes("DenyDeletesFromOtherPrincipals"));
const controlApiEnv = (t: any) => (Object.values(t.Resources) as any[]).find((r) => r.Type === "AWS::Lambda::Function" && r.Properties.FunctionName === "hamh-direct-control-api").Properties.Environment.Variables;

const FORMS = [
  { name: "an IAM role", op: { kind: "role", name: "StageOperator" } },
  { name: "an IAM Identity Center role", op: { kind: "role", name: "AWSReservedSSO_AdministratorAccess_0123456789abcdef" } },
  { name: "an IAM user", op: { kind: "user", name: "alice" } },
] as const;

describe.each(FORMS)("a stage whose operator is $name", ({ op }) => {
  const root = `arn:aws:iam::${ACCOUNT}:root`;
  const mine = operator.principalArnPatterns(ACCOUNT, op);
  const runner = `arn:aws:iam::${ACCOUNT}:role/hamh-setup-runner-role`;
  let loaded: Awaited<ReturnType<typeof under>>;
  beforeAll(async () => { loaded = await under(accountsFile(op)); });

  test("control-api takes exactly this identity as its operator principals", () => {
    const { cfg, tpl } = loaded;
    expect(cfg.OPERATOR_PRINCIPALS).toBe(operator.callerArnPatterns(ACCOUNT, op).join(","));
    expect(controlApiEnv(tpl("control")).OPERATOR_PRINCIPALS).toBe(operator.callerArnPatterns(ACCOUNT, op).join(","));
  });

  test("the E2E second principal is assumed by this identity only (the account root narrowed by aws:PrincipalArn, so the identity need not exist when the role is made)", () => {
    const { tpl } = loaded;
    const trust = roleNamed(tpl("security"), "hamh-direct-e2e-other-principal").Properties.AssumeRolePolicyDocument.Statement;
    expect(trust).toEqual([{
      Sid: "DeployingPrincipalOnly", Effect: "Allow", Principal: { AWS: root },
      Condition: { ArnLike: { "aws:PrincipalArn": mine } },
      Action: ["sts:AssumeRole", "sts:SetSourceIdentity"],
    }]);
  });

  test("this identity mints worker sessions, with the same tag rules as the Admin role did", () => {
    const { tpl } = loaded;
    const trust = roleNamed(tpl("security"), "hamh-direct-harness-worker-session").Properties.AssumeRolePolicyDocument.Statement as any[];
    expect(trust).toHaveLength(2);
    const minters = [...mine, `arn:aws:iam::${ACCOUNT}:role/hamh-direct-worker-api`, `arn:aws:iam::${ACCOUNT}:role/hamh-direct-run-session`];
    for (const st of trust) {
      expect(Object.keys(st.Condition), st.Sid).not.toContain("ArnEquals");
      expect(st.Condition.ArnLike, st.Sid).toEqual({ "aws:PrincipalArn": minters });
    }
    const tags = trust.find((s) => s.Action.includes("sts:AssumeRole"));
    expect(tags.Condition["ForAllValues:StringEquals"]["aws:TagKeys"].sort()).toEqual(["agent", "engine", "mode", "run", "tenant"]);
    expect(tags.Condition.Null).toEqual({ "aws:RequestTag/run": "false", "aws:RequestTag/agent": "false" });
  });

  test("the Go module store's filler role is assumed by this identity or the setup runner, and only they may delete from the store", () => {
    const { tpl } = loaded;
    const packages = tpl("packages");
    const filler = roleNamed(packages, "hamh-direct-gomod-filler").Properties.AssumeRolePolicyDocument.Statement;
    expect(filler).toEqual([{
      Sid: "DeployingPrincipalOnly", Effect: "Allow", Principal: { AWS: root },
      Condition: { ArnLike: { "aws:PrincipalArn": [...mine, runner] } },
      Action: ["sts:AssumeRole", "sts:SetSourceIdentity"],
    }]);
    const deletes = bucketPolicy(packages).Properties.PolicyDocument.Statement.find((s: any) => s.Sid === "DenyDeletesFromOtherPrincipals");
    expect(deletes.Condition).toEqual({ ArnNotLike: { "aws:PrincipalArn": [...mine, runner] } });
  });

  test("the setup runner is named nowhere else", () => {
    const { all } = loaded;
    const named = all().map((t, i) => [i, JSON.stringify(t).split("hamh-setup-runner-role").length - 1]).filter(([, n]) => n > 0);
    expect(named.length).toBe(1);
    expect(named[0][1]).toBe(2);
  });

  test("no template still names the Admin role", () => {
    const { all } = loaded;
    for (const t of all()) expect(JSON.stringify(t)).not.toMatch(/\bAdmin\b/);
  });
});

describe("a stage whose entry has no operator field", () => {
  let loaded: Awaited<ReturnType<typeof under>>;
  beforeAll(async () => { loaded = await under(EXAMPLE); });

  test("control-api, the trusts and the bucket policy name the IAM role Admin, as before the field existed", () => {
    const { cfg, tpl } = loaded;
    expect(cfg.OPERATOR_PRINCIPALS).toBe(`arn:aws:sts::${ACCOUNT}:assumed-role/Admin/*`);
    expect(controlApiEnv(tpl("control")).OPERATOR_PRINCIPALS).toBe(`arn:aws:sts::${ACCOUNT}:assumed-role/Admin/*`);
    const admin = `arn:aws:iam::${ACCOUNT}:role/Admin`;
    const other = roleNamed(tpl("security"), "hamh-direct-e2e-other-principal").Properties.AssumeRolePolicyDocument.Statement;
    expect(other).toEqual([{ Sid: "DeployingPrincipalOnly", Effect: "Allow", Principal: { AWS: admin }, Action: ["sts:AssumeRole", "sts:SetSourceIdentity"] }]);
    const filler = roleNamed(tpl("packages"), "hamh-direct-gomod-filler").Properties.AssumeRolePolicyDocument.Statement;
    expect(filler).toEqual([{ Sid: "DeployingPrincipalOnly", Effect: "Allow", Principal: { AWS: admin }, Action: ["sts:AssumeRole", "sts:SetSourceIdentity"] }]);
    const deletes = bucketPolicy(tpl("packages")).Properties.PolicyDocument.Statement.find((s: any) => s.Sid === "DenyDeletesFromOtherPrincipals");
    expect(deletes.Condition).toEqual({ ArnNotLike: { "aws:PrincipalArn": [admin] } });
    const session = roleNamed(tpl("security"), "hamh-direct-harness-worker-session").Properties.AssumeRolePolicyDocument.Statement as any[];
    for (const st of session) expect(st.Condition.ArnEquals).toEqual({ "aws:PrincipalArn": [admin, `arn:aws:iam::${ACCOUNT}:role/hamh-direct-worker-api`, `arn:aws:iam::${ACCOUNT}:role/hamh-direct-run-session`] });
  });

  test("the setup runner's role is not named by any template", () => {
    const { all } = loaded;
    for (const t of all()) expect(JSON.stringify(t)).not.toContain("hamh-setup-runner-role");
  });
});
