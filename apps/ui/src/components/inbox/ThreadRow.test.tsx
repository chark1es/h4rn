// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { HostSummary, ThreadSummary } from "@/gen";

import { ThreadRow } from "./ThreadRow";

function summary(branch: string | null): ThreadSummary {
  return {
    thread_id: "thread_branch",
    parent_thread_id: null,
    host: "host_a",
    harness: "claude_code",
    title: "Fix the login loop",
    workspace_path: "/data/worktrees/h4rn/fix-the-login-loop",
    ...(branch ? { branch } : {}),
    model: "claude-sonnet-4-5",
    runtime_mode: "approval_required",
    reasoning_efforts: [],
    models: [],
    fast: false,
    tab: "working",
    placed_by_user: false,
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-01-01T00:00:00.000Z",
    agent_state: "idle",
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
  } as ThreadSummary;
}

function renderRow(branch: string | null) {
  return render(
    <ThreadRow
      thread={summary(branch)}
      host={undefined}
      index={0}
      selected={false}
      now={Date.parse("2026-01-01T00:00:00.000Z")}
      onOpen={vi.fn()}
      onAcknowledge={vi.fn()}
      onSettle={vi.fn()}
      onReopen={vi.fn()}
      onPin={vi.fn()}
      onSnooze={vi.fn()}
    />,
  );
}

describe("ThreadRow branch", () => {
  afterEach(cleanup);

  it("shows the worktree branch in muted text", () => {
    renderRow("h4rn/fix-the-login-loop");
    const branch = screen.getByText("h4rn/fix-the-login-loop");
    expect(branch.closest(".q-row-branch")).not.toBeNull();
  });

  it("shows no branch for a thread in a plain folder", () => {
    renderRow(null);
    expect(document.querySelector(".q-row-branch")).toBeNull();
  });
});

const host: HostSummary = {
  host_id: "host_atlas",
  name: "atlas",
  platform: "linux",
  online: true,
  last_seen: "2026-11-23T10:00:00Z",
  harnesses: [],
  latency_ms: null,
  path: null,
};

const thread: ThreadSummary = {
  thread_id: "th_1",
  parent_thread_id: null,
  host: "host_atlas",
  harness: "claude_code",
  title: "Fix the flaky auth test",
  workspace_path: "/work/h4rn",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "working",
  placed_by_user: false,
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-11-23T10:00:00Z",
  agent_state: "idle",
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
  needs_attention: true,
  pending_guest_requests: 1,
  unread: false,
  resume_at: null,
  limit: null,
};

describe("ThreadRow", () => {
  it("shows Request in the sidebar while a guest message waits", () => {
    render(
      <ThreadRow
        thread={thread}
        host={host}
        index={0}
        selected={false}
        now={Date.now()}
        onOpen={vi.fn()}
        onAcknowledge={vi.fn()}
        onSettle={vi.fn()}
        onReopen={vi.fn()}
        onPin={vi.fn()}
        onSnooze={vi.fn()}
      />,
    );
    expect(screen.getByText("Request")).toBeTruthy();
    expect(screen.getByRole("option", { name: /Fix the flaky auth test/ })).toBeTruthy();
  });

  it("keeps a more urgent approval visible over the Request word", () => {
    render(
      <ThreadRow
        thread={{ ...thread, agent_state: "awaiting_approval" }}
        host={host}
        index={0}
        selected={false}
        now={Date.now()}
        onOpen={vi.fn()}
        onAcknowledge={vi.fn()}
        onSettle={vi.fn()}
        onReopen={vi.fn()}
        onPin={vi.fn()}
        onSnooze={vi.fn()}
      />,
    );
    expect(screen.getByText("Needs approval")).toBeTruthy();
  });
});
