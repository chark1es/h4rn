// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadSummary } from "@/gen";
import { RewindDialog } from "./RewindDialog";

const mock = vi.hoisted(() => ({
  rewindThread: vi.fn(async () => ({}) as never),
  refusal: null as string | null,
}));

vi.mock("@/lib/store/appStore", () => ({
  rewindThread: mock.rewindThread,
  rewindRefusal: () => mock.refusal,
}));

const thread: ThreadSummary = {
  thread_id: "thread_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Fix the flaky test",
  workspace_path: "/work/thread",
  branch: "h4rn/fix",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "done",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
  agent_state: "done",
  usage: {
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cost_micros: 0,
    duration_ms: 0,
    generation_ms: 0,
    tps: 0,
  },
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
  needs_attention: false,
  pending_guest_requests: 0,
  unread: false,
  resume_at: null,
  limit: null,
  rewind: "handoff",
  file_rewind: true,
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mock.refusal = null;
});

function open(overrides: Partial<ThreadSummary> = {}) {
  return render(
    <RewindDialog
      thread={{ ...thread, ...overrides }}
      log={undefined}
      itemId="item_user_1"
      open
      onOpenChange={() => {}}
    />,
  );
}

describe("RewindDialog", () => {
  it("offers both choices and warns before reverting files", () => {
    open();
    expect(screen.getByText("Revert and keep changes")).toBeTruthy();
    expect(screen.getByText("Revert files too")).toBeTruthy();
    expect(screen.queryByText(/also undoes edits you made yourself/)).toBeNull();
    fireEvent.click(screen.getByLabelText(/Revert files too/));
    expect(screen.getByText(/also undoes edits you made yourself/)).toBeTruthy();
    expect(screen.getByText(/starts a new session with a handoff/)).toBeTruthy();
  });

  it("sends the file choice to the daemon and closes on success", async () => {
    const onOpenChange = vi.fn();
    render(
      <RewindDialog
        thread={thread}
        log={undefined}
        itemId="item_user_1"
        open
        onOpenChange={onOpenChange}
      />,
    );
    fireEvent.click(screen.getByLabelText(/Revert files too/));
    fireEvent.click(screen.getByRole("button", { name: "Rewind thread" }));
    await vi.waitFor(() =>
      expect(mock.rewindThread).toHaveBeenCalledWith("thread_1", "item_user_1", true),
    );
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it("only offers the conversation when the thread is in the main folder", () => {
    open({ file_rewind: false });
    expect(screen.queryByText("Revert files too")).toBeNull();
    expect(screen.getByText(/only its conversation is rewound/)).toBeTruthy();
  });

  it("explains a refusal and disables the action", () => {
    mock.refusal = "Wait for the current turn to finish, or stop it first.";
    open({ agent_state: "thinking" });
    expect(screen.getByText(/Wait for the current turn to finish/)).toBeTruthy();
    expect(
      (screen.getByRole("button", { name: "Rewind thread" }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });
});
