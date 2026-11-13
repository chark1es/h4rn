import type * as React from "react";

import { cn } from "@/lib/utils";

export function Kbd({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <kbd
      className={cn(
        "inline-flex h-5 min-w-5 items-center justify-center rounded-[3px] border border-line bg-panel px-1 font-mono text-[10px] leading-none text-ink-muted",
        className,
      )}
    >
      {children}
    </kbd>
  );
}

export const isApple =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/** Platform-appropriate key labels. Glyphs only where the platform shows them. */
export const MOD = isApple ? "⌘" : "Ctrl";
export const SHIFT = isApple ? "⇧" : "Shift";
export const ALT = isApple ? "⌥" : "Alt";
export const ENTER = isApple ? "⏎" : "Enter";
export const BACKSPACE = isApple ? "⌫" : "Backspace";
export const ESC = "Esc";

export function shortcut(key: string): string {
  return isApple ? `${MOD} ${key}` : `${MOD}+${key}`;
}
