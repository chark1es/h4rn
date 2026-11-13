import { Check, Clock, Hand, WifiOff } from "lucide-react";
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

const ORB_SIZE = (size: number): 20 | 32 | 64 => (size >= 48 ? 64 : size >= 28 ? 32 : 20);

export function AgentGlyph({
  glyph,
  size = 20,
  className,
  labelled = false,
  title,
}: AgentGlyphProps) {
  if (glyph.kind === "orb") {
    return (
      <span
        className={cn("inline-flex shrink-0 items-center justify-center", className)}
        style={{ width: size, height: size }}
        aria-hidden={labelled ? undefined : true}
      >
        <ThinkingOrb
          state={glyph.state}
          size={ORB_SIZE(size)}
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

  const iconSize = Math.max(8, Math.round(size * 0.54));
  const Icon =
    glyph.kind === "check"
      ? Check
      : glyph.kind === "offline"
        ? WifiOff
        : glyph.kind === "limit"
          ? Clock
          : Hand;

  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-full border",
        glyph.kind === "check" &&
          (glyph.dim
            ? "border-ink-faint/50 text-ink-faint/70"
            : "border-standby/70 text-standby-ink"),
        glyph.kind === "strike" && "border-strike/70 text-strike-ink",
        glyph.kind === "limit" && "border-strike/70 text-strike-ink",
        glyph.kind === "offline" && "border-ink-faint/60 text-ink-faint",
        className,
      )}
      style={{ width: size, height: size }}
      role="img"
      aria-label={labelled ? (title ?? glyph.label) : undefined}
      aria-hidden={labelled ? undefined : true}
      title={title ?? glyph.label}
    >
      <Icon size={iconSize} strokeWidth={1.75} aria-hidden />
    </span>
  );
}
