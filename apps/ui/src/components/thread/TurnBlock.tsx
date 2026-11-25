import {
  CheckIcon,
  CircleAlertIcon,
  CircleDotIcon,
  CircleIcon,
  ClockIcon,
  CornerDownLeftIcon,
  PauseIcon,
} from "lucide-react";
import type { ReactNode } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import { ChangeSetCard } from "@/features/changes/ChangeSetCard";
import type { PlanStep, ThreadSummary } from "@/gen";
import { useSmoothText } from "@/hooks/use-smooth-text";
import { agentStateIsActive, glyphForAgentState } from "@/lib/agent";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { absoluteTime, countdown } from "@/lib/format";
import type {
  DelegatedItem,
  HarnessSwitchItem,
  MessageItem,
  PlanItem,
  StreamTurn,
  TurnItem,
} from "@/lib/store/projection";
import { stoppedNotice } from "@/lib/turn";
import { cn } from "@/lib/utils";
import { Markdown } from "./Markdown";
import { ToolCard } from "./ToolCard";

function AssistantMessage({ item, streaming }: { item: MessageItem; streaming: boolean }) {
  const text = useSmoothText(item.text, streaming);
  return <Markdown text={text} interceptHtmlLinks />;
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
          ? `Auto-resume is armed${resetAt ? `; it resumes in ${countdown(resetAt)}` : ""}.`
          : `Auto-resume is off. Resume manually${resetAt ? ` after ${countdown(resetAt)}` : ""}.`}
      </span>
    </div>
  );
}

function DelegatedItemView({ item }: { item: DelegatedItem }) {
  const glyph = glyphForAgentState(item.agentState);
  return (
    <div className="q-sub-row">
      <AgentGlyph glyph={glyph} size={14} />
      <span className="t">
        <span className="q-faint">Delegated</span> {item.task}
      </span>
      <span className="q-faint q-num" style={{ fontSize: 12 }}>
        {harnessLabel(item.harness)} · {item.model}
      </span>
      <button
        type="button"
        className="q-btn ghost"
        style={{ height: 24 }}
        onClick={() => window.dispatchEvent(new CustomEvent("h4rn:subagents"))}
      >
        Open
      </button>
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

function PlanItemView({ item }: { item: PlanItem }) {
  return (
    <div className="q-plan">
      <ul aria-label="Plan">
        {item.entries.map((step: PlanStep, index) => {
          const status =
            step.status === "completed"
              ? "completed"
              : step.status === "in_progress"
                ? "in_progress"
                : "pending";
          return (
            // biome-ignore lint/suspicious/noArrayIndexKey: a plan is an ordered list without ids
            <li key={`${index}:${step.content}`} data-status={status} className="q-plan-step">
              <span className="q-plan-mark" aria-hidden>
                {status === "completed" ? (
                  <CheckIcon className="q-icon s12" strokeWidth={2} />
                ) : status === "in_progress" ? (
                  <CircleDotIcon className="q-icon s12" strokeWidth={1.75} />
                ) : (
                  <CircleIcon className="q-icon s12" strokeWidth={1.5} />
                )}
              </span>
              <span className="q-plan-text">{step.content}</span>
              {status === "completed" ? <span className="sr-only">(done)</span> : null}
              {status === "in_progress" ? <span className="sr-only">(in progress)</span> : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function HarnessSwitchLine({ item }: { item: HarnessSwitchItem }) {
  return (
    <div className="q-switch-line">
      <span className="q-switch-rule" aria-hidden />
      <span>
        Switched from {harnessLabel(item.from)} to {harnessLabel(item.to)}
      </span>
      <span className="q-switch-rule" aria-hidden />
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
    case "delegated":
      return <DelegatedItemView item={item} />;
    case "plan":
      return <PlanItemView item={item} />;
    case "error":
      return <ErrorItemView message={item.message} hint={item.hint} retryable={item.retryable} />;
    case "harness_switch":
      return <HarnessSwitchLine item={item} />;
  }
}

export function TurnBlock({
  turn,
  threadId,
  summary,
  onReply,
  staleAt = null,
}: {
  turn: StreamTurn;
  threadId: string;
  summary: ThreadSummary;
  /** Focus the composer so the person can reply to a stopped or failed turn. */
  onReply: () => void;
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
  const live =
    turn.status === "running" && !turn.imported && agentStateIsActive(summary.agent_state);
  const stopped = stoppedNotice(turn.status);
  const failed = turn.status === "failed" || turn.status === "interrupted";
  const turnHarness = turn.harness ?? summary.harness;

  if (turn.divider) {
    return (
      <div className="q-turn q-turn-divider">
        {turn.items.map((item) => (
          <Item key={`${turn.id}:${item.itemId}`} item={item} threadId={threadId} />
        ))}
      </div>
    );
  }

  if (isUserTurn) {
    return (
      <>
        {userItems.map((item) => (
          <UserMessage key={item.itemId} item={item} at={turn.startedAt} />
        ))}
        {stopped !== null ? (
          <div className={cn("q-live-line", "q-warn")}>
            <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            <span>{stopped}</span>
            <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReply}>
              <CornerDownLeftIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              Reply
            </button>
          </div>
        ) : null}
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
          seed={harnessSeed(summary.host, turnHarness)}
          kind="harness"
          label={harnessLabel(turnHarness)}
          size={14}
        />
        <b>{harnessLabel(turnHarness)}</b>
        <span>on {summary.host}</span>
        <span>·</span>
        <span className="q-num">{absoluteTime(turn.startedAt)}</span>
      </div>

      {content}

      {turn.changes && turn.changes.files.length > 0 ? (
        <ChangeSetCard turn={turn} summary={summary} />
      ) : null}

      {staleAt !== null && turn.status === "running" && !turn.imported ? (
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

      {stopped !== null ? (
        <div className={cn("q-live-line", "q-warn")}>
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>{stopped}</span>
          <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReply}>
            <CornerDownLeftIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            Reply
          </button>
        </div>
      ) : failed ? (
        <div className={cn("q-live-line", "q-danger")}>
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>
            {turn.status === "failed" ? "The turn failed" : "The turn was interrupted"}. Nothing
            after this ran.
          </span>
          <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReply}>
            <CornerDownLeftIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            Reply
          </button>
        </div>
      ) : null}
    </div>
  );
}
