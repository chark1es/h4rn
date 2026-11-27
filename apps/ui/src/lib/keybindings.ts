import { useSyncExternalStore } from "react";
import { i18n, type Locale } from "@/lib/i18n";

/**
 * Every app shortcut is a modifier chord: Cmd on Apple platforms, Ctrl
 * elsewhere, plus optional Shift/Alt and one key. Single-key shortcuts are
 * not allowed, so typing in any field can never trigger a command.
 *
 * Bindings are stored as `mod+shift+alt+key` strings (modifiers in that
 * order, key lowercase). People can rebind any command in Settings →
 * Shortcuts; overrides live in localStorage on this device.
 */

export type Chord = { mod: boolean; shift: boolean; alt: boolean; key: string };

export type CommandGroup = "General" | "Navigate" | "Thread" | "Panels";

export type CommandId =
  | "palette.toggle"
  | "shortcuts.open"
  | "thread.new"
  | "theme.toggle"
  | "settings.open"
  | "sidebar.toggle"
  | "panel.toggle"
  | "view.toggle"
  | "nav.inbox"
  | "nav.pulls"
  | "nav.issues"
  | "nav.usage"
  | "nav.tools"
  | "tab.working"
  | "tab.done"
  | "tab.settled"
  | "thread.next"
  | "thread.previous"
  | "thread.reply"
  | "thread.permissions"
  | "approval.approve"
  | "approval.deny"
  | "turn.interrupt"
  | "turn.next"
  | "turn.previous"
  | "thread.settle"
  | "thread.acknowledge"
  | "thread.snooze"
  | "thread.pin"
  | "thread.reopen"
  | "guest.share"
  | "terminal.toggle"
  | "panel.changes"
  | "panel.subagents"
  | "panel.files"
  | "composer.attach"
  | "thread.steerQueuedMessage"
  | "thread.editQueuedMessage";

/**
 * A command's name is not kept here: it lives once, per language, in the
 * `commands` catalogue (`commands:names.<id>`), which the command palette,
 * Settings → Shortcuts and the shortcuts dialog all read through
 * `commandName`. A command without a name fails `bun run typecheck`.
 */
export type CommandSpec = {
  id: CommandId;
  group: CommandGroup;
  binding: string;
  /** Fires while focus is in a text field or editor. */
  inInputs: boolean;
  /**
   * Allows a chord without Cmd/Ctrl, as long as Option/Alt is held.
   *
   * `Alt+Up` is a text-editing key in most fields, so it is only offered by a
   * command whose own handler decides when it applies (the composer's cursor is
   * at the start).
   */
  allowBareAlt?: boolean;
};

export const COMMANDS: readonly CommandSpec[] = [
  {
    id: "palette.toggle",
    group: "General",
    binding: "mod+k",
    inInputs: true,
  },
  {
    id: "shortcuts.open",
    group: "General",
    binding: "mod+/",
    inInputs: false,
  },
  {
    id: "thread.new",
    group: "General",
    binding: "mod+shift+n",
    inInputs: true,
  },
  { id: "settings.open", group: "General", binding: "mod+,", inInputs: true },
  {
    id: "theme.toggle",
    group: "General",
    binding: "mod+shift+l",
    inInputs: true,
  },
  {
    id: "nav.inbox",
    group: "Navigate",
    binding: "mod+shift+h",
    inInputs: true,
  },
  {
    id: "nav.pulls",
    group: "Navigate",
    binding: "mod+shift+p",
    inInputs: true,
  },
  {
    id: "nav.issues",
    group: "Navigate",
    binding: "mod+shift+i",
    inInputs: true,
  },
  {
    id: "nav.usage",
    group: "Navigate",
    binding: "mod+shift+u",
    inInputs: true,
  },
  {
    id: "nav.tools",
    group: "Navigate",
    binding: "mod+shift+m",
    inInputs: true,
  },
  { id: "tab.working", group: "Navigate", binding: "mod+1", inInputs: true },
  { id: "tab.done", group: "Navigate", binding: "mod+2", inInputs: true },
  { id: "tab.settled", group: "Navigate", binding: "mod+3", inInputs: true },
  {
    id: "thread.next",
    group: "Navigate",
    binding: "mod+alt+arrowdown",
    inInputs: true,
  },
  {
    id: "thread.previous",
    group: "Navigate",
    binding: "mod+alt+arrowup",
    inInputs: true,
  },
  {
    id: "thread.reply",
    group: "Thread",
    binding: "mod+l",
    inInputs: true,
  },
  {
    id: "thread.permissions",
    group: "Thread",
    binding: "mod+shift+a",
    inInputs: true,
  },
  {
    id: "approval.approve",
    group: "Thread",
    binding: "mod+enter",
    inInputs: true,
  },
  {
    id: "approval.deny",
    group: "Thread",
    binding: "mod+backspace",
    inInputs: true,
  },
  {
    id: "turn.interrupt",
    group: "Thread",
    binding: "mod+.",
    inInputs: true,
  },
  {
    id: "turn.previous",
    group: "Thread",
    binding: "mod+[",
    inInputs: false,
  },
  { id: "turn.next", group: "Thread", binding: "mod+]", inInputs: false },
  {
    id: "thread.settle",
    group: "Thread",
    binding: "mod+shift+s",
    inInputs: true,
  },
  {
    id: "thread.acknowledge",
    group: "Thread",
    binding: "mod+alt+a",
    inInputs: true,
  },
  {
    id: "thread.snooze",
    group: "Thread",
    binding: "mod+alt+s",
    inInputs: true,
  },
  {
    id: "thread.pin",
    group: "Thread",
    binding: "mod+alt+p",
    inInputs: true,
  },
  {
    id: "guest.share",
    group: "Thread",
    binding: "mod+alt+g",
    inInputs: true,
  },
  {
    id: "thread.reopen",
    group: "Thread",
    binding: "mod+alt+u",
    inInputs: true,
  },
  {
    id: "composer.attach",
    group: "Thread",
    binding: "mod+u",
    inInputs: true,
  },
  {
    id: "thread.steerQueuedMessage",
    group: "Thread",
    binding: "mod+shift+enter",
    inInputs: true,
  },
  {
    id: "thread.editQueuedMessage",
    group: "Thread",
    binding: "alt+arrowup",
    inInputs: true,
    allowBareAlt: true,
  },
  {
    id: "sidebar.toggle",
    group: "Panels",
    binding: "mod+b",
    inInputs: false,
  },
  {
    id: "panel.toggle",
    group: "Panels",
    binding: "mod+alt+b",
    inInputs: true,
  },
  {
    id: "view.toggle",
    group: "Panels",
    binding: "mod+e",
    inInputs: false,
  },
  {
    id: "terminal.toggle",
    group: "Panels",
    binding: "mod+`",
    inInputs: true,
  },
  {
    id: "panel.changes",
    group: "Panels",
    binding: "mod+shift+d",
    inInputs: true,
  },
  {
    id: "panel.subagents",
    group: "Panels",
    binding: "mod+j",
    inInputs: false,
  },
  { id: "panel.files", group: "Panels", binding: "mod+p", inInputs: false },
];

export const COMMAND_GROUPS: readonly CommandGroup[] = ["General", "Navigate", "Thread", "Panels"];

const SPEC = new Map(COMMANDS.map((command) => [command.id, command]));

/**
 * The command's name in the active language, or in `lng` when given. The
 * command palette also searches the English name (`commandName(id, "en")`),
 * so a shortcut found by its English name keeps working in any language.
 */
export function commandName(id: CommandId, lng?: Locale): string {
  return i18n.t(`commands:names.${id}`, lng ? { lng } : {});
}

/** A command group's heading in the active language. */
export function commandGroupName(group: CommandGroup): string {
  return i18n.t(`commands:groups.${group}`);
}

export function commandSpec(id: CommandId): CommandSpec {
  const spec = SPEC.get(id);
  // i18n-ignore-next-line: CommandId is typed, so an unknown one is a bug
  if (!spec) throw new Error(`Unknown command ${id}`);
  return spec;
}

// ---------------------------------------------------------------------------
// Chords
// ---------------------------------------------------------------------------

const CODE_KEYS: Record<string, string> = {
  Backquote: "`",
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  Enter: "enter",
  NumpadEnter: "enter",
  Backspace: "backspace",
  Delete: "delete",
  Space: "space",
  Tab: "tab",
  ArrowUp: "arrowup",
  ArrowDown: "arrowdown",
  ArrowLeft: "arrowleft",
  ArrowRight: "arrowright",
};

const MODIFIER_KEYS = new Set(["Control", "Meta", "Shift", "Alt", "AltGraph", "OS", "Hyper"]);

/**
 * The physical key, so Shift and Option (which change `event.key` on most
 * layouts: ⌥B is "∫", ⇧/ is "?") never change which command matches.
 */
export function eventKey(event: Pick<KeyboardEvent, "code" | "key">): string | null {
  if (MODIFIER_KEYS.has(event.key)) return null;
  const code = event.code ?? "";
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^Numpad[0-9]$/.test(code)) return code.slice(6);
  if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase();
  const mapped = CODE_KEYS[code];
  if (mapped) return mapped;
  const key = (event.key ?? "").toLowerCase();
  return key.length > 0 && key !== "unidentified" && key !== "dead" ? key : null;
}

export function chordFromEvent(
  event: Pick<KeyboardEvent, "code" | "key" | "metaKey" | "ctrlKey" | "shiftKey" | "altKey">,
): Chord | null {
  const key = eventKey(event);
  if (!key) return null;
  // Option/Alt alone is only meaningful for a command that opted in; no other
  // command binds such a chord, so matching stays unambiguous.
  if (!(event.metaKey || event.ctrlKey)) {
    if (!event.altKey) return null;
    return { mod: false, shift: event.shiftKey, alt: true, key };
  }
  return { mod: true, shift: event.shiftKey, alt: event.altKey, key };
}

export function formatChord(chord: Chord): string {
  return [
    chord.mod ? "mod" : null,
    chord.shift ? "shift" : null,
    chord.alt ? "alt" : null,
    chord.key,
  ]
    .filter(Boolean)
    .join("+");
}

export function parseChord(value: string): Chord | null {
  const parts = value.toLowerCase().split("+");
  // "mod++" binds the plus key.
  if (value.endsWith("++")) parts.splice(parts.length - 2, 2, "+");
  const key = parts.pop();
  if (!key) return null;
  const mods = new Set(parts);
  for (const mod of mods) if (mod !== "mod" && mod !== "shift" && mod !== "alt") return null;
  // A chord with no Cmd/Ctrl must still be an Option chord; `rebind` further
  // limits it to the one command that opted in.
  if (!mods.has("mod") && !mods.has("alt")) return null;
  return {
    mod: mods.has("mod"),
    shift: mods.has("shift"),
    alt: mods.has("alt"),
    key,
  };
}

// ---------------------------------------------------------------------------
// Overrides
// ---------------------------------------------------------------------------

const STORAGE_KEY = "h4rn.keybindings";

function loadOverrides(): Partial<Record<CommandId, string>> {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const out: Partial<Record<CommandId, string>> = {};
    for (const [id, binding] of Object.entries(parsed)) {
      if (SPEC.has(id as CommandId) && typeof binding === "string" && parseChord(binding)) {
        out[id as CommandId] = binding;
      }
    }
    return out;
  } catch {
    return {};
  }
}

let overrides: Partial<Record<CommandId, string>> =
  typeof window === "undefined" ? {} : loadOverrides();
let version = 0;
const listeners = new Set<() => void>();

function commit(next: Partial<Record<CommandId, string>>): void {
  overrides = next;
  version += 1;
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Non-fatal: the binding still applies for this session.
  }
  for (const listener of listeners) listener();
}

export function bindingFor(id: CommandId): string {
  return overrides[id] ?? commandSpec(id).binding;
}

export function isCustomized(id: CommandId): boolean {
  return overrides[id] !== undefined && overrides[id] !== commandSpec(id).binding;
}

/** The command a chord already runs, other than `except`. */
export function commandForBinding(binding: string, except?: CommandId): CommandId | null {
  for (const command of COMMANDS) {
    if (command.id !== except && bindingFor(command.id) === binding) return command.id;
  }
  return null;
}

export type RebindResult = { ok: true } | { ok: false; reason: string };

export function rebind(id: CommandId, binding: string): RebindResult {
  const chord = parseChord(binding);
  if (!chord) return { ok: false, reason: i18n.t("settings:shortcuts.needsModifier") };
  if (!chord.mod && !commandSpec(id).allowBareAlt) {
    return { ok: false, reason: i18n.t("settings:shortcuts.needsModifier") };
  }
  if (!chord.mod && !chord.alt) {
    return { ok: false, reason: i18n.t("settings:shortcuts.needsModifier") };
  }
  const normalized = formatChord(chord);
  const taken = commandForBinding(normalized, id);
  if (taken) {
    return {
      ok: false,
      reason: i18n.t("settings:shortcuts.alreadyUsed", { command: commandName(taken) }),
    };
  }
  const next = { ...overrides };
  if (normalized === commandSpec(id).binding) delete next[id];
  else next[id] = normalized;
  commit(next);
  return { ok: true };
}

export function resetBinding(id: CommandId): void {
  const next = { ...overrides };
  delete next[id];
  commit(next);
}

export function resetAllBindings(): void {
  commit({});
}

export function matchCommand(chord: Chord): CommandId | null {
  return commandForBinding(formatChord(chord));
}

/** Re-renders when any binding changes. */
export function useKeybindingsVersion(): number {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => version,
    () => 0,
  );
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

export const isApplePlatform =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

const KEY_DISPLAY: Record<string, string> = {
  enter: "↵",
  backspace: "⌫",
  delete: "Del",
  arrowup: "↑",
  arrowdown: "↓",
  arrowleft: "←",
  arrowright: "→",
  space: "Space",
  tab: "Tab",
  escape: "Esc",
};

/** Keycap labels, one per key: `["⌘", "⇧", "N"]` or `["Ctrl", "Shift", "N"]`. */
export function chordKeys(binding: string, apple = isApplePlatform): string[] {
  const chord = parseChord(binding);
  if (!chord) return [];
  const keys = [];
  if (chord.mod) keys.push(apple ? "⌘" : "Ctrl");
  if (chord.alt) keys.push(apple ? "⌥" : "Alt");
  if (chord.shift) keys.push(apple ? "⇧" : "Shift");
  keys.push(
    KEY_DISPLAY[chord.key] ?? (chord.key.length === 1 ? chord.key.toUpperCase() : chord.key),
  );
  return keys;
}

/** Plain text for titles and tooltips: "Ctrl+Shift+N" or "⌘⇧N". */
export function chordText(binding: string, apple = isApplePlatform): string {
  const keys = chordKeys(binding, apple);
  return apple ? keys.join("") : keys.join("+");
}

const ARIA_KEYS: Record<string, string> = {
  enter: "Enter",
  backspace: "Backspace",
  delete: "Delete",
  arrowup: "ArrowUp",
  arrowdown: "ArrowDown",
  arrowleft: "ArrowLeft",
  arrowright: "ArrowRight",
  space: "Space",
  tab: "Tab",
  escape: "Escape",
};

/** The `aria-keyshortcuts` value for a binding: "Control+Alt+P" or "Meta+Alt+P". */
export function ariaKeyShortcut(binding: string, apple = isApplePlatform): string | undefined {
  const chord = parseChord(binding);
  if (!chord) return undefined;
  return [
    chord.mod ? (apple ? "Meta" : "Control") : null,
    chord.alt ? "Alt" : null,
    chord.shift ? "Shift" : null,
    ARIA_KEYS[chord.key] ?? (chord.key.length === 1 ? chord.key.toUpperCase() : chord.key),
  ]
    .filter(Boolean)
    .join("+");
}

export function shortcutText(id: CommandId): string {
  return chordText(bindingFor(id));
}

/** "New thread  Ctrl+Shift+N" for a button title. */
export function titleWithShortcut(label: string, id: CommandId): string {
  return `${label}  ${shortcutText(id)}`;
}
