import { afterEach, describe, expect, it, vi } from "vitest";

import type {
  ConnectDevice,
  Event,
  HostSummary,
  MachineSummary,
  ThreadId,
  ThreadSummary,
} from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";

import type {
  NotificationChannel,
  NotificationMessage,
  NotificationPermissionState,
} from "./channel";
import {
  connectedMachineId,
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
    placed_by_user: false,
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
    rewind: "handoff",
    file_rewind: false,
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

/** This device as connect registered it, optionally paired with a machine. */
function device(machineId: string | null, kind: ConnectDevice["kind"] = "web"): ConnectDevice {
  return {
    id: kind === "daemon" ? "daemon-device-1" : "device-1",
    kind,
    name: kind === "daemon" ? "atlas" : "h4rn web",
    machine_id: machineId,
    proof_public_key: "proof",
    noise_static_public_key: null,
    platform: "web",
    created_at: "2026-11-21T00:00:00Z",
    last_seen: null,
    revoked_at: null,
    scopes: [],
  };
}

function host(hostId: string, name: string): HostSummary {
  return {
    host_id: hostId,
    name,
    platform: "linux",
    online: true,
    last_seen: "2026-11-21T00:00:00Z",
    harnesses: [],
    latency_ms: null,
    path: null,
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

function approvalResolved(threadId: ThreadId, approvalId = "ap_1"): Event {
  return {
    type: "approval_resolved",
    thread_id: threadId,
    approval_id: approvalId,
    option_id: null,
    by_device: null,
  };
}

function turnCompleted(
  threadId: ThreadId,
  status: "completed" | "interrupted" | "failed" = "completed",
  turnId = "turn_1",
): Event {
  return {
    type: "turn_completed",
    thread_id: threadId,
    turn_id: turnId,
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

function turnStarted(threadId: ThreadId, turnId = "turn_1"): Event {
  return {
    type: "turn_started",
    thread_id: threadId,
    turn_id: turnId,
    started_at: "2026-11-21T00:00:00Z",
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
  state?: Partial<RuntimeState>;
  settings?: Partial<NotificationSettings>;
  remote?: Partial<RuntimeRemote>;
  client?: PresenceClient | null;
  connected?: boolean;
  permission?: () => Promise<NotificationPermissionState>;
}): Harness {
  const shown: NotificationMessage[] = [];
  const channel: NotificationChannel = {
    kind: "desktop",
    permission: options.permission ?? (async () => "granted"),
    requestPermission: async () => "granted",
    show: (message) => shown.push(message),
    onClick: () => () => undefined,
  };

  let state: RuntimeState = {
    connection: connection(options.connected === false ? "degraded" : "online"),
    threads: options.threads ?? { [THREAD]: threadSummary() },
    hosts: {},
    lastSyncAt: 1,
    localHostId: null,
    daemonMachineId: null,
    ...options.state,
  };
  let remote: RuntimeRemote = {
    user: { id: "user-1", email: null, display_name: "Dev", avatar_url: null },
    linkedMachines: [],
    linkedSupported: true,
    activeMachineId: null,
    device: null,
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

/** Lets the promise chain inside a flush reach its next await. */
async function settle(): Promise<void> {
  for (let index = 0; index < 8; index += 1) await Promise.resolve();
}

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

  it("keeps a live completion across a routine thread-list refresh", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    // A live completion before the person refreshed the list...
    h.emit(turnCompleted(THREAD));
    // ...does not stop explaining the Done summary that follows: a routine
    // refresh re-seeds placement, not the event stream.
    h.updateState({ lastSyncAt: 2 });

    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Finished: the turn is done.");
  });

  it("keeps the streamed baseline when a refresh returns Done before the completion", async () => {
    vi.useFakeTimers();
    // The thread was already Done when the page loaded...
    const h = harness({
      threads: { [THREAD]: threadSummary(THREAD, { tab: "done", agent_state: "done" }) },
    });
    h.runtime.start();
    // ...and the stream then named it Working for a new turn.
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "working" })));

    // The list refresh answers first, and it already says Done. Adopting that
    // as the baseline would swallow the live upsert that is about to arrive.
    h.updateState({
      threads: { [THREAD]: threadSummary(THREAD, { tab: "done", agent_state: "done" }) },
      lastSyncAt: 2,
    });
    h.emit(turnCompleted(THREAD));
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Finished: the turn is done.");
  });

  it("announces a live finish when the completion arrives before the refresh", async () => {
    vi.useFakeTimers();
    // Same baseline: a listed Done thread whose stream then said Working.
    const h = harness({
      threads: { [THREAD]: threadSummary(THREAD, { tab: "done", agent_state: "done" }) },
    });
    h.runtime.start();
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "working" })));

    // The completion arrives before the list refresh answers Done.
    h.emit(turnCompleted(THREAD));
    h.updateState({
      threads: { [THREAD]: threadSummary(THREAD, { tab: "done", agent_state: "done" }) },
      lastSyncAt: 2,
    });
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "done", agent_state: "done" })));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Finished: the turn is done.");
  });

  it("drops remembered completions when the stream recovers", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    // A live completion belongs to the stream that is about to be replaced...
    h.emit(turnCompleted(THREAD));
    h.updateState({ connection: connection("degraded") });
    h.updateState({ connection: connection("online"), lastSyncAt: 2 });

    // ...so a summary-only Done after the reconnect must not reuse it.
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

  it("still alerts on a failure while the thread is pinned to Working", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    h.emit(turnCompleted(THREAD, "failed"));
    h.emit(
      threadUpserted(
        threadSummary(THREAD, {
          tab: "working",
          pinned: true,
          placed_by_user: true,
          needs_attention: true,
          agent_state: "error",
        }),
      ),
    );

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Failed: the turn ended with an error.");
  });

  it("holds a first-connect failure until a summary names its thread", async () => {
    vi.useFakeTimers();
    // Nothing has named this thread yet, so the notifier cannot know whether
    // it is a subagent child.
    const h = harness({ threads: {} });
    h.runtime.start();

    h.emit(turnCompleted(THREAD, "failed"));
    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);

    // The summary names an ordinary thread, so the failure still needs the
    // person.
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "working" })));
    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Failed: the turn ended with an error.");
  });

  it("keeps a deferred failure when a queued turn starts before the summary", async () => {
    vi.useFakeTimers();
    const h = harness({ threads: {} });
    h.runtime.start();

    // A live failure whose thread nothing has named yet is held back.
    h.emit(turnCompleted(THREAD, "failed"));
    // The daemon starts the next queued turn with a new id before the summary
    // names the thread. That start belongs to a different turn and must not
    // erase the failed turn's outcome.
    h.emit(turnStarted(THREAD, "turn_2"));
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "working" })));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Failed: the turn ended with an error.");
  });

  it("still alerts when the queued turn that started later fails", async () => {
    vi.useFakeTimers();
    const h = harness({ threads: {} });
    h.runtime.start();

    h.emit(turnCompleted(THREAD, "failed"));
    h.emit(turnStarted(THREAD, "turn_2"));
    h.emit(threadUpserted(threadSummary(THREAD, { tab: "working" })));
    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);

    // The queued turn fails on its own. Announcing the earlier turn must not
    // have spent this one's failure.
    h.emit(turnCompleted(THREAD, "failed", "turn_2"));
    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(2);
    expect(h.shown[1]?.body).toBe("Failed: the turn ended with an error.");
  });

  it("stays quiet when a first-connect failure belongs to a subagent child", async () => {
    vi.useFakeTimers();
    const h = harness({ threads: {} });
    h.runtime.start();

    // The child's failure arrives before any summary names it a child. The
    // parent mirrors the moment, so no alert may be delivered.
    h.emit(turnCompleted("thr_child", "failed"));
    h.emit(
      threadUpserted(threadSummary("thr_child", { parent_thread_id: THREAD, tab: "working" })),
    );

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
  });

  it("stays quiet for another client's manual move to Done", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    // This client saw its own turn complete, but the Done summary came from
    // another client's SetThreadTab; the daemon names the placement, so the
    // remembered completion must not turn it into "Finished".
    h.emit(turnCompleted(THREAD));
    h.emit(
      threadUpserted(
        threadSummary(THREAD, { tab: "done", agent_state: "done", placed_by_user: true }),
      ),
    );

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
  });

  it("alerts when the turn of a thread moved to Done by hand fails", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    // The person moves the running thread to Done. That is a placement, and
    // the daemon's summary will never raise its hand for it again.
    h.place(THREAD);
    h.emit(
      threadUpserted(
        threadSummary(THREAD, { tab: "done", agent_state: "done", placed_by_user: true }),
      ),
    );
    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);

    // Its turn then fails. The outcome event is independent of the tab, so
    // the failure still needs the person.
    h.emit(turnCompleted(THREAD, "failed"));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Failed: the turn ended with an error.");
  });

  it("announces one failure per turn, from the outcome then the summary", async () => {
    vi.useFakeTimers();
    const h = harness({});
    h.runtime.start();

    h.emit(turnCompleted(THREAD, "failed"));
    h.emit(
      threadUpserted(
        threadSummary(THREAD, {
          tab: "working",
          needs_attention: true,
          agent_state: "error",
        }),
      ),
    );

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.body).toBe("Failed: the turn ended with an error.");
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
  const windowsOnline = machine("m_atlas", "atlas", true, [
    { id: "h_windows", name: "atlas", kind: "host", distro: null },
    { id: "h_wsl", name: "Ubuntu", kind: "wsl", distro: "Ubuntu" },
  ]);
  const windowsOffline = machine("m_atlas", "atlas", false, windowsOnline.hosts);

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

  it("never announces the relay link's machine offline while talking to it", async () => {
    vi.useFakeTimers();
    // A paired client has no machine id on its device row; its link is the
    // machine the relay's `machine_devices` row named, persisted as the
    // active remote the transport dials.
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: [offline] }));
    const h = harness({
      settings: { hostOffline: true },
      remote: {
        linkedMachines: [online],
        activeMachineId: "m_atlas",
        device: device(null),
      },
      client: { linkedMachines },
    });
    h.runtime.start();

    // The relay now reports that machine offline, but this client is talking
    // to it right now. A live presence answer would deliver a false alert.
    h.updateRemote({ linkedMachines: [offline] });

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
    expect(linkedMachines).not.toHaveBeenCalled();
  });

  it("keeps the paired machine's WSL sibling online too", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: [windowsOffline] }));
    const h = harness({
      settings: { hostOffline: true },
      state: {
        localHostId: "h_windows",
        hosts: { h_windows: host("h_windows", "atlas") },
      },
      remote: {
        linkedMachines: [windowsOnline],
        activeMachineId: "m_atlas",
        device: device(null),
      },
      client: { linkedMachines },
    });
    h.runtime.start();

    // The transport reached the machine, not one row of it: the Windows host
    // and its WSL distro are both on it, so neither may be announced offline.
    h.updateRemote({ linkedMachines: [windowsOffline] });

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
    expect(linkedMachines).not.toHaveBeenCalled();
  });

  it("keeps a directly connected daemon's machine and its WSL sibling online", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: [windowsOffline] }));
    const h = harness({
      settings: { hostOffline: true },
      state: {
        localHostId: "h_windows",
        hosts: { h_windows: host("h_windows", "atlas") },
        // `hello` reported the machine this daemon owns. A desktop app has no
        // active remote; the daemon's own signed-in session names the peer.
        daemonMachineId: "m_atlas",
      },
      remote: {
        linkedMachines: [windowsOnline],
        activeMachineId: null,
        // The account row is this UI's desktop device, not the daemon. It must
        // not be where the peer's machine comes from.
        device: device(null, "desktop"),
      },
      client: { linkedMachines },
    });
    h.runtime.start();
    h.updateRemote({ linkedMachines: [windowsOffline] });

    await vi.advanceTimersByTimeAsync(5_000);
    // The transport reached the machine, so its Windows host and the WSL
    // distro on it are both online: nothing to alert, and no delivery recheck.
    expect(h.shown).toHaveLength(0);
    expect(linkedMachines).not.toHaveBeenCalled();
  });

  it("does not announce a shared-name machine when a second one appears", async () => {
    vi.useFakeTimers();
    // A live relay answer that would confirm both machines offline if it were
    // ever asked. The suppression has to stand before any recheck.
    const bothOffline = [machine("m_one", "atlas", false), machine("m_two", "atlas", false)];
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: bothOffline }));
    const h = harness({
      settings: { hostOffline: true },
      state: {
        localHostId: "h_atlas",
        hosts: { h_atlas: host("h_atlas", "atlas") },
      },
      // The connected machine is only known by the local host's name.
      remote: { linkedMachines: [machine("m_one", "atlas", true)], device: null },
      client: { linkedMachines },
    });
    h.runtime.start();

    // A second machine with the same hostname registers. The name no longer
    // says which machine the transport reached, so neither may take the
    // relay's offline presence for the machine in active use.
    h.updateRemote({ linkedMachines: bothOffline });

    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.shown).toHaveLength(0);
    // No alert means no delivery recheck: the request is only for a due alert.
    expect(linkedMachines).not.toHaveBeenCalled();
  });

  it("drops an alert when a fresh presence fetch says the host is back", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: [online] }));
    const h = queuedHostAlert({ linkedMachines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(linkedMachines).toHaveBeenCalledTimes(1);
    expect(h.shown).toHaveLength(0);
  });

  it("drops an alert when fresh presence cannot be fetched", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => {
      throw new Error("the relay is unreachable");
    });
    const h = queuedHostAlert({ linkedMachines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(linkedMachines).toHaveBeenCalledTimes(1);
    expect(h.shown).toHaveLength(0);
  });

  it("drops an alert without a request when the client disconnected", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: [offline] }));
    const h = queuedHostAlert({ linkedMachines });

    h.updateState({ connection: connection("degraded") });
    await vi.advanceTimersByTimeAsync(1_000);

    expect(linkedMachines).not.toHaveBeenCalled();
    expect(h.shown).toHaveLength(0);
  });

  it("delivers the alert when the fresh presence still says offline", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => ({ supported: true, machines: [offline] }));
    const h = queuedHostAlert({ linkedMachines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(linkedMachines).toHaveBeenCalledTimes(1);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]).toMatchObject({
      key: "host:m_atlas",
      title: "atlas",
      body: "Host offline: h4rn can no longer reach it.",
    });
  });

  it("drops an alert when the relay did not filter to this device's links", async () => {
    vi.useFakeTimers();
    const linkedMachines = vi.fn(async () => ({ supported: false, machines: [offline] }));
    const h = queuedHostAlert({ linkedMachines });

    await vi.advanceTimersByTimeAsync(1_000);

    expect(linkedMachines).toHaveBeenCalledTimes(1);
    expect(h.shown).toHaveLength(0);
  });
});

describe("connected machine identity", () => {
  const relay = { ...connection("online"), transport: "relay" };
  const direct = { ...connection("online"), transport: "direct" };

  it("names a paired client's relay link, not its device row", () => {
    // `ConnectDevice.machine_id` is empty for a paired phone or browser; the
    // relay's `machine_devices` id is the active remote it dials.
    expect(
      connectedMachineId(
        { connection: relay, daemonMachineId: null },
        { activeMachineId: "m_atlas", device: device(null) },
      ),
    ).toBe("m_atlas");
  });

  it("names the daemon's own machine on a direct connection", () => {
    // `hello` reports the connect machine from the daemon's own session, so a
    // desktop app reaches it without any account device row standing in.
    expect(
      connectedMachineId(
        { connection: direct, daemonMachineId: "m_atlas" },
        { activeMachineId: null, device: device(null, "desktop") },
      ),
    ).toBe("m_atlas");
  });

  it("does not borrow this device's own machine row", () => {
    expect(
      connectedMachineId(
        { connection: direct, daemonMachineId: null },
        { activeMachineId: null, device: device(null) },
      ),
    ).toBeNull();
  });

  it("does not name a daemon device row on a relay link with no active machine", () => {
    // GLM P2: a relay link that recorded no machine cannot say which one it
    // reached. A daemon row in the account belongs to some machine (possibly
    // this UI's own local daemon), so it must never stand in.
    expect(
      connectedMachineId(
        { connection: relay, daemonMachineId: null },
        { activeMachineId: null, device: device("m_other", "daemon") },
      ),
    ).toBeNull();
  });

  it("prefers the peer actually dialled over the daemon's own machine", () => {
    // A daemon signed in on atlas can still reach a selected machine; the
    // selected remote is what the transport dials.
    expect(
      connectedMachineId(
        { connection: relay, daemonMachineId: "m_atlas" },
        { activeMachineId: "m_orion", device: device("m_atlas", "daemon") },
      ),
    ).toBe("m_orion");
  });
});

describe("delivery beside a pending fetch", () => {
  const online = machine("m_atlas", "atlas", true);
  const offline = machine("m_atlas", "atlas", false);

  /** A presence call that stays pending until the test answers it. */
  function pendingPresence() {
    let release: (list: MachineSummary[]) => void = () => undefined;
    const linkedMachines = vi.fn(
      () =>
        new Promise<{ supported: boolean; machines: MachineSummary[] }>((resolve) => {
          release = (list) => resolve({ supported: true, machines: list });
        }),
    );
    return { linkedMachines, release: (list: MachineSummary[]) => release(list) };
  }

  function withHostAlert(client: PresenceClient | null) {
    const h = harness({
      settings: { hostOffline: true },
      remote: { linkedMachines: [online] },
      client,
    });
    h.runtime.start();
    h.updateRemote({ linkedMachines: [offline] });
    return h;
  }

  it("shows a due approval without waiting for the host presence fetch", async () => {
    vi.useFakeTimers();
    const presence = pendingPresence();
    const h = withHostAlert({ linkedMachines: presence.linkedMachines });
    h.emit(approvalRequested(THREAD));

    await vi.advanceTimersByTimeAsync(1_000);

    // The relay request is still in flight, but the approval is not hostage
    // to it.
    expect(presence.linkedMachines).toHaveBeenCalledTimes(1);
    expect(h.shown.map((message) => message.key)).toEqual([`thread:${THREAD}`]);

    presence.release([offline]);
    await settle();
    expect(h.shown.map((message) => message.key)).toEqual([`thread:${THREAD}`, "host:m_atlas"]);
  });

  it("cancels a due approval answered while the permission query is pending", async () => {
    vi.useFakeTimers();
    let answer: (state: NotificationPermissionState) => void = () => undefined;
    const h = harness({
      permission: () =>
        new Promise<NotificationPermissionState>((resolve) => {
          answer = resolve;
        }),
    });
    h.runtime.start();
    h.emit(approvalRequested(THREAD));

    // The flush has the draft in hand but has not delivered it yet.
    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.shown).toHaveLength(0);

    h.emit(approvalResolved(THREAD));
    answer("granted");
    await settle();
    expect(h.shown).toHaveLength(0);
  });

  it("keeps a draft queued when the permission query throws", async () => {
    vi.useFakeTimers();
    let calls = 0;
    const h = harness({
      permission: async () => {
        calls += 1;
        if (calls === 1) throw new Error("the shell cannot answer yet");
        return "granted";
      },
    });
    h.runtime.start();
    h.emit(approvalRequested(THREAD));

    await vi.advanceTimersByTimeAsync(1_000);
    expect(calls).toBe(1);
    expect(h.shown).toHaveLength(0);

    // The retry is delayed instead of spinning on a deadline that already
    // passed, and the approval is still there when the shell answers.
    await vi.advanceTimersByTimeAsync(4_999);
    expect(calls).toBe(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls).toBe(2);
    expect(h.shown).toHaveLength(1);
    expect(h.shown[0]?.key).toBe(`thread:${THREAD}`);
  });
});
