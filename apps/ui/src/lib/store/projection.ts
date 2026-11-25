import type {
  AgentState,
  ApprovalDecision,
  ApprovalKind,
  ApprovalOption,
  Attachment,
  Event,
  FsTurnChanges,
  GuestRequest,
  HarnessKind,
  HostId,
  MessageRole,
  PlanStep,
  SubagentId,
  SubagentState,
  ToolCall,
  TurnId,
  TurnStatus,
  Usage,
} from "@/gen";

export type TurnStatusOrRunning = TurnStatus | "running";

export type MessageItem = {
  kind: "message";
  itemId: string;
  role: MessageRole;
  text: string;
  attachments: Attachment[];
  streaming: boolean;
};

export type ToolItem = {
  kind: "tool";
  itemId: string;
  toolCall: ToolCall;
};

export type ApprovalItem = {
  kind: "approval";
  itemId: string;
  approvalId: string;
  approvalKind: ApprovalKind;
  prompt: string;
  options: ApprovalOption[];
};

export type LimitItem = {
  kind: "limit";
  itemId: string;
  resetAt: string | null;
  autoResume: boolean;
};

export type ErrorItem = {
  kind: "error";
  itemId: string;
  message: string;
  hint: string | null;
  retryable: boolean;
};

export type DelegatedItem = {
  kind: "delegated";
  itemId: string;
  subagentId: SubagentId;
  threadId: string | null;
  task: string;
  harness: HarnessKind;
  host: HostId;
  model: string;
  state: SubagentState;
  agentState: AgentState;
};

export type PlanItem = {
  kind: "plan";
  itemId: string;
  /** Every step, in the agent's order; a new plan replaces the whole list. */
  entries: PlanStep[];
};

/** A quiet line where one provider stopped and the next began. */
export type HarnessSwitchItem = {
  kind: "harness_switch";
  itemId: string;
  from: HarnessKind;
  to: HarnessKind;
};

export type TurnItem =
  | MessageItem
  | ToolItem
  | ApprovalItem
  | LimitItem
  | ErrorItem
  | DelegatedItem
  | PlanItem
  | HarnessSwitchItem;

export type StreamTurn = {
  id: TurnId;
  startedAt: string;
  status: TurnStatusOrRunning;
  items: TurnItem[];
  usage: Usage | null;
  /** Files this turn changed, when the daemon was able to snapshot them. */
  changes: FsTurnChanges | null;
  /**
   * True for a turn that came from an imported conversation.
   *
   * Its messages carry the time they were written there, and none of them is
   * live: an imported thread has no session until the person writes again.
   */
  imported?: boolean;
  /**
   * The daemon turn a user-message cue really belongs to.
   *
   * A live user message is shown as its own row with a `cue:` id, while the
   * agent's work is a second row with the real turn id. Imported history has
   * no such second row, so the cue keeps the real id for a fork to name.
   *
   * A cue with no `forkTurnId` is a prompt this projection could not tie to a
   * turn — an older journal's queued prompt filed under the turn it waited
   * behind — and no fork is offered from it.
   */
  forkTurnId?: TurnId;
  /**
   * Harness that ran this turn, when the daemon reported one.
   *
   * A thread can switch harness between turns, so a turn keeps its own
   * provider instead of borrowing the summary's current one.
   */
  harness?: HarnessKind;
  /**
   * True for a synthetic row that only carries a quiet divider.
   *
   * It has no agent header, live line or turn actions: a harness switch landed
   * between turns, so it belongs between their rows, not inside either.
   */
  divider?: boolean;
};

export type PendingMessage = {
  id: string;
  text: string;
  attachments: Attachment[];
  at: string;
  sending: boolean;
};

export type ApprovalState = {
  status: "pending" | "sending" | "resolved" | "error";
  decision?: ApprovalDecision;
};

export type ThreadLog = {
  turns: StreamTurn[];
  nextSeq: number;
  loaded: boolean;
  loading: boolean;
  approvals: Record<string, ApprovalState>;
  pending: PendingMessage[];
  /** Guest messages waiting for this owner, oldest first. */
  guestRequests: GuestRequest[];
  /**
   * Turns an edit removed from this log.
   *
   * A rewind cuts at one cue and drops every turn from there on. An event for
   * one of those turns that arrives afterwards — a steer whose write was still
   * in flight — must not reopen a turn the person already removed.
   */
  cutTurns: TurnId[];
};

export function createThreadLog(): ThreadLog {
  return {
    turns: [],
    nextSeq: 0,
    loaded: false,
    loading: false,
    approvals: {},
    pending: [],
    guestRequests: [],
    cutTurns: [],
  };
}

function cloneLog(log: ThreadLog): ThreadLog {
  return {
    turns: log.turns.map((turn) => ({ ...turn, items: [...turn.items] })),
    nextSeq: log.nextSeq,
    loaded: log.loaded,
    loading: log.loading,
    approvals: { ...log.approvals },
    pending: [...log.pending],
    guestRequests: [...log.guestRequests],
    cutTurns: [...log.cutTurns],
  };
}

/**
 * Whether a turn is one an agent really ran.
 *
 * A harness-switch divider and a user-message cue are synthetic rows: neither
 * has an agent turn behind it, so neither can own a turn scope, a change set or
 * a receipt.
 */
export function isRealTurn(turn: StreamTurn): boolean {
  return !turn.divider && !turn.id.startsWith("cue:user:");
}

/** The newest turn an agent really ran, or `null` before one did. */
export function latestRealTurn(turns: readonly StreamTurn[] | undefined): StreamTurn | null {
  if (!turns) return null;
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    const turn = turns[index];
    if (turn && isRealTurn(turn)) return turn;
  }
  return null;
}

function latestRunningTurn(log: ThreadLog): StreamTurn | null {
  for (let index = log.turns.length - 1; index >= 0; index -= 1) {
    const turn = log.turns[index];
    if (turn && turn.status === "running") return turn;
  }
  return null;
}

/** A row that shows a person's prompt, not the daemon turn behind it. */
function isCue(turn: StreamTurn): boolean {
  return turn.id.startsWith("cue:");
}

/**
 * The newest row that is a daemon turn, skipping the prompt cues.
 *
 * A replayed error or limit names no turn, so it belongs to the turn it
 * followed. When that turn is already settled, the last cue would otherwise
 * take the row and show the person's prompt as an agent-styled block.
 */
function lastDaemonTurn(log: ThreadLog): StreamTurn | null {
  for (let index = log.turns.length - 1; index >= 0; index -= 1) {
    const turn = log.turns[index];
    if (turn && !isCue(turn)) return turn;
  }
  return null;
}

function findTurn(log: ThreadLog, turnId: TurnId): StreamTurn | null {
  return log.turns.find((turn) => turn.id === turnId) ?? null;
}

function ensureTurn(log: ThreadLog, turnId: TurnId, startedAt: string): StreamTurn {
  const existing = findTurn(log, turnId);
  if (existing) return existing;
  const turn: StreamTurn = {
    id: turnId,
    startedAt,
    status: "running",
    items: [],
    usage: null,
    changes: null,
  };
  log.turns.push(turn);
  return turn;
}

function messageExists(log: ThreadLog, itemId: string): boolean {
  return log.turns.some((turn) =>
    turn.items.some((item) => item.kind === "message" && item.itemId === itemId),
  );
}

function upsertTool(turn: StreamTurn, toolCall: ToolCall): void {
  const id = `tool:${toolCall.tool_call_id}`;
  const index = turn.items.findIndex((item) => item.kind === "tool" && item.itemId === id);
  const previous = index >= 0 ? turn.items[index] : undefined;
  // A later update (ACP sends diff content on the first frame only) must not
  // wipe what an earlier one counted.
  const stats = toolCall.stats ?? (previous?.kind === "tool" ? previous.toolCall.stats : undefined);
  const item: ToolItem = {
    kind: "tool",
    itemId: id,
    toolCall: stats === undefined ? toolCall : { ...toolCall, stats },
  };
  if (index >= 0) {
    turn.items[index] = item;
  } else {
    turn.items.push(item);
  }
}

function upsertDelegated(turn: StreamTurn, item: DelegatedItem): void {
  const index = turn.items.findIndex(
    (candidate) => candidate.kind === "delegated" && candidate.subagentId === item.subagentId,
  );
  if (index >= 0) {
    turn.items[index] = item;
  } else {
    turn.items.push(item);
  }
}

/**
 * Fold one protocol event into a thread log.
 *
 * Every thread-scoped event names its thread and turn, so no ownership is
 * inferred from "the last event we saw".
 */
export function foldEvent(
  source: ThreadLog,
  event: Event,
  options: { at?: string } = {},
): ThreadLog {
  if (event.type === "thread_upserted") return source;

  const log = cloneLog(source);
  const at = options.at ?? new Date().toISOString();

  // A turn an edit removed is gone for good, and a new turn never reuses an id.
  // A late event from one of them (a steer written after its turn settled) must
  // not fold it back into the transcript behind the cut.
  if ("turn_id" in event && event.turn_id !== undefined && log.cutTurns.includes(event.turn_id)) {
    return log;
  }

  switch (event.type) {
    case "turn_started": {
      let existing = findTurn(log, event.turn_id);
      if (!existing) {
        existing = {
          id: event.turn_id,
          startedAt: event.started_at,
          status: "running",
          items: [],
          usage: null,
          changes: null,
          ...(event.harness ? { harness: event.harness } : {}),
        };
        log.turns.push(existing);
      }
      existing.status = "running";
      if (!existing.startedAt) existing.startedAt = event.started_at;
      if (event.harness) existing.harness = event.harness;
      // A copied turn's cue already says it came from a finished conversation.
      // Mark the daemon row too, so a prompt-only copy (whose own row stays
      // empty) never shows a "Finished" receipt it did not earn.
      if (
        log.turns.some((candidate) => candidate.forkTurnId === event.turn_id && candidate.imported)
      ) {
        existing.imported = true;
      }
      return log;
    }

    case "harness_switched": {
      // Every turn journaled before a switch ran on the harness it names, even
      // when its own `TurnStarted` predates the field. Backfilling from the
      // divider is what keeps an older turn labelled with the provider that
      // really ran it instead of the thread's current one.
      for (const turn of log.turns) {
        if (!turn.divider && !turn.harness) turn.harness = event.from;
      }
      const id = `switch:${event.from}:${event.to}:${at}`;
      const item: HarnessSwitchItem = {
        kind: "harness_switch",
        itemId: id,
        from: event.from,
        to: event.to,
      };
      // A switch always happens between turns. A new row keeps it in
      // chronological place and keeps both neighbouring turns intact; the
      // same event folded twice is not shown twice.
      if (log.turns.some((turn) => turn.id === id)) return log;
      log.turns.push({
        id,
        startedAt: at,
        status: "completed",
        items: [item],
        usage: null,
        changes: null,
        divider: true,
      });
      return log;
    }

    case "message_delta": {
      const turn = ensureTurn(log, event.turn_id, at);
      const existing = turn.items.find(
        (item): item is MessageItem => item.kind === "message" && item.itemId === event.item_id,
      );
      if (existing) {
        existing.text += event.text;
        existing.streaming = true;
      } else {
        turn.items.push({
          kind: "message",
          itemId: event.item_id,
          role: "assistant",
          text: event.text,
          attachments: [],
          streaming: true,
        });
      }
      turn.status = "running";
      return log;
    }

    case "item_completed": {
      const { item, turn_id } = event;
      // An imported message carries when it was written in its own
      // conversation; a live one has no source time and keeps the journal's.
      const itemAt = event.at ?? at;
      // Imported history was already finished when it was written, and no
      // turn completion will follow it, so its turn settles here.
      const imported = event.at != null;
      if (item.kind === "message") {
        if (item.role === "user") {
          if (messageExists(log, item.item_id)) return log;
          const pendingIndex = log.pending.findIndex(
            (message) => message.text.trim() === item.text.trim(),
          );
          if (pendingIndex >= 0) log.pending.splice(pendingIndex, 1);
          log.pending = log.pending.filter((message) => message.id !== item.item_id);
          const cue: StreamTurn = {
            id: `cue:user:${item.item_id}`,
            startedAt: itemAt,
            status: "completed",
            items: [
              {
                kind: "message",
                itemId: item.item_id,
                role: "user",
                text: item.text,
                attachments: item.attachments,
                streaming: false,
              },
            ],
            usage: null,
            changes: null,
          };
          // The cue names the daemon turn it belongs to. A fork from this row
          // is only offered once that turn has settled, which the live daemon
          // row says; copied history has no live row and is settled by
          // definition.
          //
          // An older journal filed a queued prompt under the turn it waited
          // behind. That prompt is not the turn's own, and the event does not
          // name the turn it really became, so this row has no owner to fork
          // from: leaving `forkTurnId` unset is what refuses it. The same rule
          // as the daemon's replay, where such a prompt is never attached to a
          // later turn that already has one.
          const owner = log.turns.find((candidate) => candidate.id === turn_id);
          const queuedBehindAnOpenTurn =
            owner !== undefined &&
            owner.status === "running" &&
            log.turns.some(
              (candidate) => candidate.id.startsWith("cue:") && candidate.forkTurnId === turn_id,
            );
          if (!queuedBehindAnOpenTurn) cue.forkTurnId = turn_id;
          if (imported) cue.imported = true;
          log.turns.push(cue);
          return log;
        }

        const turn = ensureTurn(log, turn_id, itemAt);
        if (imported) {
          turn.imported = true;
          turn.status = "completed";
        }
        const existing = turn.items.find(
          (candidate): candidate is MessageItem =>
            candidate.kind === "message" && candidate.itemId === item.item_id,
        );
        if (existing) {
          existing.text = item.text;
          existing.streaming = false;
        } else {
          turn.items.push({
            kind: "message",
            itemId: item.item_id,
            role: item.role,
            text: item.text,
            attachments: item.attachments,
            streaming: false,
          });
        }
        return log;
      }

      const turn = ensureTurn(log, turn_id, itemAt);
      if (imported) {
        turn.imported = true;
        turn.status = "completed";
      }
      if (item.kind === "tool_call") {
        upsertTool(turn, item.tool_call);
      } else {
        upsertDelegated(turn, {
          kind: "delegated",
          itemId: item.item_id,
          subagentId: item.subagent_id,
          threadId: item.thread_id,
          task: item.task,
          harness: item.harness,
          host: item.host,
          model: item.model,
          state: item.state,
          agentState: item.agent_state,
        });
      }
      return log;
    }

    case "subagent_spawned":
    case "subagent_updated":
    case "subagent_finished": {
      const subagent = event.subagent;
      for (const turn of log.turns) {
        const index = turn.items.findIndex(
          (item) => item.kind === "delegated" && item.subagentId === subagent.subagent_id,
        );
        if (index < 0) continue;
        const current = turn.items[index];
        if (current?.kind !== "delegated") continue;
        turn.items[index] = {
          ...current,
          threadId: subagent.child_thread_id,
          task: subagent.task,
          harness: subagent.harness,
          host: subagent.host,
          model: subagent.model,
          state: subagent.state,
          agentState: subagent.agent_state,
        };
      }
      return log;
    }

    case "tool_call": {
      const turn = ensureTurn(log, event.turn_id, event.tool_call.started_at);
      upsertTool(turn, event.tool_call);
      return log;
    }

    case "plan": {
      // ACP sends the whole plan on every update, so the newest list replaces
      // the previous one instead of appending another block per step.
      const turn = ensureTurn(log, event.turn_id, at);
      const item: PlanItem = {
        kind: "plan",
        itemId: `plan:${event.turn_id}`,
        entries: event.entries,
      };
      const index = turn.items.findIndex((candidate) => candidate.kind === "plan");
      if (index >= 0) {
        turn.items[index] = item;
      } else {
        turn.items.push(item);
      }
      turn.status = "running";
      return log;
    }

    case "approval_requested": {
      const turn = ensureTurn(log, event.turn_id, at);
      const id = `approval:${event.approval_id}`;
      if (!turn.items.some((item) => item.kind === "approval" && item.itemId === id)) {
        turn.items.push({
          kind: "approval",
          itemId: id,
          approvalId: event.approval_id,
          approvalKind: event.kind,
          prompt: event.prompt,
          options: event.options,
        });
      }
      log.approvals[event.approval_id] = log.approvals[event.approval_id] ?? { status: "pending" };
      return log;
    }

    case "approval_resolved": {
      log.approvals[event.approval_id] = {
        ...log.approvals[event.approval_id],
        status: "resolved",
      };
      return log;
    }

    case "guest_requested": {
      // A guest message is not part of the transcript until the owner sends
      // it, so it waits in its own list and renders as a card above the
      // composer.
      const request = event.request;
      if (!log.guestRequests.some((entry) => entry.request_id === request.request_id)) {
        log.guestRequests.push(request);
      }
      return log;
    }

    case "guest_request_resolved": {
      log.guestRequests = log.guestRequests.filter(
        (entry) => entry.request_id !== event.request_id,
      );
      return log;
    }

    case "thread_rewound": {
      // The edited message's cue and every later turn leave the active thread.
      // The prompt itself comes back only to the device that asked, in the
      // request's answer, so a replay never reopens a composer.
      const index = log.turns.findIndex((turn) =>
        turn.items.some((item) => item.kind === "message" && item.itemId === event.before_item_id),
      );
      if (index >= 0) {
        for (const turn of log.turns.slice(index)) log.cutTurns.push(turn.id);
        log.turns = log.turns.slice(0, index);
      }
      return log;
    }

    case "turn_completed": {
      const turn = ensureTurn(log, event.turn_id, at);
      turn.status = event.status;
      turn.usage = event.usage;
      if (event.changes) turn.changes = event.changes;
      // The daemon row of a copied prompt-only turn is created here (there are
      // no items to create it earlier); its cue says it is copied history.
      if (
        log.turns.some((candidate) => candidate.forkTurnId === event.turn_id && candidate.imported)
      ) {
        turn.imported = true;
      }
      for (const item of turn.items) {
        if (item.kind === "message") item.streaming = false;
      }
      return log;
    }

    case "usage_limit_hit": {
      // A replayed transcript settles each turn before its rows arrive, so a
      // copied limit row has no running turn to join; it belongs to the turn
      // it followed, exactly as the live stream attaches it to the newest one.
      // A prompt cue is never that turn.
      const turn = latestRunningTurn(log) ?? lastDaemonTurn(log);
      if (!turn) return log;
      if (!turn.items.some((item) => item.kind === "limit")) {
        turn.items.push({
          kind: "limit",
          itemId: `limit:${turn.id}`,
          resetAt: event.reset_at,
          autoResume: event.auto_resume,
        });
      }
      return log;
    }

    case "error": {
      // Same as a limit row: a copied error belongs to the turn it followed,
      // which the replay has already settled. A prompt cue is never that turn.
      const turn = latestRunningTurn(log) ?? lastDaemonTurn(log);
      if (!turn) return log;
      turn.items.push({
        kind: "error",
        itemId: `error:${turn.id}:${turn.items.length}`,
        message: event.message,
        hint: event.hint,
        retryable: event.retryable,
      });
      return log;
    }

    default:
      return source;
  }
}

/** Turn-level usage totals for the composer/status bar. */
export function totalTokens(usage: Usage): number {
  return usage.input_tokens + usage.output_tokens;
}
