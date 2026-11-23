// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { FsDiff, FsDiffFiles, ThreadId } from "@/gen";

import { DiffView } from "./DiffView";
import { addReviewComment, resetReviewComments } from "./reviewComments";
import { sourceDescription } from "./sources";

const thread = "th_1" as ThreadId;

const diff: FsDiff = {
  path: "a.txt",
  against: { kind: "head" },
  label: "HEAD",
  before: "a\nb\n",
  after: "a\nB\n",
  binary: false,
  too_large: false,
  note: null,
};

const files: FsDiffFiles = {
  against: { kind: "base", base: "main" },
  label: "vs main",
  base: "main",
  merge_base: "abc",
  truncated: false,
  note: null,
  files: [
    {
      path: "a.txt",
      old_path: null,
      status: "modified",
      added: 1,
      removed: 1,
      binary: false,
      counts_omitted: false,
    },
    {
      path: "b.txt",
      old_path: "old-b.txt",
      status: "renamed",
      added: 2,
      removed: 0,
      binary: false,
      counts_omitted: false,
    },
  ],
};

function view(overrides: Partial<Parameters<typeof DiffView>[0]> = {}) {
  return (
    <DiffView
      path="a.txt"
      source="head"
      diff={diff}
      loading={false}
      error={null}
      changes={null}
      files={null}
      filesLoading={false}
      filesError={null}
      commits={null}
      host={null}
      harness={null}
      threadId={thread}
      sourceLabel="uncommitted (HEAD)"
      changeTitle={null}
      onSelectFile={() => undefined}
      onOpenFile={() => undefined}
      onRevert={() => undefined}
      onSendComments={() => undefined}
      onCompareHead={() => undefined}
      onRefresh={() => undefined}
      {...overrides}
    />
  );
}

beforeEach(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    }),
  });
});

afterEach(() => {
  cleanup();
  resetReviewComments();
});

describe("diff review", () => {
  it("adds, edits and deletes an inline comment from the keyboard", () => {
    render(view());
    const added = screen.getByRole("button", { name: /Line 2: B/ });

    // A row is a keyboard target: Enter opens the comment editor for it.
    fireEvent.keyDown(added, { key: "Enter" });
    const box = screen.getByLabelText("Review comment");
    fireEvent.change(box, { target: { value: "Use the injected clock here." } });
    fireEvent.click(screen.getByRole("button", { name: "Add comment" }));

    expect(screen.getByText("Use the injected clock here.")).toBeTruthy();
    expect(screen.getByText("1 comment")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    const edit = screen.getByLabelText("Edit review comment");
    fireEvent.change(edit, { target: { value: "Reworded for the agent." } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByText("Reworded for the agent.")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(screen.queryByText("Reworded for the agent.")).toBeNull();
    expect(screen.getByText("0 comments")).toBeTruthy();
  });

  it("extends a selection with Shift+Arrow and comments on the range", async () => {
    render(view());
    const first = screen.getByRole("button", { name: /Line 1: a/ });
    fireEvent.click(first);
    await waitFor(() => expect(first.getAttribute("aria-pressed")).toBe("true"));
    fireEvent.keyDown(first, { key: "ArrowDown", shiftKey: true });
    // The removed and added sides of the change are separate rows; extend to
    // the added line so the range covers new lines 1–2.
    const removed = screen.getByRole("button", { name: /Line 2: b/ });
    fireEvent.keyDown(removed, { key: "ArrowDown", shiftKey: true });
    const added = screen.getByRole("button", { name: /Line 2: B/ });
    await waitFor(() => expect(added.getAttribute("aria-pressed")).toBe("true"));
    fireEvent.keyDown(added, { key: "Enter" });
    expect(screen.getByLabelText(/lines 1–2/)).toBeTruthy();
  });

  it("sends the thread's pending comments and sends nothing itself", () => {
    addReviewComment({
      threadId: thread,
      path: "a.txt",
      side: "new",
      startLine: 2,
      endLine: 2,
      lines: [{ number: 2, sign: "+", text: "B" }],
      text: "Keep the original name.",
      scope: "head",
      source: "uncommitted (HEAD)",
    });
    const onSendComments = vi.fn();
    render(view({ onSendComments }));
    fireEvent.click(screen.getByRole("button", { name: "Send to agent" }));
    expect(onSendComments).toHaveBeenCalledTimes(1);
    const sent = onSendComments.mock.calls[0]?.[0] as Array<{ text: string }>;
    expect(sent).toHaveLength(1);
    expect(sent[0]?.text).toBe("Keep the original name.");
  });

  it("shows a branch's files and opens the chosen one", () => {
    const onSelectFile = vi.fn();
    render(
      view({
        source: "branch",
        files,
        sourceLabel: "branch vs main",
        diff: { ...diff, against: { kind: "base", base: "main" }, label: "vs main" },
        onSelectFile,
      }),
    );
    expect(screen.getByRole("complementary", { name: "Changed files" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "b.txt · renamed (was old-b.txt)" }));
    expect(onSelectFile).toHaveBeenCalledWith("b.txt");
  });

  it("shows the too-large state instead of a text diff", () => {
    render(view({ diff: { ...diff, too_large: true } }));
    expect(screen.getByText(/too large to diff/)).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Diff lines" })).toBeNull();
  });

  it("keeps the branch file list and comment bar beside a too-large file", () => {
    const onSelectFile = vi.fn();
    render(
      view({
        source: "branch",
        files,
        sourceLabel: "branch vs main",
        diff: {
          ...diff,
          against: { kind: "base", base: "main" },
          label: "vs main",
          too_large: true,
        },
        onSelectFile,
      }),
    );
    expect(screen.getByRole("complementary", { name: "Changed files" })).toBeTruthy();
    expect(screen.getByText(/too large to diff/)).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Diff lines" })).toBeNull();
    // The source bar, the count and the send action are still usable.
    expect(screen.getByText("2 files")).toBeTruthy();
    expect(screen.getByText("0 comments")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Send to agent" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "b.txt · renamed (was old-b.txt)" }));
    expect(onSelectFile).toHaveBeenCalledWith("b.txt");
  });

  it("keeps the branch file list when the chosen file is binary", () => {
    render(
      view({
        source: "branch",
        files,
        sourceLabel: "branch vs main",
        diff: { ...diff, against: { kind: "base", base: "main" }, label: "vs main", binary: true },
      }),
    );
    expect(screen.getByRole("complementary", { name: "Changed files" })).toBeTruthy();
    expect(screen.getByText(/Both sides are binary/)).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Diff lines" })).toBeNull();
  });

  it("offers HEAD when a branch review has no base", () => {
    const onCompareHead = vi.fn();
    render(
      view({
        source: "branch",
        sourceLabel: "branch vs base",
        diff: null,
        files: null,
        filesError: {
          code: "invalid_request",
          message: "No base branch: this repository has no remote default branch.",
          hint: null,
        },
        onCompareHead,
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Compare with HEAD" }));
    expect(onCompareHead).toHaveBeenCalledTimes(1);
  });

  it("keeps a comment from another diff source out of the lines", () => {
    addReviewComment({
      threadId: thread,
      path: "a.txt",
      side: "new",
      startLine: 2,
      endLine: 2,
      lines: [{ number: 2, sign: "+", text: "B" }],
      text: "Written against the branch.",
      scope: "base:main",
      source: "branch vs main",
    });
    render(view({ sourceLabel: "uncommitted (HEAD)" }));
    const lines = screen.getByRole("region", { name: "Diff lines" });
    expect(within(lines).queryByText("Written against the branch.")).toBeNull();
    expect(screen.getByText("Written against the branch.")).toBeTruthy();
    expect(screen.getByText(/another diff source/)).toBeTruthy();
    expect(screen.getByText(/branch vs main/)).toBeTruthy();
  });

  it("keeps a comment with its turn when two turns share a label", () => {
    // UUIDv7 turn ids a moment apart share their first 8 characters, so the
    // pane's labels read the same. The turn id, not the label, is what a
    // comment belongs to.
    const label = "Turn 0192f0c1";
    const first = "0192f0c1-2345-7abc-8def-0123456789ab";
    addReviewComment({
      threadId: thread,
      path: "a.txt",
      side: "new",
      startLine: 2,
      endLine: 2,
      lines: [{ number: 2, sign: "+", text: "B" }],
      text: "Written during the first turn.",
      scope: `turn:${thread}:${first}`,
      source: sourceDescription("agent", label),
    });
    render(
      view({
        source: "agent",
        sourceLabel: sourceDescription("agent", label),
        diff: {
          ...diff,
          against: {
            kind: "agent_turn",
            thread_id: thread,
            turn_id: "0192f0c1-9876-7abc-8def-fedcba987654" as never,
          },
          label,
        },
      }),
    );
    const lines = screen.getByRole("region", { name: "Diff lines" });
    expect(within(lines).queryByText("Written during the first turn.")).toBeNull();
    expect(screen.getByText("Written during the first turn.")).toBeTruthy();
    expect(screen.getByText(/turn 0192f0c1/)).toBeTruthy();
  });

  it("marks a binary or unread file in the list instead of fake counts", () => {
    render(
      view({
        source: "branch",
        files: {
          ...files,
          files: [
            {
              path: "a.txt",
              old_path: null,
              status: "modified",
              added: 1,
              removed: 1,
              binary: true,
              counts_omitted: false,
            },
            {
              path: "b.txt",
              old_path: "old-b.txt",
              status: "renamed",
              added: 0,
              removed: 0,
              binary: false,
              counts_omitted: true,
            },
          ],
        },
        sourceLabel: "branch vs main",
        diff: { ...diff, against: { kind: "base", base: "main" }, label: "vs main" },
      }),
    );
    expect(screen.getByText("binary")).toBeTruthy();
    expect(screen.getByText("no counts")).toBeTruthy();
  });
});
