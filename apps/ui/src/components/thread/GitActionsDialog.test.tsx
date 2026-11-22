// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { GitThreadStatus, ThreadSummary } from "@/gen";

const request = vi.hoisted(() => vi.fn());
const pushToast = vi.hoisted(() => vi.fn());

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  pushToast,
}));

import { CreatePullRequestDialog, GitActionsDialog } from "./GitActionsDialog";

const STATUS: GitThreadStatus = {
  branch: "feature/widget",
  upstream: "origin/feature/widget",
  ahead: 2,
  behind: 1,
  staged: { count: 1, paths: ["src/widget.ts"], truncated: false },
  unstaged: { count: 2, paths: ["src/app.ts", "README.md"], truncated: false },
  untracked: { count: 0, paths: [], truncated: false },
  operation: null,
  default_branch: "main",
};

const THREAD = {
  thread_id: "th_1",
  title: "Add the widget",
  workspace_path: "/work/widget",
  branch: "feature/widget",
} as unknown as ThreadSummary;

const REPO = {
  forge: "github",
  base_url: "https://github.com",
  owner: "acme",
  repo: "widget",
};

function respond(handler: (body: { type: string; [key: string]: unknown }) => unknown) {
  request.mockImplementation(async (body: { type: string }) => handler(body));
}

function renderActions() {
  const onOpenChange = vi.fn();
  const view = render(<GitActionsDialog summary={THREAD} open onOpenChange={onOpenChange} />);
  return { ...view, onOpenChange };
}

describe("GitActionsDialog", () => {
  beforeEach(() => {
    request.mockReset();
    pushToast.mockReset();
  });

  afterEach(cleanup);

  it("shows the real branch, upstream and change counts", async () => {
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    expect(await screen.findByText("feature/widget")).toBeTruthy();
    expect(screen.getByText(/Tracking origin\/feature\/widget/)).toBeTruthy();
    expect(screen.getByText(/2 ahead/)).toBeTruthy();
    expect(screen.getByText(/1 behind/)).toBeTruthy();
    expect(screen.getByRole("list", { name: "Staged paths" })).toBeTruthy();
    expect(screen.getByText("src/widget.ts")).toBeTruthy();
    expect(screen.getByText("src/app.ts")).toBeTruthy();
  });

  it("shows a git error in an alert instead of guessing", async () => {
    respond(() => {
      throw new Error("the workspace has a detached HEAD; check out a branch before committing");
    });
    renderActions();
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("detached HEAD");
  });

  it("Generate fills the message only and never commits", async () => {
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      if (body.type === "generate_git_commit_message") {
        return { type: "generated_commit_message", message: "Add the widget\n\nIt helps." };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText("feature/widget");
    fireEvent.click(screen.getByRole("button", { name: "Generate" }));
    await waitFor(() =>
      expect((screen.getByLabelText("Commit message") as HTMLTextAreaElement).value).toBe(
        "Add the widget\n\nIt helps.",
      ),
    );
    expect(request.mock.calls.some(([body]) => body.type === "git_commit")).toBe(false);
    expect(request.mock.calls.some(([body]) => body.type === "git_push")).toBe(false);
  });

  it("leaves the message empty and explains when generation is off", async () => {
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      if (body.type === "generate_git_commit_message") {
        return { type: "generated_commit_message", message: "" };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText("feature/widget");
    fireEvent.click(screen.getByRole("button", { name: "Generate" }));
    await screen.findByText(/Generation is off/);
    expect((screen.getByLabelText("Commit message") as HTMLTextAreaElement).value).toBe("");
  });

  it("commits with the keyboard shortcut and refreshes the counts", async () => {
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      if (body.type === "git_commit") {
        expect(body.message).toBe("Add the widget");
        expect(body.include_unstaged).toBe(false);
        return {
          type: "git_committed",
          result: {
            commit: "a".repeat(40),
            short_commit: "aaaaaaa",
            subject: "Add the widget",
            status: { ...STATUS, staged: { count: 0, paths: [], truncated: false } },
          },
        };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText("feature/widget");
    const field = screen.getByLabelText("Commit message") as HTMLTextAreaElement;
    fireEvent.change(field, { target: { value: "Add the widget" } });
    fireEvent.keyDown(field, { key: "Enter", ctrlKey: true });
    await waitFor(() =>
      expect(request.mock.calls.some(([body]) => body.type === "git_commit")).toBe(true),
    );
    await waitFor(() => expect(pushToast).toHaveBeenCalled());
    expect((screen.getByLabelText("Commit message") as HTMLTextAreaElement).value).toBe("");
  });

  it("Include unstaged is a real checkbox that reaches the request", async () => {
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      if (body.type === "git_commit") {
        expect(body.include_unstaged).toBe(true);
        return {
          type: "git_committed",
          result: {
            commit: "b".repeat(40),
            short_commit: "bbbbbbb",
            subject: "Add all",
            status: STATUS,
          },
        };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText("feature/widget");
    const checkbox = screen.getByRole("checkbox", { name: /Include unstaged changes/ });
    fireEvent.click(checkbox);
    fireEvent.change(screen.getByLabelText("Commit message"), {
      target: { value: "Add all" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Commit$/ }));
    await waitFor(() =>
      expect(request.mock.calls.some(([body]) => body.type === "git_commit")).toBe(true),
    );
  });
});

describe("CreatePullRequestDialog", () => {
  beforeEach(() => {
    request.mockReset();
    pushToast.mockReset();
  });

  afterEach(cleanup);

  it("Generate fills title and body only; opening asks the forge", async () => {
    respond((body) => {
      if (body.type === "forge_detect_repo") {
        return { type: "forge_repos", repos: [{ remote: "origin", repo: REPO }] };
      }
      if (body.type === "generate_pull_request_text") {
        return {
          type: "generated_pull_request_text",
          title: "Add the widget",
          body: "This adds a widget.",
        };
      }
      if (body.type === "forge_open_pr") {
        expect(body.draft).toBe(true);
        expect(body.base).toBe("main");
        return {
          type: "forge_pr_opened",
          pr: { summary: { repo: REPO, number: 7 } },
        };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    const onOpenChange = vi.fn();
    render(
      <CreatePullRequestDialog summary={THREAD} status={STATUS} open onOpenChange={onOpenChange} />,
    );
    await screen.findByText(/acme\/widget/);
    fireEvent.click(screen.getByRole("button", { name: "Generate" }));
    await waitFor(() =>
      expect((screen.getByLabelText("Pull request title") as HTMLInputElement).value).toBe(
        "Add the widget",
      ),
    );
    expect((screen.getByLabelText("Pull request description") as HTMLTextAreaElement).value).toBe(
      "This adds a widget.",
    );
    expect(request.mock.calls.some(([body]) => body.type === "forge_open_pr")).toBe(false);

    fireEvent.click(screen.getByRole("checkbox", { name: /Open as a draft/ }));
    fireEvent.click(screen.getByRole("button", { name: "Open pull request" }));
    await waitFor(() =>
      expect(request.mock.calls.some(([body]) => body.type === "forge_open_pr")).toBe(true),
    );
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it("shows a missing forge remote instead of pretending one exists", async () => {
    respond((body) => {
      if (body.type === "forge_detect_repo") return { type: "forge_repos", repos: [] };
      throw new Error(`unexpected ${body.type}`);
    });
    render(
      <CreatePullRequestDialog summary={THREAD} status={STATUS} open onOpenChange={vi.fn()} />,
    );
    await screen.findByText(/No supported forge remote/);
    expect(
      (screen.getByRole("button", { name: "Open pull request" }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(
      request.mock.calls.every((call) => (call[0] as { type: string }).type !== "forge_open_pr"),
    ).toBe(true);
  });
});
