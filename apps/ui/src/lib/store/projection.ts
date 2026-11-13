import type {
  ApprovalDecision,
  ApprovalKind,
  ApprovalOption,
  Event,
  MessageRole,
  ThreadId,
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

export type TurnItem = MessageItem | ToolItem | ApprovalItem | LimitItem | ErrorItem;

export type StreamTurn = {
  id: TurnId;
  startedAt: string;
  status: TurnStatusOrRunning;
  items: TurnItem[];
  usage: Usage | null;
};

export type PendingMessage = {
  id: string;
  text: string;
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
};

export function createThreadLog(): ThreadLog {
  return {
    turns: [],
    nextSeq: 0,
    loaded: false,
    loading: false,
    approvals: {},
    pending: [],
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
  const item: ToolItem = { kind: "tool", itemId: id, toolCall };
  if (index >= 0) {
    turn.items[index] = item;
  } else {
    turn.items.push(item);
  }
}

/**
 * Fold one protocol event into a thread log.
 *
 * `threadHint` is supplied for batched responses (`get_thread`) because
 * `item_completed` has no thread id on the wire; the caller resolves live
 * item ids before calling.
 */
export function foldEvent(
  source: ThreadLog,
  event: Event,
  options: { threadHint?: ThreadId; at?: string } = {},
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
          streaming: true,
        });
      }
      turn.status = "running";
      return log;
    }

    case "item_completed": {
      if (event.kind === "message") {
        if (event.role === "user") {
          if (messageExists(log, event.item_id)) return log;
          const pendingIndex = log.pending.findIndex(
            (message) => message.text.trim() === event.text.trim(),
          );
          if (pendingIndex >= 0) log.pending.splice(pendingIndex, 1);
          log.pending = log.pending.filter((message) => message.id !== event.item_id);
          log.turns.push({
            id: `cue:user:${event.item_id}`,
            startedAt: at,
            status: "completed",
            items: [
              {
                kind: "message",
                itemId: event.item_id,
                role: "user",
                text: event.text,
                streaming: false,
              },
            ],
            usage: null,
          });
          return log;
        }

        for (const turn of log.turns) {
          const item = turn.items.find(
            (candidate): candidate is MessageItem =>
              candidate.kind === "message" && candidate.itemId === event.item_id,
          );
          if (item) {
            item.text = event.text;
            item.streaming = false;
            turn.status = turn.status === "running" ? "running" : turn.status;
            return log;
          }
        }
        const turn = latestRunningTurn(log);
        const target =
          turn ??
          (() => {
            const created: StreamTurn = {
              id: `cue:item:${event.item_id}`,
              startedAt: at,
              status: "running",
              items: [],
              usage: null,
            };
            log.turns.push(created);
            return created;
          })();
        target.items.push({
          kind: "message",
          itemId: event.item_id,
          role: event.role,
          text: event.text,
          streaming: false,
        });
        return log;
      }

      const tool = event.tool_call;
      for (const turn of log.turns) {
        if (
          turn.items.some(
            (item) => item.kind === "tool" && item.itemId === `tool:${tool.tool_call_id}`,
          )
        ) {
          upsertTool(turn, tool);
          return log;
        }
      }
      const turn = latestRunningTurn(log);
      if (turn) upsertTool(turn, tool);
      return log;
    }

    case "tool_call": {
      const turn = ensureTurn(log, event.turn_id, event.tool_call.started_at);
      upsertTool(turn, event.tool_call);
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

    case "turn_completed": {
      const turn = ensureTurn(log, event.turn_id, at);
      turn.status = event.status;
      turn.usage = event.usage;
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
      if (!event.thread_id && !options.threadHint) return log;
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
