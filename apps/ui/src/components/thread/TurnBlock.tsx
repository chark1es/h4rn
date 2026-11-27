import {
  CheckIcon,
  CircleAlertIcon,
  CircleDotIcon,
  CircleIcon,
  ClockIcon,
  CornerDownLeftIcon,
  GitForkIcon,
  MoreHorizontalIcon,
  PauseIcon,
  PencilLineIcon,
} from "lucide-react";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Menu, MenuContent, MenuItem, MenuTrigger } from "@/components/ui/menu";
import { MessageAttachments } from "@/features/attachments/MessageAttachments";
import { ChangeSetCard } from "@/features/changes/ChangeSetCard";
import type { PlanStep, ThreadSummary } from "@/gen";
import { useSmoothText } from "@/hooks/use-smooth-text";
import { agentStateIsActive, glyphForAgentState } from "@/lib/agent";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { forkableTurn } from "@/lib/fork";
import { absoluteTime } from "@/lib/format";
import { formatDuration } from "@/lib/i18n";
import { forkThread } from "@/lib/store/appStore";
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

function TurnMenu({
  threadId,
  turnId,
  label,
}: {
  threadId: string;
  turnId: string;
  label: string;
}) {
  const { t } = useTranslation("thread");
  return (
    <Menu>
      <MenuTrigger className="q-turn-menu" aria-label={label} title={label}>
        <MoreHorizontalIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </MenuTrigger>
      <MenuContent align="end" className="w-44">
        <MenuItem icon={<GitForkIcon />} onSelect={() => void forkThread(threadId, turnId)}>
          {t("turn.forkFromHere")}
        </MenuItem>
      </MenuContent>
    </Menu>
  );
}

function UserMessage({
  item,
  at,
  onEdit,
  editDisabledReason = null,
  fork,
}: {
  item: MessageItem;
  at: string;
  onEdit?: ((item: MessageItem) => void) | undefined;
  editDisabledReason?: string | null | undefined;
  fork?: { threadId: string; turnId: string; label: string } | undefined;
}) {
  const { t } = useTranslation("thread");
  return (
    <div className="q-turn q-you-msg">
      {item.text ? <div className="q-bubble">{item.text}</div> : null}
      <MessageAttachments attachments={item.attachments} align="end" />
      <div className="q-you-meta">
        <span>{absoluteTime(at)}</span>
        {onEdit ? (
          <button
            type="button"
            className="q-btn ghost q-edit-here"
            disabled={Boolean(editDisabledReason)}
            title={editDisabledReason ?? t("turn.editHint")}
            onClick={() => onEdit(item)}
          >
            <PencilLineIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("turn.editFromHere")}
          </button>
        ) : null}
        {fork ? <TurnMenu {...fork} /> : null}
      </div>
    </div>
  );
}

/** How long until a usage limit resets, or null when the time is unknown or past. */
function untilReset(resetAt: string | null, now = Date.now()): string | null {
  const then = resetAt ? Date.parse(resetAt) : Number.NaN;
  if (!Number.isFinite(then) || then <= now) return null;
  return formatDuration(then - now);
}

function LimitItemView({ resetAt, autoResume }: { resetAt: string | null; autoResume: boolean }) {
  const { t } = useTranslation("thread");
  const time = untilReset(resetAt);
  return (
    <div className="q-banner" style={{ margin: "0 0 12px" }}>
      <span className="ic q-warn">
        <ClockIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        <b>{t("turn.limit.title")}</b>{" "}
        {autoResume
          ? time
            ? t("turn.limit.armedIn", { time })
            : t("turn.limit.armed")
          : time
            ? t("turn.limit.offAfter", { time })
            : t("turn.limit.off")}
      </span>
    </div>
  );
}

function DelegatedItemView({ item }: { item: DelegatedItem }) {
  const { t } = useTranslation("thread");
  const glyph = glyphForAgentState(item.agentState);
  return (
    <div className="q-sub-row">
      <AgentGlyph glyph={glyph} size={14} />
      <span className="t">
        <span className="q-faint">{t("turn.delegated")}</span> {item.task}
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
        {t("turn.open")}
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
  const { t } = useTranslation("thread");
  return (
    <div className="q-banner err" style={{ margin: "0 0 12px" }}>
      <span className="ic">
        <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        <b>{message}</b>
        {hint ? ` ${hint}` : ""}
        {retryable ? ` ${t("turn.retryable")}` : ""}
      </span>
    </div>
  );
}

function PlanItemView({ item }: { item: PlanItem }) {
  const { t } = useTranslation("thread");
  return (
    <div className="q-plan">
      <ul aria-label={t("turn.plan")}>
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
                  <CheckIcon className="q-icon s14" strokeWidth={2} />
                ) : status === "in_progress" ? (
                  <CircleDotIcon className="q-icon s14" strokeWidth={1.75} />
                ) : (
                  <CircleIcon className="q-icon s14" strokeWidth={1.5} />
                )}
              </span>
              <span className="q-plan-text">{step.content}</span>
              {status === "completed" ? (
                <span className="sr-only">{t("turn.planDone")}</span>
              ) : null}
              {status === "in_progress" ? (
                <span className="sr-only">{t("turn.planInProgress")}</span>
              ) : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function HarnessSwitchLine({ item }: { item: HarnessSwitchItem }) {
  const { t } = useTranslation("thread");
  return (
    <div className="q-switch-line">
      <span className="q-switch-rule" aria-hidden />
      <span>
        {t("turn.switched", { from: harnessLabel(item.from), to: harnessLabel(item.to) })}
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
  turns,
  threadId,
  summary,
  onReply,
  onEditFromHere,
  editDisabledReason = null,
  staleAt = null,
}: {
  turn: StreamTurn;
  /** Every row in the stream, so a cue can look up the turn it belongs to. */
  turns: StreamTurn[];
  threadId: string;
  summary: ThreadSummary;
  /** Focus the composer so the person can reply to a stopped or failed turn. */
  onReply: () => void;
  /** Rewind to before one of this turn's sent prompts. */
  onEditFromHere?: ((item: MessageItem) => void) | undefined;
  /** Why the action is unavailable right now, if it is. */
  editDisabledReason?: string | null | undefined;
  staleAt?: number | null;
}) {
  const { t } = useTranslation("thread");
  const userItems = turn.items.filter(
    (item): item is MessageItem => item.kind === "message" && item.role === "user",
  );
  // Only a turn's first user item is its cue. A steer is journaled as another
  // user item inside the running turn, and editing it would also drop the cue
  // before it, so the action is offered on the cue alone.
  const cueItemId = userItems[0]?.itemId;
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
  const forkTurnId = forkableTurn(turn, turns);
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
          <UserMessage
            key={item.itemId}
            item={item}
            at={turn.startedAt}
            onEdit={item.itemId === cueItemId ? onEditFromHere : undefined}
            editDisabledReason={editDisabledReason}
            fork={
              forkTurnId
                ? {
                    threadId,
                    turnId: forkTurnId,
                    label: t("turn.forkFromTurn"),
                  }
                : undefined
            }
          />
        ))}
        {stopped !== null ? (
          <div className={cn("q-live-line", "q-warn")}>
            <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            <span>{stopped}</span>
            <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReply}>
              <CornerDownLeftIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {t("turn.reply")}
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
        <UserMessage
          key={`${turn.id}:${item.itemId}`}
          item={item}
          at={turn.startedAt}
          onEdit={item.itemId === cueItemId ? onEditFromHere : undefined}
          editDisabledReason={editDisabledReason}
        />
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
        <span>{t("turn.onHost", { host: summary.host })}</span>
        <span>·</span>
        <span className="q-num">{absoluteTime(turn.startedAt)}</span>
        {forkTurnId ? (
          <TurnMenu threadId={threadId} turnId={forkTurnId} label={t("turn.actions")} />
        ) : null}
      </div>

      {content}

      {turn.changes && turn.changes.files.length > 0 ? (
        <ChangeSetCard turn={turn} summary={summary} />
      ) : null}

      {staleAt !== null && turn.status === "running" && !turn.imported ? (
        <div className="q-live-line">
          <PauseIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>{t("turn.paused", { time: absoluteTime(new Date(staleAt).toISOString()) })}</span>
        </div>
      ) : live ? (
        <div className="q-live-line">
          <AgentGlyph glyph={glyph} size={14} />
          <span>{t("turn.live", { state: glyph.label })}</span>
        </div>
      ) : null}

      {stopped !== null ? (
        <div className={cn("q-live-line", "q-warn")}>
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>{stopped}</span>
          <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReply}>
            <CornerDownLeftIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("turn.reply")}
          </button>
        </div>
      ) : failed ? (
        <div className={cn("q-live-line", "q-danger")}>
          <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>{turn.status === "failed" ? t("turn.failed") : t("turn.interrupted")}</span>
          <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReply}>
            <CornerDownLeftIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("turn.reply")}
          </button>
        </div>
      ) : null}
    </div>
  );
}
