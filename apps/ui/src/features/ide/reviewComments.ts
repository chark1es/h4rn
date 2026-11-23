import { useSyncExternalStore } from "react";

import type { ThreadId } from "@/gen";

/**
 * Review comments live with a thread, not with the file or scope that was open
 * when they were written. They stay in page memory for the thread, so moving
 * between files, commits and diff modes never loses one; a reload clears them.
 * Sending only appends text to the thread's composer draft; nothing leaves the
 * client until the person presses Send.
 */
export type ReviewSide = "old" | "new";

export type QuotedLine = {
  /** The line number on the selected side, when that line exists there. */
  number: number | null;
  /** `+` for an added line, `-` for a removed one, a space for context. */
  sign: "+" | "-" | " ";
  text: string;
};

export type ReviewComment = {
  id: string;
  threadId: ThreadId;
  path: string;
  side: ReviewSide;
  startLine: number;
  endLine: number;
  lines: QuotedLine[];
  text: string;
  /** "branch vs main", "commit 1a2b3c4", "uncommitted (HEAD)". */
  source: string;
  createdAt: number;
};

export type ReviewCommentInput = Omit<ReviewComment, "id" | "createdAt">;

export type SelectableLine = {
  kind: "context" | "added" | "removed";
  text: string;
  oldLine: number | null;
  newLine: number | null;
};

const EMPTY: ReviewComment[] = [];
const byThread = new Map<string, ReviewComment[]>();
const listeners = new Set<() => void>();
let sequence = 0;

function emit(): void {
  for (const listener of [...listeners]) listener();
}

export function listReviewComments(threadId: ThreadId | null): ReviewComment[] {
  if (!threadId) return EMPTY;
  return byThread.get(threadId) ?? EMPTY;
}

export function subscribeReviewComments(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function addReviewComment(input: ReviewCommentInput): ReviewComment {
  const comment: ReviewComment = {
    ...input,
    text: input.text.trim(),
    id: `review-${Date.now().toString(36)}-${sequence++}`,
    createdAt: Date.now(),
  };
  byThread.set(comment.threadId, [...(byThread.get(comment.threadId) ?? []), comment]);
  emit();
  return comment;
}

export function updateReviewComment(id: string, text: string): void {
  for (const [threadId, comments] of byThread) {
    const index = comments.findIndex((comment) => comment.id === id);
    if (index < 0) continue;
    const next = [...comments];
    const current = next[index];
    if (!current) return;
    next[index] = { ...current, text: text.trim() };
    byThread.set(threadId, next);
    emit();
    return;
  }
}

export function deleteReviewComment(id: string): void {
  for (const [threadId, comments] of byThread) {
    const next = comments.filter((comment) => comment.id !== id);
    if (next.length === comments.length) continue;
    byThread.set(threadId, next);
    emit();
    return;
  }
}

/** Tests share this module; clear it between cases. */
export function resetReviewComments(): void {
  byThread.clear();
  sequence = 0;
  emit();
}

/** Subscribes a component to one thread's pending comments. */
export function useReviewComments(threadId: ThreadId | null): ReviewComment[] {
  return useSyncExternalStore(
    subscribeReviewComments,
    () => listReviewComments(threadId),
    () => EMPTY,
  );
}

/**
 * The side and line range a drag or shift-selection covers.
 *
 * The new side wins when the range touches a line that exists in the new file;
 * a range of only removed lines reports the old side. Line numbers are the
 * first and last on that side, not the display positions.
 */
export function commentRange(
  lines: SelectableLine[],
  startIndex: number,
  endIndex: number,
): { side: ReviewSide; startLine: number; endLine: number; lines: QuotedLine[] } | null {
  if (lines.length === 0) return null;
  const start = Math.max(0, Math.min(startIndex, endIndex));
  const end = Math.min(lines.length - 1, Math.max(startIndex, endIndex));
  if (start > end) return null;
  const selected = lines.slice(start, end + 1);
  if (selected.length === 0) return null;
  const side: ReviewSide = selected.some((line) => line.newLine !== null) ? "new" : "old";
  const numbers = selected
    .map((line) => (side === "new" ? line.newLine : line.oldLine))
    .filter((value): value is number => value !== null);
  if (numbers.length === 0) return null;
  return {
    side,
    startLine: numbers[0] ?? 0,
    endLine: numbers[numbers.length - 1] ?? 0,
    lines: selected.map((line) => ({
      number: side === "new" ? line.newLine : line.oldLine,
      sign: line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " ",
      text: line.text,
    })),
  };
}

function lineLabel(comment: Pick<ReviewComment, "startLine" | "endLine">): string {
  return comment.startLine === comment.endLine
    ? `line ${comment.startLine}`
    : `lines ${comment.startLine}–${comment.endLine}`;
}

/** One context block for the composer: path, side, range, quoted lines, text. */
export function reviewCommentBlock(comment: ReviewComment): string {
  const header = `Review comment · ${comment.path} · ${comment.side} side, ${lineLabel(comment)} · ${comment.source}`;
  const quoted = comment.lines
    .map((line) => `> ${line.number ?? ""} ${line.sign} ${line.text}`.trimEnd())
    .join("\n");
  return `${header}\n${quoted}\nComment: ${comment.text}`;
}

/**
 * Appends one block per comment to the composer draft.
 *
 * Plain quoted text is the existing mechanism the composer already understands
 * (its draft store). No message is sent here.
 */
export function appendReviewBlocks(existing: string, comments: ReviewComment[]): string {
  if (comments.length === 0) return existing;
  const blocks = comments.map(reviewCommentBlock).join("\n\n");
  const base = existing.trimEnd();
  return base.length === 0 ? blocks : `${base}\n\n${blocks}`;
}

/**
 * Appends the comments to the thread's composer draft and clears them from the
 * pane. It only touches draft storage; there is no send call anywhere in this
 * path, so nothing reaches the agent until the person presses Send.
 */
export function sendCommentsToDraft(input: {
  host: string;
  threadId: ThreadId;
  comments: ReviewComment[];
  read: (host: string, threadId: ThreadId) => string;
  write: (host: string, threadId: ThreadId, text: string) => void;
  remove: (id: string) => void;
}): string {
  const text = appendReviewBlocks(input.read(input.host, input.threadId), input.comments);
  input.write(input.host, input.threadId, text);
  for (const comment of input.comments) input.remove(comment.id);
  return text;
}
