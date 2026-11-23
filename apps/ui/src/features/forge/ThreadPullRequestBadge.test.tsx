// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    watch_notice: null,
    linked_at: "2026-01-01T00:00:00Z",
  },
  {
    repo,
    number: 9,
    account_id: "a1",
    summary: summary("draft", 9, "Old experiment"),
    error: null,
    watching: false,
    watch_notice: null,
    linked_at: "2026-01-02T00:00:00Z",
  },
];

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function openBadge(): void {
  const trigger = screen.getByRole("button", { name: "Linked items" });
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
    const badge = screen.getByRole("button", { name: "Linked items" });
    expect(badge.textContent).toContain("#7 Open");
    expect(badge.textContent).toContain("+1");
  });

  it("lists every link with open, watch and unlink actions", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
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

    expect(screen.getByText("Linked items")).toBeTruthy();
    expect(screen.getByText("Add PR watch")).toBeTruthy();
    expect(screen.getByText("Old experiment")).toBeTruthy();
    expect(screen.getByText("Draft")).toBeTruthy();

    fireEvent.click(screen.getByRole("menuitem", { name: /Add PR watch/ }));
    expect(open).toHaveBeenCalledWith(
      "https://github.com/h4rn/api/pull/7",
      "_blank",
      "noopener,noreferrer",
    );

    openBadge();
    fireEvent.click(screen.getByRole("menuitem", { name: /Stop watching/ }));
    expect(onUnwatch).toHaveBeenCalledWith(pulls[0]);

    openBadge();
    fireEvent.click(screen.getByRole("menuitem", { name: /Watch for changes/ }));
    expect(onWatch).toHaveBeenCalledWith(pulls[1]);

    openBadge();
    fireEvent.click(screen.getAllByRole("menuitem", { name: /Unlink/ })[0] as HTMLElement);
    expect(onUnlink).toHaveBeenCalledWith(pulls[0]);
  });

  it("opens the panel and runs an action with the keyboard", async () => {
    const onUnwatch = vi.fn();
    render(
      <LinkedPullRequestsMenu
        pulls={pulls}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={onUnwatch}
      />,
    );

    const trigger = screen.getByRole("button", { name: "Linked items" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "Enter" });

    const openRow = await screen.findByRole("menuitem", { name: /Add PR watch/ });
    fireEvent.keyDown(openRow, { key: "ArrowDown" });
    const stop = screen.getByRole("menuitem", { name: /Stop watching/ });
    await waitFor(() => expect(document.activeElement).toBe(stop));

    fireEvent.keyDown(stop, { key: "Enter" });
    expect(onUnwatch).toHaveBeenCalledWith(pulls[0]);
  });

  it("shows an eye only on the row that ends a watch", () => {
    render(
      <LinkedPullRequestsMenu
        pulls={pulls}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    openBadge();

    const stop = screen.getByRole("menuitem", { name: /Stop watching/ });
    const watch = screen.getByRole("menuitem", { name: /Watch for changes/ });
    expect(stop.querySelector("svg")).not.toBeNull();
    expect(watch.querySelector("svg")).toBeNull();
  });

  it("offers no watch on a merged or closed pull request", () => {
    const [primary, other] = pulls;
    if (!primary || !other) throw new Error("the badge fixtures are missing");
    render(
      <LinkedPullRequestsMenu
        pulls={[
          { ...primary, watching: false, summary: summary("merged", 7, "Add PR watch") },
          { ...other, summary: summary("closed", 9, "Old experiment") },
        ]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    openBadge();

    expect(screen.getByText("Merged")).toBeTruthy();
    expect(screen.getByText("Closed").className).toContain("q-danger");
    expect(screen.queryByRole("menuitem", { name: /Watch for changes/ })).toBeNull();
    expect(screen.getAllByRole("menuitem", { name: /Unlink/ })).toHaveLength(2);
  });

  it("says what a watch cannot cover when the forge has no login", () => {
    const notice =
      "Comments and reviews are not watched: this account has no login on the forge, so the daemon cannot tell your own words from someone else's.";
    const [primary, other] = pulls;
    if (!primary || !other) throw new Error("the badge fixtures are missing");
    render(
      <LinkedPullRequestsMenu
        pulls={[{ ...primary, watch_notice: notice }, other]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    openBadge();
    expect(screen.getByText(notice)).toBeTruthy();
  });
});
