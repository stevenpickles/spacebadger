import { describe, expect, it } from "vitest";
import { contextAt, keywords, values } from "./completion";

describe("git: completion", () => {
  it("offers git: as a keyword", () => {
    expect(keywords("g").map((k) => k.text)).toEqual(["git:"]);
    expect(keywords("").some((k) => k.text === "git:")).toBe(true);
  });

  it("completes the value after git:, also after !", () => {
    expect(contextAt("*.js; git:w", 11)).toMatchObject({ kind: "value", prefix: "git:", typed: "w", start: 6 });
    expect(contextAt("!git:", 5)).toMatchObject({ kind: "value", prefix: "git:", typed: "", start: 1 });
    expect(contextAt("GIT:Re", 6)).toMatchObject({ kind: "value", prefix: "git:", typed: "Re" });
  });

  it("offers every value with a description", () => {
    expect(values("git:", "").map((k) => k.text)).toEqual(["git:repo", "git:worktree", "git:metadata"]);
    expect(values("git:", "M").map((k) => k.text)).toEqual(["git:metadata"]);
    expect(values("git:", "w")[0]?.describe).toBe("in linked worktree checkouts");
  });

  it("leaves other contexts as they were", () => {
    expect(contextAt("dir:no", 6)?.kind).toBe("folder");
    expect(contextAt("is:vi", 5)?.kind).toBe("type");
    expect(contextAt("size:>1", 7)?.prefix).toBe("size:");
    expect(contextAt("*.j", 3)?.kind).toBe("extension");
  });
});
