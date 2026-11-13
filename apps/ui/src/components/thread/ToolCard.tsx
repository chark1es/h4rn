import { ChevronRightIcon } from "lucide-react";
import { useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import type { ToolCall } from "@/gen";
import { glyphForToolStatus } from "@/lib/agent";
import { duration } from "@/lib/format";
import { cn } from "@/lib/utils";

const STATUS_WORD: Record<ToolCall["status"], string> = {
  running: "Running",
  completed: "Completed",
  failed: "Failed",
  denied: "Denied",
};

function stringify(value: unknown): string {
  if (value === null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

export function ToolCard({ toolCall }: { toolCall: ToolCall }) {
  const [open, setOpen] = useState(false);
  const glyph = glyphForToolStatus(toolCall.status);
  const elapsed = toolCall.completed_at
    ? Date.parse(toolCall.completed_at) - Date.parse(toolCall.started_at)
    : null;
  const hasDetails = toolCall.input !== null || Boolean(toolCall.output);

  return (
    <div
      className={cn(
        "rounded-sm border border-line/80 bg-panel/60",
        toolCall.status === "failed" || toolCall.status === "denied"
          ? "border-strike/40 bg-strike-tint/40"
          : "",
      )}
    >
      <button
        type="button"
        className="flex h-7 w-full items-center gap-2 px-2 text-left transition-colors duration-[var(--dur-instant)] hover:bg-panel"
        aria-expanded={open}
        disabled={!hasDetails}
        onClick={() => setOpen((value) => !value)}
      >
        <ChevronRightIcon
          className={cn("size-3 shrink-0 text-ink-faint transition-transform", open && "rotate-90")}
          aria-hidden
        />
        <AgentGlyph glyph={glyph} size={14} />
        <span className="shrink-0 font-mono text-micro font-medium text-ink">{toolCall.name}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-micro text-ink-muted">
          {toolCall.summary}
        </span>
        <span
          className={cn(
            "hidden shrink-0 text-micro sm:inline",
            toolCall.status === "failed" || toolCall.status === "denied"
              ? "text-strike-ink"
              : "text-ink-faint",
          )}
        >
          {STATUS_WORD[toolCall.status]}
        </span>
        {elapsed !== null && elapsed >= 0 ? (
          <span className="shrink-0 font-mono text-micro tabular text-ink-faint">
            {duration(elapsed)}
          </span>
        ) : null}
      </button>

      {open && hasDetails ? (
        <div className="space-y-2 border-t border-line/70 px-2 py-2">
          {toolCall.input !== null ? (
            <div>
              <p className="mb-1 text-label text-ink-faint">Arguments</p>
              <pre className="max-h-40 overflow-auto rounded-sm bg-raised p-2 font-mono text-micro leading-relaxed text-ink-muted">
                {stringify(toolCall.input)}
              </pre>
            </div>
          ) : null}
          {toolCall.output ? (
            <div>
              <p className="mb-1 text-label text-ink-faint">Result</p>
              <pre className="max-h-52 overflow-auto rounded-sm bg-raised p-2 font-mono text-micro leading-relaxed text-ink-muted">
                {toolCall.output}
              </pre>
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
