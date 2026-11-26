/**
 * When a notification is worth sending, and how bursts become one.
 *
 * This module is deliberately free of the DOM: the runtime asks it what a
 * protocol event means and when a queued message is due, and the tests drive
 * the same functions directly.
 *
 * A notification is worded in the active language when it is drafted, like a
 * toast: the system shows it once and h4rn cannot relabel it afterwards. The
 * thread title, the agent's prompt and a turn's error are shown as written.
 */

import type {
  ApprovalKind,
  Event,
  MachineSummary,
  ThreadId,
  ThreadSummary,
  TurnStatus,
} from "@/gen";
import type { ConnectionStatus, EventEnvelope } from "@/lib/client/types";
import { i18n } from "@/lib/i18n";

import type { NotificationSettings } from "./settings";

export type NotificationKind = "approval" | "finished" | "host_offline";

/** One message the notifier may show. `key` coalesces per thread or host. */
export type NotificationDraft = {
  key: string;
  kind: NotificationKind;
  title: string;
  body: string;
  threadId: ThreadId | null;
  /** The approval this draft announces, so its resolution can cancel it. */
  approvalId?: string;
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
  return threads[threadId]?.title ?? i18n.t("shell:notifications.untitledThread");
}

/** The one-line reason an approval needs a person. */
export function approvalBody(kind: ApprovalKind, prompt: string): string {
  const text = prompt.trim();
  if (kind === "question") return text || i18n.t("shell:notifications.question");
  if (kind === "command") {
    return text
      ? i18n.t("shell:notifications.wantsToRun", { command: text })
      : i18n.t("shell:notifications.wantsToRunCommand");
  }
  if (kind === "file_change") {
    return text
      ? i18n.t("shell:notifications.wantsToChange", { files: text })
      : i18n.t("shell:notifications.wantsToChangeFiles");
  }
  return text || i18n.t("shell:notifications.needsApproval");
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
    /** A subagent child thread: its parent mirrors the approval already. */
    childThread?: boolean;
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
    /** A subagent child thread: its parent mirrors the approval already. */
    childThread?: boolean;
  },
): NotificationDraft | null {
  const { threads, settings, focus } = options;

  if (event.type === "approval_requested") {
    if (!settings.approvals) return null;
    // A child thread's approval is mirrored on the parent with a namespaced
    // id, so announcing the child's own copy would notify twice.
    if (options.childThread) return null;
    const draft: NotificationDraft = {
      key: threadKey(event.thread_id),
      kind: "approval",
      title: threadTitle(threads, event.thread_id),
      body: approvalBody(event.kind, event.prompt),
      threadId: event.thread_id,
      approvalId: event.approval_id,
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
  if (!lastTurn) return i18n.t("shell:notifications.finished");
  switch (lastTurn.status) {
    case "failed": {
      const error = lastTurn.error?.trim();
      return error
        ? i18n.t("shell:notifications.failed", { error })
        : i18n.t("shell:notifications.failedUnknown");
    }
    case "interrupted":
      return i18n.t("shell:notifications.interrupted");
    case "token_limit":
      return i18n.t("shell:notifications.tokenLimit");
    case "turn_limit":
      return i18n.t("shell:notifications.turnLimit");
    default:
      return i18n.t("shell:notifications.finished");
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
    /** A live (not replayed) `turn_completed` for the current turn was seen. */
    liveCompletion?: boolean;
    /**
     * The summary says the person just placed the thread in this tab.
     *
     * Another client's manual move to Done arrives as a plain summary and is
     * not the end of a turn, even when this client still remembers a live
     * completion. The same flag is also set for the hand-set Working pin and
     * for a manual move to Working; those are per-thread overrides, not a
     * finished turn, and must never silence a failure alert.
     */
    placedByUser?: boolean;
  },
): NotificationDraft | null {
  // A child thread finishes with its subagent, and the parent's mirrored
  // approval already covers the moment a person is needed.
  if (next.parent_thread_id) return null;
  const transition = finishedTransition(previous, next);
  if (!transition) return null;
  // Only a Done transition can be the placement itself. A failure raises its
  // hand while the thread is still Working (possibly because the person pinned
  // it there), and staying quiet would hide the one thing worth saying.
  if (transition === "done" && options.placedByUser) return null;
  if (!options.settings.finishedTurns) return null;
  // The summary moving to Done is not news by itself: a different client may
  // have moved the thread, or the list may have healed a gap after a failed
  // subscribe. Only a live completion for this thread's current turn makes
  // this the end of something the person was waiting for.
  if (!options.liveCompletion) return null;
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

/**
 * A notification for a turn that reported a failure outcome on the live stream.
 *
 * This is the turn itself, not a tab placement: a thread the person already
 * moved to Done still has a turn that can fail, and the summary may never
 * raise `needs_attention` because the thread is past Working. The outcome
 * event is the one signal that is independent of where the tab sits, so a
 * real failure alerts even there. One failure is announced once per turn; the
 * summary path and this one share `announcedFailure`.
 */
export function failedOutcomeDraft(
  threadId: ThreadId,
  options: {
    threads: Record<ThreadId, ThreadSummary>;
    settings: NotificationSettings;
    focus: FocusSnapshot;
    lastTurn: LastTurn | null;
    /** A failure for this same turn was already announced. */
    announcedFailure?: boolean;
    /** A subagent child thread: its parent mirrors the moment. */
    childThread?: boolean;
    /** The title from the summary that named a deferred failure's thread. */
    title?: string;
  },
): NotificationDraft | null {
  if (options.childThread) return null;
  if (!options.settings.finishedTurns) return null;
  // Only a real failure. Token and turn limits are stops the daemon may be
  // resuming from, and their attention state still comes from the summary.
  if (options.lastTurn?.status !== "failed") return null;
  if (options.announcedFailure) return null;
  const draft: NotificationDraft = {
    key: threadKey(threadId),
    kind: "finished",
    title: options.title ?? threadTitle(options.threads, threadId),
    body: finishedBody(options.lastTurn),
    threadId,
  };
  return alreadyVisible(draft, options.focus) ? null : draft;
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
        ...(pending.approvalId === undefined ? {} : { approvalId: pending.approvalId }),
      });
    } else {
      remaining[key] = pending;
    }
  }
  return { ready, queue: remaining };
}

/** The earliest deadline in the queue, or `null` when it is empty. */
export function nextDeadline(
  queue: NotificationQueue,
  skip?: (pending: PendingNotification) => boolean,
): number | null {
  let earliest: number | null = null;
  for (const pending of Object.values(queue)) {
    if (skip?.(pending)) continue;
    if (earliest === null || pending.dueAt < earliest) earliest = pending.dueAt;
  }
  return earliest;
}

/** What the hosts poll knows about one host. */
export type HostState = { name: string; online: boolean };

/** Everything the notifier's presence rows read, in one snapshot. */
export type PresenceSource = {
  connection: ConnectionStatus;
  signedIn: boolean;
  /**
   * Connect machines this device is linked to, as reported by the relay. Only
   * the relay has an independent reachability signal, so a direct session has
   * no presence to report.
   */
  machines: MachineSummary[];
  /**
   * The machine the live transport is actually reaching.
   *
   * It comes from the peer, never from the account's device rows: a relay
   * link names the machine its URL was minted for (pairing writes the relay's
   * `machine_devices` id, a manual pick writes the chosen machine), and a
   * direct connection names the connect machine the daemon owns, which `hello`
   * reports from the daemon's own signed-in session. When neither exists the
   * id is `null` and the local host name is the fallback.
   */
  connectedMachineId: string | null;
  /**
   * Name of the host this client is connected to (`hello`'s local host id
   * resolved through the daemon's host list).
   *
   * It cannot be the id: connect mints its own machine and host ids when a
   * machine registers, so the relay's `MachineHost.id` never equals the id
   * `hello` reports. The name is the one field both lists agree on, and it is
   * how the sidebar pairs them (`localByName`).
   */
  localHostName: string | null;
};

/**
 * The same presence the sidebar shows, shaped for the notifier.
 *
 * The live transport is authoritative for the machine this client is
 * connected to, so the relay can never report our own working machine as
 * offline. Which machine that is comes from the peer: a verified machine id
 * from the transport (a relay link's machine, or the connect machine the
 * daemon reports in `hello` on a direct connection). That id keeps every host
 * of the machine online, so a Windows host and its WSL siblings are not split
 * by a stale relay answer.
 * Only when there is no such id does the local host's name stand in, because
 * connect mints its own machine and host ids; a name match is then narrow, per
 * host, so working in one WSL distribution does not mark the others online.
 * A name shared by two machines cannot say which one the transport reached,
 * so those hosts are left out of presence entirely instead of feeding the
 * relay's (possibly stale) answer for a machine in active use.
 *
 * Other linked machines keep connect's presence. With no relay list there is
 * no independent signal, so this returns `null` (direct sessions hide the row
 * rather than show a switch that can never fire).
 */
export function hostPresence(source: PresenceSource): Record<string, HostState> | null {
  if (source.connection !== "online") return null;
  if (!source.signedIn || source.machines.length === 0) return null;
  const localHostName = source.localHostName?.toLowerCase() ?? null;

  const known = (machineId: string | null) =>
    machineId !== null && source.machines.some((machine) => machine.id === machineId)
      ? machineId
      : null;
  let connectedMachineId = known(source.connectedMachineId);
  /** The one host a narrow name match vouches for, if that is how we matched. */
  let namedHostId: string | null = null;
  /** Hosts an ambiguous name match can no longer speak for. */
  const suppressed = new Set<string>();
  if (connectedMachineId === null && localHostName !== null) {
    const matches: Array<{ machineId: string; hostId: string }> = [];
    for (const machine of source.machines) {
      for (const host of machineHosts(machine)) {
        if (host.name.toLowerCase() === localHostName) {
          matches.push({ machineId: machine.id, hostId: host.id });
        }
      }
    }
    const match = matches.length === 1 ? matches[0] : undefined;
    if (match) {
      connectedMachineId = match.machineId;
      namedHostId = match.hostId;
    } else {
      // Two machines answer to the same name: neither can be trusted as the
      // one the transport reached. Leaving them out of presence means no
      // offline transition can ever be raised for them.
      for (const ambiguous of matches) suppressed.add(ambiguous.hostId);
    }
  }

  const next: Record<string, HostState> = {};
  for (const machine of source.machines) {
    const hosts = machineHosts(machine);
    const connectedMachine = machine.id === connectedMachineId;
    for (const host of hosts) {
      if (suppressed.has(host.id)) continue;
      // A verified machine id vouches for the machine's hosts as a group. A
      // name-only match can only vouch for the one host it named.
      const local = connectedMachine && (namedHostId === null || host.id === namedHostId);
      next[host.id] = { name: host.name, online: local ? true : machine.online };
    }
  }

  return Object.keys(next).length > 0 ? next : null;
}

/** A machine's addressable hosts, or its own row when none were reported. */
function machineHosts(machine: MachineSummary): MachineSummary["hosts"] {
  return machine.hosts.length > 0
    ? machine.hosts
    : [{ id: machine.id, name: machine.name, kind: "host", distro: null }];
}

/**
 * Whether a queued host alert is still true at delivery time.
 *
 * The queue waits out the quiet period, so the host may have come back (or
 * this client may have lost its transport) before the message is due. In that
 * case the alert is stale and must be dropped.
 */
export function stillOffline(
  draft: NotificationDraft,
  presence: Record<string, HostState> | null,
  connection: ConnectionStatus,
): boolean {
  if (draft.kind !== "host_offline") return true;
  if (connection !== "online" || presence === null) return false;
  const hostId = draft.key.startsWith("host:") ? draft.key.slice("host:".length) : "";
  const host = presence[hostId];
  return host !== undefined && !host.online;
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
      body: i18n.t("shell:notifications.hostOffline"),
      threadId: null,
    };
    if (!alreadyVisible(draft, focus)) drafts.push(draft);
  }
  return drafts;
}

/**
 * Drops a thread's queued approval when the thread turns out to be a child.
 *
 * The approval can arrive before the summary that names its parent. The parent
 * mirrors a child's approval, so the queued child draft is withdrawn rather
 * than delivered once the wire says it is a child.
 */
export function dropThreadApproval(
  queue: NotificationQueue,
  threadId: ThreadId,
): NotificationQueue {
  const key = threadKey(threadId);
  const pending = queue[key];
  if (pending?.kind !== "approval") return queue;
  const next = { ...queue };
  delete next[key];
  return next;
}

/**
 * Drops a thread's queued approval when that exact approval is answered.
 *
 * The queue is keyed per thread, so a newer approval that re-queued the same
 * key keeps its place: only a resolution of the approval the draft names
 * cancels it.
 */
export function dropResolvedApproval(
  queue: NotificationQueue,
  threadId: ThreadId,
  approvalId: string,
): NotificationQueue {
  const key = threadKey(threadId);
  const pending = queue[key];
  if (!pending || pending.approvalId !== approvalId) return queue;
  const next = { ...queue };
  delete next[key];
  return next;
}
