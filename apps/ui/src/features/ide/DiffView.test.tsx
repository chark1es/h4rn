// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { FsDiff, FsDiffFiles, ThreadId } from "@/gen";

import { DiffView } from "./DiffView";
import { addReviewComment, resetReviewComments } from "./reviewComments";

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
    },
    {
      path: "b.txt",
      old_path: "old-b.txt",
      status: "renamed",
      added: 2,
      removed: 0,
      binary: false,
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
});
