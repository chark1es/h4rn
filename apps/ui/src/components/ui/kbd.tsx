import {
  ArrowBigUpIcon,
  ArrowDownIcon,
  ArrowLeftIcon,
  ArrowRightIcon,
  ArrowUpIcon,
  CommandIcon,
  CornerDownLeftIcon,
  DeleteIcon,
  type LucideIcon,
  OptionIcon,
} from "lucide-react";
import * as React from "react";

import {
  bindingFor,
  type CommandId,
  chordKeys,
  isApplePlatform,
  useKeybindingsVersion,
} from "@/lib/keybindings";
import { cn } from "@/lib/utils";

// Drawn as icons: Geist has no glyph for most of these, so they rendered as tofu.
const KEY_ICONS: Record<string, LucideIcon> = {
  "⌘": CommandIcon,
  "↵": CornerDownLeftIcon,
  "⌫": DeleteIcon,
  "⇧": ArrowBigUpIcon,
  "↑": ArrowUpIcon,
  "↓": ArrowDownIcon,
  "←": ArrowLeftIcon,
  "→": ArrowRightIcon,
  "⌥": OptionIcon,
};

const KEY_LABELS: Record<string, string> = {
  "⌘": "Command",
  "↵": "Return",
  "⌫": "Backspace",
  "⇧": "Shift",
  "↑": "Up",
  "↓": "Down",
  "←": "Left",
  "→": "Right",
  "⌥": "Option",
};

function keyLabel(value: string): string {
  return [...value].map((char) => KEY_LABELS[char] ?? char).join("");
}

function textOf(children: React.ReactNode): string | null {
  const parts = React.Children.toArray(children);
  if (parts.length === 0) return null;
  return parts.every((part) => typeof part === "string" || typeof part === "number")
    ? parts.join("")
    : null;
}

/** Keycap text with glyph characters swapped for icons. */
function KeyGlyphs({ value }: { value: string }) {
  const nodes: React.ReactNode[] = [];
  let text = "";
  const flush = () => {
    if (text) nodes.push(<span key={`t${nodes.length}`}>{text}</span>);
    text = "";
  };
  for (const char of value) {
    const Icon = KEY_ICONS[char];
    if (!Icon) {
      text += char;
      continue;
    }
    flush();
    nodes.push(<Icon key={`i${nodes.length}`} aria-hidden="true" strokeWidth={2} />);
  }
  flush();
  return <>{nodes}</>;
}

function Kbd({ className, children, ...props }: React.ComponentProps<"kbd">) {
  const text = textOf(children);
  return (
    <kbd
      data-slot="kbd"
      aria-label={text === null ? undefined : keyLabel(text)}
      className={cn(
        "pointer-events-none inline-flex h-4.5 min-w-4.5 shrink-0 items-center justify-center gap-0.5 rounded-sm border border-border px-1 font-sans text-xs leading-none font-medium whitespace-nowrap text-muted-foreground tabular-nums select-none",
        "[&_svg]:size-3 [&_svg]:shrink-0",
        "in-[[data-slot=button][data-variant=default]]:border-primary-foreground/30 in-[[data-slot=button][data-variant=default]]:text-primary-foreground",
        className,
      )}
      {...props}
    >
      {text === null ? children : <KeyGlyphs value={text} />}
    </kbd>
  );
}

function KbdGroup({ className, ...props }: React.ComponentProps<"span">) {
  return (
    <span
      data-slot="kbd-group"
      className={cn("inline-flex shrink-0 items-center gap-0.5", className)}
      {...props}
    />
  );
}

/** One keycap per key in a space-separated shortcut string. */
function KeyCaps({ value, className }: { value: string; className?: string | undefined }) {
  const seen = new Map<string, number>();
  const keys = value
    .split(/\s+/)
    .filter(Boolean)
    .map((key) => {
      const ordinal = seen.get(key) ?? 0;
      seen.set(key, ordinal + 1);
      return { key, id: `${key}-${ordinal}` };
    });
  return (
    <KbdGroup className={className}>
      {keys.map(({ key, id }) => (
        <Kbd key={id}>{key}</Kbd>
      ))}
    </KbdGroup>
  );
}

/** Keycaps for a command's current binding; follows rebinding live. */
function ShortcutCaps({
  command,
  className,
}: {
  command: CommandId;
  className?: string | undefined;
}) {
  useKeybindingsVersion();
  return <KeyCaps value={chordKeys(bindingFor(command)).join(" ")} className={className} />;
}

const isApple = isApplePlatform;

/** Platform-appropriate key labels. Glyphs only where the platform shows them. */
const MOD = isApple ? "⌘" : "Ctrl";
const SHIFT = isApple ? "⇧" : "Shift";
const ALT = isApple ? "⌥" : "Alt";
const ENTER = "↵";
const BACKSPACE = "⌫";
const ESC = "Esc";

function shortcut(key: string): string {
  return isApple ? `${MOD} ${key}` : `${MOD}+${key}`;
}

export {
  ALT,
  BACKSPACE,
  ENTER,
  ESC,
  Kbd,
  KbdGroup,
  KeyCaps,
  KeyGlyphs,
  MOD,
  SHIFT,
  ShortcutCaps,
  shortcut,
};
