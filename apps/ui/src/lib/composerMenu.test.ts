import { describe, expect, it } from "vitest";

import {
  applyCompletion,
  detectComposerTrigger,
  fileReference,
  fuzzyScore,
  placePopover,
  rankCommands,
  rankFilePaths,
  splitPath,
  stepSelection,
  triggerKey,
} from "./composerMenu";

describe("@ file trigger", () => {
  it("opens at the start of the composer", () => {
    expect(detectComposerTrigger("@", 1)).toEqual({
      kind: "file",
      query: "",
      start: 0,
      end: 1,
    });
  });

  it("opens at a word boundary and carries the typed query", () => {
    expect(detectComposerTrigger("look at @src/comp", 17)).toEqual({
      kind: "file",
      query: "src/comp",
      start: 8,
      end: 17,
    });
  });

  it("does not open inside an email address", () => {
    expect(detectComposerTrigger("me@example.com", 14)).toBeNull();
  });

  it("does not open inside a word after another @", () => {
    expect(detectComposerTrigger("foo@bar", 7)).toBeNull();
  });

  it("treats any non-identifier character as a boundary", () => {
    for (const before of ["(", "[", "{", '"', "'", ",", "=", ":", "-", "@"]) {
      const value = `see ${before}@src`;
      const trigger = detectComposerTrigger(value, value.length);
      expect(trigger, `after ${before}`).toEqual({
        kind: "file",
        query: "src",
        start: 5,
        end: value.length,
      });
    }
  });

  it("keeps path characters in the query", () => {
    const value = "(see @apps/ui/src/comp-menu.ts";
    expect(detectComposerTrigger(value, value.length)?.query).toBe("apps/ui/src/comp-menu.ts");
  });

  it("replaces the whole token when the caret sits before its end", () => {
    expect(detectComposerTrigger("ask @src now", 6)).toEqual({
      kind: "file",
      query: "s",
      start: 4,
      end: 8,
    });
  });

  it("treats a newline as a boundary", () => {
    expect(detectComposerTrigger("line one\n@read", 14)?.kind).toBe("file");
  });

  it("tracks the query only up to the caret", () => {
    expect(detectComposerTrigger("ask @src/co now", 11)?.query).toBe("src/co");
  });
});

describe("/ command trigger", () => {
  it("opens on the first character alone", () => {
    expect(detectComposerTrigger("/", 1)).toEqual({
      kind: "command",
      query: "",
      start: 0,
      end: 1,
    });
  });

  it("filters as the first token grows", () => {
    expect(detectComposerTrigger("/term", 5)).toEqual({
      kind: "command",
      query: "term",
      start: 0,
      end: 5,
    });
  });

  it("does not open a slash later in the prompt", () => {
    expect(detectComposerTrigger("  /stop", 7)).toBeNull();
  });

  it("closes once the first token is finished", () => {
    expect(detectComposerTrigger("/stop now", 9)).toBeNull();
  });

  it("does not treat a path as a command", () => {
    expect(detectComposerTrigger("see docs/ux.md", 15)).toBeNull();
  });
});

describe("fuzzy path ranking", () => {
  const files = [
    "README.md",
    "apps/ui/src/components/Composer.tsx",
    "apps/ui/src/components/thread/Composer.tsx",
    "apps/ui/src/lib/composerMenu.ts",
    "docs/ux.md",
    "src/with space.ts",
  ];

  it("matches a path segment anywhere in the path", () => {
    expect(rankFilePaths(files, "src/comp")[0]).toContain("src/components");
  });

  it("is case-insensitive and prefers the basename", () => {
    expect(rankFilePaths(files, "readme")[0]).toBe("README.md");
    expect(rankFilePaths(files, "MENU")[0]).toBe("apps/ui/src/lib/composerMenu.ts");
  });

  it("keeps path order when the query is a scattered subsequence", () => {
    const ranked = rankFilePaths(files, "compbut");
    expect(ranked).toEqual([]);
    expect(rankFilePaths(files, "composer")).toContain("apps/ui/src/components/Composer.tsx");
  });

  it("returns an empty list when nothing matches", () => {
    expect(rankFilePaths(files, "zzzz")).toEqual([]);
  });

  it("caps the result count", () => {
    const many = Array.from({ length: 120 }, (_, index) => `src/file-${index}.ts`);
    expect(rankFilePaths(many, "file", 50)).toHaveLength(50);
  });

  it("scores exact substrings above scattered characters", () => {
    const exact = fuzzyScore("ux", "docs/ux.md");
    const scattered = fuzzyScore("ux", "src/utils/extra.tsx");
    expect(exact).not.toBeNull();
    expect(scattered).not.toBeNull();
    expect(exact ?? 0).toBeGreaterThan(scattered ?? 0);
  });
});

describe("command ranking", () => {
  const commands = [
    { label: "Stop the turn", keywords: "interrupt cancel" },
    { label: "Open the terminal", keywords: "shell" },
    { label: "New thread", keywords: "start create" },
  ];

  it("lists every command for an empty query", () => {
    expect(rankCommands(commands, "")).toEqual(commands);
  });

  it("matches the label", () => {
    expect(rankCommands(commands, "term")[0]?.label).toBe("Open the terminal");
  });

  it("matches a keyword", () => {
    expect(rankCommands(commands, "cancel")[0]?.label).toBe("Stop the turn");
  });

  it("returns an empty list when nothing matches", () => {
    expect(rankCommands(commands, "zzz")).toEqual([]);
  });
});

describe("insertion", () => {
  it("references a plain path with a leading @", () => {
    expect(fileReference("src/main.ts")).toBe("@src/main.ts");
  });

  it("quotes a path with a space", () => {
    expect(fileReference("src/with space.ts")).toBe('@"src/with space.ts"');
  });

  it("replaces the trigger and leaves a trailing space and caret", () => {
    const applied = applyCompletion("ask @src now", { start: 4, end: 8 }, "@src/main.ts");
    expect(applied.value).toBe("ask @src/main.ts now");
    expect(applied.caret).toBe(17);
  });

  it("does not double an existing space after the trigger", () => {
    const applied = applyCompletion("@src main", { start: 0, end: 4 }, "@src/main");
    expect(applied.value).toBe("@src/main main");
  });

  it("clears a command token without adding a space", () => {
    const applied = applyCompletion("/stop", { start: 0, end: 5 }, "");
    expect(applied.value).toBe("");
    expect(applied.caret).toBe(0);
  });

  it("keeps text that follows the command token", () => {
    const applied = applyCompletion("/term and more", { start: 0, end: 5 }, "");
    expect(applied.value).toBe(" and more");
  });
});

describe("popover placement", () => {
  const viewport = { width: 1_200, height: 800 };
  const parent = { left: 16, top: 600, width: 500, height: 96 };

  it("sits above the caret when the measured height fits", () => {
    const placement = placePopover({
      anchor: { left: 40, bottom: 80 },
      size: { width: 420, height: 320 },
      parent,
      viewport,
    });
    expect(placement).toEqual({ left: 40, bottom: 80, top: null, flipped: false });
  });

  it("clamps a measured width to the right edge", () => {
    const placement = placePopover({
      anchor: { left: 900, bottom: 80 },
      size: { width: 420, height: 320 },
      parent,
      viewport,
    });
    expect(placement.left).toBe(756);
    expect(parent.left + placement.left + 420).toBe(viewport.width - 8);
  });

  it("never leaves the left edge off screen", () => {
    const placement = placePopover({
      anchor: { left: -100, bottom: 80 },
      size: { width: 420, height: 320 },
      parent: { ...parent, left: 0 },
      viewport,
    });
    expect(placement.left).toBe(8);
  });

  it("flips below the caret when there is no room above", () => {
    const placement = placePopover({
      anchor: { left: 20, bottom: 80 },
      size: { width: 300, height: 320 },
      parent: { left: 16, top: 0, width: 500, height: 96 },
      viewport,
    });
    expect(placement.bottom).toBeNull();
    expect(placement.top).toBeGreaterThan(0);
    expect(placement.flipped).toBe(true);
  });
});

describe("menu helpers", () => {
  it("wraps the highlight in both directions", () => {
    expect(stepSelection(0, 3, -1)).toBe(2);
    expect(stepSelection(2, 3, 1)).toBe(0);
    expect(stepSelection(0, 0, 1)).toBe(0);
  });

  it("splits a path into a dim directory and a name", () => {
    expect(splitPath("apps/ui/src/Composer.tsx")).toEqual({
      directory: "apps/ui/src/",
      name: "Composer.tsx",
    });
    expect(splitPath("README.md")).toEqual({ directory: "", name: "README.md" });
  });

  it("identifies a dismissed trigger by kind and start", () => {
    expect(triggerKey({ kind: "file", query: "src", start: 4, end: 7 })).toBe("file:4");
  });
});
