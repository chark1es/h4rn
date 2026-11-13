import { Check, CircleCheck, CircleX, Clock, Pause, WifiOff } from "lucide-react";
import { ThinkingOrb } from "thinking-orbs";

import type { AgentGlyphSpec } from "@/lib/agent";
import { cn } from "@/lib/utils";

type AgentGlyphProps = {
  glyph: AgentGlyphSpec;
  size?: number;
  className?: string;
  /** Set when no adjacent word names the state. */
  labelled?: boolean;
  title?: string;
};

export function AgentGlyph({
  glyph,
  size = 14,
  className,
  labelled = false,
  title,
}: AgentGlyphProps) {
  if (glyph.kind === "you") {
    return (
      <span
        className={cn("inline-flex shrink-0 items-center justify-center", className)}
        style={{ width: size, height: size }}
        aria-hidden={labelled ? undefined : true}
        {...(labelled ? { role: "img", "aria-label": title ?? glyph.label } : {})}
        title={title ?? glyph.label}
      >
        <span className="q-dot you" aria-hidden />
      </span>
    );
  }

  if (glyph.kind === "orb") {
    return (
      <span
        className={cn("inline-flex shrink-0 items-center justify-center", className)}
        style={{ width: size, height: size }}
        aria-hidden={labelled ? undefined : true}
      >
        <ThinkingOrb
          state={glyph.state}
          size={20}
          {...(glyph.speed !== undefined ? { speed: glyph.speed } : {})}
          {...(glyph.paused !== undefined ? { paused: glyph.paused } : {})}
          {...(glyph.opts !== undefined ? { opts: glyph.opts } : {})}
          style={{ width: size, height: size }}
          {...(labelled ? { "aria-label": title ?? glyph.label } : {})}
          title={title ?? glyph.label}
        />
      </span>
    );
  }

  const iconSize = Math.max(10, Math.round(size));
  const Icon =
    glyph.kind === "check"
      ? glyph.dim
        ? Check
        : CircleCheck
      : glyph.kind === "offline"
        ? WifiOff
        : glyph.kind === "limit"
          ? Clock
          : glyph.kind === "pause"
            ? Pause
            : CircleX;

  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center",
        glyph.kind === "check" && (glyph.dim ? "q-faint" : "q-muted"),
        glyph.kind === "strike" && "q-danger",
        glyph.kind === "limit" && "q-warn",
        glyph.kind === "pause" && "q-warn",
        glyph.kind === "offline" && "q-danger",
        className,
      )}
      style={{ width: size, height: size }}
      {...(labelled
        ? { role: "img", "aria-label": title ?? glyph.label }
        : { "aria-hidden": true })}
      title={title ?? glyph.label}
    >
      <Icon size={iconSize} strokeWidth={1.5} aria-hidden />
    </span>
  );
}
