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
import type { ConnectDevice, MachineSummary, ThreadId, ThreadSummary, TurnId } from "@/gen";
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
  failedOutcomeDraft,
  finishedDraft,
  finishedTransition,
  type HostState,
  hostPresence,
  type LastTurn,
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
  "connection" | "threads" | "hosts" | "lastSyncAt" | "localHostId" | "daemonMachineId"
>;

/** The parts of a thread summary a transition is measured against. */
type SummaryBaseline = Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state">;

/** The bits of remote state the notifier reads. */
export type RuntimeRemote = {
  user: RemoteState["user"];
  linkedMachines: MachineSummary[];
  /**
   * Whether the relay honoured `?linked=true`; `null` until a successful
   * linked answer arrives.
   */
  linkedSupported: boolean | null;
  /**
   * The machine the active transport dials, from the persisted descriptor.
   *
   * Pairing writes the relay's `machine_devices` id here and a manual pick
   * writes the chosen machine, so this is the peer for relay and direct
   * addresses alike.
   */
  activeMachineId: string | null;
  /**
   * This device as connect registered it.
   *
   * Never a source for the peer's machine: a web or desktop device's row has
   * no machine id, and even a daemon row would name a different thing.
   */
  device: ConnectDevice | null;
};

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
  /**
   * The last summary the live stream sent per thread.
   *
   * Kept apart from list snapshots: a list reply can answer Done before the
   * completion and its upsert arrive, and adopting that placement as the
   * baseline would swallow the very transition the completion explains.
   */
  private readonly streamed = new Map<ThreadId, SummaryBaseline>();
  /**
   * The placement a thread was first seen in through a list response.
   *
   * Used only for threads the stream has not named yet, so a fresh page knows
   * a thread was already Done instead of treating its next tick as a finish.
   */
  private readonly listed = new Map<ThreadId, SummaryBaseline>();
  /** Threads the daemon reported as subagent children. */
  private readonly childThreads = new Set<ThreadId>();
  /**
   * Live failure reports waiting for a summary to name their thread.
   *
   * Keyed by turn id, not just thread: a queued turn can start before the
   * summary arrives, and its `turn_started` must not erase the failed turn's
   * outcome before the thread is known not to be a child.
   */
  private readonly pendingFailures = new Map<ThreadId, { turnId: TurnId; lastTurn: LastTurn }>();
  /** What each thread's most recent turn did, until the next turn starts. */
  private readonly memory = new NotificationMemory();
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

    this.seedListed();
    this.lastConnection = this.deps.state().connection.status;

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
    if (event.type === "turn_started") {
      this.memory.startTurn(event.thread_id);
      // Only this same turn starting is news that erases its own pending
      // failure. A queued turn starts with a new id; waiting for the summary
      // must not be cancelled by the turn behind it.
      if (this.pendingFailures.get(event.thread_id)?.turnId === event.turn_id) {
        this.pendingFailures.delete(event.thread_id);
      }
    }
    if (event.type === "turn_completed") {
      this.memory.completeTurn(event.thread_id, event.status, this.now(), !envelope.replayed);
      // The completion is the turn's own outcome, wherever the tab sits. A
      // person who already moved a running thread to Done still needs to hear
      // that its turn failed; the summary may never raise `needs_attention`
      // there, so placement must not be the thing that decides this.
      if (!envelope.replayed) {
        const threadId = event.thread_id;
        if (event.status === "failed" && !this.summaryKnown(threadId)) {
          // A failure can arrive before any summary names its thread. Keep the
          // outcome by turn id: if it turns out to be a subagent child, the
          // parent already mirrors the moment and there must be no alert, but
          // a queued turn starting meanwhile must not lose the failure.
          const lastTurn = this.memory.lastTurn(threadId);
          if (lastTurn !== null) {
            this.pendingFailures.set(threadId, { turnId: event.turn_id, lastTurn });
          }
        } else {
          this.announceOutcomeFailure(threadId);
        }
      }
    }

    if (event.type === "thread_upserted") {
      const deferredFailure = this.pendingFailures.get(event.thread_id) ?? null;
      this.pendingFailures.delete(event.thread_id);
      if (event.parent_thread_id) {
        // An approval can arrive before the summary that names the child. The
        // parent mirrors it, so withdraw the queued child draft now.
        this.childThreads.add(event.thread_id);
        this.queue = dropThreadApproval(this.queue, event.thread_id);
        this.schedule();
      } else {
        this.childThreads.delete(event.thread_id);
      }
      const previous = this.streamed.get(event.thread_id) ?? this.listed.get(event.thread_id);
      const transition = finishedTransition(previous, event);
      this.streamed.set(event.thread_id, {
        tab: event.tab,
        needs_attention: event.needs_attention,
        agent_state: event.agent_state,
      });
      if (deferredFailure !== null && !event.parent_thread_id) {
        // The summary is the first thing to name this thread. It is not a
        // child, so its live failure still needs the person, even when a
        // queued turn started in the meantime.
        this.announceOutcomeFailure(event.thread_id, {
          lastTurn: deferredFailure.lastTurn,
          title: event.title,
        });
      }
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

  /** Whether a summary (live or listed) already names this thread. */
  private summaryKnown(threadId: ThreadId): boolean {
    return (
      this.childThreads.has(threadId) ||
      this.deps.state().threads[threadId] !== undefined ||
      this.streamed.has(threadId) ||
      this.listed.has(threadId)
    );
  }

  /** Queues a live failure outcome once the thread is known not to be a child. */
  private announceOutcomeFailure(
    threadId: ThreadId,
    options: { lastTurn?: LastTurn; title?: string } = {},
  ): void {
    const isChild =
      this.childThreads.has(threadId) ||
      this.deps.state().threads[threadId]?.parent_thread_id != null;
    const failed = failedOutcomeDraft(threadId, {
      threads: this.deps.state().threads,
      settings: this.deps.settings(),
      focus: this.deps.focus(),
      lastTurn: options.lastTurn ?? this.memory.lastTurn(threadId),
      announcedFailure: this.memory.failureAnnounced(threadId),
      childThread: isChild,
      ...(options.title === undefined ? {} : { title: options.title }),
    });
    if (failed !== null) {
      // The outcome was the announcement: a later Done summary for the
      // same turn has nothing left to say. Only the turn this memory still
      // holds may be marked: a queued turn's `turn_started` already cleared
      // the live completion, and marking it would silence that turn's own
      // failure later.
      const sameTurn = this.memory.hasLiveCompletion(threadId);
      this.memory.announceFinished(threadId);
      if (sameTurn) this.memory.announceFailure(threadId);
    }
    this.queueDraft(failed);
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
      connectedMachineId: connectedMachineId(state, remote),
      localHostName: state.localHostId ? (state.hosts[state.localHostId]?.name ?? null) : null,
    });
  }

  /** Whether the relay said it filtered `?linked=true` for this device. */
  private linkedAvailable(): boolean {
    return this.linkedSupported ?? this.deps.remote().linkedSupported ?? false;
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
   * finished a turn, and its first transition would be ignored. A thread's
   * first listed placement is kept: later list ticks never replace it, because
   * a reply can outrun the stream with a Done that the live upsert is about to
   * explain. The stream's own summaries live in `streamed` and always win.
   */
  private seedListed(): void {
    for (const [threadId, summary] of Object.entries(this.deps.state().threads)) {
      if (this.listed.has(threadId)) continue;
      this.listed.set(threadId, {
        tab: summary.tab,
        needs_attention: summary.needs_attention,
        agent_state: summary.agent_state,
      });
    }
  }

  /**
   * Starts a fresh baseline after the connection becomes ready.
   *
   * A reconnect or a failed subscribe can leave the event stream with a gap
   * that only the thread list fills. The list is authoritative at that moment:
   * if it says Done, a later summary update is not the moment a turn finished.
   * Remembered live completions belong to the previous stream, so they are
   * cleared too.
   */
  private resetBaseline(): void {
    this.streamed.clear();
    this.listed.clear();
    this.pendingFailures.clear();
    this.memory.clearCompletions();
    this.seedListed();
  }

  /**
   * Queues a deferred failure once a list snapshot names its thread.
   *
   * A child's failure is cancelled rather than announced: its parent mirrors
   * the moment.
   */
  private resolvePendingFailures(): void {
    if (this.pendingFailures.size === 0) return;
    const threads = this.deps.state().threads;
    for (const [threadId, deferred] of [...this.pendingFailures]) {
      const summary = threads[threadId];
      if (!summary) continue;
      this.pendingFailures.delete(threadId);
      if (summary.parent_thread_id == null) {
        this.announceOutcomeFailure(threadId, {
          lastTurn: deferred.lastTurn,
          title: summary.title,
        });
      }
    }
  }

  private onStoreChange(): void {
    const state = this.deps.state();
    const becameOnline = state.connection.status === "online" && this.lastConnection !== "online";
    this.lastConnection = state.connection.status;
    if (becameOnline) {
      // Every ready handshake starts a fresh baseline, before any list arrives.
      this.resetBaseline();
    } else {
      // A routine refresh may introduce a thread the stream has not named yet,
      // but it never replaces a baseline: the event stream owns every thread it
      // has spoken about, and a list that answers Done first must not swallow
      // the live completion and upsert that follow.
      this.seedListed();
    }
    this.resolvePendingFailures();
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

/**
 * The machine the live transport is actually reaching.
 *
 * A relay link with no recorded machine cannot say which one it reached: the
 * account's device rows are this UI's own registrations, not the peer, so they
 * never stand in. A direct socket reaches exactly the daemon that answered
 * `hello`, and that daemon reports the connect machine it owns from its own
 * signed-in session (`daemon_machine_id`). That verified id is what keeps all
 * of the machine's hosts online, WSL siblings included. `null` (no active
 * link, a signed-out daemon or an older daemon) leaves the local host name as
 * the only fallback, narrowed to the one host it names.
 */
export function connectedMachineId(
  state: Pick<RuntimeState, "connection" | "daemonMachineId">,
  remote: Pick<RuntimeRemote, "activeMachineId" | "device">,
): string | null {
  if (remote.activeMachineId !== null) return remote.activeMachineId;
  // `remote.device` is deliberately ignored: it is this UI's own device, and
  // its `machine_id` (if any) names a different thing from the peer.
  if (state.connection.transport === "relay") return null;
  return state.daemonMachineId;
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
