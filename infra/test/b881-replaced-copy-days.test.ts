import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { resourcesOf, template } from "./helpers.ts";

// The client tells a user for how long a copy of a run's work is kept after a newer copy replaced it
// (crates/ham-core/src/storage_notice.rs, REPLACED_COPY_DAYS). The number is the bucket's rule for replaced versions; this test
// holds the two together.
describe("b881 the days the client names equal the bucket's rule for replaced copies", () => {
  test("REPLACED_COPY_DAYS of the client is the bucket's NoncurrentDays", () => {
    const t = template("direct", "data");
    const buckets = Object.values(resourcesOf(t, "AWS::S3::Bucket"));
    const days = buckets
      .flatMap((b: any) => b.Properties?.LifecycleConfiguration?.Rules ?? [])
      .map((r: any) => r.NoncurrentVersionExpiration?.NoncurrentDays)
      .filter((d: unknown) => typeof d === "number");
    expect(days.length).toBeGreaterThan(0);
    const source = readFileSync(new URL("../../crates/ham-core/src/storage_notice.rs", import.meta.url), "utf8");
    const named = /pub const REPLACED_COPY_DAYS: u32 = (\d+);/.exec(source);
    expect(named, "REPLACED_COPY_DAYS in crates/ham-core/src/storage_notice.rs").not.toBeNull();
    // The run's copies are in the harness bucket: every bucket rule for replaced versions of this stack is the same number.
    for (const d of days) expect(d).toBe(Number(named![1]));
  });
});
