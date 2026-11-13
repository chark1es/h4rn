import type { ITheme } from "@xterm/xterm";

/**
 * Maps the DESIGN.md stage palette onto xterm's ANSI slots.
 *
 * The terminal is the one surface that stays stage-black in both themes, so
 * text and selection colours come from the stage tokens while the ANSI hues
 * mirror the semantic palette.
 */
export function terminalTheme(dark: boolean): ITheme {
  const stage = dark ? "#101419" : "#232a33";
  const stageInk = dark ? "#e9edf2" : "#e8edf3";
  return {
    background: stage,
    foreground: stageInk,
    cursor: dark ? "#e0b15a" : "#d9a23e",
    cursorAccent: stage,
    selectionBackground: dark ? "rgba(224, 177, 90, 0.28)" : "rgba(217, 162, 62, 0.32)",
    black: "#2b313a",
    red: dark ? "#e06c75" : "#c95862",
    green: dark ? "#8fc98a" : "#6fae72",
    yellow: dark ? "#e0b15a" : "#c99a3f",
    blue: dark ? "#7fa6d9" : "#5e87bf",
    magenta: dark ? "#c58fd4" : "#a973b8",
    cyan: dark ? "#7fc1c0" : "#5da6a5",
    white: dark ? "#c9d1db" : "#c4ccd6",
    brightBlack: dark ? "#5b6572" : "#7a8695",
    brightRed: "#f07f89",
    brightGreen: "#a5dda0",
    brightYellow: "#f0c674",
    brightBlue: "#9db9e6",
    brightMagenta: "#d9a8e6",
    brightCyan: "#9ad6d5",
    brightWhite: "#f4f7fa",
  };
}

/** Resolves the active theme without depending on a React render. */
export function isDarkTheme(): boolean {
  const root = document.documentElement;
  if (root.classList.contains("dark")) return true;
  if (root.classList.contains("light")) return false;
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
}
