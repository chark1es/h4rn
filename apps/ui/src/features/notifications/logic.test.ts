import { describe, expect, it } from "vitest";
import type {
  ApprovalKind,
  Event,
  HostSummary,
  MachineSummary,
  ThreadId,
  ThreadSummary,
  TurnStatus,
} from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";

import {
  alreadyVisible,
  approvalBody,
  COALESCE_MAX_MS,
  draftForEnvelope,
  draftForEvent,
  dueNotifications,
  enqueue,
  type FocusSnapshot,
  finishedBody,
  finishedDraft,
  type HostState,
  hostPresence,
  type LastTurn,
  type NotificationQueue,
  offlineTransitions,
  type PresenceSource,
} from "./logic";
import { NOTIFICATION_DEFAULTS, type NotificationSettings } from "./settings";

const THREAD = "thr_1";
const OTHER = "thr_2";

function summary(threadId: ThreadId = THREAD, patch: Partial<ThreadSummary> = {}): ThreadSummary {
  const base = {
    parent_thread_id: null,
    host: "host_atlas",
    harness: "claude_code",
    title: threadId === THREAD ? "Fix the flaky auth test" : "Parse WSL distro hosts",
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
  return { ...base, thread_id: threadId, ...patch } as ThreadSummary;
}

function threads(): Record<ThreadId, ThreadSummary> {
  return { [THREAD]: summary(THREAD), [OTHER]: summary(OTHER) };
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

function lastTurn(status: TurnStatus, error: string | null = null): LastTurn {
  return { status, error };
}

function wasIn(
  patch: Partial<Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state">> = {},
) {
  return {
    tab: "working" as const,
    needs_attention: false,
    agent_state: "thinking" as const,
    ...patch,
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

  it("ignores an approval that arrived in the subscribe replay", () => {
    const envelope: EventEnvelope = { seq: 4, event: approval(), replayed: true };
    expect(
      draftForEnvelope(envelope, {
        threads: threads(),
        settings: settings(),
        focus: focus(),
      }),
    ).toBeNull();
  });

  it("notifies for the same approval when it is live", () => {
    const envelope: EventEnvelope = { seq: 4, event: approval() };
    expect(
      draftForEnvelope(envelope, {
        threads: threads(),
        settings: settings(),
        focus: focus(),
      }),
    ).not.toBeNull();
  });
});

describe("finished-turn notifications", () => {
  it("announces a thread that lands in Done", () => {
    const draft = finishedDraft(wasIn(), summary(THREAD, { tab: "done", agent_state: "done" }), {
      settings: settings(),
      focus: focus(),
      lastTurn: lastTurn("completed"),
    });
    expect(draft).toEqual({
      key: `thread:${THREAD}`,
      kind: "finished",
      title: "Fix the flaky auth test",
      body: "Finished: the turn is done.",
      threadId: THREAD,
    });
  });

  it("stays quiet when the next queued turn starts in the same update", () => {
    expect(
      finishedDraft(wasIn(), summary(THREAD, { tab: "working" }), {
        settings: settings(),
        focus: focus(),
        lastTurn: lastTurn("completed"),
      }),
    ).toBeNull();
  });

  it("announces an interrupted turn that lands in Done", () => {
    const draft = finishedDraft(wasIn(), summary(THREAD, { tab: "done" }), {
      settings: settings(),
      focus: focus(),
      lastTurn: lastTurn("interrupted"),
    });
    expect(draft?.body).toBe("Finished: the turn is done.");
  });

  it("names the error when the turn failed", () => {
    const draft = finishedDraft(wasIn(), summary(THREAD, { tab: "done", agent_state: "error" }), {
      settings: settings(),
      focus: focus(),
      lastTurn: lastTurn("failed", "the provider rejected the model"),
    });
    expect(draft?.body).toBe("Failed: the provider rejected the model");
  });

  it("falls back to a plain failure sentence", () => {
    const draft = finishedDraft(wasIn(), summary(THREAD, { tab: "done", agent_state: "error" }), {
      settings: settings(),
      focus: focus(),
      lastTurn: lastTurn("failed"),
    });
    expect(draft?.body).toBe("Failed: the turn ended with an error.");
  });

  it("announces a failure as soon as the thread raises its hand", () => {
    const draft = finishedDraft(
      wasIn(),
      summary(THREAD, {
        tab: "working",
        needs_attention: true,
        agent_state: "error",
      }),
      { settings: settings(), focus: focus(), lastTurn: lastTurn("failed", "the host went away") },
    );
    expect(draft?.body).toBe("Failed: the host went away");
  });

  it("does not repeat a failure when the same turn reaches Done", () => {
    expect(
      finishedDraft(
        wasIn({ needs_attention: true, agent_state: "error" }),
        summary(THREAD, { tab: "done", needs_attention: true, agent_state: "error" }),
        {
          settings: settings(),
          focus: focus(),
          lastTurn: lastTurn("failed", "the host went away"),
          announcedFailure: true,
        },
      ),
    ).toBeNull();
  });

  it("keeps a mid-turn error that is not a failed turn quiet", () => {
    expect(
      finishedDraft(
        wasIn(),
        summary(THREAD, {
          tab: "working",
          needs_attention: true,
          agent_state: "error",
        }),
        { settings: settings(), focus: focus(), lastTurn: lastTurn("completed") },
      ),
    ).toBeNull();
  });

  it("calls a token limit a stop, never a finish", () => {
    expect(finishedBody(lastTurn("token_limit"))).toBe("Stopped: the agent hit its token limit.");
    expect(finishedBody(lastTurn("turn_limit"))).toBe("Stopped: the agent hit its turn limit.");
  });

  it("treats the first summary it sees as a baseline", () => {
    expect(
      finishedDraft(undefined, summary(THREAD, { tab: "done" }), {
        settings: settings(),
        focus: focus(),
        lastTurn: lastTurn("completed"),
      }),
    ).toBeNull();
  });

  it("says nothing again once the thread is already Done", () => {
    expect(
      finishedDraft(wasIn({ tab: "done" }), summary(THREAD, { tab: "done" }), {
        settings: settings(),
        focus: focus(),
        lastTurn: lastTurn("completed"),
      }),
    ).toBeNull();
  });

  it("stays quiet for the open thread", () => {
    expect(
      finishedDraft(wasIn(), summary(THREAD, { tab: "done" }), {
        settings: settings(),
        focus: focus({ windowFocused: true, openThreadId: THREAD }),
        lastTurn: lastTurn("completed"),
      }),
    ).toBeNull();
  });

  it("respects the Finished turns switch", () => {
    expect(
      finishedDraft(wasIn(), summary(THREAD, { tab: "done" }), {
        settings: settings({ finishedTurns: false }),
        focus: focus(),
        lastTurn: lastTurn("completed"),
      }),
    ).toBeNull();
  });
});

describe("coalescing", () => {
  function approvalDraft(prompt: string) {
    const draft = draftForEvent(approval(THREAD, "command", prompt), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    if (!draft) throw new Error("expected a draft");
    return draft;
  }

  it("announces the last of ten approvals when the burst finally goes quiet", () => {
    let queue: NotificationQueue = {};
    let last = 0;
    for (let index = 0; index < 10; index += 1) {
      last = index * 110;
      queue = enqueue(queue, approvalDraft(`command ${index}`), last);
    }

    expect(dueNotifications(queue, last + 999).ready).toHaveLength(0);
    const due = dueNotifications(queue, last + 1_000);
    expect(due.ready).toHaveLength(1);
    expect(due.ready[0]?.body).toBe("Wants to run: command 9");
    expect(due.queue).toEqual({});
  });

  it("does not let a non-stop stream postpone the message forever", () => {
    let queue: NotificationQueue = {};
    let dueAt = 0;
    for (let now = 0; now <= 10_000; now += 500) {
      queue = enqueue(queue, approvalDraft(`command ${now}`), now);
      dueAt = queue[`thread:${THREAD}`]?.dueAt ?? 0;
    }
    expect(dueAt).toBeLessThanOrEqual(COALESCE_MAX_MS);
    const due = dueNotifications(queue, COALESCE_MAX_MS);
    expect(due.ready).toHaveLength(1);
    expect(due.ready[0]?.body).toBe("Wants to run: command 10000");
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
    expect(dueNotifications(queue, 2_000).ready).toHaveLength(2);
  });
});

describe("host offline", () => {
  const previous: Record<string, HostState> = {
    host_atlas: { name: "atlas", online: true },
    host_buildbox: { name: "buildbox", online: false },
  };

  function machine(
    id: string,
    name: string,
    online: boolean,
    hosts = [{ id, name, kind: "host" as const, distro: null }],
  ): MachineSummary {
    return {
      id,
      name,
      online,
      last_seen: online ? null : "2026-11-21T00:00:00Z",
      daemon_device_id: `daemon-${id}`,
      hosts,
      addresses: [],
    };
  }

  function host(hostId: string, online: boolean): HostSummary {
    return {
      host_id: hostId,
      name: hostId,
      platform: "linux",
      online,
      last_seen: "2026-11-21T00:00:00Z",
      harnesses: [],
      latency_ms: null,
      path: null,
    };
  }

  function source(patch: Partial<PresenceSource> = {}): PresenceSource {
    return {
      connection: "online",
      signedIn: true,
      machines: [machine("m_atlas", "atlas", false), machine("m_buildbox", "buildbox", true)],
      activeMachineId: "m_atlas",
      hosts: {},
      localHostId: null,
      ...patch,
    };
  }

  it("trusts the live transport over the relay for the active machine", () => {
    // The relay reports the active machine offline, but this client is
    // talking to it right now.
    const next = hostPresence(source());
    expect(next?.m_atlas).toEqual({ name: "atlas", online: true });
  });

  it("notifies only for another paired machine that actually went offline", () => {
    const before = hostPresence(source());
    const after = hostPresence(
      source({
        machines: [machine("m_atlas", "atlas", false), machine("m_buildbox", "buildbox", false)],
      }),
    );
    if (!before || !after) throw new Error("expected presence");
    const drafts = offlineTransitions(before, after, focus());
    expect(drafts).toHaveLength(1);
    expect(drafts[0]).toEqual({
      key: "host:m_buildbox",
      kind: "host_offline",
      title: "buildbox",
      body: "Host offline: h4rn can no longer reach it.",
      threadId: null,
    });
  });

  it("uses the daemon's hosts for a direct session", () => {
    const next = hostPresence(
      source({
        signedIn: false,
        machines: [],
        activeMachineId: null,
        hosts: {
          host_local: host("host_local", false),
          host_remote: host("host_remote", false),
        },
        localHostId: "host_local",
      }),
    );
    expect(next).toEqual({
      host_local: { name: "host_local", online: true },
      host_remote: { name: "host_remote", online: false },
    });
  });

  it("knows nothing while this client is disconnected", () => {
    expect(hostPresence(source({ connection: "degraded" }))).toBeNull();
    expect(hostPresence(source({ connection: "offline" }))).toBeNull();
  });

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
      offlineTransitions(previous, { host_atlas: { name: "atlas", online: false } }, focus()),
    ).toHaveLength(1);
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
