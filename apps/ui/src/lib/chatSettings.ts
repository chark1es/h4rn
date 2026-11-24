import { useSyncExternalStore } from "react";

/**
 * How a message sent while the agent is working is delivered.
 *
 * This is a setting per client (it lives on this device), not a daemon
 * preference: two people watching the same thread may want different defaults.
 */
export type FollowUpBehavior = "queue" | "steer";

const FOLLOW_UP_KEY = "h4rn.chat.followUp";
const MULTILINE_KEY = "h4rn.chat.multilineEnter";

function readFollowUp(): FollowUpBehavior {
  try {
    return window.localStorage.getItem(FOLLOW_UP_KEY) === "steer" ? "steer" : "queue";
  } catch {
    return "queue";
  }
}

function readMultiline(): boolean {
  try {
    return window.localStorage.getItem(MULTILINE_KEY) === "true";
  } catch {
    return false;
  }
}

let followUp: FollowUpBehavior = typeof window === "undefined" ? "queue" : readFollowUp();
let multiline: boolean = typeof window === "undefined" ? false : readMultiline();
const listeners = new Set<() => void>();

function notify(): void {
  for (const listener of listeners) listener();
}

export function followUpBehavior(): FollowUpBehavior {
  return followUp;
}

export function setFollowUpBehavior(next: FollowUpBehavior): void {
  followUp = next;
  try {
    window.localStorage.setItem(FOLLOW_UP_KEY, next);
  } catch {
    // Non-fatal: the choice still applies for this session.
  }
  notify();
}

/** Whether the send button should steer: only while a supported turn runs. */
export function defaultFollowUpFor(running: boolean, canSteer: boolean): FollowUpBehavior {
  if (!running) return "queue";
  if (!canSteer) return "queue";
  return followUp;
}

export function multilineEnter(): boolean {
  return multiline;
}

export function setMultilineEnter(next: boolean): void {
  multiline = next;
  try {
    window.localStorage.setItem(MULTILINE_KEY, String(next));
  } catch {
    // Non-fatal.
  }
  notify();
}

export function useFollowUpBehavior(): FollowUpBehavior {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => followUp,
    () => "queue",
  );
}

export function useMultilineEnter(): boolean {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => multiline,
    () => false,
  );
}
