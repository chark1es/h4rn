import type { ITheme } from "@xterm/xterm";

/**
 * Maps the Quiet native palette onto xterm's ANSI slots.
 *
 * The terminal follows the app theme: the light theme uses the sunk surface
 * with dark ink, and the dark theme uses the dark sunk surface. ANSI hues
 * mirror the semantic palette so command output stays legible in both.
 */
export function terminalTheme(dark: boolean): ITheme {
  if (dark) {
    return {
      background: "#0f1012",
      foreground: "#ecedef",
      cursor: "#80b3fd",
      cursorAccent: "#0f1012",
      selectionBackground: "rgba(128, 179, 253, 0.26)",
      black: "#2b2d31",
      red: "#f87e77",
      green: "#76cf8a",
      yellow: "#f0be67",
      blue: "#80b3fd",
      magenta: "#d5a6e6",
      cyan: "#7fd1cf",
      white: "#c8cacd",
      brightBlack: "#8a8c90",
      brightRed: "#ffa39c",
      brightGreen: "#9ce0aa",
      brightYellow: "#f7d392",
      brightBlue: "#a8c9ff",
      brightMagenta: "#e5c0f2",
      brightCyan: "#a2e2e0",
      brightWhite: "#f7f8f9",
    };
  }
  return {
    background: "#f7f8f9",
    foreground: "#17181b",
    cursor: "#145ec1",
    cursorAccent: "#f7f8f9",
    selectionBackground: "rgba(20, 94, 193, 0.18)",
    black: "#17181b",
    red: "#be2323",
    green: "#1d7339",
    yellow: "#8f520d",
    blue: "#145ec1",
    magenta: "#8b3f9e",
    cyan: "#1c6a72",
    white: "#505356",
    brightBlack: "#64666a",
    brightRed: "#d13a34",
    brightGreen: "#2a8a48",
    brightYellow: "#a86510",
    brightBlue: "#2f72d6",
    brightMagenta: "#a052b4",
    brightCyan: "#2a7f88",
    brightWhite: "#17181b",
  };
}

/** Resolves the active theme without depending on a React render. */
export function isDarkTheme(): boolean {
  const root = document.documentElement;
  if (root.classList.contains("dark")) return true;
  if (root.classList.contains("light")) return false;
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
}
