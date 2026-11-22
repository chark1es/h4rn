// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ThreadSummary } from "@/gen";

const mock = vi.hoisted(() => {
  const state = {
    hosts: {} as Record<string, unknown>,
  };
  const subagents = {
    delegateOpen: true,
    spawning: false,
  };
  return { state, subagents, closeDelegate: vi.fn(), spawnSubagent: vi.fn() };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
}));

vi.mock("./subagentStore", () => ({
  useSubagentStore: (selector: (state: unknown) => unknown) => selector(mock.subagents),
  closeDelegate: mock.closeDelegate,
  spawnSubagent: mock.spawnSubagent,
}));

import { DelegateDialog } from "./DelegateDialog";

function thread(overrides: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: "th_parent",
    parent_thread_id: null,
    host: "host_a",
    harness: "claude_code",
    agent_id: null,
    title: "Parent thread",
    workspace_path: "/home/you/api",
    branch: null,
    model: "claude-sonnet-5",
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T00:00:00.000Z",
    agent_state: "idle",
    usage: {},
    ttft_ms: null,
    context_window: null,
    context_fill_percent: null,
    needs_attention: false,
    unread: false,
    ...overrides,
  } as ThreadSummary;
}

describe("DelegateDialog draft", () => {
  beforeEach(() => {
    cleanup();
    mock.state.hosts = {
      host_a: { host_id: "host_a", name: "atlas", online: true, harnesses: ["claude_code"] },
    };
    mock.subagents.delegateOpen = true;
    mock.subagents.spawning = false;
  });

  afterEach(() => {
    cleanup();
  });

  it("keeps the task, summary and budget when the parent summary refreshes", () => {
    const view = render(<DelegateDialog thread={thread()} />);

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "Reproduce the race" } });
    fireEvent.change(screen.getByLabelText("Max turns"), { target: { value: "12" } });
    fireEvent.change(screen.getByLabelText("Context summary"), {
      target: { value: "the auth test flakes" },
    });

    // A parent summary refresh: same identity, new activity, usage and title.
    view.rerender(
      <DelegateDialog
        thread={thread({
          title: "Parent thread (renamed)",
          last_activity: "2026-01-01T00:05:00.000Z",
          agent_state: "thinking",
          usage: { input_tokens: 120 },
        })}
      />,
    );

    expect((screen.getByLabelText("Task") as HTMLTextAreaElement).value).toBe("Reproduce the race");
    expect((screen.getByLabelText("Max turns") as HTMLInputElement).value).toBe("12");
    expect((screen.getByLabelText("Context summary") as HTMLInputElement).value).toBe(
      "the auth test flakes",
    );
  });
});
