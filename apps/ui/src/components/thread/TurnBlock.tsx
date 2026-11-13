import { CircleAlertIcon, ClockIcon, HandIcon, RotateCcwIcon } from "lucide-react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ALT, Kbd, SHIFT } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import type { ThreadSummary } from "@/gen";
import { useSmoothText } from "@/hooks/use-smooth-text";
import { glyphForAgentState } from "@/lib/agent";
import { harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { absoluteTime, cost, countdown, duration, tokens, tokensPerSecond } from "@/lib/format";
import type { MessageItem, StreamTurn, TurnItem } from "@/lib/store/projection";
import { cn } from "@/lib/utils";
import { ApprovalCard } from "./ApprovalCard";
import { Markdown } from "./Markdown";
import { ToolCard } from "./ToolCard";

function AssistantMessage({ item, streaming }: { item: MessageItem; streaming: boolean }) {
  const text = useSmoothText(item.text, streaming);
  return <Markdown text={text} />;
}

function UserMessage({ item, at }: { item: MessageItem; at: string }) {
  return (
    <div className="flex flex-col items-end gap-1">
      <div className="flex items-center gap-2 text-micro text-ink-faint">
        <span className="font-medium text-ink-muted">You</span>
        <span className="font-mono tabular">{absoluteTime(at)}</span>
      </div>
      <div className="max-w-[68ch] rounded-md border border-line bg-panel px-3 py-2 text-compact leading-relaxed whitespace-pre-wrap text-ink">
        {item.text}
      </div>
    </div>
  );
}

function TurnUsage({ turn }: { turn: StreamTurn }) {
  if (!turn.usage) return null;
  const total = turn.usage.input_tokens + turn.usage.output_tokens;
  return (
    <p className="flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-micro tabular text-ink-faint">
      <span>{tokens(total)} tok</span>
      <span aria-hidden>·</span>
      <span>{cost(turn.usage.cost_micros)}</span>
      <span aria-hidden>·</span>
      <span>{tokensPerSecond(turn.usage.output_tokens, turn.usage.duration_ms)} TPS</span>
      <span aria-hidden>·</span>
      <span>{duration(turn.usage.duration_ms)}</span>
    </p>
  );
}

function LimitItemView({ resetAt, autoResume }: { resetAt: string | null; autoResume: boolean }) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-strike/30 bg-strike-tint/50 px-3 py-2">
      <ClockIcon className="mt-0.5 size-4 shrink-0 text-strike-ink" aria-hidden />
      <div className="space-y-0.5">
        <p className="text-compact font-medium text-strike-ink">Usage limit hit</p>
        <p className="text-micro text-strike-ink/80">
          {autoResume
            ? `Auto-resume is armed${resetAt ? `; it resumes in ${countdown(resetAt)}` : ""}. Cancel it with ${ALT} R.`
            : `Auto-resume is off. Resume manually${resetAt ? ` after ${countdown(resetAt)}` : ""}.`}
        </p>
      </div>
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
    <div className="flex items-start gap-2 rounded-md border border-strike/40 bg-strike-tint px-3 py-2">
      <CircleAlertIcon className="mt-0.5 size-4 shrink-0 text-strike-ink" aria-hidden />
      <div className="min-w-0 space-y-0.5">
        <p className="text-compact font-medium text-strike-ink">{message}</p>
        {hint ? <p className="text-micro text-strike-ink/80">{hint}</p> : null}
        {retryable ? (
          <p className="pt-0.5 text-micro text-strike-ink/80">
            The turn can be retried from the composer.
          </p>
        ) : null}
      </div>
    </div>
  );
}

function Item({ item, threadId, at }: { item: TurnItem; threadId: string; at: string }) {
  switch (item.kind) {
    case "message":
      return item.role === "user" ? (
        <UserMessage item={item} at={at} />
      ) : (
        <AssistantMessage item={item} streaming={item.streaming} />
      );
    case "tool":
      return <ToolCard toolCall={item.toolCall} />;
    case "approval":
      return (
        <ApprovalCard
          threadId={threadId}
          approvalId={item.approvalId}
          approvalKind={item.approvalKind}
          prompt={item.prompt}
          options={item.options}
        />
      );
    case "limit":
      return <LimitItemView resetAt={item.resetAt} autoResume={item.autoResume} />;
    case "error":
      return <ErrorItemView message={item.message} hint={item.hint} retryable={item.retryable} />;
  }
}

export function TurnBlock({
  turn,
  index,
  threadId,
  summary,
  current,
  onRetry,
}: {
  turn: StreamTurn;
  index: number;
  threadId: string;
  summary: ThreadSummary;
  current: boolean;
  onRetry: () => void;
}) {
  const isUserTurn =
    turn.items.length > 0 &&
    turn.items.every((item) => item.kind === "message" && item.role === "user");
  const runningTool = [...turn.items]
    .reverse()
    .find((item) => item.kind === "tool" && item.toolCall.status === "running");
  const toolName = runningTool?.kind === "tool" ? runningTool.toolCall.name : undefined;
  const glyph = glyphForAgentState(summary.agent_state, toolName);
  const showHeader = !isUserTurn;

  return (
    <div
      data-cue={index + 1}
      className="group/cue relative grid scroll-mt-4 grid-cols-[32px_minmax(0,1fr)] gap-2 pr-3 pl-1 sm:grid-cols-[48px_minmax(0,1fr)] sm:gap-3 sm:pr-6 sm:pl-2"
    >
      <div className="flex flex-col items-end pt-0.5">
        <span
          className={cn(
            "font-mono text-micro tabular",
            current ? "text-cue-ink" : "text-ink-faint",
            turn.status === "failed" || turn.status === "interrupted" ? "line-through" : "",
          )}
        >
          {(index + 1).toString().padStart(2, "0")}
        </span>
        {current ? <span aria-hidden className="mt-1 h-4 w-0.5 rounded-sm bg-cue" /> : null}
      </div>

      <div className="min-w-0 space-y-2 pb-4">
        {showHeader ? (
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-micro text-ink-muted">
            <AgentGlyph glyph={glyph} size={16} />
            <EntityAvatar
              seed={harnessSeed(summary.host, summary.harness)}
              kind="harness"
              label={harnessLabel(summary.harness)}
              size={14}
            />
            <span className="font-medium text-ink">{harnessLabel(summary.harness)}</span>
            <EntityAvatar
              seed={machineSeed(summary.host)}
              kind="machine"
              label={summary.host}
              size={12}
            />
            <span>{summary.host}</span>
            <span aria-hidden className="text-ink-faint">
              ·
            </span>
            <span>{glyph.label.toLowerCase()}</span>
            <span className="ml-auto font-mono tabular text-ink-faint">
              {absoluteTime(turn.startedAt)}
            </span>
          </div>
        ) : null}

        {turn.items.map((item) => (
          <Item
            key={`${turn.id}:${item.itemId}`}
            item={item}
            threadId={threadId}
            at={turn.startedAt}
          />
        ))}

        {turn.status !== "running" ? <TurnUsage turn={turn} /> : null}

        {turn.status === "failed" || turn.status === "interrupted" ? (
          <div className="flex items-center gap-2 text-micro text-strike-ink">
            <HandIcon className="size-3.5" aria-hidden />
            <span>
              {turn.status === "failed" ? "The turn failed" : "The turn was interrupted"}. Nothing
              after this ran.
            </span>
            <Button size="sm" variant="ghost" className="h-6 px-1.5" onClick={onRetry}>
              <RotateCcwIcon aria-hidden />
              Retry
              <Kbd>{SHIFT} R</Kbd>
            </Button>
          </div>
        ) : null}
      </div>
    </div>
  );
}
