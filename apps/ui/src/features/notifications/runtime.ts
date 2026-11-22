/**
 * Watches the live stream and turns a few events into OS notifications.
 *
 * It is intentionally thin: `logic.ts` decides what and when, `channel.ts`
 * knows how to show it, and this module owns the wiring — the queue timer, the
 * focus snapshot, the host presence snapshot and the click that opens a
 * thread.
 */

import { getRemoteState, remoteStore } from "@/features/remote/remoteStore";
import { currentConnectClient } from "@/features/remote/session";
import type { MachineSummary, ThreadId, ThreadSummary } from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";
import { getState, onAppEvent, openThread, refreshHosts, store } from "@/lib/store/appStore";

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
  stillOffline,
} from "./logic";
import { NotificationMemory } from "./memory";
import { getNotificationSettings, type NotificationSettings } from "./settings";

/** How often connect's machine list is re-read for host presence. */
const HOST_POLL_MS = 30_000;

let started = false;
let channel: NotificationChannel | null = null;
let queue: NotificationQueue = {};
let timer: ReturnType<typeof setTimeout> | null = null;
let flushing = false;
/** `null` until the first presence snapshot establishes a baseline. */
let hosts: Record<string, HostState> | null = null;
/** The last polled connect machine list, while signed in. */
let machines: MachineSummary[] | null = null;
/** The daemon summary each thread was last seen in. */
const summaries = new Map<
  ThreadId,
  Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state">
>();
/** Threads the daemon reported as subagent children. */
const childThreads = new Set<ThreadId>();
/** What each thread's most recent turn did, until the next turn starts. */
const memory = new NotificationMemory();
/** The store's last list sync, so a healed list can re-seed the baseline. */
let lastThreadsSyncAt: number | null = null;
let lastConnection: string | null = null;

function focusSnapshot(): FocusSnapshot {
  const state = getState();
  return {
    windowFocused: typeof document === "undefined" ? true : document.hasFocus(),
    openThreadId: state.ui.route.kind === "thread" ? state.ui.route.threadId : null,
    connection: state.connection.status,
  };
}

function enabledFor(kind: NotificationKind, settings: NotificationSettings): boolean {
  if (kind === "approval") return settings.approvals;
  if (kind === "finished") return settings.finishedTurns;
  return settings.hostOffline;
}

function schedule(): void {
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
  const deadline = nextDeadline(queue);
  if (deadline === null) return;
  timer = setTimeout(
    () => {
      timer = null;
      void flush();
    },
    Math.max(0, deadline - Date.now()),
  );
}

async function flush(): Promise<void> {
  if (flushing) return;
  flushing = true;
  try {
    const { ready, queue: remaining } = dueNotifications(queue, Date.now());
    queue = remaining;
    if (ready.length === 0 || !channel) return;
    let permission: NotificationPermissionState;
    try {
      permission = await channel.permission();
    } catch {
      // A shell that cannot answer is treated like one without permission.
      return;
    }
    if (permission !== "granted" && permission !== "unknown") return;
    const settings = getNotificationSettings();
    const focus = focusSnapshot();
    for (const draft of ready) {
      // The person may have opened the thread or one of the switches while
      // the burst was coalescing.
      if (!enabledFor(draft.kind, settings)) continue;
      if (alreadyVisible(draft, focus)) continue;
      // A host may have come back (or this client disconnected) while the
      // alert waited out its quiet period; announce only what is still true.
      if (!stillOffline(draft, presenceSnapshot(), getState().connection.status)) continue;
      channel.show({
        id: notificationId(draft.key),
        key: draft.key,
        title: draft.title,
        body: draft.body,
        threadId: draft.threadId,
      });
    }
  } finally {
    flushing = false;
    schedule();
  }
}

function queueDraft(draft: NotificationDraft | null): void {
  if (!draft) return;
  if (!enabledFor(draft.kind, getNotificationSettings())) return;
  queue = enqueue(queue, draft, Date.now());
  schedule();
}

function handleEnvelope(envelope: EventEnvelope): void {
  const { event } = envelope;

  // Memory is kept even for replayed history: the notifier still has to
  // explain a Done transition that only finished arriving after a reconnect.
  // Only a live completion may be the reason a notification fires, though.
  if (event.type === "error") {
    memory.rememberError(event.thread_id, event.message, Date.now());
  }
  if (event.type === "turn_started") memory.startTurn(event.thread_id);
  if (event.type === "turn_completed") {
    memory.completeTurn(event.thread_id, event.status, Date.now(), !envelope.replayed);
  }

  if (event.type === "thread_upserted") {
    if (event.parent_thread_id) childThreads.add(event.thread_id);
    else childThreads.delete(event.thread_id);
    const previous = summaries.get(event.thread_id);
    const transition = finishedTransition(previous, event);
    summaries.set(event.thread_id, {
      tab: event.tab,
      needs_attention: event.needs_attention,
      agent_state: event.agent_state,
    });
    if (envelope.replayed) return;
    const liveCompletion = memory.hasLiveCompletion(event.thread_id);
    const draft = finishedDraft(previous, event, {
      settings: getNotificationSettings(),
      focus: focusSnapshot(),
      lastTurn: memory.lastTurn(event.thread_id),
      announcedFailure: memory.failureAnnounced(event.thread_id),
      liveCompletion,
    });
    // One live completion explains one transition, even when the person is
    // already looking at the thread or the switch is off, so a later manual
    // trip back to Done cannot reuse it.
    if (transition !== null && liveCompletion) memory.announceFinished(event.thread_id);
    queueDraft(draft);
    if (draft !== null && transition === "failure") memory.announceFailure(event.thread_id);
    return;
  }

  if (event.type === "approval_resolved") {
    queue = dropResolvedApproval(queue, event.thread_id, event.approval_id);
    schedule();
    return;
  }

  if (event.type !== "approval_requested") return;
  const isChild =
    childThreads.has(event.thread_id) ||
    getState().threads[event.thread_id]?.parent_thread_id != null;
  queueDraft(
    draftForEnvelope(envelope, {
      threads: getState().threads,
      settings: getNotificationSettings(),
      focus: focusSnapshot(),
      childThread: isChild,
    }),
  );
}

/** The current host snapshot, in the same shape the sidebar's rows use. */
function presenceSnapshot(): Record<string, HostState> | null {
  const state = getState();
  const remote = getRemoteState();
  return hostPresence({
    connection: state.connection.status,
    signedIn: remote.user !== null,
    // Only machines this device is linked to are watched. Connect's unflagged
    // list is every machine on the account, and this device cannot see the
    // others' hosts.
    machines: machines ?? remote.linkedMachines,
    activeMachineId: remote.activeMachineId,
    localHostId: state.localHostId,
  });
}

/** Whether this session has the relay presence that makes the row meaningful. */
function relayPresenceAvailable(): boolean {
  const remote = getRemoteState();
  return remote.user !== null && (machines ?? remote.linkedMachines).length > 0;
}

/**
 * Adopts summaries that arrived through a list response, not an event.
 *
 * `startNotifications` runs before the first thread list loads, so without
 * this a thread that was already Done at startup would look brand new when it
 * later finished a turn, and its first transition would be ignored.
 */
function seedSummaries(replace = false): void {
  for (const [threadId, summary] of Object.entries(getState().threads)) {
    if (!replace && summaries.has(threadId)) continue;
    summaries.set(threadId, {
      tab: summary.tab,
      needs_attention: summary.needs_attention,
      agent_state: summary.agent_state,
    });
  }
}

/**
 * Re-seeds from the daemon store without transitions.
 *
 * A reconnect or a failed subscribe can leave the event stream with a gap that
 * only the thread list fills. The list is authoritative for the baseline: if
 * it says Done, a later summary update is not the moment a turn finished.
 */
function reseedSummaries(): void {
  summaries.clear();
  seedSummaries();
  lastThreadsSyncAt = getState().lastSyncAt;
}

function onStoreChange(): void {
  const state = getState();
  const becameOnline = state.connection.status === "online" && lastConnection !== "online";
  lastConnection = state.connection.status;
  if (becameOnline) {
    // Every ready handshake starts a fresh baseline, before any list arrives.
    reseedSummaries();
  } else if (state.lastSyncAt !== lastThreadsSyncAt) {
    // The thread list loaded and may have healed a gap.
    reseedSummaries();
  } else {
    seedSummaries();
  }
  evaluateHosts();
}

function evaluateHosts(): void {
  if (!getNotificationSettings().hostOffline || !relayPresenceAvailable()) {
    hosts = null;
    return;
  }
  const next = presenceSnapshot();
  if (next === null) {
    hosts = null;
    return;
  }
  if (hosts !== null) {
    for (const draft of offlineTransitions(hosts, next, focusSnapshot())) queueDraft(draft);
  }
  hosts = next;
}

/**
 * Re-reads connect's machine list and the daemon's host list.
 *
 * The relay list is filtered by the server to the machines this device is
 * linked to, and the active machine's presence comes from the live transport,
 * not from this answer. The daemon host list is refreshed on the same poll so
 * a direct session's rows change when its hosts do.
 */
async function pollMachines(): Promise<void> {
  if (!getNotificationSettings().hostOffline) {
    machines = null;
    hosts = null;
    return;
  }
  const remote = getRemoteState();
  if (remote.user === null) {
    // A direct session has no relay list; the daemon's hosts are enough for
    // the rest of the UI, but the notifier has no presence signal to watch.
    machines = null;
    void refreshHosts();
    evaluateHosts();
    return;
  }
  if (getState().connection.status !== "online") {
    machines = null;
    hosts = null;
    return;
  }
  void refreshHosts();
  const client = currentConnectClient();
  if (!client) return;
  let list: MachineSummary[];
  try {
    list = await client.machines({ linked: true });
  } catch {
    // A relay that cannot answer says nothing about a host's presence.
    return;
  }
  if (getState().connection.status !== "online") {
    machines = null;
    hosts = null;
    return;
  }
  machines = list;
  evaluateHosts();
}

/**
 * Starts the watcher; safe to call more than once.
 *
 * `App` calls it on mount. The run stays alive for the life of the page: the
 * connection can drop and return, and the same queue keeps its place.
 */
export function startNotifications(): void {
  if (started) return;
  started = true;
  channel = notificationChannel();
  if (!channel) return;

  seedSummaries();
  lastConnection = getState().connection.status;
  lastThreadsSyncAt = getState().lastSyncAt;

  channel.onClick((threadId) => {
    if (threadId) openThread(threadId);
  });
  onAppEvent(handleEnvelope);

  // Host presence is the sidebar's signal: transport status for the active
  // host, connect's list for the others, and the daemon's hosts for a direct
  // session. Re-evaluate on every state change and poll connect for changes
  // it does not push.
  store.subscribe(onStoreChange);
  remoteStore.subscribe(evaluateHosts);
  evaluateHosts();

  if (typeof window !== "undefined") {
    const poll = () => void pollMachines();
    const hostTimer = window.setInterval(poll, HOST_POLL_MS);
    window.setTimeout(poll, 3_000);
    // Do not leave a timer behind when the page unloads in a test harness.
    window.addEventListener("pagehide", () => window.clearInterval(hostTimer), { once: true });
  }
}
