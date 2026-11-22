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
import type { Event, MachineSummary, ThreadId, ThreadSummary } from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";
import { getState, onAppEvent, openThread, store } from "@/lib/store/appStore";

import {
  type NotificationChannel,
  type NotificationPermissionState,
  notificationChannel,
  notificationId,
} from "./channel";
import {
  alreadyVisible,
  draftForEnvelope,
  dueNotifications,
  enqueue,
  type FocusSnapshot,
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
} from "./logic";
import { getNotificationSettings, type NotificationSettings } from "./settings";

/** How often connect's machine list is re-read for host presence. */
const HOST_POLL_MS = 30_000;

/** How long an error remains the explanation for a failed turn. */
const ERROR_MEMORY_MS = 2 * 60_000;

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
/** What each thread's most recent turn did, until the next turn starts. */
const lastTurns = new Map<ThreadId, LastTurn>();
/** Threads whose current turn's failure was already announced. */
const announcedFailures = new Set<ThreadId>();
const recentErrors = new Map<ThreadId, { message: string; at: number }>();

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

function rememberError(event: Extract<Event, { type: "error" }>): void {
  if (!event.thread_id) return;
  recentErrors.set(event.thread_id, { message: event.message, at: Date.now() });
}

function lastErrorFor(threadId: ThreadId): string | null {
  const remembered = recentErrors.get(threadId);
  if (!remembered) return null;
  if (Date.now() - remembered.at > ERROR_MEMORY_MS) {
    recentErrors.delete(threadId);
    return null;
  }
  return remembered.message;
}

function startTurn(threadId: ThreadId): void {
  // A new attempt is not explained by the previous attempt's failure.
  recentErrors.delete(threadId);
  lastTurns.delete(threadId);
  announcedFailures.delete(threadId);
}

function completeTurn(event: Extract<Event, { type: "turn_completed" }>): void {
  lastTurns.set(event.thread_id, {
    status: event.status,
    error: lastErrorFor(event.thread_id),
  });
}

function handleEnvelope(envelope: EventEnvelope): void {
  const { event } = envelope;

  // Memory is kept even for replayed history: the notifier still has to
  // explain a Done transition that only finished arriving after a reconnect.
  if (event.type === "error") rememberError(event);
  if (event.type === "turn_started") startTurn(event.thread_id);
  if (event.type === "turn_completed") completeTurn(event);

  if (event.type === "thread_upserted") {
    const previous = summaries.get(event.thread_id);
    const transition = finishedTransition(previous, event);
    summaries.set(event.thread_id, {
      tab: event.tab,
      needs_attention: event.needs_attention,
      agent_state: event.agent_state,
    });
    if (envelope.replayed) return;
    const draft = finishedDraft(previous, event, {
      settings: getNotificationSettings(),
      focus: focusSnapshot(),
      lastTurn: lastTurns.get(event.thread_id) ?? null,
      announcedFailure: announcedFailures.has(event.thread_id),
    });
    queueDraft(draft);
    if (transition === "failure" && draft !== null) announcedFailures.add(event.thread_id);
    return;
  }

  if (event.type !== "approval_requested") return;
  queueDraft(
    draftForEnvelope(envelope, {
      threads: getState().threads,
      settings: getNotificationSettings(),
      focus: focusSnapshot(),
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
    machines: machines ?? remote.machines,
    activeMachineId: remote.activeMachineId,
    hosts: state.hosts,
    localHostId: state.localHostId,
  });
}

/**
 * Adopts summaries that arrived through a list response, not an event.
 *
 * `startNotifications` runs before the first thread list loads, so without
 * this a thread that was already Done at startup would look brand new when it
 * later finished a turn, and its first transition would be ignored.
 */
function seedSummaries(): void {
  for (const [threadId, summary] of Object.entries(getState().threads)) {
    if (summaries.has(threadId)) continue;
    summaries.set(threadId, {
      tab: summary.tab,
      needs_attention: summary.needs_attention,
      agent_state: summary.agent_state,
    });
  }
}

function onStoreChange(): void {
  seedSummaries();
  evaluateHosts();
}

function evaluateHosts(): void {
  if (!getNotificationSettings().hostOffline) {
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
 * Re-reads connect's machine list. The list is device-scoped, and the active
 * machine's presence comes from the live transport, not from this answer.
 */
async function pollMachines(): Promise<void> {
  if (!getNotificationSettings().hostOffline) {
    machines = null;
    hosts = null;
    return;
  }
  const remote = getRemoteState();
  if (remote.user === null) {
    // A direct session has no relay list; the daemon's hosts are enough.
    machines = null;
    evaluateHosts();
    return;
  }
  if (getState().connection.status !== "online") {
    machines = null;
    hosts = null;
    return;
  }
  const client = currentConnectClient();
  if (!client) return;
  let list: MachineSummary[];
  try {
    list = await client.machines();
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
