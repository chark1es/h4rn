import { ChevronRightIcon } from "lucide-react";
import { useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import type { ToolCall } from "@/gen";
import { glyphForToolStatus } from "@/lib/agent";
import { duration } from "@/lib/format";
import { cn } from "@/lib/utils";

function stringify(value: unknown): string {
  if (value === null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function verbOf(name: string): string {
  const word = name.replace(/[_-]+/g, " ");
  return word.length > 9 ? `${word.slice(0, 8)}…` : word;
}

export function ToolCard({ toolCall }: { toolCall: ToolCall }) {
  const [open, setOpen] = useState(false);
  const glyph = glyphForToolStatus(toolCall.status);
  const elapsed = toolCall.completed_at
    ? Date.parse(toolCall.completed_at) - Date.parse(toolCall.started_at)
    : null;
  const output = [toolCall.input !== null ? stringify(toolCall.input) : "", toolCall.output ?? ""]
    .filter(Boolean)
    .join("\n\n");
  const hasDetails = output.length > 0;
  const failed = toolCall.status === "failed" || toolCall.status === "denied";

  return (
    <>
      <button
        type="button"
        className="q-tool"
        aria-expanded={open}
        disabled={!hasDetails}
        onClick={() => setOpen((value) => !value)}
      >
        <span className={cn("st", failed && "q-danger")}>
          <AgentGlyph glyph={glyph} size={14} />
        </span>
        <span className="verb">{verbOf(toolCall.name)}</span>
        <span className="tgt">{toolCall.summary}</span>
        <span className={cn("m q-num", failed && "q-danger")}>
          {elapsed !== null && elapsed >= 0 ? duration(elapsed) : toolCall.status}
        </span>
        <ChevronRightIcon className="q-icon s14 chev" strokeWidth={1.5} aria-hidden />
      </button>
      {open && hasDetails ? <div className="q-tool-out">{output}</div> : null}
    </>
  );
}
