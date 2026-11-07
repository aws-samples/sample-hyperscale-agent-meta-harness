import { existsSync, readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import * as config from "../lib/config.ts";
import { ACTIVE, DIRECT_AZS, HISTORY, TEST_ENTRY, THREE_ACCOUNTS, threeAccountsFile, underEntry, withEnv, writeAccountsFile } from "./accounts-three.ts";

// The tests that need every role build their accounts file from infra/test/accounts-three.ts: the committed accounts.json holds one neutral entry. The helper is the same
// file as the shared fixture of the script tests, scripts/tests/fixtures/accounts-three.json, and it must stay so.
describe("the three-entry accounts fixture", () => {
  test("equals scripts/tests/fixtures/accounts-three.json when that file is in the tree", () => {
    const file = new URL("../../scripts/tests/fixtures/accounts-three.json", import.meta.url);
    if (!existsSync(file)) return; // an exported tree may leave the script tests out
    expect(JSON.parse(readFileSync(file, "utf8"))).toEqual(THREE_ACCOUNTS);
  });

  test("selectTarget gives each entry its constants and its direct AZs; the active entry is the default", () => {
    const core = (t: config.Target) => ({ account: t.account, region: t.region, profile: t.profile, role: t.role });
    for (const e of [HISTORY, ACTIVE, TEST_ENTRY]) {
      const t = config.selectTarget(THREE_ACCOUNTS, { HAMH_AWS_PROFILE: e.profile });
      expect(core(t), e.profile).toEqual(e);
      expect(t.azs.direct, e.profile).toEqual(DIRECT_AZS[e.role]);
    }
    expect(core(config.selectTarget(THREE_ACCOUNTS, {}))).toEqual(ACTIVE);
  });

  test("the file it writes is what HAMH_ACCOUNTS_FILE loads, and the environment is put back", async () => {
    const path = writeAccountsFile();
    expect(config.loadAccounts({ HAMH_ACCOUNTS_FILE: path })).toEqual(THREE_ACCOUNTS);
    const before = { file: process.env.HAMH_ACCOUNTS_FILE, profile: process.env.HAMH_AWS_PROFILE };
    await underEntry(TEST_ENTRY.profile, () => {
      expect(process.env.HAMH_ACCOUNTS_FILE).toBe(threeAccountsFile());
      expect(process.env.HAMH_AWS_PROFILE).toBe(TEST_ENTRY.profile);
    });
    expect({ file: process.env.HAMH_ACCOUNTS_FILE, profile: process.env.HAMH_AWS_PROFILE }).toEqual(before);
    await withEnv({ HAMH_AWS_PROFILE: undefined }, () => expect(process.env.HAMH_AWS_PROFILE).toBeUndefined());
  });
});
