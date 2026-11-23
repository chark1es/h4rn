// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ForgePrSummary, ForgeRepoRef, ThreadPullRequest } from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";

const mock = vi.hoisted(() => {
  const listeners = new Set<(envelope: EventEnvelope) => void>();
  const state = {
    pulls: [] as ThreadPullRequest[],
    calls: 0,
  };
  return {
    state,
    onAppEvent: (listener: (envelope: EventEnvelope) => void) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getClient: () => ({
      request: async (request: { type: string; thread_id?: string }) => {
        if (request.type !== "list_thread_pull_requests") {
          throw new Error(`unexpected request ${request.type}`);
        }
        state.calls += 1;
        return {
          type: "thread_pull_requests",
          thread_id: request.thread_id ?? "",
          pulls: state.pulls,
        };
      },
    }),
    pushToast: () => undefined,
    emit: (envelope: EventEnvelope) => {
      for (const listener of listeners) listener(envelope);
    },
    listenerCount: () => listeners.size,
  };
});

vi.mock("@/lib/store/appStore", () => ({
  getClient: mock.getClient,
  onAppEvent: mock.onAppEvent,
  pushToast: mock.pushToast,
}));

import { ThreadPullRequestBadge } from "./ThreadPullRequestBadge";

const repo: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "h4rn",
  repo: "api",
};

function summary(state: ForgePrSummary["state"]): ForgePrSummary {
  return {
    repo,
    number: 7,
    title: "Add PR watch",
    state,
    author: "alice",
    head_branch: "h4rn/watch",
    base_branch: "main",
    head_sha: "abc",
    updated_at: "2026-01-01T00:00:00Z",
    web_url: "https://github.com/h4rn/api/pull/7",
    checks: "pending",
    review: "pending",
    draft: false,
  };
}

function pull(state: ForgePrSummary["state"], watching: boolean): ThreadPullRequest {
  return {
    repo,
    number: 7,
    account_id: "a1",
    summary: summary(state),
    error: null,
    watching,
    watch_notice: null,
    linked_at: "2026-01-01T00:00:00Z",
  };
}

function userMessage(threadId: string): EventEnvelope {
  return {
    seq: 3,
    event: {
      type: "item_completed",
      thread_id: threadId,
      turn_id: "turn_1",
      item: {
        kind: "message",
        item_id: "item_1",
        role: "user",
        text: "PR #7 “Add PR watch” was merged.",
        attachments: [],
      },
    },
  };
}

function openBadge(): void {
  const trigger = screen.getByRole("button", { name: "Linked pull requests" });
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.pointerUp(trigger, { button: 0, pointerType: "mouse" });
  fireEvent.click(trigger);
}

beforeEach(() => {
  mock.state.calls = 0;
  mock.state.pulls = [pull("open", true)];
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("the linked pull request badge follows the daemon", () => {
  it("re-reads after a wake message ends a watch", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    const badge = await screen.findByRole("button", { name: "Linked pull requests" });
    expect(badge.textContent).toContain("#7 Open");
    expect(mock.state.calls).toBe(1);

    mock.state.pulls = [pull("merged", false)];
    act(() => mock.emit(userMessage("th_1")));

    await waitFor(() => expect(badge.textContent).toContain("#7 Merged"));
    expect(mock.state.calls).toBe(2);

    // The panel now shows the finished watch honestly.
    openBadge();
    expect(screen.queryByRole("menuitem", { name: /Stop watching/ })).toBeNull();
    expect(screen.getByText("Merged")).toBeTruthy();
  });

  it("ignores a message in another thread", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    await screen.findByRole("button", { name: "Linked pull requests" });

    act(() => mock.emit(userMessage("th_other")));
    expect(mock.state.calls).toBe(1);
  });

  it("does not re-read for a wake that a reconnect replayed", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    await screen.findByRole("button", { name: "Linked pull requests" });
    expect(mock.state.calls).toBe(1);

    // Replayed history is not news: a reconnect must not cost one forge read
    // per replayed message.
    act(() => mock.emit({ ...userMessage("th_1"), replayed: true }));
    expect(mock.state.calls).toBe(1);
  });

  it("re-reads when the panel opens", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    const badge = await screen.findByRole("button", { name: "Linked pull requests" });
    expect(mock.state.calls).toBe(1);

    mock.state.pulls = [pull("merged", false)];
    openBadge();

    await waitFor(() => expect(mock.state.calls).toBe(2));
    await waitFor(() => expect(badge.textContent).toContain("#7 Merged"));
  });

  it("stops listening when it unmounts", async () => {
    const { unmount } = render(<ThreadPullRequestBadge threadId="th_1" />);
    await screen.findByRole("button", { name: "Linked pull requests" });
    unmount();
    expect(mock.listenerCount()).toBe(0);
  });
});
