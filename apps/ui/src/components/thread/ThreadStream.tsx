import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef } from "react";

import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import type { ThreadSummary } from "@/gen";
import type { PendingMessage, StreamTurn } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { TurnBlock } from "./TurnBlock";

type Row =
  | { kind: "turn"; turn: StreamTurn; cue: number; key: string }
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

export function ThreadStream({
  threadId,
  summary,
  turns,
  pending,
  onRetry,
  staleAt = null,
}: {
  threadId: string;
  summary: ThreadSummary;
  turns: StreamTurn[];
  pending: PendingMessage[];
  onRetry: () => void;
  staleAt?: number | null;
}) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const atBottom = useRef(true);
  const focusCue = useRef(0);

  const rows = useMemo<Row[]>(() => {
    const next: Row[] = turns.map((turn, index) => ({
      kind: "turn",
      turn,
      cue: index,
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
      focusCue.current = Math.max(0, turns.length - 1);
    });
    return () => cancelAnimationFrame(id);
  }, [signature, rows.length, turns.length, virtualizer]);

  useEffect(() => {
    const handler = (event: Event) => {
      const direction = (event as CustomEvent<{ direction: number }>).detail?.direction ?? 0;
      const next = Math.max(0, Math.min(turns.length - 1, focusCue.current + direction));
      focusCue.current = next;
      virtualizer.scrollToIndex(next, { align: "start" });
    };
    window.addEventListener("h4rn:cue", handler);
    return () => window.removeEventListener("h4rn:cue", handler);
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
      <div className="q-col" style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
        {virtualizer.getVirtualItems().map((item) => {
          const row = rows[item.index];
          if (!row) return null;
          return (
            <div
              key={item.key}
              ref={virtualizer.measureElement}
              data-index={item.index}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${item.start}px)`,
              }}
            >
              {row.kind === "turn" ? (
                <TurnBlock
                  turn={row.turn}
                  threadId={threadId}
                  summary={summary}
                  onRetry={onRetry}
                  staleAt={item.index === rows.length - 1 ? staleAt : null}
                />
              ) : (
                <PendingTurn message={row.message} />
              )}
            </div>
          );
        })}
      </div>
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
        <div className="q-skel" style={{ width: "66%", height: 10, marginBottom: 22 }} />
        <div className="q-skel" style={{ width: "100%", height: 76, marginBottom: 22 }} />
        <div className="q-skel" style={{ width: "88%", height: 10, marginBottom: 10 }} />
        <div className="q-skel" style={{ width: "72%", height: 10 }} />
      </div>
    </div>
  );
}
