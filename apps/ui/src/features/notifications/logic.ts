/**
 * When a notification is worth sending, and how bursts become one.
 *
 * This module is deliberately free of the DOM: the runtime asks it what a
 * protocol event means and when a queued message is due, and the tests drive
 * the same functions directly.
 */

import type {
  ApprovalKind,
  Event,
  HostSummary,
  MachineSummary,
  ThreadId,
  ThreadSummary,
  TurnStatus,
} from "@/gen";
import type { ConnectionStatus, EventEnvelope } from "@/lib/client/types";

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

/** A queued message, when its quiet period began, and when it becomes due. */
export type PendingNotification = NotificationDraft & { firstAt: number; dueAt: number };

export type NotificationQueue = Record<string, PendingNotification>;

/**
 * How long a thread has to stay quiet before one message is shown.
 *
 * A burst does not wait for a fixed window to close: every new event starts
 * the quiet period again, and the last thing the agent asked wins.
 */
export const COALESCE_MS = 1_000;

/**
 * However busy a thread is, its message is announced within this long of the
 * first event. The quiet period cannot postpone it forever.
 */
export const COALESCE_MAX_MS = 3_000;

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

/**
 * The draft for a live protocol event, or `null` when it should stay silent.
 *
 * Replayed events are history and never produce a notification; the caller
 * keeps applying them to its own state.
 */
export function draftForEnvelope(
  envelope: EventEnvelope,
  options: {
    threads: Record<ThreadId, ThreadSummary>;
    settings: NotificationSettings;
    focus: FocusSnapshot;
  },
): NotificationDraft | null {
  if (envelope.replayed) return null;
  return draftForEvent(envelope.event, options);
}

/** The draft for a live protocol event, or `null` when it should stay silent. */
export function draftForEvent(
  event: Event,
  options: {
    threads: Record<ThreadId, ThreadSummary>;
    settings: NotificationSettings;
    focus: FocusSnapshot;
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

  return null;
}

/** What the last completed turn did, for the finished/failed wording. */
export type LastTurn = { status: TurnStatus; error: string | null };

/**
 * The body for a completed turn.
 *
 * The daemon is authoritative about when a thread is Done; this only says how
 * it got there. Limits are stops, not finishes.
 */
export function finishedBody(lastTurn: LastTurn | null): string {
  if (!lastTurn) return "Finished: the turn is done.";
  switch (lastTurn.status) {
    case "failed":
      return `Failed: ${lastTurn.error?.trim() || "the turn ended with an error."}`;
    case "token_limit":
      return "Stopped: the agent hit its token limit.";
    case "turn_limit":
      return "Stopped: the agent hit its turn limit.";
    default:
      return "Finished: the turn is done.";
  }
}

/**
 * Whether this summary update is the authoritative end of a turn.
 *
 * `done` is the ordinary landing. `failure` is a thread that raises its hand
 * while still settling (for example a host that went unreachable at the same
 * time); the turn is over even though the tab has not reached Done yet.
 */
export type FinishedTransition = "done" | "failure";

export function finishedTransition(
  previous: Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state"> | undefined,
  next: Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state">,
): FinishedTransition | null {
  if (!previous) return null;
  if (next.tab === "done" && previous.tab !== "done") return "done";
  if (
    !previous.needs_attention &&
    next.needs_attention &&
    next.agent_state === "error" &&
    next.tab !== "done"
  ) {
    return "failure";
  }
  return null;
}

/**
 * A notification for a thread whose turn has just ended.
 *
 * This follows the daemon's thread summary, not the raw `turn_completed`
 * event: a turn that completes while the next queued turn starts in the same
 * update stays Working and says nothing, and an interrupted turn that lands
 * in Done still notifies. A failure announced while the thread settles is not
 * repeated when it reaches Done.
 */
export function finishedDraft(
  previous: Pick<ThreadSummary, "tab" | "needs_attention" | "agent_state"> | undefined,
  next: ThreadSummary,
  options: {
    settings: NotificationSettings;
    focus: FocusSnapshot;
    lastTurn: LastTurn | null;
    /** A failure for this same turn was already announced. */
    announcedFailure?: boolean;
  },
): NotificationDraft | null {
  const transition = finishedTransition(previous, next);
  if (!transition) return null;
  if (!options.settings.finishedTurns) return null;
  if (transition === "done" && options.announcedFailure) return null;
  if (transition === "failure" && !isFailureStatus(options.lastTurn)) return null;
  const draft: NotificationDraft = {
    key: threadKey(next.thread_id),
    kind: "finished",
    title: next.title,
    body: finishedBody(options.lastTurn),
    threadId: next.thread_id,
  };
  return alreadyVisible(draft, options.focus) ? null : draft;
}

function isFailureStatus(lastTurn: LastTurn | null): boolean {
  return (
    lastTurn !== null &&
    (lastTurn.status === "failed" ||
      lastTurn.status === "token_limit" ||
      lastTurn.status === "turn_limit")
  );
}

function threadKey(threadId: ThreadId): string {
  return `thread:${threadId}`;
}

/**
 * Adds a draft to the queue and restarts its quiet period.
 *
 * `dueAt` is the earlier of "quiet since this event" and the burst's hard
 * deadline, so a non-stop stream of events cannot postpone a notification
 * forever.
 */
export function enqueue(
  queue: NotificationQueue,
  draft: NotificationDraft,
  now: number,
  windowMs = COALESCE_MS,
  maxMs = COALESCE_MAX_MS,
): NotificationQueue {
  const existing = queue[draft.key];
  const firstAt = existing?.firstAt ?? now;
  const dueAt = Math.min(now + windowMs, firstAt + maxMs);
  return {
    ...queue,
    [draft.key]: { ...draft, firstAt, dueAt },
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

/** Everything the sidebar's presence rows read, in one snapshot. */
export type PresenceSource = {
  connection: ConnectionStatus;
  signedIn: boolean;
  /** Connect machines, when this device is signed in to a relay. */
  machines: MachineSummary[];
  activeMachineId: string | null;
  /** Hosts the connected daemon reported in its hello. */
  hosts: Record<string, HostSummary>;
  localHostId: string | null;
};

/**
 * The same presence the sidebar shows, shaped for the notifier.
 *
 * The live transport is authoritative for the host this client is connected
 * to, so the relay can never report our own working host as offline. Other
 * machines keep connect's presence; a direct session uses the daemon's host
 * list. With no working transport the client learns nothing and returns
 * `null`, which resets the baseline instead of inventing a transition.
 */
export function hostPresence(source: PresenceSource): Record<string, HostState> | null {
  if (source.connection !== "online") return null;
  const next: Record<string, HostState> = {};

  if (source.signedIn && source.machines.length > 0) {
    for (const machine of source.machines) {
      const online =
        machine.id === source.activeMachineId ? source.connection === "online" : machine.online;
      const hosts =
        machine.hosts.length > 0 ? machine.hosts : [{ id: machine.id, name: machine.name }];
      for (const host of hosts) next[host.id] = { name: host.name, online };
    }
  } else {
    for (const host of Object.values(source.hosts)) {
      const active = source.localHostId !== null && host.host_id === source.localHostId;
      next[host.host_id] = { name: host.name, online: active ? true : host.online };
    }
  }

  return Object.keys(next).length > 0 ? next : null;
}

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
