import type { ThreadLog } from "@/lib/store/projection";

/**
 * Prompt recall reads the thread's own user messages, so it works on every
 * device that can already see the thread and needs no second copy of history.
 */

/** Prompts the user sent in this thread, oldest first, without blanks or repeats. */
export function sentPrompts(log: ThreadLog | undefined | null): string[] {
  if (!log) return [];
  const prompts: string[] = [];
  const add = (text: string) => {
    const prompt = text.trim();
    if (prompt.length === 0) return;
    if (prompts[prompts.length - 1] === prompt) return;
    prompts.push(prompt);
  };
  for (const turn of log.turns) {
    for (const item of turn.items) {
      if (item.kind === "message" && item.role === "user") add(item.text);
    }
  }
  // A just-sent message is still pending until the daemon echoes it back.
  for (const pending of log.pending) add(pending.text);
  return prompts;
}

export type PromptHistoryState = {
  /** Index in the prompt list, or -1 while the in-progress text is showing. */
  index: number;
  /** What the user was typing before the first ArrowUp. */
  draft: string;
};

export type PromptHistoryStep = PromptHistoryState & { text: string };

/** ArrowUp walks back through `prompts`; ArrowDown walks forward and restores the draft. */
export function stepPromptHistory(input: {
  direction: "backward" | "forward";
  prompts: readonly string[];
  state: PromptHistoryState;
  current: string;
}): PromptHistoryStep | null {
  const { prompts, state, current } = input;

  if (input.direction === "backward") {
    if (prompts.length === 0) return null;
    if (state.index < 0) {
      const index = prompts.length - 1;
      const text = prompts[index];
      if (text === undefined) return null;
      return { index, draft: current, text };
    }
    const index = state.index - 1;
    if (index < 0) return null;
    const text = prompts[index];
    if (text === undefined) return null;
    return { index, draft: state.draft, text };
  }

  if (state.index < 0) return null;
  const index = state.index + 1;
  if (index >= prompts.length) {
    return { index: -1, draft: "", text: state.draft };
  }
  const text = prompts[index];
  if (text === undefined) return null;
  return { index, draft: state.draft, text };
}

/**
 * Recall starts only with the caret on the first line and nothing selected,
 * so ArrowUp inside a multi-line draft still moves the caret.
 */
export function caretOnFirstLine(
  value: string,
  selectionStart: number,
  selectionEnd: number,
): boolean {
  if (selectionStart !== selectionEnd) return false;
  if (selectionStart <= 0) return true;
  return value.lastIndexOf("\n", selectionStart - 1) === -1;
}
