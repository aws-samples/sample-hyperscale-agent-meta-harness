import { describe, expect, it } from "vitest";
import { textStats } from "../src/stats";

describe("R1 textStats", () => {
  it("counts words and characters and builds the slug", () => {
    expect(textStats("Hello big world")).toEqual({ words: 3, chars: 15, slug: "hello-big-world" });
  });

  it("drops punctuation from the slug", () => {
    expect(textStats("Hi, there!").slug).toBe("hi-there");
  });
});

describe("R3 textStats of an empty text", () => {
  it("is all zero with an empty slug", () => {
    expect(textStats("")).toEqual({ words: 0, chars: 0, slug: "" });
  });
});
