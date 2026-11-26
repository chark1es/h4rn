// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ForgePrSummary, ForgeRepoRef, ThreadSummary } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";

import { LinkPullRequestDialog } from "./LinkPullRequestDialog";
import { ForgeApi } from "./store";

const repo: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "h4rn",
  repo: "api",
};

const thread: ThreadSummary = {
  thread_id: "th_1",
  parent_thread_id: null,
  host: "host_1",
  harness: "claude_code",
  title: "Watch the PR",
  workspace_path: "/work/api",
  branch: "h4rn/pr-watch",
  model: "claude-sonnet",
  runtime_mode: "approval_required",
  reasoning_efforts: [],
  models: [],
  fast: false,
  tab: "working",
  pinned: false,
  snoozed_until: null,
  last_activity: "2026-01-01T00:00:00Z",
  agent_state: "done",
  usage: {
    input_tokens: 0,
    output_tokens: 0,
    cached_input_tokens: 0,
    cost_micros: 0,
    duration_ms: 0,
    generation_ms: 0,
    tps: 0,
  },
  ttft_ms: null,
  context_window: null,
  context_fill_percent: null,
  needs_attention: false,
  pending_guest_requests: 0,
  unread: false,
  placed_by_user: false,
  resume_at: null,
  rewind: "handoff",
  file_rewind: false,
  limit: null,
  queued_count: 0,
  queue_revision: 0,
  steer: true,
};

const summary: ForgePrSummary = {
  repo,
  number: 12,
  title: "Fix the widget",
  state: "open",
  author: "alice",
  head_branch: "fix-widget",
  base_branch: "main",
  head_sha: "abc",
  updated_at: "2026-01-01T00:00:00Z",
  web_url: "https://github.com/h4rn/api/pull/12",
  checks: "passing",
  review: "approved",
  draft: false,
};

function client(): { api: () => ForgeApi; request: ReturnType<typeof vi.fn> } {
  const request = vi.fn(async (body: { type: string; number?: number }) => {
    switch (body.type) {
      case "forge_detect_repo":
        return {
          type: "forge_repos",
          repos: [{ remote: "origin", repo, url: "git@github.com:h4rn/api.git" }],
        };
      case "forge_list_prs":
        return { type: "forge_prs", prs: [summary], page: 1, has_more: false };
      case "link_pull_request":
        return {
          type: "thread_pull_request",
          pull: {
            repo,
            number: body.number ?? 12,
            account_id: "a1",
            summary,
            error: null,
            watching: false,
            watch_notice: null,
            linked_at: "2026-01-01T00:00:00Z",
          },
        };
      default:
        throw new Error(`unexpected ${body.type}`);
    }
  });
  return {
    api: () => new ForgeApi((body) => request(body as never) as never),
    request,
  };
}

afterEach(cleanup);

describe("Link pull request dialog", () => {
  it("picks an open pull request from the workspace's repository", async () => {
    const { api, request } = client();
    const onClose = vi.fn();
    render(<LinkPullRequestDialog open thread={thread} onClose={onClose} api={api} />);

    const row = await screen.findByRole("button", { name: /Fix the widget/ });
    fireEvent.click(row);

    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(request).toHaveBeenCalledWith({
      type: "link_pull_request",
      thread_id: "th_1",
      account_id: null,
      repo,
      number: 12,
    });
  });

  it("links a pasted pull request URL", async () => {
    const { api, request } = client();
    const onClose = vi.fn();
    render(<LinkPullRequestDialog open thread={thread} onClose={onClose} api={api} />);

    fireEvent.change(screen.getByLabelText("Pull request URL"), {
      target: { value: "https://github.com/h4rn/api/pull/42" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Link" }));

    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(request).toHaveBeenCalledWith(
      expect.objectContaining({ type: "link_pull_request", number: 42, repo }),
    );
  });
});

describe("Link pull request dialog in another language", () => {
  afterEach(async () => {
    await setLocalePreference("en");
  });

  it("words the dialog and a link it cannot read in Spanish", async () => {
    const { api, request } = client();
    await setLocalePreference("es");
    render(<LinkPullRequestDialog open thread={thread} onClose={() => undefined} api={api} />);

    expect(screen.getByRole("heading", { name: "Vincular pull request" })).toBeTruthy();
    expect(
      await screen.findByText("Pega un enlace o elige un pull request abierto de h4rn/api."),
    ).toBeTruthy();
    fireEvent.change(screen.getByLabelText("URL del pull request"), {
      target: { value: "not a link" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Vincular" }));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Ese no es un enlace de pull request que h4rn pueda leer",
    );
    expect(request).not.toHaveBeenCalledWith(
      expect.objectContaining({ type: "link_pull_request" }),
    );
  });
});
