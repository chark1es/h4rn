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

  it("Generate with the mode off never clears a typed message", async () => {
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      if (body.type === "generate_git_commit_message") {
        return { type: "generated_commit_message", message: "" };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText("feature/widget");
    const field = screen.getByLabelText("Commit message") as HTMLTextAreaElement;
    fireEvent.change(field, { target: { value: "Mine, typed by hand" } });
    fireEvent.click(screen.getByRole("button", { name: "Generate" }));
    await screen.findByText(/Generation is off/);
    expect(field.value).toBe("Mine, typed by hand");
  });

  it("uses the live status: a detached HEAD disables push and the PR dialog", async () => {
    respond((body) => {
      if (body.type === "git_status") {
        return {
          type: "git_status",
          status: {
            ...STATUS,
            branch: null,
            upstream: null,
            ahead: 0,
            behind: 0,
          },
        };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText(/detached HEAD/);
    // The thread's old branch name is not shown as if it were checked out.
    expect(screen.queryByText("feature/widget")).toBeNull();
    expect((screen.getByRole("button", { name: "Push" }) as HTMLButtonElement).disabled).toBe(true);
    expect(
      (screen.getByRole("button", { name: /Create pull request/ }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("shows the live branch once the status has loaded", async () => {
    respond((body) => {
      if (body.type === "git_status") {
        return { type: "git_status", status: { ...STATUS, branch: "release/2" } };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    expect(await screen.findByText("release/2")).toBeTruthy();
    expect(screen.queryByText("feature/widget")).toBeNull();
  });

  it("keeps one busy state on Commit & push through the commit and the push", async () => {
    const pending: {
      commit: ((value: unknown) => void) | null;
      push: ((value: unknown) => void) | null;
    } = { commit: null, push: null };
    respond((body) => {
      if (body.type === "git_status") return { type: "git_status", status: STATUS };
      if (body.type === "git_commit") {
        return new Promise((resolve) => {
          pending.commit = resolve;
        });
      }
      if (body.type === "git_push") {
        return new Promise((resolve) => {
          pending.push = resolve;
        });
      }
      throw new Error(`unexpected ${body.type}`);
    });
    renderActions();
    await screen.findByText("feature/widget");
    fireEvent.change(screen.getByLabelText("Commit message"), {
      target: { value: "Add the widget" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Commit & push" }));

    const busy = await screen.findByRole("button", { name: "Committing & pushing…" });
    expect((busy as HTMLButtonElement).disabled).toBe(true);
    await waitFor(() => expect(pending.commit).not.toBeNull());
    pending.commit?.({
      type: "git_committed",
      result: {
        commit: "c".repeat(40),
        short_commit: "ccccccc",
        subject: "Add the widget",
        status: STATUS,
      },
    });

    await waitFor(() => expect(pending.push).not.toBeNull());
    expect(
      (screen.getByRole("button", { name: "Committing & pushing…" }) as HTMLButtonElement).disabled,
    ).toBe(true);
    pending.push?.({
      type: "git_pushed",
      result: { branch: "feature/widget", upstream: "origin/feature/widget", status: STATUS },
    });
    await waitFor(() => expect(screen.getByRole("button", { name: "Commit & push" })).toBeTruthy());
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
    const openCall = request.mock.calls.find(([body]) => body.type === "forge_open_pr");
    expect(openCall?.[1]).toEqual({ timeoutMs: 200_000 });
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it("seeds the fields on open only, not when the thread title changes", async () => {
    respond((body) => {
      if (body.type === "forge_detect_repo") {
        return { type: "forge_repos", repos: [{ remote: "origin", repo: REPO }] };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    const onOpenChange = vi.fn();
    const { rerender } = render(
      <CreatePullRequestDialog summary={THREAD} status={STATUS} open onOpenChange={onOpenChange} />,
    );
    await screen.findByText(/acme\/widget/);
    fireEvent.change(screen.getByLabelText("Pull request title"), {
      target: { value: "Typed by me" },
    });

    rerender(
      <CreatePullRequestDialog
        summary={{ ...THREAD, title: "A generated title arrived" }}
        status={STATUS}
        open
        onOpenChange={onOpenChange}
      />,
    );
    expect((screen.getByLabelText("Pull request title") as HTMLInputElement).value).toBe(
      "Typed by me",
    );

    // A fresh open does seed the new title.
    rerender(
      <CreatePullRequestDialog
        summary={{ ...THREAD, title: "A generated title arrived" }}
        status={STATUS}
        open={false}
        onOpenChange={onOpenChange}
      />,
    );
    rerender(
      <CreatePullRequestDialog
        summary={{ ...THREAD, title: "A generated title arrived" }}
        status={STATUS}
        open
        onOpenChange={onOpenChange}
      />,
    );
    await waitFor(() =>
      expect((screen.getByLabelText("Pull request title") as HTMLInputElement).value).toBe(
        "A generated title arrived",
      ),
    );
  });

  it("leaves a missing default branch empty and says what to do", async () => {
    respond((body) => {
      if (body.type === "forge_detect_repo") {
        return { type: "forge_repos", repos: [{ remote: "origin", repo: REPO }] };
      }
      throw new Error(`unexpected ${body.type}`);
    });
    render(
      <CreatePullRequestDialog
        summary={THREAD}
        status={{ ...STATUS, default_branch: null }}
        open
        onOpenChange={vi.fn()}
      />,
    );
    await screen.findByText(/acme\/widget/);
    expect((screen.getByLabelText("Base branch") as HTMLInputElement).value).toBe("");
    expect(screen.getByText(/No default branch was detected/)).toBeTruthy();
    expect(
      (screen.getByRole("button", { name: "Open pull request" }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("disables the submit while the request is in flight and opens one request", async () => {
    const pending: { open: ((value: unknown) => void) | null } = { open: null };
    respond((body) => {
      if (body.type === "forge_detect_repo") {
        return { type: "forge_repos", repos: [{ remote: "origin", repo: REPO }] };
      }
      if (body.type === "forge_open_pr") {
        return new Promise((resolve) => {
          pending.open = resolve;
        });
      }
      throw new Error(`unexpected ${body.type}`);
    });
    const onOpenChange = vi.fn();
    render(
      <CreatePullRequestDialog summary={THREAD} status={STATUS} open onOpenChange={onOpenChange} />,
    );
    await screen.findByText(/acme\/widget/);
    const submit = screen.getByRole("button", { name: "Open pull request" });
    fireEvent.click(submit);
    const opening = await screen.findByRole("button", { name: "Opening…" });
    expect((opening as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(opening);
    expect(request.mock.calls.filter(([body]) => body.type === "forge_open_pr")).toHaveLength(1);

    pending.open?.({
      type: "forge_pr_opened",
      pr: { summary: { repo: REPO, number: 7 } },
    });
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(onOpenChange).toHaveBeenCalledTimes(1);
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
