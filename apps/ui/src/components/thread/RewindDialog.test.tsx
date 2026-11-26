// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { RewindMethod, ThreadSummary } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";
import { RewindDialog } from "./RewindDialog";

const mock = vi.hoisted(() => ({
  rewindThread: vi.fn(async () => ({}) as never),
  planRewind: vi.fn(),
  refusal: null as string | null,
  filesAvailable: true,
  plan: {
    method: "handoff" as RewindMethod,
    filesEnabled: true,
    error: null as string | null,
    thrown: undefined as unknown,
    leftAlone: [] as string[],
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  planRewind: mock.planRewind,
  rewindThread: mock.rewindThread,
  rewindRefusal: () => mock.refusal,
  filesRewindAvailable: () => mock.filesAvailable,
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
  queued_count: 0,
  queue_revision: 0,
  steer: false,
  rewind: "handoff",
  file_rewind: true,
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mock.refusal = null;
  mock.filesAvailable = true;
  mock.plan = {
    method: "handoff",
    filesEnabled: true,
    error: null,
    thrown: undefined,
    leftAlone: [],
  };
});

function open(overrides: Partial<ThreadSummary> = {}) {
  mock.planRewind.mockImplementation(async () => {
    if (mock.plan.error) throw new Error(mock.plan.error);
    if (mock.plan.thrown !== undefined) throw mock.plan.thrown;
    return {
      method: mock.plan.method,
      files_enabled: mock.plan.filesEnabled,
      files_available: mock.filesAvailable,
      left_alone: mock.plan.leftAlone,
    };
  });
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
  it("offers both choices and warns before reverting files", async () => {
    open();
    expect(await screen.findByText("Revert and keep changes")).toBeTruthy();
    expect(screen.getByText("Revert files too")).toBeTruthy();
    expect(screen.queryByText(/also undoes edits you made yourself/)).toBeNull();
    fireEvent.click(screen.getByLabelText(/Revert files too/));
    expect(screen.getByText(/also undoes edits you made yourself/)).toBeTruthy();
    expect(screen.getByText(/starts a new session with a handoff/)).toBeTruthy();
  });

  it("names the ignored files a restore leaves alone", async () => {
    mock.plan.leftAlone = [".env", "secret.log"];
    open();
    await screen.findByText("Revert files too");
    expect(screen.queryByText(/Ignored files stay as they are/)).toBeNull();
    fireEvent.click(screen.getByLabelText(/Revert files too/));
    expect(screen.getByText(/Ignored files stay as they are: \.env, secret\.log\./)).toBeTruthy();
  });

  it("sends the file choice to the daemon and closes on success", async () => {
    const onOpenChange = vi.fn();
    mock.planRewind.mockImplementation(async () => ({
      method: "handoff",
      files_enabled: true,
      files_available: true,
      left_alone: [],
    }));
    render(
      <RewindDialog
        thread={thread}
        log={undefined}
        itemId="item_user_1"
        open
        onOpenChange={onOpenChange}
      />,
    );
    await screen.findByText("Revert files too");
    fireEvent.click(screen.getByLabelText(/Revert files too/));
    fireEvent.click(screen.getByRole("button", { name: "Rewind thread" }));
    await vi.waitFor(() =>
      expect(mock.rewindThread).toHaveBeenCalledWith("thread_1", "item_user_1", true),
    );
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it("hides the file choice entirely while the Labs switch is off", async () => {
    mock.plan.filesEnabled = false;
    open();
    expect(await screen.findByText(/starts a new session with a handoff/)).toBeTruthy();
    expect(screen.queryByText("Revert files too")).toBeNull();
    expect(screen.queryByText("Revert and keep changes")).toBeNull();
    expect(screen.queryByText(/no complete file checkpoint/)).toBeNull();
    expect(screen.queryByText(/only its conversation is rewound/)).toBeNull();
  });

  it("names the handoff for a prompt with no recorded provider boundary", async () => {
    mock.plan.method = "handoff";
    open();
    expect(
      await screen.findByText(
        "This message can't be dropped natively, so h4rn starts a new session with a handoff.",
      ),
    ).toBeTruthy();
    expect(screen.queryByText(/drops this message and every later turn/)).toBeNull();
  });

  it("names the native truncation when the prompt has a boundary", async () => {
    mock.plan.method = "native";
    open();
    expect(
      await screen.findByText(/drops this message and every later turn from its own history/),
    ).toBeTruthy();
  });

  it("refuses when the plan cannot be read", async () => {
    mock.plan.error = "The thread is not loaded yet.";
    open();
    expect(await screen.findByText("The thread is not loaded yet.")).toBeTruthy();
    expect(
      (screen.getByRole("button", { name: "Rewind thread" }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("words a plan error with no message of its own in the current language", async () => {
    try {
      mock.plan.thrown = { code: 7 };
      open();
      expect(await screen.findByText("The rewind plan could not be read.")).toBeTruthy();
      await act(() => setLocalePreference("ja"));
      expect(screen.getByText("巻き戻しの計画を読み取れませんでした。")).toBeTruthy();
    } finally {
      await act(() => setLocalePreference("en"));
    }
  });

  it("explains a missing complete checkpoint", async () => {
    mock.filesAvailable = false;
    open();
    await screen.findByText(/starts a new session with a handoff/);
    expect(screen.queryByText("Revert files too")).toBeNull();
    expect(screen.getByText(/no complete file checkpoint/)).toBeTruthy();
  });

  it("only offers the conversation when the thread is in the main folder", async () => {
    mock.filesAvailable = false;
    open({ file_rewind: false });
    await screen.findByText(/starts a new session with a handoff/);
    expect(screen.queryByText("Revert files too")).toBeNull();
    expect(screen.getByText(/only its conversation is rewound/)).toBeTruthy();
  });

  it("explains a refusal and disables the action", async () => {
    mock.refusal = "Wait for the current turn to finish, or stop it first.";
    open({ agent_state: "thinking" });
    expect(await screen.findByText(/Wait for the current turn to finish/)).toBeTruthy();
    expect(
      (screen.getByRole("button", { name: "Rewind thread" }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });
});
