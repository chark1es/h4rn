import { describe, expect, it } from "vitest";

import {
  closeTab,
  createTab,
  dirtyTabs,
  isDirty,
  openTab,
  reorderTabs,
  type TabState,
  updateTab,
} from "./tabs";

const empty = { tabs: [], activePath: null };

function firstTab(state: TabState) {
  const tab = state.tabs[0];
  if (!tab) throw new Error("expected a tab");
  return tab;
}

describe("IDE tabs", () => {
  it("opens a path once and focuses the existing tab", () => {
    const first = openTab(empty, "src/a.ts");
    const second = openTab(first, "src/b.ts");
    const again = openTab(second, "src/a.ts");
    expect(again.tabs.map((tab) => tab.path)).toEqual(["src/a.ts", "src/b.ts"]);
    expect(again.activePath).toBe("src/a.ts");
  });

  it("tracks dirty state against the last acknowledged text", () => {
    const state = openTab(empty, "src/a.ts");
    const loaded = updateTab(state, "src/a.ts", {
      content: "one\n",
      savedContent: "one\n",
      loading: false,
    });
    expect(isDirty(firstTab(loaded))).toBe(false);
    const edited = updateTab(loaded, "src/a.ts", { content: "two\n" });
    expect(isDirty(firstTab(edited))).toBe(true);
    expect(dirtyTabs(edited)).toHaveLength(1);
    const saved = updateTab(edited, "src/a.ts", {
      savedContent: "two\n",
      etag: "next",
    });
    expect(isDirty(firstTab(saved))).toBe(false);
  });

  it("does not mark a loading tab dirty", () => {
    expect(isDirty(createTab("src/a.ts"))).toBe(false);
  });

  it("closes the active tab and focuses its neighbour", () => {
    let state = openTab(openTab(openTab(empty, "a"), "b"), "c");
    state = closeTab(state, "b");
    expect(state.tabs.map((tab) => tab.path)).toEqual(["a", "c"]);
    expect(state.activePath).toBe("c");
    state = closeTab(state, "c");
    expect(state.activePath).toBe("a");
    state = closeTab(state, "a");
    expect(state).toEqual({ tabs: [], activePath: null });
  });

  it("keeps focus when closing a background tab", () => {
    let state = openTab(openTab(empty, "a"), "b");
    state = closeTab(state, "a");
    expect(state.activePath).toBe("b");
  });

  it("reorders by index and clamps out-of-range drops", () => {
    const state = openTab(openTab(openTab(empty, "a"), "b"), "c");
    expect(reorderTabs(state, "c", 0).tabs.map((tab) => tab.path)).toEqual(["c", "a", "b"]);
    expect(reorderTabs(state, "a", 99).tabs.map((tab) => tab.path)).toEqual(["b", "c", "a"]);
    expect(reorderTabs(state, "missing", 0)).toBe(state);
  });
});
