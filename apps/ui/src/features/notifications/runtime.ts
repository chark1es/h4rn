/**
 * Watches the live stream and turns a few events into OS notifications.
 *
 * It is intentionally thin: `logic.ts` decides what and when, `channel.ts`
 * knows how to show it, and this module owns the wiring — the queue timer, the
 * focus snapshot, the host poll and the click that opens a thread.
 */

import { currentConnectClient } from "@/features/remote/session";
import type { Event, MachineSummary, ThreadId } from "@/gen";
import type { EventEnvelope } from "@/lib/client/types";
import { getState, onAppEvent, openThread } from "@/lib/store/appStore";

import {
  type NotificationChannel,
  type NotificationPermissionState,
  notificationChannel,
  notificationId,
} from "./channel";
import {
  alreadyVisible,
  draftForEvent,
  dueNotifications,
  enqueue,
  type FocusSnapshot,
  type HostState,
  type NotificationDraft,
  type NotificationKind,
  type NotificationQueue,
  nextDeadline,
  offlineTransitions,
} from "./logic";
import { getNotificationSettings, type NotificationSettings } from "./settings";

/** Replayed events after a reconnect are history, not news. */
const RECONNECT_GRACE_MS = 2_000;

/** How often the paired-machine list is re-read for the host-offline switch. */
const HOST_POLL_MS = 30_000;

/** How long an error remains the explanation for a failed turn. */
const ERROR_MEMORY_MS = 2 * 60_000;

let started = false;
let channel: NotificationChannel | null = null;
let queue: NotificationQueue = {};
let timer: ReturnType<typeof setTimeout> | null = null;
let flushing = false;
/** `null` until the first poll establishes a baseline. */
let hosts: Record<string, HostState> | null = null;
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

function withinReconnectGrace(): boolean {
  const { connection } = getState();
  if (connection.status !== "online") return true;
  return (
    connection.lastConnectedAt !== null &&
    Date.now() - connection.lastConnectedAt < RECONNECT_GRACE_MS
  );
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
    if (permission !== "granted") return;
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

function queueDraft(draft: NotificationDraft): void {
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

function handleEvent({ event }: EventEnvelope): void {
  if (event.type === "error") rememberError(event);
  if (withinReconnectGrace()) return;
  const draft = draftForEvent(event, {
    threads: getState().threads,
    settings: getNotificationSettings(),
    focus: focusSnapshot(),
    lastError:
      event.type === "turn_completed" && event.status === "failed"
        ? lastErrorFor(event.thread_id)
        : null,
  });
  if (draft) queueDraft(draft);
}

function snapshotMachines(machines: MachineSummary[]): Record<string, HostState> {
  const next: Record<string, HostState> = {};
  for (const machine of machines) {
    next[machine.id] = { name: machine.name, online: machine.online };
  }
  return next;
}

async function pollHosts(): Promise<void> {
  const settings = getNotificationSettings();
  if (!settings.hostOffline || getState().connection.status !== "online") {
    hosts = null;
    return;
  }
  const client = currentConnectClient();
  if (!client) {
    hosts = null;
    return;
  }
  let machines: MachineSummary[];
  try {
    machines = await client.machines();
  } catch {
    // A relay that cannot answer says nothing about a host's presence.
    return;
  }
  if (getState().connection.status !== "online") {
    hosts = null;
    return;
  }
  const next = snapshotMachines(machines);
  // The first poll after a sign-in is the baseline, never a transition.
  if (hosts !== null) {
    for (const draft of offlineTransitions(hosts, next, focusSnapshot())) queueDraft(draft);
  }
  hosts = next;
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

  channel.onClick((threadId) => {
    if (threadId) openThread(threadId);
  });
  onAppEvent(handleEvent);

  if (typeof window !== "undefined") {
    const poll = () => void pollHosts();
    const hostTimer = window.setInterval(poll, HOST_POLL_MS);
    window.setTimeout(poll, 3_000);
    // Do not leave a timer behind when the page unloads in a test harness.
    window.addEventListener("pagehide", () => window.clearInterval(hostTimer), { once: true });
  }
}
