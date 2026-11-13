import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef, useState } from "react";

import { Skeleton } from "@/components/ui/skeleton";
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
    <div className="grid grid-cols-[32px_minmax(0,1fr)] gap-2 pr-3 pl-1 sm:grid-cols-[48px_minmax(0,1fr)] sm:gap-3 sm:pr-6 sm:pl-2">
      <div className="flex flex-col items-end pt-0.5">
        <span className="font-mono text-micro tabular text-ink-faint">··</span>
      </div>
      <div className="flex flex-col items-end gap-1 pb-4">
        <div className="flex items-center gap-2 text-micro text-ink-faint">
          <span className="font-medium text-ink-muted">You</span>
          <span>{message.sending ? "Sending…" : "Sent"}</span>
        </div>
        {message.text ? (
          <div
            className={cn(
              "max-w-[68ch] rounded-md border border-line bg-panel px-3 py-2 text-compact leading-relaxed whitespace-pre-wrap text-ink",
              message.sending && "opacity-70",
            )}
          >
            {message.text}
          </div>
        ) : null}
        <MessageAttachments attachments={message.attachments} align="end" />
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
}: {
  threadId: string;
  summary: ThreadSummary;
  turns: StreamTurn[];
  pending: PendingMessage[];
  onRetry: () => void;
}) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const atBottom = useRef(true);
  const [focusCue, setFocusCue] = useState(0);

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
      setFocusCue(Math.max(0, turns.length - 1));
    });
    return () => cancelAnimationFrame(id);
  }, [signature, rows.length, turns.length, virtualizer]);

  useEffect(() => {
    const handler = (event: Event) => {
      const direction = (event as CustomEvent<{ direction: number }>).detail?.direction ?? 0;
      setFocusCue((current) => {
        const next = Math.max(0, Math.min(turns.length - 1, current + direction));
        virtualizer.scrollToIndex(next, { align: "start" });
        return next;
      });
    };
    window.addEventListener("h4rn:cue", handler);
    return () => window.removeEventListener("h4rn:cue", handler);
  }, [turns.length, virtualizer]);

  return (
    <div
      ref={scrollRef}
      className="min-h-0 flex-1 overflow-y-auto overscroll-contain"
      onScroll={(event) => {
        const element = event.currentTarget;
        atBottom.current = element.scrollHeight - element.scrollTop - element.clientHeight < 160;
      }}
      aria-label="Thread stream"
      role="log"
    >
      <div
        className="mx-auto w-full max-w-[760px] pt-4"
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
                  index={row.cue}
                  threadId={threadId}
                  summary={summary}
                  current={row.cue === focusCue}
                  onRetry={onRetry}
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
    <div
      className="min-h-0 flex-1 overflow-hidden"
      role="status"
      aria-busy="true"
      aria-label="Loading thread"
    >
      <div className="mx-auto w-full max-w-[760px] space-y-4 px-4 py-4">
        <div className="flex items-center gap-2">
          <Skeleton className="size-4 rounded-full" />
          <Skeleton className="h-3 w-28" />
        </div>
        <div className="space-y-2">
          <Skeleton className="h-3 w-11/12" />
          <Skeleton className="h-3 w-10/12" />
          <Skeleton className="h-3 w-7/12" />
        </div>
        <div className="space-y-1.5 rounded-sm border border-line p-2">
          <Skeleton className="h-2.5 w-24" />
          <Skeleton className="h-2.5 w-40" />
        </div>
        <div className="space-y-2">
          <Skeleton className="h-3 w-9/12" />
          <Skeleton className="h-3 w-6/12" />
        </div>
      </div>
    </div>
  );
}
