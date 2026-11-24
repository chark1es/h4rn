// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ThreadSummary } from "@/gen";
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
});
