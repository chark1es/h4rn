import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronDownIcon,
  ChevronUpIcon,
  CircleCheckIcon,
  CornerDownRightIcon,
  PencilIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef } from "react";

import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import type { QueuedMessage, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { isTurnRunning } from "@/lib/agent";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime, cost, duration, tokens } from "@/lib/format";
import {
  moveQueuedMessage,
  removeQueuedMessage,
  sendQueuedNow,
  useApp,
} from "@/lib/store/appStore";
import type { PendingMessage, StreamTurn } from "@/lib/store/projection";
import { isRealTurn } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { TurnBlock } from "./TurnBlock";

type Row =
  | { kind: "turn"; turn: StreamTurn; key: string }
  | { kind: "pending"; message: PendingMessage; key: string }
  | { kind: "queued"; message: QueuedMessage; key: string; index: number };

/**
 * The copy for what Send now will really do with this row right now.
 *
 * The daemon publishes `steerable` per row: a message with its own permission
 * ceiling, or one that may already have been delivered, is moved to the front
 * instead. The hint says exactly that instead of promising a steer.
 */
export function sendNowHint(summary: ThreadSummary, message: QueuedMessage): string {
  if (message.delivery_unknown) {
    return "May have been delivered; sending again adds a second copy";
  }
  if (isTurnRunning(summary)) {
    return message.steerable
      ? "Send now steers it into this turn"
      : "Waits for this turn to finish";
  }
  return "Send now starts it next";
}

export function QueuedTurn({
  threadId,
  summary,
  message,
  index,
  total,
  editing,
}: {
  threadId: string;
  summary: ThreadSummary;
  message: QueuedMessage;
  index: number;
  /**
   * How many messages wait in total.
   *
   * The rows are a bounded page, so the last visible row is not necessarily the
   * queue's last; the caller knows the full count.
   */
  total: number;
  editing: boolean;
}) {
  const steers = isTurnRunning(summary) && message.steerable;
  return (
    <div className="q-queued" data-editing={editing || undefined}>
      <div className="q-queued-h">
        <span className="q-queued-label">
          {message.delivery_unknown ? "May have been delivered" : "Queued"}
        </span>
        <span className="q-faint">{sendNowHint(summary, message)}</span>
      </div>
      <div className="q-queued-text">{message.text}</div>
      {message.attachments.length > 0 ? (
        <MessageAttachments attachments={message.attachments} align="start" />
      ) : null}
      <div className="q-queued-a">
        <button
          type="button"
          className="q-btn ghost"
          onClick={() => void sendQueuedNow(threadId, message.message_id)}
          title={
            message.delivery_unknown
              ? "Send this message again; it may already be in the turn"
              : steers
                ? "Steer this message into the running turn"
                : "Move this message to the front of the queue"
          }
        >
          <CornerDownRightIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {message.delivery_unknown ? "Send again" : "Send now"}
        </button>
        <button
          type="button"
          className="q-btn ghost"
          onClick={() =>
            window.dispatchEvent(
              new CustomEvent("h4rn:edit-queued", {
                detail: { threadId, message },
              }),
            )
          }
          title="Edit in the composer"
        >
          <PencilIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          Edit
        </button>
        <button
          type="button"
          className="q-btn ghost"
          onClick={() => void removeQueuedMessage(threadId, message.message_id)}
          title={message.delivery_unknown ? "Discard this row" : "Remove from the queue"}
        >
          <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
          {message.delivery_unknown ? "Discard" : "Remove"}
        </button>
        <span className="grow" />
        <button
          type="button"
          className="q-ibtn"
          disabled={index === 0}
          aria-label="Move up"
          title="Move up"
          onClick={() => void moveQueuedMessage(threadId, message.message_id, "up")}
        >
          <ChevronUpIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
        <button
          type="button"
          className="q-ibtn"
          disabled={index === total - 1}
          aria-label="Move down"
          title="Move down"
          onClick={() => void moveQueuedMessage(threadId, message.message_id, "down")}
        >
          <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
      </div>
    </div>
  );
}

/**
 * The row a turn shortcut moves to.
 *
 * `realTurns` is the ascending list of rows an agent really ran. Moving back
 * from a row past the first real turn stays on that turn instead of landing on
 * a divider, and moving forward from the last one stays there.
 */
export function nextTurnIndex(
  realTurns: readonly number[],
  current: number,
  direction: number,
): number {
  if (realTurns.length === 0) return Math.max(0, current);
  if (direction < 0) {
    const previous = realTurns.filter((index) => index < current).at(-1);
    return previous ?? (realTurns[0] as number);
  }
  return realTurns.find((index) => index > current) ?? (realTurns.at(-1) as number);
}

function PendingTurn({ message }: { message: PendingMessage }) {
  return (
    <div className={cn("q-turn q-you-msg", message.sending && "q-faint")}>
      {message.text ? <div className="q-bubble">{message.text}</div> : null}
      <MessageAttachments attachments={message.attachments} align="end" />
      <div className="q-you-meta">
        <span>{message.sending ? "Sending…" : "Sent"}</span>
      </div>
    </div>
  );
}

/**
 * The line under a quiet thread: how it finished, or where it came from.
 *
 * An imported conversation was written elsewhere, so claiming a duration and
 * token count for it would be invented: it says where it came from instead.
 */
export function ThreadReceipt({ summary, turns }: { summary: ThreadSummary; turns: StreamTurn[] }) {
  if (summary.tab !== "done" || summary.agent_state !== "done") return null;
  // A switch divider is not a turn the agent wrote: it must neither make a
  // conversation look imported nor hide that it was.
  const written = turns.filter(isRealTurn);
  // An imported conversation with no agent turn at all (only the person's
  // messages, or only the cue rows that stand in for them) still came from
  // somewhere: the summary remembers the source even when no turn carries the
  // flag.
  const copiedOnly = written.length > 0 && written.every((turn) => turn.imported);
  // Copied history is not a finished turn of this thread. An imported
  // conversation says where it came from; a fork says nothing here until it has
  // run a turn of its own, and then its own receipt is the honest one.
  if (copiedOnly && summary.forked_from) return null;
  const imported = written.length > 0 ? copiedOnly : summary.imported_from != null;
  // The thread may have switched harness since the import. The receipt names
  // where the conversation came from, never the provider it runs on now.
  const source =
    summary.imported_from ??
    written.find((turn) => turn.imported && turn.harness)?.harness ??
    summary.harness;
  return (
    <div className="q-col q-receipt">
      <div className="q-live-line">
        <CircleCheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        <span>
          {imported ? (
            <>
              Imported from {harnessLabel(source)} · {absoluteTime(summary.last_activity)}
            </>
          ) : (
            <>
              Finished {absoluteTime(summary.last_activity)} · {duration(summary.usage.duration_ms)}{" "}
              · {tokens(summary.usage.input_tokens + summary.usage.output_tokens)} tokens ·{" "}
              {cost(summary.usage.cost_micros)} est.
            </>
          )}
        </span>
      </div>
    </div>
  );
}

export function ThreadStream({
  threadId,
  summary,
  turns,
  pending,
  onReply,
  staleAt = null,
}: {
  threadId: string;
  summary: ThreadSummary;
  turns: StreamTurn[];
  pending: PendingMessage[];
  onReply: () => void;
  staleAt?: number | null;
}) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const atBottom = useRef(true);
  const focusTurn = useRef(0);
  const isPhone = useIsPhone();

  const editingId = useApp((state) => state.ui.queuedEdit?.messageId ?? null);
  const queuedEditThread = useApp((state) => state.ui.queuedEdit?.threadId ?? null);
  // The queue's rows are fetched separately from the summary, which only counts
  // them: a full queue can be larger than one transport frame.
  const queue = useApp((state) => state.queues[threadId]);
  const queued = queue?.queued;
  const queuedTotal = queue?.total ?? 0;

  const rows = useMemo<Row[]>(() => {
    const next: Row[] = turns.map((turn) => ({
      kind: "turn",
      turn,
      key: turn.id,
    }));
    for (const message of pending) {
      next.push({ kind: "pending", message, key: message.id });
    }
    (queued ?? []).forEach((message, index) => {
      next.push({
        kind: "queued",
        message,
        key: `queued:${message.message_id}`,
        index,
      });
    });
    return next;
  }, [turns, pending, queued]);

  // The rows Previous Turn and Next Turn stop on: the turns an agent really
  // ran. A switch divider and a user-message cue have no work behind them, so
  // stopping on one looks like the shortcut did nothing.
  const realTurns = useMemo(
    () => turns.flatMap((turn, index) => (isRealTurn(turn) ? [index] : [])),
    [turns],
  );

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 160,
    overscan: 4,
    paddingStart: isPhone ? 8 : 28,
    paddingEnd: 20,
    getItemKey: (index) => rows[index]?.key ?? index,
  });

  const lastRow = rows[rows.length - 1];
  const lastItem =
    lastRow?.kind === "turn" ? lastRow.turn.items[lastRow.turn.items.length - 1] : undefined;
  const signature = lastRow
    ? lastRow.kind === "turn"
      ? `${lastRow.key}:${lastRow.turn.items.length}:${lastRow.turn.status}:${
          lastItem?.kind === "message"
            ? `${lastItem.itemId}:${lastItem.text.length}`
            : lastItem?.kind === "tool"
              ? `${lastItem.toolCall.tool_call_id}:${lastItem.toolCall.status}`
              : (lastItem?.itemId ?? "")
        }`
      : lastRow.kind === "pending"
        ? `${lastRow.key}:${lastRow.message.sending}`
        : `${lastRow.key}:${lastRow.message.text.length}`
    : "";

  // Follow the tail only while the reader is already at the bottom.
  // biome-ignore lint/correctness/useExhaustiveDependencies: signature is the content fingerprint that retriggers the tail follow
  useEffect(() => {
    if (!atBottom.current || rows.length === 0) return;
    const id = requestAnimationFrame(() => {
      virtualizer.scrollToIndex(rows.length - 1, { align: "end" });
      focusTurn.current = Math.max(0, turns.length - 1);
    });
    return () => cancelAnimationFrame(id);
  }, [signature, rows.length, turns.length, virtualizer]);

  useEffect(() => {
    const handler = (event: Event) => {
      const direction = (event as CustomEvent<{ direction: number }>).detail?.direction ?? 0;
      const next = nextTurnIndex(realTurns, focusTurn.current, direction);
      focusTurn.current = next;
      virtualizer.scrollToIndex(next, { align: "start" });
    };
    window.addEventListener("h4rn:turn", handler);
    return () => window.removeEventListener("h4rn:turn", handler);
  }, [realTurns, virtualizer]);

  return (
    <div
      ref={scrollRef}
      className="q-stream"
      onScroll={(event) => {
        const element = event.currentTarget;
        atBottom.current = element.scrollHeight - element.scrollTop - element.clientHeight < 160;
      }}
      aria-label="Thread stream"
      role="log"
    >
      <div
        className="q-col q-col-v"
        style={{ height: virtualizer.getTotalSize(), position: "relative" }}
      >
        {virtualizer.getVirtualItems().map((item) => {
          const row = rows[item.index];
          if (!row) return null;
          return (
            <div
              key={item.key}
              ref={virtualizer.measureElement}
              data-index={item.index}
              className="q-vrow"
              style={{
                transform: `translateY(${item.start}px)`,
              }}
            >
              {row.kind === "turn" ? (
                <TurnBlock
                  turn={row.turn}
                  turns={turns}
                  threadId={threadId}
                  summary={summary}
                  onReply={onReply}
                  staleAt={item.index === rows.length - 1 ? staleAt : null}
                />
              ) : row.kind === "pending" ? (
                <PendingTurn message={row.message} />
              ) : (
                <QueuedTurn
                  threadId={threadId}
                  summary={summary}
                  message={row.message}
                  index={row.index}
                  total={queuedTotal}
                  editing={queuedEditThread === threadId && editingId === row.message.message_id}
                />
              )}
            </div>
          );
        })}
      </div>
      <ThreadReceipt summary={summary} turns={turns} />
    </div>
  );
}

export function ThreadStreamSkeleton() {
  return (
    <div className="q-stream" role="status" aria-busy="true" aria-label="Loading thread">
      <div className="q-col">
        <div style={{ display: "flex", justifyContent: "flex-end", marginBottom: 32 }}>
          <div className="q-skel" style={{ width: "58%", height: 62, borderRadius: 14 }} />
        </div>
        <div className="q-skel" style={{ width: 240, height: 12, marginBottom: 10 }} />
        <div className="q-skel" style={{ width: "92%", height: 10, marginBottom: 10 }} />
        <div className="q-skel" style={{ width: "84%", height: 10, marginBottom: 10 }} />
        <div className="q-skel" style={{ width: "66%", height: 10, marginBottom: 14 }} />
        <div style={{ display: "flex", gap: 8, marginBottom: 14 }}>
          <span
            className="q-skel"
            style={{ width: 14, height: 14, borderRadius: 4, flex: "none" }}
          />
          <span className="q-skel" style={{ width: "54%", height: 11 }} />
        </div>
        <div className="q-skel" style={{ width: "100%", height: 76, marginBottom: 22 }} />
        <div style={{ display: "flex", gap: 8, marginBottom: 14 }}>
          <span
            className="q-skel"
            style={{ width: 14, height: 14, borderRadius: 4, flex: "none" }}
          />
          <span className="q-skel" style={{ width: "42%", height: 11 }} />
        </div>
        <div className="q-skel" style={{ width: "88%", height: 10, marginBottom: 10 }} />
        <div className="q-skel" style={{ width: "72%", height: 10 }} />
      </div>
    </div>
  );
}
