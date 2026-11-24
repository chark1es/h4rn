import { useVirtualizer } from "@tanstack/react-virtual";
import { CircleCheckIcon } from "lucide-react";
import { useEffect, useMemo, useRef } from "react";

import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import type { ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { harnessLabel } from "@/lib/entities";
import { absoluteTime, cost, duration, tokens } from "@/lib/format";
import type { PendingMessage, StreamTurn } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { TurnBlock } from "./TurnBlock";

type Row =
  | { kind: "turn"; turn: StreamTurn; key: string }
  | { kind: "pending"; message: PendingMessage; key: string };

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
  // Copied history is not a finished turn of this thread. An imported
  // conversation says where it came from; a fork says nothing here until it has
  // run a turn of its own, and then its own receipt is the honest one.
  const copiedOnly = turns.length > 0 && turns.every((turn) => turn.imported);
  if (copiedOnly && summary.forked_from) return null;
  const imported = copiedOnly;
  return (
    <div className="q-col q-receipt">
      <div className="q-live-line">
        <CircleCheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        <span>
          {imported ? (
            <>
              Imported from {harnessLabel(summary.harness)} · {absoluteTime(summary.last_activity)}
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

  const rows = useMemo<Row[]>(() => {
    const next: Row[] = turns.map((turn) => ({
      kind: "turn",
      turn,
      key: turn.id,
    }));
    for (const message of pending) {
      next.push({ kind: "pending", message, key: message.id });
    }
    return next;
  }, [turns, pending]);

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
      : `${lastRow.key}:${lastRow.message.sending}`
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
      const next = Math.max(0, Math.min(turns.length - 1, focusTurn.current + direction));
      focusTurn.current = next;
      virtualizer.scrollToIndex(next, { align: "start" });
    };
    window.addEventListener("h4rn:turn", handler);
    return () => window.removeEventListener("h4rn:turn", handler);
  }, [turns.length, virtualizer]);

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
              ) : (
                <PendingTurn message={row.message} />
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
