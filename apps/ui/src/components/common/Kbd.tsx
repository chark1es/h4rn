import type * as React from "react";

import { cn } from "@/lib/utils";

/** Key glyphs as strokes, so every platform's font can render them. */
const KEY_GLYPHS: Record<string, React.ReactNode> = {
  "⌘": <path d="M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3" />,
  "↵": (
    <>
      <polyline points="9 10 4 15 9 20" />
      <path d="M20 4v7a4 4 0 0 1-4 4H4" />
    </>
  ),
  "⌫": (
    <>
      <path d="M10 5a2 2 0 0 0-1.344.519l-6.328 5.74a1 1 0 0 0 0 1.481l6.328 5.741A2 2 0 0 0 10 19h10a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2z" />
      <path d="m12 9 6 6" />
      <path d="m18 9-6 6" />
    </>
  ),
  "⇧": <path d="M9 18v-6H5l7-7 7 7h-4v6H9z" />,
  "↑": (
    <>
      <path d="m5 12 7-7 7 7" />
      <path d="M12 19V5" />
    </>
  ),
  "↓": (
    <>
      <path d="M12 5v14" />
      <path d="m19 12-7 7-7-7" />
    </>
  ),
};

const KEY_LABELS: Record<string, string> = {
  "⌘": "Command",
  "↵": "Return",
  "⌫": "Backspace",
  "⇧": "Shift",
  "↑": "Up",
  "↓": "Down",
};

function keyLabel(value: string): string {
  return [...value].map((char) => KEY_LABELS[char] ?? char).join("");
}

/** Render keycap text, swapping glyph characters for inline strokes. */
export function KeyGlyphs({ value }: { value: string }) {
  const nodes: React.ReactNode[] = [];
  for (const char of value) {
    const key = `${char}-${nodes.length}`;
    nodes.push(
      KEY_GLYPHS[char] ? (
        <svg key={key} viewBox="0 0 24 24" className="q-kbd-glyph" aria-hidden="true">
          {KEY_GLYPHS[char]}
        </svg>
      ) : (
        <span key={key}>{char}</span>
      ),
    );
  }
  return <>{nodes}</>;
}

export function Kbd({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <kbd
      className={cn(
        "inline-flex h-5 min-w-5 items-center justify-center gap-0.5 rounded-[3px] border border-line bg-bg-side px-1 font-mono text-[10px] leading-none whitespace-nowrap text-fg-2",
        className,
      )}
      aria-label={typeof children === "string" ? keyLabel(children) : undefined}
    >
      {typeof children === "string" ? <KeyGlyphs value={children} /> : children}
    </kbd>
  );
}

export const isApple =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/** Platform-appropriate key labels. Glyphs only where the platform shows them. */
export const MOD = isApple ? "⌘" : "Ctrl";
export const SHIFT = isApple ? "⇧" : "Shift";
export const ALT = isApple ? "⌥" : "Alt";
export const ENTER = "↵";
export const BACKSPACE = "⌫";
export const ESC = "Esc";

export function shortcut(key: string): string {
  return isApple ? `${MOD} ${key}` : `${MOD}+${key}`;
}
