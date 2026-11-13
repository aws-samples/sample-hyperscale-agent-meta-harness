import { describe, expect, it } from "vitest";
import { formatStats, textStats } from "../src/index";

describe("R4 the package entry point", () => {
  it("re-exports both functions", () => {
    expect(formatStats(textStats("Hello big world"))).toBe("hello-big-world: 3 words, 15 chars");
  });
});
