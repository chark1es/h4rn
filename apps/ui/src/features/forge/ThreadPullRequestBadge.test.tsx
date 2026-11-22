// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ForgePrSummary, ForgeRepoRef, ThreadPullRequest } from "@/gen";

import { LinkedPullRequestsMenu } from "./ThreadPullRequestBadge";

const repo: ForgeRepoRef = {
  forge: "github",
  base_url: "https://github.com",
  owner: "h4rn",
  repo: "api",
};

function summary(state: ForgePrSummary["state"], number: number, title: string): ForgePrSummary {
  return {
    repo,
    number,
    title,
    state,
    author: "alice",
    head_branch: "h4rn/watch",
    base_branch: "main",
    head_sha: "abc",
    updated_at: "2026-01-01T00:00:00Z",
    web_url: `https://github.com/h4rn/api/pull/${number}`,
    checks: "pending",
    review: "pending",
    draft: false,
  };
}

const pulls: ThreadPullRequest[] = [
  {
    repo,
    number: 7,
    account_id: "a1",
    summary: summary("open", 7, "Add PR watch"),
    error: null,
    watching: true,
    linked_at: "2026-01-01T00:00:00Z",
  },
  {
    repo,
    number: 9,
    account_id: "a1",
    summary: summary("closed", 9, "Old experiment"),
    error: null,
    watching: false,
    linked_at: "2026-01-02T00:00:00Z",
  },
];

afterEach(cleanup);

function openBadge(): void {
  const trigger = screen.getByRole("button", { name: "Linked pull requests" });
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.pointerUp(trigger, { button: 0, pointerType: "mouse" });
  fireEvent.click(trigger);
}

describe("linked pull request badge and panel", () => {
  it("shows the primary review number, state and how many more", () => {
    render(
      <LinkedPullRequestsMenu
        pulls={pulls}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    const badge = screen.getByRole("button", { name: "Linked pull requests" });
    expect(badge.textContent).toContain("#7 Open");
    expect(badge.textContent).toContain("+1");
  });

  it("lists every link with open, watch and unlink actions", () => {
    const onUnwatch = vi.fn();
    const onWatch = vi.fn();
    const onUnlink = vi.fn();
    render(
      <LinkedPullRequestsMenu
        pulls={pulls}
        onUnlink={onUnlink}
        onWatch={onWatch}
        onUnwatch={onUnwatch}
      />,
    );

    openBadge();

    expect(screen.getByText("Linked pull requests")).toBeTruthy();
    expect(screen.getByText("Add PR watch")).toBeTruthy();
    expect(screen.getByText("Old experiment")).toBeTruthy();
    expect(screen.getByText("Closed")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Stop watching/ }));
    expect(onUnwatch).toHaveBeenCalledWith(pulls[0]);

    fireEvent.click(screen.getByRole("button", { name: /Watch/ }));
    expect(onWatch).toHaveBeenCalledWith(pulls[1]);

    const unlink = screen.getAllByRole("button", { name: /Unlink/ })[0];
    expect(unlink).toBeTruthy();
    if (unlink) fireEvent.click(unlink);
    expect(onUnlink).toHaveBeenCalledWith(pulls[0]);
  });

  it("points an open row at the forge page", () => {
    render(
      <LinkedPullRequestsMenu
        pulls={pulls}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    openBadge();
    const link = screen.getByRole("link", { name: /Add PR watch/ });
    expect(link.getAttribute("href")).toBe("https://github.com/h4rn/api/pull/7");
  });
});
