/**
 * When a notification is worth sending, and how bursts become one.
 *
 * This module is deliberately free of the DOM: the runtime asks it what a
 * protocol event means and when a queued message is due, and the tests drive
 * the same functions directly.
 */

import type { ApprovalKind, Event, ThreadId, ThreadSummary } from "@/gen";
import type { ConnectionStatus } from "@/lib/client/types";

import type { NotificationSettings } from "./settings";

export type NotificationKind = "approval" | "finished" | "host_offline";

/** One message the notifier may show. `key` coalesces per thread or host. */
export type NotificationDraft = {
  key: string;
  kind: NotificationKind;
  title: string;
  body: string;
  threadId: ThreadId | null;
};

/** What the person can already see, at the moment the decision is made. */
export type FocusSnapshot = {
  windowFocused: boolean;
  openThreadId: ThreadId | null;
  connection: ConnectionStatus;
};

/** A queued message and the moment it becomes due. */
export type PendingNotification = NotificationDraft & { dueAt: number };

export type NotificationQueue = Record<string, PendingNotification>;

/**
 * How long a thread's messages wait before one is shown.
 *
 * Ten approvals in a second still become one notification for that thread,
 * and the last thing the agent asked wins.
 */
export const COALESCE_MS = 900;

/** A thread with no loaded summary still gets a readable title. */
export function threadTitle(threads: Record<ThreadId, ThreadSummary>, threadId: ThreadId): string {
  return threads[threadId]?.title ?? "A thread";
}

/** The one-line reason an approval needs a person. */
export function approvalBody(kind: ApprovalKind, prompt: string): string {
  const text = prompt.trim();
  if (kind === "question") return text || "A question is waiting.";
  if (kind === "command") return text ? `Wants to run: ${text}` : "Wants to run a command.";
  if (kind === "file_change") return text ? `Wants to change: ${text}` : "Wants to change files.";
  return text || "Needs an approval.";
}

/** Whether the person is already looking at the thing the notification names. */
export function alreadyVisible(draft: NotificationDraft, focus: FocusSnapshot): boolean {
  if (!focus.windowFocused) return false;
  // A host going offline is shown in the hosts list and the status bar. The
  // person is in the app; do not also pull them out of it.
  if (draft.kind === "host_offline") return true;
  return draft.threadId !== null && focus.openThreadId === draft.threadId;
}

/** The draft for a protocol event, or `null` when it should stay silent. */
export function draftForEvent(
  event: Event,
  options: {
    threads: Record<ThreadId, ThreadSummary>;
    settings: NotificationSettings;
    focus: FocusSnapshot;
    /** The most recent error message for a thread, for "Failed: …". */
    lastError?: string | null;
  },
): NotificationDraft | null {
  const { threads, settings, focus } = options;

  if (event.type === "approval_requested") {
    if (!settings.approvals) return null;
    const draft: NotificationDraft = {
      key: threadKey(event.thread_id),
      kind: "approval",
      title: threadTitle(threads, event.thread_id),
      body: approvalBody(event.kind, event.prompt),
      threadId: event.thread_id,
    };
    return alreadyVisible(draft, focus) ? null : draft;
  }

  if (event.type === "turn_completed") {
    if (!settings.finishedTurns) return null;
    // A turn the person stopped is not news they need.
    if (event.status === "interrupted") return null;
    const draft: NotificationDraft = {
      key: threadKey(event.thread_id),
      kind: "finished",
      title: threadTitle(threads, event.thread_id),
      body:
        event.status === "failed"
          ? `Failed: ${options.lastError?.trim() || "the turn ended with an error."}`
          : "Finished: the turn is done.",
      threadId: event.thread_id,
    };
    return alreadyVisible(draft, focus) ? null : draft;
  }

  return null;
}

function threadKey(threadId: ThreadId): string {
  return `thread:${threadId}`;
}

/** Adds a draft to the queue, keeping the burst's original deadline. */
export function enqueue(
  queue: NotificationQueue,
  draft: NotificationDraft,
  now: number,
  windowMs = COALESCE_MS,
): NotificationQueue {
  const existing = queue[draft.key];
  return {
    ...queue,
    [draft.key]: { ...draft, dueAt: existing?.dueAt ?? now + windowMs },
  };
}

/** Everything whose deadline passed, plus the queue that is left. */
export function dueNotifications(
  queue: NotificationQueue,
  now: number,
): { ready: NotificationDraft[]; queue: NotificationQueue } {
  const ready: NotificationDraft[] = [];
  const remaining: NotificationQueue = {};
  for (const [key, pending] of Object.entries(queue)) {
    if (pending.dueAt <= now) {
      ready.push({
        key: pending.key,
        kind: pending.kind,
        title: pending.title,
        body: pending.body,
        threadId: pending.threadId,
      });
    } else {
      remaining[key] = pending;
    }
  }
  return { ready, queue: remaining };
}

/** The earliest deadline in the queue, or `null` when it is empty. */
export function nextDeadline(queue: NotificationQueue): number | null {
  let earliest: number | null = null;
  for (const pending of Object.values(queue)) {
    if (earliest === null || pending.dueAt < earliest) earliest = pending.dueAt;
  }
  return earliest;
}

/** What the hosts poll knows about one host. */
export type HostState = { name: string; online: boolean };

/**
 * Hosts that went from online to offline between two polls.
 *
 * The first poll is a baseline and never notifies: a host that was already
 * offline at startup is not news. A disconnected client learns nothing, so it
 * is silent too.
 */
export function offlineTransitions(
  previous: Record<string, HostState> | null,
  next: Record<string, HostState>,
  focus: FocusSnapshot,
): NotificationDraft[] {
  if (previous === null || focus.connection !== "online") return [];
  const drafts: NotificationDraft[] = [];
  for (const [hostId, host] of Object.entries(next)) {
    const before = previous[hostId];
    if (!before?.online || host.online) continue;
    const draft: NotificationDraft = {
      key: `host:${hostId}`,
      kind: "host_offline",
      title: host.name,
      body: "Host offline: h4rn can no longer reach it.",
      threadId: null,
    };
    if (!alreadyVisible(draft, focus)) drafts.push(draft);
  }
  return drafts;
}
