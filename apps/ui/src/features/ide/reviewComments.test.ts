import { afterEach, describe, expect, it, vi } from "vitest";

import type { ThreadId } from "@/gen";

import {
  addReviewComment,
  appendReviewBlocks,
  commentRange,
  deleteReviewComment,
  listReviewComments,
  type ReviewComment,
  resetReviewComments,
  reviewCommentBlock,
  sendCommentsToDraft,
  updateReviewComment,
} from "./reviewComments";

const thread = "th_1" as ThreadId;

function comment(overrides: Partial<ReviewComment> = {}): ReviewComment {
  return {
    id: "review-1",
    threadId: thread,
    path: "src/auth/session.ts",
    side: "new",
    startLine: 12,
    endLine: 13,
    lines: [
      { number: 12, sign: "+", text: "const now = clock.now();" },
      { number: 13, sign: "+", text: "expect(now).toBe(fixed);" },
    ],
    text: "Use the injected clock here.",
    source: "branch vs main",
    createdAt: 1,
    ...overrides,
  };
}

afterEach(() => {
  resetReviewComments();
});

describe("review comments", () => {
  it("adds, edits and deletes a comment for one thread", () => {
    const added = addReviewComment({
      threadId: thread,
      path: "a.ts",
      side: "new",
      startLine: 3,
      endLine: 3,
      lines: [{ number: 3, sign: " ", text: "let x = 1;" }],
      text: "  keep it const  ",
      source: "uncommitted (HEAD)",
    });
    expect(added.text).toBe("keep it const");
    expect(listReviewComments(thread)).toHaveLength(1);

    updateReviewComment(added.id, "actually, delete it");
    expect(listReviewComments(thread)[0]?.text).toBe("actually, delete it");

    deleteReviewComment(added.id);
    expect(listReviewComments(thread)).toHaveLength(0);
  });

  it("keeps comments apart by thread", () => {
    addReviewComment({
      threadId: thread,
      path: "a.ts",
      side: "new",
      startLine: 1,
      endLine: 1,
      lines: [],
      text: "one",
      source: "uncommitted (HEAD)",
    });
    expect(listReviewComments("th_2" as ThreadId)).toHaveLength(0);
  });

  it("reports the new side and the real line numbers for a selection", () => {
    const lines = [
      { kind: "context" as const, text: "a", oldLine: 1, newLine: 1 },
      { kind: "removed" as const, text: "b", oldLine: 2, newLine: null },
      { kind: "added" as const, text: "B", oldLine: null, newLine: 2 },
      { kind: "context" as const, text: "c", oldLine: 3, newLine: 3 },
    ];
    expect(commentRange(lines, 0, 2)).toEqual({
      side: "new",
      startLine: 1,
      endLine: 2,
      lines: [
        { number: 1, sign: " ", text: "a" },
        { number: null, sign: "-", text: "b" },
        { number: 2, sign: "+", text: "B" },
      ],
    });
    expect(commentRange(lines, 1, 1)).toMatchObject({
      side: "old",
      startLine: 2,
      endLine: 2,
    });
    expect(commentRange([], 0, 0)).toBeNull();
  });

  it("builds one context block with path, side, range, quote and source", () => {
    expect(reviewCommentBlock(comment())).toBe(
      [
        "Review comment · src/auth/session.ts · new side, lines 12–13 · branch vs main",
        "> 12 + const now = clock.now();",
        "> 13 + expect(now).toBe(fixed);",
        "Comment: Use the injected clock here.",
      ].join("\n"),
    );
    expect(
      reviewCommentBlock(
        comment({ startLine: 4, endLine: 4, lines: [{ number: 4, sign: " ", text: "x" }] }),
      ),
    ).toContain("line 4 ·");
  });

  it("appends blocks after the draft's existing text", () => {
    const one = comment();
    const two = comment({ id: "review-2", path: "src/b.ts", text: "And here." });
    expect(appendReviewBlocks("", [one])).toBe(reviewCommentBlock(one));
    expect(appendReviewBlocks("existing\n\n", [one, two])).toBe(
      `existing\n\n${reviewCommentBlock(one)}\n\n${reviewCommentBlock(two)}`,
    );
    expect(appendReviewBlocks("existing", [])).toBe("existing");
  });

  it("sends only by writing the draft and clears the pending comments", () => {
    const one = comment();
    const two = comment({ id: "review-2", path: "src/b.ts", text: "And here." });
    const write = vi.fn<(host: string, threadId: ThreadId, text: string) => void>();
    const remove = vi.fn<(id: string) => void>();
    const read = vi.fn(() => "earlier note");

    const text = sendCommentsToDraft({
      host: "atlas",
      threadId: thread,
      comments: [one, two],
      read,
      write,
      remove,
    });

    expect(read).toHaveBeenCalledWith("atlas", thread);
    expect(write).toHaveBeenCalledWith("atlas", thread, text);
    expect(text).toContain("earlier note");
    expect(text).toContain("Review comment · src/auth/session.ts");
    expect(text).toContain("Review comment · src/b.ts");
    expect(remove).toHaveBeenCalledTimes(2);
  });
});
