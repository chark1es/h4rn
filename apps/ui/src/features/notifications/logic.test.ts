import { describe, expect, it } from "vitest";
import type {
  ApprovalKind,
  Event,
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
  dropResolvedApproval,
  dropThreadApproval,
  dueNotifications,
  enqueue,
  type FocusSnapshot,
  failedOutcomeDraft,
  finishedBody,
  finishedDraft,
  type HostState,
  hostPresence,
  type LastTurn,
  type NotificationQueue,
  offlineTransitions,
  type PresenceSource,
  stillOffline,
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
      approvalId: "ap_1",
    });
  });

  it("skips a child thread because the parent already mirrors its approval", () => {
    expect(
      draftForEvent(approval(), {
        threads: threads(),
        settings: settings(),
        focus: focus(),
        childThread: true,
      }),
    ).toBeNull();
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
  /** A Done/failure draft with a live completion unless a test says otherwise. */
  function finished(
    next: ThreadSummary,
    options: {
      previous?: Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state">;
      settings?: NotificationSettings;
      focus?: FocusSnapshot;
      lastTurn?: LastTurn;
      announcedFailure?: boolean;
      liveCompletion?: boolean;
      placedByUser?: boolean;
    } = {},
  ) {
    return finishedDraft(options.previous ?? wasIn(), next, {
      settings: options.settings ?? settings(),
      focus: options.focus ?? focus(),
      lastTurn: options.lastTurn ?? lastTurn("completed"),
      liveCompletion: options.liveCompletion ?? true,
      ...(options.announcedFailure === undefined
        ? {}
        : { announcedFailure: options.announcedFailure }),
      ...(options.placedByUser === undefined ? {} : { placedByUser: options.placedByUser }),
    });
  }

  it("announces a thread that lands in Done", () => {
    expect(finished(summary(THREAD, { tab: "done", agent_state: "done" }))).toEqual({
      key: `thread:${THREAD}`,
      kind: "finished",
      title: "Fix the flaky auth test",
      body: "Finished: the turn is done.",
      threadId: THREAD,
    });
  });

  it("stays quiet when the next queued turn starts in the same update", () => {
    expect(finished(summary(THREAD, { tab: "working" }))).toBeNull();
  });

  it("announces an interrupted turn as stopped, not finished", () => {
    const draft = finished(summary(THREAD, { tab: "done" }), {
      lastTurn: lastTurn("interrupted"),
    });
    expect(draft?.body).toBe("Stopped: the turn was interrupted.");
  });

  it("names the error when the turn failed", () => {
    const draft = finished(summary(THREAD, { tab: "done", agent_state: "error" }), {
      lastTurn: lastTurn("failed", "the provider rejected the model"),
    });
    expect(draft?.body).toBe("Failed: the provider rejected the model");
  });

  it("falls back to a plain failure sentence", () => {
    const draft = finished(summary(THREAD, { tab: "done", agent_state: "error" }), {
      lastTurn: lastTurn("failed"),
    });
    expect(draft?.body).toBe("Failed: the turn ended with an error.");
  });

  it("announces a failure as soon as the thread raises its hand", () => {
    const draft = finished(
      summary(THREAD, {
        tab: "working",
        needs_attention: true,
        agent_state: "error",
      }),
      { lastTurn: lastTurn("failed", "the host went away") },
    );
    expect(draft?.body).toBe("Failed: the host went away");
  });

  it("announces a failure while the thread is pinned to Working", () => {
    // A hand-set Working pin marks `placed_by_user`, but it is a per-thread
    // override, not a finished turn: the failure still needs the person.
    const draft = finished(
      summary(THREAD, {
        tab: "working",
        pinned: true,
        placed_by_user: true,
        needs_attention: true,
        agent_state: "error",
      }),
      {
        lastTurn: lastTurn("failed", "the provider rejected the model"),
        placedByUser: true,
      },
    );
    expect(draft?.body).toBe("Failed: the provider rejected the model");
  });

  it("announces a failure after the person moved the thread to Working by hand", () => {
    const draft = finished(
      summary(THREAD, {
        tab: "working",
        placed_by_user: true,
        needs_attention: true,
        agent_state: "error",
      }),
      {
        lastTurn: lastTurn("failed", "the host went away"),
        placedByUser: true,
      },
    );
    expect(draft?.body).toBe("Failed: the host went away");
  });

  it("stays quiet when the person placed the thread in Done", () => {
    expect(
      finished(summary(THREAD, { tab: "done", placed_by_user: true }), {
        placedByUser: true,
      }),
    ).toBeNull();
  });

  it("does not repeat a failure when the same turn reaches Done", () => {
    expect(
      finished(summary(THREAD, { tab: "done", needs_attention: true, agent_state: "error" }), {
        previous: wasIn({ needs_attention: true, agent_state: "error" }),
        lastTurn: lastTurn("failed", "the host went away"),
        announcedFailure: true,
      }),
    ).toBeNull();
  });

  it("stays quiet when another client moved the thread to Done", () => {
    // No live `turn_completed` for this thread's current turn: only the
    // summary changed, and that is someone else's action, not news.
    expect(
      finished(summary(THREAD, { tab: "done", agent_state: "done" }), {
        liveCompletion: false,
      }),
    ).toBeNull();
  });

  it("stays quiet for a replayed completion followed by a live summary tick", () => {
    // The completion arrived only in the replay, so the later live Done tick
    // is history healing itself, not a finished turn.
    expect(
      finished(summary(THREAD, { tab: "done", agent_state: "done" }), {
        lastTurn: lastTurn("completed"),
        liveCompletion: false,
      }),
    ).toBeNull();
  });

  it("stays quiet for a subagent child thread", () => {
    expect(
      finishedDraft(wasIn(), summary(THREAD, { tab: "done", parent_thread_id: OTHER }), {
        settings: settings(),
        focus: focus(),
        lastTurn: lastTurn("completed"),
        liveCompletion: true,
      }),
    ).toBeNull();
  });

  it("keeps a mid-turn error that is not a failed turn quiet", () => {
    expect(
      finished(
        summary(THREAD, {
          tab: "working",
          needs_attention: true,
          agent_state: "error",
        }),
        { lastTurn: lastTurn("completed") },
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
        liveCompletion: true,
      }),
    ).toBeNull();
  });

  it("says nothing again once the thread is already Done", () => {
    expect(
      finished(summary(THREAD, { tab: "done" }), {
        previous: wasIn({ tab: "done" }),
      }),
    ).toBeNull();
  });

  it("stays quiet for the open thread", () => {
    expect(
      finished(summary(THREAD, { tab: "done" }), {
        focus: focus({ windowFocused: true, openThreadId: THREAD }),
      }),
    ).toBeNull();
  });

  it("respects the Finished turns switch", () => {
    expect(
      finished(summary(THREAD, { tab: "done" }), {
        settings: settings({ finishedTurns: false }),
      }),
    ).toBeNull();
  });
});

describe("a failed turn's own outcome", () => {
  function outcome(
    status: TurnStatus,
    options: {
      threads?: Record<ThreadId, ThreadSummary>;
      settings?: NotificationSettings;
      focus?: FocusSnapshot;
      announcedFailure?: boolean;
      childThread?: boolean;
    } = {},
  ) {
    return failedOutcomeDraft(THREAD, {
      threads: options.threads ?? { [THREAD]: summary(THREAD, { tab: "done" }) },
      settings: options.settings ?? settings(),
      focus: options.focus ?? focus(),
      lastTurn: lastTurn(status),
      ...(options.announcedFailure === undefined
        ? {}
        : { announcedFailure: options.announcedFailure }),
      ...(options.childThread === undefined ? {} : { childThread: options.childThread }),
    });
  }

  it("announces a failure wherever the thread sits, including Done", () => {
    // The summary's error transition never comes for a thread already in Done,
    // but the turn still failed while the person was waiting for it.
    const draft = outcome("failed");
    expect(draft).toEqual({
      key: `thread:${THREAD}`,
      kind: "finished",
      title: "Fix the flaky auth test",
      body: "Failed: the turn ended with an error.",
      threadId: THREAD,
    });
  });

  it("says nothing for a successful, interrupted or limited turn", () => {
    expect(outcome("completed")).toBeNull();
    expect(outcome("interrupted")).toBeNull();
    // A limit is a stop the daemon may resume from; its attention state comes
    // from the summary, not from this outcome path.
    expect(outcome("token_limit")).toBeNull();
    expect(outcome("turn_limit")).toBeNull();
  });

  it("does not repeat a failure the summary already announced", () => {
    expect(outcome("failed", { announcedFailure: true })).toBeNull();
  });

  it("stays quiet for a child thread, the switch, or the open thread", () => {
    expect(outcome("failed", { childThread: true })).toBeNull();
    expect(outcome("failed", { settings: settings({ finishedTurns: false }) })).toBeNull();
    expect(
      outcome("failed", { focus: focus({ windowFocused: true, openThreadId: THREAD }) }),
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

describe("resolved approvals", () => {
  function queued(approvalId: string, prompt: string): NotificationQueue {
    const event = approval(THREAD, "command", prompt);
    if (event.type !== "approval_requested") throw new Error("unexpected event");
    const draft = draftForEvent(
      { ...event, approval_id: approvalId },
      {
        threads: threads(),
        settings: settings(),
        focus: focus(),
      },
    );
    if (!draft) throw new Error("expected a draft");
    return enqueue({}, draft, 1_000);
  }

  it("drops a thread's queued approval when that approval is answered", () => {
    const queue = dropResolvedApproval(queued("ap_1", "npm test"), THREAD, "ap_1");
    expect(queue).toEqual({});
  });

  it("keeps a newer approval that re-queued the same thread", () => {
    const queue = dropResolvedApproval(queued("ap_2", "npm run build"), THREAD, "ap_1");
    expect(queue[`thread:${THREAD}`]?.body).toBe("Wants to run: npm run build");
  });

  it("withdraws a queued approval once the thread is named a child", () => {
    const queue = dropThreadApproval(queued("ap_1", "npm test"), THREAD);
    expect(queue).toEqual({});
  });

  it("leaves a queued finished message alone when a child is named", () => {
    const finished = draftForEvent(approval(THREAD), {
      threads: threads(),
      settings: settings(),
      focus: focus(),
    });
    if (!finished) throw new Error("expected an approval draft");
    const queue = enqueue({}, { ...finished, kind: "finished" }, 1_000);
    expect(dropThreadApproval(queue, THREAD)).toBe(queue);
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
    hosts: MachineSummary["hosts"] = [{ id, name, kind: "host", distro: null }],
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

  function source(patch: Partial<PresenceSource> = {}): PresenceSource {
    return {
      connection: "online",
      signedIn: true,
      machines: [machine("m_atlas", "atlas", false), machine("m_buildbox", "buildbox", true)],
      connectedMachineId: "m_atlas",
      localHostName: null,
      ...patch,
    };
  }

  it("trusts the live transport over the relay for the active machine", () => {
    // The relay reports the active machine offline, but this client is
    // talking to it right now.
    const next = hostPresence(source());
    expect(next?.m_atlas).toEqual({ name: "atlas", online: true });
  });

  it("trusts the local host's name when the transport names no machine", () => {
    // A direct connection through a relay has no verified machine id. Connect
    // mints its own machine and host ids when a machine registers, so the
    // relay's id never equals `hello`'s local host id; the name is the only
    // field both lists agree on.
    const relayId = "0b5f2b1e-9f3a-4a44-9a5e-71f0a2f7c9d1";
    const next = hostPresence(
      source({
        connectedMachineId: null,
        localHostName: "atlas",
        machines: [machine(relayId, "atlas", false)],
      }),
    );
    expect(next?.[relayId]).toEqual({ name: "atlas", online: true });
  });

  it("matches the local host name without regard to case", () => {
    const relayId = "0b5f2b1e-9f3a-4a44-9a5e-71f0a2f7c9d1";
    const next = hostPresence(
      source({
        connectedMachineId: null,
        localHostName: "Atlas",
        machines: [machine(relayId, "atlas", false)],
      }),
    );
    expect(next?.[relayId]?.online).toBe(true);
  });

  it("does not merge two machines that share a hostname", () => {
    // With no verified machine id, the name cannot say which of the two the
    // live transport reached. Neither is fed relay presence, so one cannot
    // mask the other's presence and neither can raise an offline alert for the
    // machine the person is actually using.
    const next = hostPresence(
      source({
        connectedMachineId: null,
        localHostName: "atlas",
        machines: [machine("m_one", "atlas", false), machine("m_two", "atlas", false)],
      }),
    );
    expect(next).toBeNull();
  });

  it("keys the connected machine by the id the transport reached", () => {
    // The peer's id (a relay link's machine, or the daemon's own device) needs
    // no manual pick, so the relay can never report that machine offline.
    const next = hostPresence(
      source({
        connectedMachineId: "m_one",
        localHostName: null,
        machines: [machine("m_one", "atlas", false), machine("m_two", "atlas", false)],
      }),
    );
    expect(next?.m_one).toEqual({ name: "atlas", online: true });
    expect(next?.m_two?.online).toBe(false);
  });

  it("keeps every host of the verified machine online, including WSL siblings", () => {
    // The transport reached the machine, not one row of it: a relay that has
    // not caught up must not split the Windows host from its WSL distro.
    const windows = machine("m_atlas", "atlas", false, [
      { id: "h_windows", name: "atlas", kind: "host", distro: null },
      { id: "h_wsl", name: "Ubuntu", kind: "wsl", distro: "Ubuntu" },
    ]);
    const next = hostPresence(
      source({
        connectedMachineId: "m_atlas",
        localHostName: "atlas",
        machines: [windows],
      }),
    );
    expect(next?.h_windows?.online).toBe(true);
    expect(next?.h_wsl?.online).toBe(true);
  });

  it("marks only the named host when the name fallback matched", () => {
    // Without a verified id the name says just which host answered. A sibling
    // WSL distro keeps the relay's answer rather than riding on the match.
    const windows = machine("m_atlas", "atlas", false, [
      { id: "h_windows", name: "atlas", kind: "host", distro: null },
      { id: "h_wsl", name: "Ubuntu", kind: "wsl", distro: "Ubuntu" },
    ]);
    const next = hostPresence(
      source({
        connectedMachineId: null,
        localHostName: "atlas",
        machines: [windows],
      }),
    );
    expect(next?.h_windows?.online).toBe(true);
    expect(next?.h_wsl?.online).toBe(false);
  });

  it("has no presence for a direct-only session", () => {
    // No relay list means no independent reachability signal; the row is
    // hidden rather than fed a guess.
    expect(
      hostPresence(
        source({
          signedIn: false,
          machines: [],
          connectedMachineId: null,
        }),
      ),
    ).toBeNull();
    expect(hostPresence(source({ machines: [], connectedMachineId: null }))).toBeNull();
  });

  it("notifies only for another linked machine that actually went offline", () => {
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

  it("drops a queued host alert if the host came back before delivery", () => {
    const draft = {
      key: "host:host_atlas",
      kind: "host_offline" as const,
      title: "atlas",
      body: "",
      threadId: null,
    };
    const presence = {
      host_atlas: { name: "atlas", online: true },
    };
    expect(stillOffline(draft, presence, "online")).toBe(false);
  });

  it("drops a queued host alert if this client disconnected", () => {
    const draft = {
      key: "host:host_atlas",
      kind: "host_offline" as const,
      title: "atlas",
      body: "",
      threadId: null,
    };
    expect(stillOffline(draft, previous, "degraded")).toBe(false);
    expect(stillOffline(draft, null, "online")).toBe(false);
  });

  it("delivers a queued host alert that is still true", () => {
    const draft = {
      key: "host:host_atlas",
      kind: "host_offline" as const,
      title: "atlas",
      body: "",
      threadId: null,
    };
    expect(stillOffline(draft, { host_atlas: { name: "atlas", online: false } }, "online")).toBe(
      true,
    );
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
