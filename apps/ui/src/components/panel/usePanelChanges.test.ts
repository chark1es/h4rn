// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ThreadSummary } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";
import { usePanelChanges } from "./usePanelChanges";

const mock = vi.hoisted(() => ({
  state: {
    logs: {
      th_1: {
        turns: [
          { id: "turn_a", divider: false, items: [], status: "completed" },
          {
            id: "switch:acp:codex:2026-01-01T00:00:00Z",
            divider: true,
            items: [],
            status: "completed",
          },
          { id: "turn_b", divider: false, items: [], status: "completed" },
        ],
      },
    },
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getClient: () => ({ request: vi.fn() }),
  pushToast: vi.fn(),
}));

vi.mock("@/features/changes/store", () => ({
  againstFor: vi.fn(),
  CHANGES_SELECT_EVENT: "h4rn:changes-select",
  loadFileDiff: vi.fn(),
  loadTurnChanges: vi.fn(async () => ({ turn_id: "turn_b", files: [] })),
  revertFiles: vi.fn(),
  revertFile: vi.fn(),
}));

describe("usePanelChanges", () => {
  it("skips the switch divider and keeps the turn numbering", () => {
    const thread = { thread_id: "th_1" } as ThreadSummary;
    const { result } = renderHook(() => usePanelChanges(thread));

    expect(result.current.turns).toEqual([
      { id: "turn_a", label: "Turn 1" },
      { id: "turn_b", label: "Turn 2" },
    ]);
    // A switch is a divider, not a turn: the panel opens on the last real one.
    expect(result.current.turnId).toBe("turn_b");
  });

  it("still opens on the last real turn when the switch is the newest row", () => {
    const log = mock.state.logs.th_1;
    const original = log.turns;
    log.turns = [
      ...original,
      {
        id: "switch:codex:claude_code:2026-01-02T00:00:00Z",
        divider: true,
        items: [],
        status: "completed",
      },
    ];
    try {
      const thread = { thread_id: "th_1" } as ThreadSummary;
      const { result, unmount } = renderHook(() => usePanelChanges(thread));
      expect(result.current.turnId).toBe("turn_b");
      unmount();
    } finally {
      log.turns = original;
    }
  });

  it("rewords the turn labels when the language changes, without a new log", async () => {
    const thread = { thread_id: "th_1" } as ThreadSummary;
    const { result, unmount } = renderHook(() => usePanelChanges(thread));
    try {
      expect(result.current.turns.map((turn) => turn.label)).toEqual(["Turn 1", "Turn 2"]);
      await act(() => setLocalePreference("ja"));
      expect(result.current.turns.map((turn) => turn.label)).toEqual(["ターン 1", "ターン 2"]);
    } finally {
      unmount();
      await act(() => setLocalePreference("en"));
    }
  });
});
