import { CircleAlertIcon, ClockIcon, PauseIcon, RotateCcwIcon } from "lucide-react";
import type { ReactNode } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ALT, SHIFT } from "@/components/common/Kbd";
import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import type { ThreadSummary } from "@/gen";
import { useSmoothText } from "@/hooks/use-smooth-text";
import { glyphForAgentState } from "@/lib/agent";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteTime, countdown } from "@/lib/format";
import type { MessageItem, StreamTurn, TurnItem } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { Markdown } from "./Markdown";
import { ToolCard } from "./ToolCard";

function AssistantMessage({ item, streaming }: { item: MessageItem; streaming: boolean }) {
  const text = useSmoothText(item.text, streaming);
  return <Markdown text={text} />;
}

function UserMessage({ item, at }: { item: MessageItem; at: string }) {
  return (
    <div className="q-turn q-you-msg">
      {item.text ? <div className="q-bubble">{item.text}</div> : null}
      <MessageAttachments attachments={item.attachments} align="end" />
      <div className="q-you-meta">
        <span>{absoluteTime(at)}</span>
      </div>
    </div>
  );
}

function LimitItemView({ resetAt, autoResume }: { resetAt: string | null; autoResume: boolean }) {
  return (
    <div className="q-banner" style={{ margin: "0 0 12px" }}>
      <span className="ic q-warn">
        <ClockIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        <b>Usage limit hit.</b>{" "}
        {autoResume
          ? `Auto-resume is armed${resetAt ? `; it resumes in ${countdown(resetAt)}` : ""}. Cancel it with ${ALT} R.`
          : `Auto-resume is off. Resume manually${resetAt ? ` after ${countdown(resetAt)}` : ""}.`}
      </span>
    </div>
  );
}

function ErrorItemView({
  message,
  hint,
  retryable,
}: {
  message: string;
  hint: string | null;
  retryable: boolean;
}) {
  return (
    <div className="q-banner err" style={{ margin: "0 0 12px" }}>
      <span className="ic">
        <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        <b>{message}</b>
        {hint ? ` ${hint}` : ""}
        {retryable ? " The turn can be retried from the composer." : ""}
      </span>
    </div>
  );
}

function Item({ item, threadId }: { item: TurnItem; threadId: string }) {
  void threadId;
  switch (item.kind) {
    case "message":
      return <AssistantMessage item={item} streaming={item.streaming} />;
    case "tool":
      return <ToolCard toolCall={item.toolCall} />;
    case "approval":
      return null;
    case "limit":
      return <LimitItemView resetAt={item.resetAt} autoResume={item.autoResume} />;
    case "error":
      return <ErrorItemView message={item.message} hint={item.hint} retryable={item.retryable} />;
  }
}

export function TurnBlock({
  turn,
  threadId,
  summary,
  onRetry,
  staleAt = null,
}: {
  turn: StreamTurn;
  threadId: string;
  summary: ThreadSummary;
  onRetry: () => void;
  staleAt?: number | null;
}) {
  const userItems = turn.items.filter(
    (item): item is MessageItem => item.kind === "message" && item.role === "user",
  );
  const isUserTurn = turn.items.length > 0 && userItems.length === turn.items.length;
  const runningTool = [...turn.items]
    .reverse()
    .find((item) => item.kind === "tool" && item.toolCall.status === "running");
  const toolName = runningTool?.kind === "tool" ? runningTool.toolCall.name : undefined;
  const glyph = glyphForAgentState(summary.agent_state, toolName);
  const live = turn.status === "running" && summary.agent_state !== "awaiting_approval";
  const failed = turn.status === "failed" || turn.status === "interrupted";

  if (isUserTurn) {
    return (
      <>
        {userItems.map((item) => (
          <UserMessage key={item.itemId} item={item} at={turn.startedAt} />
        ))}
      </>
    );
  }

  const content: ReactNode[] = [];
  let toolGroup: TurnItem[] = [];
  const flushTools = () => {
    if (toolGroup.length === 0) return;
    const group = toolGroup;
    toolGroup = [];
    content.push(
      <div className="q-tools" key={`tools:${group[0]?.itemId ?? content.length}`}>
        {group.map((item) =>
          item.kind === "tool" ? <ToolCard key={item.itemId} toolCall={item.toolCall} /> : null,
        )}
      </div>,
    );
  };
  for (const item of turn.items) {
    if (item.kind === "tool") {
      toolGroup.push(item);
      continue;
    }
    flushTools();
    content.push(
      item.kind === "message" && item.role === "user" ? (
        <UserMessage key={`${turn.id}:${item.itemId}`} item={item} at={turn.startedAt} />
      ) : (
        <Item key={`${turn.id}:${item.itemId}`} item={item} threadId={threadId} />
      ),
    );
  }
  flushTools();

  return (
    <div className="q-turn">
      <div className="q-ag-head">
        <EntityAvatar
          seed={harnessSeed(summary.host, summary.harness)}
          kind="harness"
          label={harnessLabel(summary.harness)}
          size={14}
        />
        <b>{harnessLabel(summary.harness)}</b>
        <span>on {summary.host}</span>
        <span>·</span>
        <span className="q-num">{absoluteTime(turn.startedAt)}</span>
      </div>

      {content}

      {staleAt !== null && turn.status === "running" ? (
        <div className="q-live-line">
          <PauseIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>
            Stream paused at {absoluteTime(new Date(staleAt).toISOString())} · will catch up on
            reconnect
          </span>
        </div>
      ) : live ? (
        <div className="q-live-line">
          <AgentGlyph glyph={glyph} size={14} />
          <span>{glyph.label} · live</span>
        </div>
      ) : null}

      {failed ? (
        <div className={cn("q-live-line", "q-danger")}>
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>
            {turn.status === "failed" ? "The turn failed" : "The turn was interrupted"}. Nothing
            after this ran.
          </span>
          <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onRetry}>
            <RotateCcwIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            Retry <span className="q-kbd">{SHIFT} R</span>
          </button>
        </div>
      ) : null}
    </div>
  );
}
