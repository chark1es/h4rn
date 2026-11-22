/**
 * Watches the live stream and turns a few events into OS notifications.
 *
 * It is intentionally thin: `logic.ts` decides what and when, `channel.ts`
 * knows how to show it, and this module owns the wiring — the queue timer, the
 * focus snapshot, the host presence snapshot and the click that opens a
 * thread. `NotificationRuntime` takes that wiring as a small environment so
 * tests can drive the real store loop without React or a daemon.
 */

import { getRemoteState, type RemoteState, remoteStore } from "@/features/remote/remoteStore";
import { currentConnectClient } from "@/features/remote/session";
import type { MachineSummary, ThreadId, ThreadSummary } from "@/gen";
import type { ConnectionStatus, EventEnvelope } from "@/lib/client/types";
import {
  type AppState,
  getState,
  onAppEvent,
  openThread,
  refreshHosts,
  store,
} from "@/lib/store/appStore";

import {
  type NotificationChannel,
  type NotificationPermissionState,
  notificationChannel,
  notificationId,
} from "./channel";
import {
  alreadyVisible,
  draftForEnvelope,
  dropResolvedApproval,
  dropThreadApproval,
  dueNotifications,
  enqueue,
  type FocusSnapshot,
  finishedDraft,
  finishedTransition,
  type HostState,
  hostPresence,
  type NotificationDraft,
  type NotificationKind,
  type NotificationQueue,
  nextDeadline,
  offlineTransitions,
  type PendingNotification,
  stillOffline,
} from "./logic";
import { NotificationMemory } from "./memory";
import { onManualPlacement } from "./placement";
import { getNotificationSettings, type NotificationSettings } from "./settings";

/** How often connect's machine list is re-read for host presence. */
const HOST_POLL_MS = 30_000;

/**
 * How long to wait before retrying a flush whose permission query threw.
 *
 * The drafts stay in the queue; without a delay the already-due deadline would
 * arm a zero-millisecond timer and the shell would be asked in a hot loop.
 */
const PERMISSION_RETRY_MS = 5_000;

/** The bits of app state the notifier reads. */
export type RuntimeState = Pick<
  AppState,
  "connection" | "threads" | "hosts" | "lastSyncAt" | "localHostId"
>;

/** The bits of remote state the notifier reads. */
export type RuntimeRemote = Pick<
  RemoteState,
  "user" | "linkedMachines" | "linkedSupported" | "activeMachineId"
>;

/** One linked-machine answer from connect. */
export type PresenceClient = {
  /** The machines this device is linked to, and whether the relay filtered. */
  linkedMachines(): Promise<{ supported: boolean; machines: MachineSummary[] }>;
};

/**
 * Everything `NotificationRuntime` touches outside its own decision logic.
 *
 * Production wires the real stores and shells; tests provide a tiny store and
 * a recording channel.
 */
export type NotificationRuntimeDeps = {
  channel: NotificationChannel;
  state: () => RuntimeState;
  remote: () => RuntimeRemote;
  onEvent: (listener: (envelope: EventEnvelope) => void) => () => void;
  onStateChange: (listener: () => void) => () => void;
  onRemoteChange: (listener: () => void) => () => void;
  onManualPlacement?: (listener: (threadId: ThreadId) => void) => () => void;
  openThread?: (threadId: ThreadId) => void;
  refreshHosts: () => void;
  connectClient: () => PresenceClient | null;
  settings: () => NotificationSettings;
  focus: () => FocusSnapshot;
  now?: () => number;
};

function enabledFor(kind: NotificationKind, settings: NotificationSettings): boolean {
  if (kind === "approval") return settings.approvals;
  if (kind === "finished") return settings.finishedTurns;
  return settings.hostOffline;
}

export class NotificationRuntime {
  private readonly deps: NotificationRuntimeDeps;
  private readonly now: () => number;
  private queue: NotificationQueue = {};
  private timer: ReturnType<typeof setTimeout> | null = null;
  /** True while a flush is asking permission or showing its drafts. */
  private flushing = false;
  /** True while due host alerts are waiting on their own presence re-check. */
  private hostFetching = false;
  /** `null` until the first presence snapshot establishes a baseline. */
  private hosts: Record<string, HostState> | null = null;
  /** The last polled connect machine list, while signed in. */
  private machines: MachineSummary[] | null = null;
  /** Whether the relay honours `?linked=true`; null until a poll answers. */
  private linkedSupported: boolean | null = null;
  /** The daemon summary each thread was last seen in. */
  private readonly summaries = new Map<
    ThreadId,
    Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state">
  >();
  /** Threads the daemon reported as subagent children. */
  private readonly childThreads = new Set<ThreadId>();
  /** What each thread's most recent turn did, until the next turn starts. */
  private readonly memory = new NotificationMemory();
  /** The store's last list sync, so a healed list can re-seed the baseline. */
  private lastThreadsSyncAt: number | null = null;
  private lastConnection: string | null = null;
  private started = false;

  constructor(deps: NotificationRuntimeDeps) {
    this.deps = deps;
    this.now = deps.now ?? (() => Date.now());
  }

  /** Starts the watcher; safe to call more than once. */
  start(): void {
    if (this.started) return;
    this.started = true;

    this.seedSummaries();
    this.lastConnection = this.deps.state().connection.status;
    this.lastThreadsSyncAt = this.deps.state().lastSyncAt;

    this.deps.channel.onClick((threadId) => {
      if (threadId) this.deps.openThread?.(threadId);
    });
    this.deps.onEvent((envelope) => this.handleEnvelope(envelope));
    this.deps.onStateChange(() => this.onStoreChange());
    this.deps.onRemoteChange(() => this.evaluateHosts());
    this.deps.onManualPlacement?.((threadId) => this.memory.announceFinished(threadId));
    this.evaluateHosts();
  }

  /** Re-reads connect's machine list and the daemon's host list. */
  async pollHosts(): Promise<void> {
    if (!this.deps.settings().hostOffline) {
      this.machines = null;
      this.hosts = null;
      return;
    }
    const remote = this.deps.remote();
    if (remote.user === null) {
      // A direct session has no relay list; the daemon's hosts are enough for
      // the rest of the UI, but the notifier has no presence signal to watch.
      this.machines = null;
      this.deps.refreshHosts();
      this.evaluateHosts();
      return;
    }
    if (this.deps.state().connection.status !== "online") {
      this.machines = null;
      this.hosts = null;
      return;
    }
    this.deps.refreshHosts();
    const client = this.deps.connectClient();
    if (!client) return;
    let answer: { supported: boolean; machines: MachineSummary[] };
    try {
      answer = await client.linkedMachines();
    } catch {
      // A relay that cannot answer says nothing about a host's presence.
      return;
    }
    if (this.deps.state().connection.status !== "online") {
      this.machines = null;
      this.hosts = null;
      return;
    }
    this.linkedSupported = answer.supported;
    this.machines = answer.supported ? answer.machines : null;
    this.evaluateHosts();
  }

  private schedule(notBefore: number | null = null): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    // Host alerts waiting on their own presence request are left out: that
    // request re-checks them when it answers, and scheduling them now would
    // arm a zero-millisecond timer that cannot deliver anything.
    const deadline = nextDeadline(
      this.queue,
      (pending) => this.hostFetching && pending.kind === "host_offline",
    );
    if (deadline === null) return;
    const at = notBefore === null ? deadline : Math.max(deadline, notBefore);
    this.timer = setTimeout(
      () => {
        this.timer = null;
        void this.flush();
      },
      Math.max(0, at - this.now()),
    );
  }

  /**
   * Shows everything due, then re-arms the timer for what is left.
   *
   * Permission is asked before anything leaves the queue, so a shell that
   * cannot answer keeps its drafts for the next attempt. Approved thread
   * drafts are shown right away; a due host alert re-checks presence on its
   * own (`flushHosts`), because a relay request must not hold an approval or a
   * finished turn back.
   */
  private async flush(): Promise<void> {
    if (this.flushing) return;
    this.flushing = true;
    let retryAt: number | null = null;
    try {
      const { ready } = dueNotifications(this.queue, this.now());
      if (ready.length === 0) return;

      let permission: NotificationPermissionState;
      try {
        permission = await this.deps.channel.permission();
      } catch {
        // The queue was not touched: the same drafts are retried, not lost.
        retryAt = this.now() + PERMISSION_RETRY_MS;
        return;
      }
      if (permission !== "granted" && permission !== "unknown") {
        for (const draft of ready) this.takeQueued(draft);
        return;
      }

      const settings = this.deps.settings();
      const focus = this.deps.focus();
      let hasHostDrafts = false;
      for (const draft of ready) {
        if (draft.kind === "host_offline") {
          hasHostDrafts = true;
          continue;
        }
        // The person may have opened the thread, answered the approval or
        // switched the kind off while the permission query was pending.
        if (!enabledFor(draft.kind, settings) || alreadyVisible(draft, focus)) {
          this.takeQueued(draft);
          continue;
        }
        // Taken only now, at delivery: an `approval_resolved` that arrived
        // while this flush waited has already removed (or replaced) it.
        if (this.takeQueued(draft) === null) continue;
        this.show(draft);
      }
      if (hasHostDrafts) void this.flushHosts();
    } finally {
      this.flushing = false;
      this.schedule(retryAt);
    }
  }

  /**
   * Re-checks a due host alert's presence and shows it only while the host is
   * still away.
   *
   * Runs beside the thread drafts so a slow relay cannot delay an approval or
   * a finished turn that became due at the same moment. The alerts are taken
   * out of the queue before the request, so nothing re-arms a timer for them
   * while the answer is in flight.
   */
  private async flushHosts(): Promise<void> {
    if (this.hostFetching) return;
    this.hostFetching = true;
    try {
      const { ready } = dueNotifications(this.queue, this.now());
      const claimed: NotificationDraft[] = [];
      for (const draft of ready) {
        if (draft.kind !== "host_offline") continue;
        if (this.takeQueued(draft) !== null) claimed.push(draft);
      }
      if (claimed.length === 0) return;

      const fresh = await this.freshPresence();
      const settings = this.deps.settings();
      const focus = this.deps.focus();
      for (const draft of claimed) {
        if (!stillOffline(draft, fresh.presence, fresh.connection)) continue;
        if (!enabledFor(draft.kind, settings)) continue;
        if (alreadyVisible(draft, focus)) continue;
        this.show(draft);
      }
    } finally {
      this.hostFetching = false;
      this.schedule();
    }
  }

  /**
   * Removes a due draft from the queue at the moment it is delivered.
   *
   * The key alone is not enough: an answered approval, or a newer approval for
   * the same thread, can replace the entry while a flush waits for permission
   * or a presence answer. Matching the approval id keeps the replacement in
   * the queue and delivers nothing stale.
   */
  private takeQueued(draft: NotificationDraft): PendingNotification | null {
    const pending = this.queue[draft.key];
    if (!pending) return null;
    if (pending.approvalId !== draft.approvalId) return null;
    if (pending.dueAt > this.now()) return null;
    const next = { ...this.queue };
    delete next[draft.key];
    this.queue = next;
    return pending;
  }

  private show(draft: NotificationDraft): void {
    this.deps.channel.show({
      id: notificationId(draft.key),
      key: draft.key,
      title: draft.title,
      body: draft.body,
      threadId: draft.threadId,
    });
  }

  /**
   * Presence for a due host alert, fetched now rather than trusted from the
   * poll that queued it. A fetch that fails, or a client that disconnected
   * meanwhile, reports `null` so the alerts are dropped: a stale "offline"
   * message is worse than none.
   */
  private async freshPresence(): Promise<{
    presence: Record<string, HostState> | null;
    connection: ConnectionStatus;
  }> {
    const connection = this.deps.state().connection.status;
    if (connection !== "online") return { presence: null, connection };
    const remote = this.deps.remote();
    const client = this.deps.connectClient();
    if (remote.user === null || client === null) return { presence: null, connection };
    let answer: { supported: boolean; machines: MachineSummary[] };
    try {
      answer = await client.linkedMachines();
    } catch {
      return {
        presence: null,
        connection: this.deps.state().connection.status,
      };
    }
    const after = this.deps.state().connection.status;
    if (after !== "online") return { presence: null, connection: after };
    this.linkedSupported = answer.supported;
    this.machines = answer.supported ? answer.machines : null;
    return { presence: this.presenceSnapshot(), connection: after };
  }

  private queueDraft(draft: NotificationDraft | null): void {
    if (!draft) return;
    if (!enabledFor(draft.kind, this.deps.settings())) return;
    this.queue = enqueue(this.queue, draft, this.now());
    this.schedule();
  }

  private handleEnvelope(envelope: EventEnvelope): void {
    const { event } = envelope;

    // Memory is kept even for replayed history: the notifier still has to
    // explain a Done transition that only finished arriving after a reconnect.
    // Only a live completion may be the reason a notification fires, though.
    if (event.type === "error") {
      this.memory.rememberError(event.thread_id, event.message, this.now());
    }
    if (event.type === "turn_started") this.memory.startTurn(event.thread_id);
    if (event.type === "turn_completed") {
      this.memory.completeTurn(event.thread_id, event.status, this.now(), !envelope.replayed);
    }

    if (event.type === "thread_upserted") {
      if (event.parent_thread_id) {
        // An approval can arrive before the summary that names the child. The
        // parent mirrors it, so withdraw the queued child draft now.
        this.childThreads.add(event.thread_id);
        this.queue = dropThreadApproval(this.queue, event.thread_id);
        this.schedule();
      } else {
        this.childThreads.delete(event.thread_id);
      }
      const previous = this.summaries.get(event.thread_id);
      const transition = finishedTransition(previous, event);
      this.summaries.set(event.thread_id, {
        tab: event.tab,
        needs_attention: event.needs_attention,
        agent_state: event.agent_state,
      });
      if (envelope.replayed) return;
      const liveCompletion = this.memory.hasLiveCompletion(event.thread_id);
      const draft = finishedDraft(previous, event, {
        settings: this.deps.settings(),
        focus: this.deps.focus(),
        lastTurn: this.memory.lastTurn(event.thread_id),
        announcedFailure: this.memory.failureAnnounced(event.thread_id),
        liveCompletion,
        // A Done that another client placed by hand is a placement, not a
        // completion, even while this client still remembers a live one.
        placedByUser: event.placed_by_user,
      });
      // One live completion explains one transition, even when the person is
      // already looking at the thread or the switch is off, so a later manual
      // trip back to Done cannot reuse it.
      if (transition !== null && liveCompletion) this.memory.announceFinished(event.thread_id);
      this.queueDraft(draft);
      if (draft !== null && transition === "failure") {
        this.memory.announceFailure(event.thread_id);
      }
      return;
    }

    if (event.type === "approval_resolved") {
      this.queue = dropResolvedApproval(this.queue, event.thread_id, event.approval_id);
      this.schedule();
      return;
    }

    if (event.type !== "approval_requested") return;
    const isChild =
      this.childThreads.has(event.thread_id) ||
      this.deps.state().threads[event.thread_id]?.parent_thread_id != null;
    this.queueDraft(
      draftForEnvelope(envelope, {
        threads: this.deps.state().threads,
        settings: this.deps.settings(),
        focus: this.deps.focus(),
        childThread: isChild,
      }),
    );
  }

  /** The current host snapshot, in the same shape the sidebar's rows use. */
  private presenceSnapshot(): Record<string, HostState> | null {
    const state = this.deps.state();
    const remote = this.deps.remote();
    if (!this.linkedAvailable()) return null;
    return hostPresence({
      connection: state.connection.status,
      signedIn: remote.user !== null,
      // Only machines this device is linked to are watched. Connect's unflagged
      // list is every machine on the account, and this device cannot see the
      // others' hosts.
      machines: this.machines ?? remote.linkedMachines,
      activeMachineId: remote.activeMachineId,
      localHostName: state.localHostId ? (state.hosts[state.localHostId]?.name ?? null) : null,
    });
  }

  /** Whether the relay said it filtered `?linked=true` for this device. */
  private linkedAvailable(): boolean {
    return this.linkedSupported ?? this.deps.remote().linkedSupported;
  }

  /** Whether this session has the relay presence that makes the row meaningful. */
  private relayPresenceAvailable(): boolean {
    const remote = this.deps.remote();
    return (
      remote.user !== null &&
      this.linkedAvailable() &&
      (this.machines ?? remote.linkedMachines).length > 0
    );
  }

  /**
   * Adopts summaries that arrived through a list response, not an event.
   *
   * `start()` runs before the first thread list loads, so without this a
   * thread that was already Done at startup would look brand new when it later
   * finished a turn, and its first transition would be ignored.
   */
  private seedSummaries(replace = false): void {
    for (const [threadId, summary] of Object.entries(this.deps.state().threads)) {
      if (!replace && this.summaries.has(threadId)) continue;
      this.summaries.set(threadId, {
        tab: summary.tab,
        needs_attention: summary.needs_attention,
        agent_state: summary.agent_state,
      });
    }
  }

  /**
   * Re-seeds from the daemon store without transitions.
   *
   * A reconnect or a failed subscribe can leave the event stream with a gap
   * that only the thread list fills. The list is authoritative for the
   * baseline: if it says Done, a later summary update is not the moment a turn
   * finished. Remembered live completions belong to that same stream, so a
   * gap recovery clears them too.
   */
  private reseedSummaries(clearCompletions = false): void {
    this.summaries.clear();
    if (clearCompletions) this.memory.clearCompletions();
    this.seedSummaries();
    this.lastThreadsSyncAt = this.deps.state().lastSyncAt;
  }

  private onStoreChange(): void {
    const state = this.deps.state();
    const becameOnline = state.connection.status === "online" && this.lastConnection !== "online";
    this.lastConnection = state.connection.status;
    if (becameOnline) {
      // Every ready handshake starts a fresh baseline, before any list arrives.
      this.reseedSummaries(true);
    } else if (state.lastSyncAt !== this.lastThreadsSyncAt) {
      // A routine refresh also adopts the list's placement, but it does not
      // replace the event stream: a live completion that arrived before the
      // person opened Settings or refreshed is still the reason a later Done
      // summary is news.
      this.reseedSummaries();
    } else {
      this.seedSummaries();
    }
    this.evaluateHosts();
  }

  private evaluateHosts(): void {
    if (!this.deps.settings().hostOffline || !this.relayPresenceAvailable()) {
      this.hosts = null;
      return;
    }
    const next = this.presenceSnapshot();
    if (next === null) {
      this.hosts = null;
      return;
    }
    if (this.hosts !== null) {
      for (const draft of offlineTransitions(this.hosts, next, this.deps.focus())) {
        this.queueDraft(draft);
      }
    }
    this.hosts = next;
  }
}

function focusSnapshot(): FocusSnapshot {
  const state = getState();
  return {
    windowFocused: typeof document === "undefined" ? true : document.hasFocus(),
    openThreadId: state.ui.route.kind === "thread" ? state.ui.route.threadId : null,
    connection: state.connection.status,
  };
}

let current: NotificationRuntime | null = null;

/**
 * Starts the watcher; safe to call more than once.
 *
 * `App` calls it on mount. The run stays alive for the life of the page: the
 * connection can drop and return, and the same queue keeps its place.
 */
export function startNotifications(): void {
  if (current) return;
  const channel = notificationChannel();
  if (!channel) return;

  const runtime = new NotificationRuntime({
    channel,
    state: () => getState(),
    remote: () => getRemoteState(),
    onEvent: onAppEvent,
    onStateChange: (listener) => store.subscribe(listener),
    onRemoteChange: (listener) => remoteStore.subscribe(listener),
    onManualPlacement,
    openThread,
    refreshHosts,
    connectClient: currentConnectClient,
    settings: getNotificationSettings,
    focus: focusSnapshot,
  });
  runtime.start();
  current = runtime;

  if (typeof window !== "undefined") {
    const poll = () => void runtime.pollHosts();
    const hostTimer = window.setInterval(poll, HOST_POLL_MS);
    window.setTimeout(poll, 3_000);
    // Do not leave a timer behind when the page unloads in a test harness.
    window.addEventListener("pagehide", () => window.clearInterval(hostTimer), { once: true });
  }
}
