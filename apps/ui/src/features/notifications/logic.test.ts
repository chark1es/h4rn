import { describe, expect, it } from "vitest";
import type { ApprovalKind, Event, ThreadId, ThreadSummary } from "@/gen";

import {
  alreadyVisible,
  approvalBody,
  draftForEvent,
  dueNotifications,
  enqueue,
  type FocusSnapshot,
  type HostState,
  offlineTransitions,
} from "./logic";
import { NOTIFICATION_DEFAULTS, type NotificationSettings } from "./settings";

const THREAD = "thr_1";
const OTHER = "thr_2";

function threads(): Record<ThreadId, ThreadSummary> {
  const base = {
    parent_thread_id: null,
    host: "host_atlas",
    harness: "claude_code",
    workspace_path: "/work/h4rn",
    model: "claude-sonnet",
    tab: "working",
    pinned: false,
    snoozed_until: null,
    last_activity: "2026-11-21T00:00:00Z",
    agent_state: "thinking",
    ttft_ms: null,
    context_window: null,
    needs_attention: false,
    unread: false,
    limit: null,
    resume_at: null,
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cost_micros: 0,
      duration_ms: 0,
      generation_ms: 0,
      tps: 0,
    },
  };
  return {
    [THREAD]: { ...base, thread_id: THREAD, title: "Fix the flaky auth test" } as ThreadSummary,
    [OTHER]: { ...base, thread_id: OTHER, title: "Parse WSL distro hosts" } as ThreadSummary,
  };
}

function focus(patch: Partial<FocusSnapshot> = {}): FocusSnapshot {
  return {
    windowFocused: false,
    openThreadId: null,
    connection: "online",
    ...patch,
  };
}

function settings(patch: Partial<NotificationSettings> = {}): NotificationSettings {
  return { ...NOTIFICATION_DEFAULTS, ...patch };
}

function approval(
  threadId: ThreadId = THREAD,
  kind: ApprovalKind = "command",
  prompt = "rm -rf node_modules",
): Event {
  return {
    type: "approval_requested",
    thread_id: threadId,
    turn_id: "turn_1",
    approval_id: "ap_1",
    subagent_id: null,
    kind,
    prompt,
    options: [],
  };
}

function completed(status: "completed" | "failed" | "interrupted", threadId = THREAD): Event {
  return {
    type: "turn_completed",
    thread_id: threadId,
    turn_id: "turn_1",
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cost_micros: 0,
      duration_ms: 0,
      generation_ms: 0,
      tps: 0,
    },
    status,
  };
}

describe("approval notifications", () => {
  it("names the thread and what it wants to run when the window is elsewhere", () => {
    const draft = draftForEvent(approval(), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    expect(draft).toEqual({
      key: `thread:${THREAD}`,
      kind: "approval",
      title: "Fix the flaky auth test",
      body: "Wants to run: rm -rf node_modules",
      threadId: THREAD,
    });
  });

  it("stays quiet when the person is already looking at that thread", () => {
    expect(
      draftForEvent(approval(), {
        threads: threads(),
        settings: settings(),
        focus: focus({ windowFocused: true, openThreadId: THREAD }),
      }),
    ).toBeNull();
  });

  it("still notifies while the window is focused on another thread", () => {
    expect(
      draftForEvent(approval(), {
        threads: threads(),
        settings: settings(),
        focus: focus({ windowFocused: true, openThreadId: OTHER }),
      }),
    ).not.toBeNull();
  });

  it("respects the Approvals switch", () => {
    expect(
      draftForEvent(approval(), {
        threads: threads(),
        settings: settings({ approvals: false }),
        focus: focus(),
      }),
    ).toBeNull();
  });

  it("puts the question itself in the body", () => {
    expect(approvalBody("question", "Which branch should I push?")).toBe(
      "Which branch should I push?",
    );
  });
});

describe("finished-turn notifications", () => {
  it("reports a finished turn", () => {
    const draft = draftForEvent(completed("completed"), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    expect(draft?.title).toBe("Fix the flaky auth test");
    expect(draft?.body).toBe("Finished: the turn is done.");
  });

  it("names the error when the turn failed", () => {
    const draft = draftForEvent(completed("failed"), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
      lastError: "the provider rejected the model",
    });
    expect(draft?.body).toBe("Failed: the provider rejected the model");
  });

  it("falls back to a plain failure sentence", () => {
    const draft = draftForEvent(completed("failed"), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    expect(draft?.body).toBe("Failed: the turn ended with an error.");
  });

  it("does not announce a turn the person stopped", () => {
    expect(
      draftForEvent(completed("interrupted"), {
        threads: threads(),
        settings: settings(),
        focus: focus(),
      }),
    ).toBeNull();
  });

  it("stays quiet for the open thread", () => {
    expect(
      draftForEvent(completed("completed"), {
        threads: threads(),
        settings: settings(),
        focus: focus({ windowFocused: true, openThreadId: THREAD }),
      }),
    ).toBeNull();
  });

  it("respects the Finished turns switch", () => {
    expect(
      draftForEvent(completed("completed"), {
        threads: threads(),
        settings: settings({ finishedTurns: false }),
        focus: focus(),
      }),
    ).toBeNull();
  });
});

describe("coalescing", () => {
  it("keeps one pending message per thread and lets the newest win", () => {
    const first = draftForEvent(approval(THREAD, "command", "npm test"), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    const second = draftForEvent(approval(THREAD, "command", "npm run build"), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    expect(first && second).toBeTruthy();
    if (!first || !second) return;

    let queue = enqueue({}, first, 1_000);
    queue = enqueue(queue, second, 1_400);

    const early = dueNotifications(queue, 1_899);
    expect(early.ready).toHaveLength(0);

    const due = dueNotifications(queue, 1_900);
    expect(due.ready).toHaveLength(1);
    expect(due.ready[0]?.body).toBe("Wants to run: npm run build");
    expect(due.queue).toEqual({});
  });

  it("keeps separate threads separate", () => {
    const one = draftForEvent(approval(THREAD), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    const two = draftForEvent(approval(OTHER), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    if (!one || !two) throw new Error("expected drafts");

    const queue = enqueue(enqueue({}, one, 1_000), two, 1_000);
    expect(dueNotifications(queue, 1_900).ready).toHaveLength(2);
  });
});

describe("host offline", () => {
  const previous: Record<string, HostState> = {
    host_atlas: { name: "atlas", online: true },
    host_buildbox: { name: "buildbox", online: false },
  };

  it("reports only the host that went from online to offline", () => {
    const drafts = offlineTransitions(
      previous,
      {
        host_atlas: { name: "atlas", online: false },
        host_buildbox: { name: "buildbox", online: false },
      },
      focus(),
    );
    expect(drafts).toHaveLength(1);
    expect(drafts[0]).toEqual({
      key: "host:host_atlas",
      kind: "host_offline",
      title: "atlas",
      body: "Host offline: h4rn can no longer reach it.",
      threadId: null,
    });
  });

  it("treats the first poll as a baseline", () => {
    expect(
      offlineTransitions(null, { host_atlas: { name: "atlas", online: false } }, focus()),
    ).toEqual([]);
  });

  it("says nothing while the client itself is disconnected", () => {
    expect(
      offlineTransitions(
        previous,
        { host_atlas: { name: "atlas", online: false } },
        focus({ connection: "degraded" }),
      ),
    ).toEqual([]);
  });

  it("stays quiet when the app is focused", () => {
    expect(
      offlineTransitions(
        previous,
        { host_atlas: { name: "atlas", online: false } },
        focus({ windowFocused: true }),
      ),
    ).toEqual([]);
  });

  it("is already visible when the hosts page is open", () => {
    expect(
      alreadyVisible(
        {
          key: "host:host_atlas",
          kind: "host_offline",
          title: "atlas",
          body: "",
          threadId: null,
        },
        focus({ windowFocused: true }),
      ),
    ).toBe(true);
  });
});
