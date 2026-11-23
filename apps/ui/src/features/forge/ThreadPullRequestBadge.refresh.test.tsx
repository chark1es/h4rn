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
  const trigger = screen.getByRole("button", { name: "Linked items" });
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
    const badge = await screen.findByRole("button", { name: "Linked items" });
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
    await screen.findByRole("button", { name: "Linked items" });

    act(() => mock.emit(userMessage("th_other")));
    expect(mock.state.calls).toBe(1);
  });

  it("catches up after a reconnect, once per replayed burst", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    const badge = await screen.findByRole("button", { name: "Linked items" });
    expect(mock.state.calls).toBe(1);

    // The PR merged while the client was offline. The replay arrives as one
    // burst; the badge must read once, not once per replayed message.
    // (`burstReload.test.ts` pins the coalescer itself, which React's own
    // batching would otherwise hide here.)
    mock.state.pulls = [pull("merged", false)];
    await act(async () => {
      mock.emit({ ...userMessage("th_1"), seq: 3, replayed: true });
      mock.emit({ ...userMessage("th_1"), seq: 4, replayed: true });
    });

    await waitFor(() => expect(mock.state.calls).toBe(2));
    await waitFor(() => expect(badge.textContent).toContain("#7 Merged"));
    expect(mock.state.calls).toBe(2);
  });

  it("does not read for another thread's replayed history", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    await screen.findByRole("button", { name: "Linked items" });
    expect(mock.state.calls).toBe(1);

    // A reconnect replays every thread's history. Only this thread's wakes
    // say anything about its linked PRs.
    await act(async () => {
      mock.emit({ ...userMessage("th_other"), seq: 3, replayed: true });
      mock.emit({ ...userMessage("th_other"), seq: 4, replayed: true });
    });
    await Promise.resolve();
    expect(mock.state.calls).toBe(1);
  });

  it("does not read for replayed history that is not a user message", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    await screen.findByRole("button", { name: "Linked items" });
    expect(mock.state.calls).toBe(1);

    const assistant: EventEnvelope = {
      seq: 3,
      replayed: true,
      event: {
        type: "item_completed",
        thread_id: "th_1",
        turn_id: "turn_1",
        item: {
          kind: "message",
          item_id: "item_1",
          role: "assistant",
          text: "PR #7 was merged.",
          attachments: [],
        },
      },
    };
    await act(async () => {
      mock.emit(assistant);
    });
    await Promise.resolve();
    expect(mock.state.calls).toBe(1);
  });

  it("re-reads when the panel opens", async () => {
    render(<ThreadPullRequestBadge threadId="th_1" />);
    const badge = await screen.findByRole("button", { name: "Linked items" });
    expect(mock.state.calls).toBe(1);

    mock.state.pulls = [pull("merged", false)];
    openBadge();

    await waitFor(() => expect(mock.state.calls).toBe(2));
    await waitFor(() => expect(badge.textContent).toContain("#7 Merged"));
  });

  it("stops listening when it unmounts", async () => {
    const { unmount } = render(<ThreadPullRequestBadge threadId="th_1" />);
    await screen.findByRole("button", { name: "Linked items" });
    unmount();
    expect(mock.listenerCount()).toBe(0);
  });
});
