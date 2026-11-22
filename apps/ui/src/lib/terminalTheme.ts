import { useSyncExternalStore } from "react";

const KEY = "h4rn.terminalMatchesApp";
const ATTR = "data-terminal-theme";

function read(): boolean {
  try {
    return window.localStorage.getItem(KEY) !== "false";
  } catch {
    return true;
  }
}

let matches = typeof window === "undefined" ? true : read();
const listeners = new Set<() => void>();

/** Off pins the terminal to its dark palette whatever the app theme is. */
export function applyTerminalTheme(): void {
  if (typeof document === "undefined") return;
  if (matches) document.documentElement.removeAttribute(ATTR);
  else document.documentElement.setAttribute(ATTR, "dark");
}

export function setTerminalMatchesApp(next: boolean): void {
  matches = next;
  try {
    window.localStorage.setItem(KEY, String(next));
  } catch {
    // Non-fatal: the choice still applies for this session.
  }
  applyTerminalTheme();
  for (const listener of listeners) listener();
}

export function useTerminalMatchesApp(): boolean {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => matches,
    () => true,
  );
}
