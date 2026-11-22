import { afterEach, describe, expect, it, vi } from "vitest";

import type { Event, MachineSummary, ThreadId, ThreadSummary } from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";

import type { NotificationChannel, NotificationMessage } from "./channel";
import {
  NotificationRuntime,
  type NotificationRuntimeDeps,
  type PresenceClient,
  type RuntimeRemote,
  type RuntimeState,
} from "./runtime";
import { NOTIFICATION_DEFAULTS, type NotificationSettings } from "./settings";

const THREAD = "thr_1";

function connection(
  status: RuntimeState["connection"]["status"] = "online",
): RuntimeState["connection"] {
  return {
    status,
    attempt: 0,
    transport: "direct",
    lastConnectedAt: null,
    lastEventAt: null,
    latencyMs: null,
    error: null,
  };
}

function threadSummary(
  threadId: ThreadId = THREAD,
  patch: Partial<ThreadSummary> = {},
): ThreadSummary {
  return {
    thread_id: threadId,
    parent_thread_id: null,
    host: "host_atlas",
    harness: "claude_code",
    title: "Fix the flaky auth test",
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
    ...patch,
  } as ThreadSummary;
}

function machine(id: string, name: string, online: boolean): MachineSummary {
  return {
    id,
    name,
    online,
    last_seen: online ? null : "2026-11-21T00:00:00Z",
    daemon_device_id: `daemon-${id}`,
    hosts: [{ id, name, kind: "host", distro: null }],
    addresses: [],
  };
}

function approvalRequested(threadId: ThreadId): Event {
  return {
    type: "approval_requested",
    thread_id: threadId,
    turn_id: "turn_1",
    approval_id: "ap_1",
    subagent_id: null,
    kind: "command",
    prompt: "npm test",
    options: [],
  };
}

function turnCompleted(
  threadId: ThreadId,
  status: "completed" | "interrupted" = "completed",
): Event {
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

function threadUpserted(summary: ThreadSummary): Event {
  return { type: "thread_upserted", ...summary } as Event;
}

type Harness = {
  runtime: NotificationRuntime;
  shown: NotificationMessage[];
  emit: (event: Event) => void;
  place: (threadId: ThreadId) => void;
  updateState: (patch: Partial<RuntimeState>) => void;
  updateRemote: (patch: Partial<RuntimeRemote>) => void;
};

function harness(options: {
  threads?: Record<ThreadId, ThreadSummary>;
  settings?: Partial<NotificationSettings>;
  remote?: Partial<RuntimeRemote>;
  client?: PresenceClient | null;
  connected?: boolean;
}): Harness {
  const shown: NotificationMessage[] = [];
  const channel: NotificationChannel = {
    kind: "desktop",
    permission: async () => "granted",
    requestPermission: async () => "granted",
    show: (message) => shown.push(message),
    onClick: () => () => undefined,
  };

  let state: RuntimeState = {
    connection: connection(options.connected === false ? "degraded" : "online"),
    threads: options.threads ?? { [THREAD]: threadSummary() },
    lastSyncAt: 1,
    localHostId: null,
  };
  let remote: RuntimeRemote = {
    user: { id: "user-1", email: null, display_name: "Dev", avatar_url: null },
    linkedMachines: [],
    activeMachineId: null,
    ...options.remote,
  };
  const settings: NotificationSettings = {
    ...NOTIFICATION_DEFAULTS,
    ...options.settings,
  };
  const client: PresenceClient | null = options.client ?? null;

  const eventListeners = new Set<(envelope: EventEnvelope) => void>();
  const stateListeners = new Set<() => void>();
  const remoteListeners = new Set<() => void>();
  const placementListeners = new Set<(threadId: ThreadId) => void>();

  const deps: NotificationRuntimeDeps = {
    channel,
    state: () => state,
    remote: () => remote,
    onEvent: (listener) => {
      eventListeners.add(listener);
      return () => eventListeners.delete(listener);
    },
    onStateChange: (listener) => {
      stateListeners.add(listener);
      return () => stateListeners.delete(listener);
    },
    onRemoteChange: (listener) => {
      remoteListeners.add(listener);
      return () => remoteListeners.delete(listener);
    },
    onManualPlacement: (listener) => {
      placementListeners.add(listener);
      return () => placementListeners.delete(listener);
    },
    openThread: () => undefined,
    refreshHosts: () => undefined,
    connectClient: () => client,
    settings: () => settings,
    focus: () => ({
      windowFocused: false,
      openThreadId: null,
      connection: state.connection.status,
    }),
  };

  let seq = 0;
  const runtime = new NotificationRuntime(deps);
  return {
    runtime,
    shown,
    emit: (event) => {
      seq += 1;
      for (const listener of eventListeners) listener({ seq, event });
    },
    place: (threadId) => {
      for (const listener of placementListeners) listener(threadId);
    },
    updateState: (patch) => {
      state = { ...state, ...patch };
      for (const listener of stateListeners) listener();
    },
    updateRemote: (patch) => {
      remote = { ...remote, ...patch };
      for (const listener of remoteListeners) listener();
    },
  };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("notification runtime wiring", () => {
  it("delivers one message when the coalescing timer fires", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    h.emit(approvalRequested(THREAD));
    await vi.advanceTimersByTimeAsync(999);
    expect(h.shown).toHaveLength(0);

    await vi.advanceTimersByTimeAsync(1);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]).toMatchObject({
      key: `thread:${THREAD}`,
      title: "Fix the flaky auth test",
      body: "Wants to run: npm test",
    });

    // The timer is not left armed for the same queue entry.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(h.shown).toHaveLength(1);
  });

  it("re-seeds the baseline and drops completions when the list syncs", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    // A live completion before the list healed the gap...
    h.emit(turnCompleted(THREAD));
    // ...then the authoritative list arrives, still saying Working.
    h.updateState({ lastSyncAt: 2 });

    // A summary-only Done from another client must not reuse that completion.
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
  });

  it("keeps a seeded Done thread quiet on a summary-only tick", async () => {
    vi.useFakeTimers();
    const h = harness({
      threads: {
        [THREAD]: threadSummary(THREAD, { tab: "done", agent_state: "done" }),
      },
    });
    h.runtime.start();

    // The baseline already said Done; a later summary with no live completion
    // is not the moment the turn finished.
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
  });

  it("discards a live completion when the person places the thread by hand", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    h.emit(turnCompleted(THREAD));
    h.place(THREAD);
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
  });

  it("announces a Done transition that the live completion caused", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    h.emit(turnCompleted(THREAD));
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Finished: the turn is done.");
  });

  it("withdraws an approval queued before its thread is named a child", async () => {
    vi.useFakeTimers();
    const h = harness({ threads: {} });
    h.runtime.start();

    // Wire order: the child's approval arrives before any summary for it.
    h.emit(approvalRequested("thr_child"));
    h.emit(
      threadUpserted(threadSummary("thr_child", { parent_thread_id: THREAD, tab: "working" })),
    );

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);

    // The thread now known as a child stays quiet for later approvals too.
    h.emit(approvalRequested("thr_child"));
    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
  });
});

describe("host offline delivery", () => {
  const online = machine("m_atlas", "atlas", true);
  const offline = machine("m_atlas", "atlas", false);

  function queuedHostAlert(client: PresenceClient | null, connected = true) {
    const h = harness({
      threads: {},
      settings: { hostOffline: true },
      remote: { linkedMachines: [online] },
      client,
      connected,
    });
    h.runtime.start();
    // The poll sees the machine go offline and queues one alert.
    h.updateRemote({ linkedMachines: [offline] });
    return h;
  }

  it("drops an alert when a fresh presence fetch says the host is back", async () => {
    vi.useFakeTimers();
    const machines = vi.fn(async () => [online]);
    const h = queuedHostAlert({ machines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(machines).toHaveBeenCalledTimes(1);
    expect(machines).toHaveBeenCalledWith({ linked: true });
    expect(h.shown).toHaveLength(0);
  });

  it("drops an alert when fresh presence cannot be fetched", async () => {
    vi.useFakeTimers();
    const machines = vi.fn(async () => {
      throw new Error("the relay is unreachable");
    });
    const h = queuedHostAlert({ machines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(machines).toHaveBeenCalledTimes(1);
    expect(h.shown).toHaveLength(0);
  });

  it("drops an alert without a request when the client disconnected", async () => {
    vi.useFakeTimers();
    const machines = vi.fn(async () => [offline]);
    const h = queuedHostAlert({ machines });

    h.updateState({ connection: connection("degraded") });
    await vi.advanceTimersByTimeAsync(1_000);

    expect(machines).not.toHaveBeenCalled();
    expect(h.shown).toHaveLength(0);
  });

  it("delivers the alert when the fresh presence still says offline", async () => {
    vi.useFakeTimers();
    const machines = vi.fn(async () => [offline]);
    const h = queuedHostAlert({ machines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(machines).toHaveBeenCalledTimes(1);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]).toMatchObject({
      key: "host:m_atlas",
      title: "atlas",
      body: "Host offline: h4rn can no longer reach it.",
    });
  });
});
