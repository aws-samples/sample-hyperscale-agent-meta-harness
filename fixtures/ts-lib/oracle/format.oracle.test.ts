import { describe, expect, it } from "vitest";
import { formatStats } from "../src/format";

describe("R2 formatStats", () => {
  it("renders slug, words and chars", () => {
    expect(formatStats({ words: 3, chars: 15, slug: "hello-big-world" })).toBe(
      "hello-big-world: 3 words, 15 chars",
    );
  });
});
