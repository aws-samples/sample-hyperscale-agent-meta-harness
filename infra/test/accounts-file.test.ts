// The account-file override HAMH_ACCOUNTS_FILE: setup.sh synthesizes a stranger's stage from a generated account file
// instead of the committed accounts.json. The rules are scripts/lib/aws_target.py's:
//   - an absolute path replaces the committed file; unset or empty reads the committed file;
//   - a relative path, a missing file and a directory throw, and never fall back to the committed file;
//   - the file rules of selectTarget apply to the override.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test, vi } from "vitest";
import * as config from "../lib/config.ts";
import { egressDomains } from "../lib/stacks/network.ts";

const COMMITTED = fileURLToPath(new URL("../../accounts.json", import.meta.url));
const EXAMPLE = fileURLToPath(new URL("../../fixtures/setup/accounts-generated.json", import.meta.url));
const EXAMPLE_DOC = JSON.parse(readFileSync(EXAMPLE, "utf8"));
const scratch = (): string => mkdtempSync(join(tmpdir(), "accounts-file-"));

describe("accountsFilePath", () => {
  test("the variable is HAMH_ACCOUNTS_FILE", () => {
    expect(config.ACCOUNTS_FILE_ENV).toBe("HAMH_ACCOUNTS_FILE");
  });

  test("unset or empty names the committed accounts.json", () => {
    expect(config.accountsFilePath({})).toBe(COMMITTED);
    expect(config.accountsFilePath({ HAMH_ACCOUNTS_FILE: "" })).toBe(COMMITTED);
    expect(config.accountsFilePath({ HAMH_ACCOUNTS_FILE: undefined })).toBe(COMMITTED);
  });

  test("an absolute path is returned as given", () => {
    expect(config.accountsFilePath({ HAMH_ACCOUNTS_FILE: EXAMPLE })).toBe(EXAMPLE);
  });

  test("a relative path is refused, not resolved against the working directory", () => {
    for (const rel of ["accounts.json", "./accounts.json", "fixtures/setup/accounts-generated.json", "~/accounts.json"]) {
      expect(() => config.accountsFilePath({ HAMH_ACCOUNTS_FILE: rel })).toThrow(/HAMH_ACCOUNTS_FILE must be an absolute path/);
      expect(() => config.accountsFilePath({ HAMH_ACCOUNTS_FILE: rel })).toThrow(rel);
    }
  });
});

describe("loadAccounts", () => {
  test("unset reads the committed file", () => {
    expect(config.loadAccounts({})).toEqual(JSON.parse(readFileSync(COMMITTED, "utf8")));
  });

  test("the override is read instead, and the selection follows it", () => {
    const doc = config.loadAccounts({ HAMH_ACCOUNTS_FILE: EXAMPLE });
    expect(doc).toEqual(EXAMPLE_DOC);
    const t = config.selectTarget(doc, {});
    expect([t.account, t.region, t.role, t.profile]).toEqual(["111122223333", "us-east-1", "active", "your-aws-profile"]);
    expect(t.azs.direct).toEqual(["us-east-1a"]);
    expect(t.azIds.direct).toEqual(["use1-az1"]);
  });

  test("a missing file and a directory are refusals that name the path", () => {
    const dir = scratch();
    const gone = join(dir, "gone.json");
    expect(() => config.loadAccounts({ HAMH_ACCOUNTS_FILE: gone })).toThrow(gone);
    expect(() => config.loadAccounts({ HAMH_ACCOUNTS_FILE: dir })).toThrow(dir);
  });

  test("a file that is not JSON is a refusal that names the path", () => {
    const p = join(scratch(), "junk.json");
    writeFileSync(p, "{");
    expect(() => config.loadAccounts({ HAMH_ACCOUNTS_FILE: p })).toThrow(p);
  });

  test("the file rules apply to the override", () => {
    const dir = scratch();
    const write = (name: string, doc: unknown): string => {
      const p = join(dir, name);
      writeFileSync(p, JSON.stringify(doc));
      return p;
    };
    const two = { ...EXAMPLE_DOC, accounts: [EXAMPLE_DOC.accounts[0], { ...EXAMPLE_DOC.accounts[0], profile: "other", account: "444455556666" }] };
    expect(() => config.selectTarget(config.loadAccounts({ HAMH_ACCOUNTS_FILE: write("two.json", two) }), {})).toThrow(/exactly one active/);
    const sel = { ...EXAMPLE_DOC, selector: "AWS_PROFILE" };
    expect(() => config.selectTarget(config.loadAccounts({ HAMH_ACCOUNTS_FILE: write("sel.json", sel) }), {})).toThrow(/selector/);
  });
});

// A fresh module graph per selection: config.ts reads HAMH_ACCOUNTS_FILE and HAMH_AWS_PROFILE when it loads.
async function under(accountsFile: string | undefined) {
  const prev = process.env.HAMH_ACCOUNTS_FILE;
  if (accountsFile === undefined) delete process.env.HAMH_ACCOUNTS_FILE;
  else process.env.HAMH_ACCOUNTS_FILE = accountsFile;
  vi.resetModules();
  try {
    const cfg = await import("../lib/config.ts");
    const { buildApp } = await import("../lib/app.ts");
    const { Template } = await import("aws-cdk-lib/assertions");
    return { cfg, buildApp, Template };
  } finally {
    if (prev === undefined) delete process.env.HAMH_ACCOUNTS_FILE;
    else process.env.HAMH_ACCOUNTS_FILE = prev;
  }
}

describe("the module values follow the override", () => {
  test("ACCOUNT, REGION, PROFILE and the stage AZs are the override's", async () => {
    const { cfg } = await under(EXAMPLE);
    expect([cfg.ACCOUNT, cfg.REGION, cfg.PROFILE]).toEqual(["111122223333", "us-east-1", "your-aws-profile"]);
    expect(cfg.stageConfig("direct").azs).toEqual(["us-east-1a"]);
  });

  test("unset still gives the committed active entry", async () => {
    const { cfg } = await under(undefined);
    expect(cfg.selectTarget(JSON.parse(readFileSync(COMMITTED, "utf8")), {}).account).toBe(cfg.ACCOUNT);
    expect(cfg.ACCOUNT).not.toBe("111122223333");
  });
});

describe("synth with HAMH_ACCOUNTS_FILE", () => {
  test("every stack's env, the subnet AZ, the allowlist and the operator principals follow the file", async () => {
    const { buildApp, Template } = await under(EXAMPLE);
    const app = buildApp({ stage: "direct" });
    const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
    expect(stacks).toHaveLength(9);
    for (const s of stacks) {
      expect([s.stackName, s.account, s.region]).toEqual([s.stackName, "111122223333", "us-east-1"]);
    }
    const net = Template.fromStack(stacks.find((s) => s.stackName === "hamh-direct-network"));
    const subnets = Object.values(net.findResources("AWS::EC2::Subnet")) as any[];
    const privateAzs = subnets.filter((r) => JSON.stringify(r.Properties.Tags).includes('"private"'))
      .map((r) => r.Properties.AvailabilityZone).sort();
    expect(privateAzs).toEqual(["us-east-1a"]);
    const group = Object.values(net.findResources("AWS::NetworkFirewall::RuleGroup"))[0] as any;
    expect(group.Properties.RuleGroup.RulesSource.RulesSourceList.Targets).toEqual(egressDomains("us-east-1"));
    const control = Template.fromStack(stacks.find((s) => s.stackName === "hamh-direct-control"));
    const fn = (Object.values(control.findResources("AWS::Lambda::Function")) as any[]).find((r) => r.Properties.FunctionName === "hamh-direct-control-api");
    expect(fn.Properties.Environment.Variables.OPERATOR_PRINCIPALS).toBe("arn:aws:sts::111122223333:assumed-role/Admin/*");
  });

  test("the committed file is untouched: the same app without the variable targets the committed active entry", async () => {
    const { cfg, buildApp } = await under(undefined);
    const app = buildApp({ stage: "direct" });
    const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
    for (const s of stacks) expect([s.account, s.region]).toEqual([cfg.ACCOUNT, cfg.REGION]);
    expect(cfg.ACCOUNT).not.toBe("111122223333");
  });
});
