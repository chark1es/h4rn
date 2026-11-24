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

export type TurnItem =
  | MessageItem
  | ToolItem
  | ApprovalItem
  | LimitItem
  | ErrorItem
  | DelegatedItem
  | PlanItem;

export type StreamTurn = {
  id: TurnId;
  startedAt: string;
  status: TurnStatusOrRunning;
  items: TurnItem[];
  usage: Usage | null;
  /** Files this turn changed, when the daemon was able to snapshot them. */
  changes: FsTurnChanges | null;
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
  };
}

function latestRunningTurn(log: ThreadLog): StreamTurn | null {
  for (let index = log.turns.length - 1; index >= 0; index -= 1) {
    const turn = log.turns[index];
    if (turn && turn.status === "running") return turn;
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

  switch (event.type) {
    case "turn_started": {
      const existing = findTurn(log, event.turn_id);
      if (existing) {
        existing.status = "running";
        if (!existing.startedAt) existing.startedAt = event.started_at;
      } else {
        log.turns.push({
          id: event.turn_id,
          startedAt: event.started_at,
          status: "running",
          items: [],
          usage: null,
          changes: null,
        });
      }
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
      if (item.kind === "message") {
        if (item.role === "user") {
          if (messageExists(log, item.item_id)) return log;
          const pendingIndex = log.pending.findIndex(
            (message) => message.text.trim() === item.text.trim(),
          );
          if (pendingIndex >= 0) log.pending.splice(pendingIndex, 1);
          log.pending = log.pending.filter((message) => message.id !== item.item_id);
          log.turns.push({
            id: `cue:user:${item.item_id}`,
            startedAt: at,
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
          });
          return log;
        }

        const turn = ensureTurn(log, turn_id, at);
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

      const turn = ensureTurn(log, turn_id, at);
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

    case "turn_completed": {
      const turn = ensureTurn(log, event.turn_id, at);
      turn.status = event.status;
      turn.usage = event.usage;
      if (event.changes) turn.changes = event.changes;
      for (const item of turn.items) {
        if (item.kind === "message") item.streaming = false;
      }
      return log;
    }

    case "usage_limit_hit": {
      const turn = latestRunningTurn(log);
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
      const turn = latestRunningTurn(log);
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
