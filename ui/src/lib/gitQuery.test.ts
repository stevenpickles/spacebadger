import { describe, expect, it } from "vitest";
import { gitScope, gitTerm, hasGitTerms, withGitScope } from "./gitQuery";

describe("gitTerm", () => {
  it("reads values as sb-core does: any start of a keyword, ignoring case", () => {
    expect(gitTerm("git:repo")).toEqual({ negated: false, value: "repo" });
    expect(gitTerm("GIT: Repositories ")).toEqual({ negated: false, value: "repo" });
    expect(gitTerm("git:re")?.value).toBe("repo");
    expect(gitTerm("git:w")?.value).toBe("worktree");
    expect(gitTerm("git:met")?.value).toBe("metadata");
    expect(gitTerm("! git:meta")).toEqual({ negated: true, value: "metadata" });
  });

  it("marks rejected and still-typed values", () => {
    expect(gitTerm("git:x")?.value).toBeNull();
    expect(gitTerm("git:repox")?.value).toBeNull();
    expect(gitTerm("git:metadatas")?.value).toBeNull();
    expect(gitTerm("git:")?.value).toBeUndefined();
  });

  it("ignores other terms", () => {
    expect(gitTerm("dir:git:stuff")).toBeNull();
    expect(gitTerm("git :repo")).toBeNull();
    expect(gitTerm("*.git")).toBeNull();
  });
});

describe("gitScope", () => {
  it.each([
    ["", "all"],
    ["*.jpg; dir:cache", "all"],
    ["git:", "all"],
    ["git:repo", "repo"],
    ["git:r; is:code", "repo"],
    ["git:worktree", "worktree"],
    ["git:repo; !git:metadata", "repo"],
    ["git:repo; !git:worktree", "repo"],
    ["git:worktree; !git:metadata", "worktree"],
    ["git:repo; !git:repo", "custom"],
    ["git:worktree; !git:worktree", "custom"],
    ["git:worktree; !git:repo", "custom"],
    ["git:metadata", "custom"],
    ["!git:metadata", "custom"],
    ["git:repo; git:worktree", "custom"],
    ["git:x", "custom"],
    ["git:repo; !git:x", "custom"],
  ])("%j is %s", (query, scope) => {
    expect(gitScope(query)).toBe(scope);
  });
});

describe("withGitScope", () => {
  it("keeps other terms verbatim and in order", () => {
    expect(withGitScope("*.jpg;  dir:My Photos ; size:>1MB", "repo")).toBe("*.jpg; dir:My Photos; size:>1MB; git:repo");
    expect(withGitScope("*.jpg; git:worktree; !*.tmp", "repo")).toBe("*.jpg; !*.tmp; git:repo");
    expect(withGitScope("*.js;;", "repo")).toBe("*.js; git:repo");
    expect(withGitScope("dir:git:stuff", "repo")).toBe("dir:git:stuff; git:repo");
  });

  it("keeps exclusions that don't leave the choice out", () => {
    expect(withGitScope("git:repo; is:code; !git:metadata", "worktree")).toBe("is:code; !git:metadata; git:worktree");
    expect(withGitScope("!git:worktree; git:metadata", "repo")).toBe("!git:worktree; git:repo");
    expect(withGitScope("!git:worktree; git:repo", "worktree")).toBe("git:worktree");
    expect(withGitScope("!git:repo; is:video", "worktree")).toBe("is:video; git:worktree");
    expect(withGitScope("!git:repo", "repo")).toBe("git:repo");
  });

  it("drops every valid Git term for all", () => {
    expect(withGitScope("is:code; git:repo; !git:metadata; git:", "all")).toBe("is:code");
    expect(withGitScope("git:repo", "all")).toBe("");
  });

  it("leaves rejected Git terms for the filter to report", () => {
    expect(withGitScope("git:x; *.js", "all")).toBe("git:x; *.js");
    expect(withGitScope("!git:x; git:repo", "worktree")).toBe("!git:x; git:worktree");
  });

  it("round-trips through gitScope", () => {
    for (const scope of ["repo", "worktree", "all"] as const) {
      for (const base of ["", "*.jpg", "git:metadata; size:>1GB", "!git:repo; is:video", "!git:worktree"]) {
        expect(gitScope(withGitScope(base, scope))).toBe(scope);
      }
    }
  });

  it("finds Git terms even while typed", () => {
    expect(hasGitTerms("*.jpg; git:")).toBe(true);
    expect(hasGitTerms("*.jpg")).toBe(false);
  });
});
