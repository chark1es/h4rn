// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type {
  ForgeIssueSummary,
  ForgePrSummary,
  ForgeRepoRef,
  ThreadIssue,
  ThreadPullRequest,
} from "@/gen";

import { setLocalePreference } from "@/lib/i18n";
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

const issueSummary: ForgeIssueSummary = {
  repo,
  number: 12,
  title: "Dark mode flickers",
  state: "open",
  author: "alice",
  labels: [{ name: "bug", color: "d73a4a" }],
  assignees: [{ login: "bob", name: null }],
  comment_count: 1,
  updated_at: "2026-01-01T00:00:00Z",
  web_url: "https://github.com/h4rn/api/issues/12",
};

const issues: ThreadIssue[] = [
  {
    scope: { tracker: "forge", repo },
    issue_id: "12",
    account_id: "a1",
    state: { tracker: "forge", issue: issueSummary },
    error: null,
    identifier: null,
    title: issueSummary.title,
    linked_at: "2026-01-01T00:00:00Z",
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

  it("lists a linked issue with its label, title and unlink", () => {
    const onUnlinkIssue = vi.fn();
    render(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={issues}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
        onUnlinkIssue={onUnlinkIssue}
      />,
    );
    expect(screen.getByRole("button", { name: "Linked items" }).textContent).toContain(
      "h4rn/api#12 Open",
    );
    openBadge();
    expect(screen.getByText("Dark mode flickers")).toBeTruthy();
    expect(screen.getAllByText("h4rn/api#12 Open").length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("menuitem", { name: /Unlink issue/ }));
    expect(onUnlinkIssue).toHaveBeenCalledWith(issues[0]);
  });

  it("keys and labels two issues with the same number by their repository", () => {
    const otherRepo: ForgeRepoRef = { ...repo, owner: "other", repo: "web" };
    const otherSummary: ForgeIssueSummary = {
      ...issueSummary,
      repo: otherRepo,
      title: "Other flicker",
      web_url: "https://github.com/other/web/issues/12",
    };
    const error = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const onUnlinkIssue = vi.fn();
    render(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[
          issues[0] as ThreadIssue,
          {
            scope: { tracker: "forge", repo: otherRepo },
            issue_id: "12",
            account_id: "a1",
            state: { tracker: "forge", issue: otherSummary },
            error: null,
            identifier: null,
            title: otherSummary.title,
            linked_at: "2026-01-02T00:00:00Z",
          },
        ]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
        onUnlinkIssue={onUnlinkIssue}
      />,
    );
    openBadge();
    expect(screen.getByText("Dark mode flickers")).toBeTruthy();
    expect(screen.getByText("Other flicker")).toBeTruthy();
    expect(screen.getAllByText("h4rn/api#12 Open").length).toBeGreaterThan(0);
    expect(screen.getAllByText("other/web#12 Open").length).toBeGreaterThan(0);
    const duplicateKey = error.mock.calls.some((call) => String(call[0]).includes("same key"));
    expect(duplicateKey).toBe(false);
    fireEvent.click(screen.getAllByRole("menuitem", { name: /Unlink issue/ })[1] as HTMLElement);
    expect(onUnlinkIssue).toHaveBeenLastCalledWith(
      expect.objectContaining({ scope: { tracker: "forge", repo: otherRepo } }),
    );
  });

  it("names and keys a Linear row without its internal team id", () => {
    const linked: ThreadIssue = {
      scope: { tracker: "linear", team_id: "team_uuid_1" },
      issue_id: "issue_uuid_1",
      account_id: "lin_1",
      state: {
        tracker: "linear",
        issue: {
          id: "issue_uuid_1",
          identifier: "ENG-1",
          title: "Rate limit the import",
          state_name: "In Progress",
          state_type: "started",
          team_id: "team_uuid_1",
          team_key: "ENG",
          assignees: [],
          labels: [],
          updated_at: "2026-01-02T00:00:00Z",
          url: "https://linear.app/acme/issue/ENG-1",
        },
      },
      error: null,
      identifier: "ENG-1",
      title: "Rate limit the import",
      linked_at: "2026-01-01T00:00:00Z",
    };
    const { rerender } = render(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[linked]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    // The team is named the way Linear names it, never by its UUID.
    expect(screen.getAllByText(/ENG-1 \(ENG\)/).length).toBeGreaterThan(0);
    expect(screen.queryByText(/team_uuid_1/)).toBeNull();

    openBadge();
    const row = document.querySelector(".q-pr-links");
    expect(row).not.toBeNull();

    // A failed refresh empties the live state. The row's key comes from the
    // stored team and issue ids, so the row keeps its place instead of
    // remounting and closing what the person had open, and the identity
    // stored at link time keeps the issue's name.
    rerender(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[{ ...linked, state: null, error: "the tracker did not answer" }]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    expect(screen.getAllByText("ENG-1").length).toBeGreaterThan(0);
    expect(screen.getByText("Rate limit the import")).toBeTruthy();
    expect(screen.getByText("the tracker did not answer")).toBeTruthy();
    expect(document.querySelector(".q-pr-links")).toBe(row);
    expect(screen.queryByText(/team_uuid_1/)).toBeNull();
    expect(screen.queryByText(/issue_uuid_1/)).toBeNull();
  });

  it("names a missing Linear issue without its internal id", () => {
    const linked: ThreadIssue = {
      scope: { tracker: "linear", team_id: "team_uuid_1" },
      issue_id: "issue_uuid_1",
      account_id: "lin_1",
      state: null,
      // An older daemon named the provider's UUID in its not-found message.
      error: "Linear issue issue_uuid_1 was not found",
      identifier: "ENG-1",
      title: "Rate limit the import",
      linked_at: "2026-01-01T00:00:00Z",
    };
    const { rerender } = render(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[linked]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    openBadge();
    expect(screen.getByText("ENG-1 was not found")).toBeTruthy();
    expect(screen.queryByText(/issue_uuid_1/)).toBeNull();

    // A link made before the identifier was stored still gets plain words,
    // never the provider's internal id.
    rerender(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[{ ...linked, identifier: null, title: null }]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    expect(screen.getByText("Linear issue was not found")).toBeTruthy();
    expect(screen.queryByText(/issue_uuid_1/)).toBeNull();
  });

  it("hides an uppercase UUID an older daemon echoed", () => {
    const linked: ThreadIssue = {
      scope: { tracker: "linear", team_id: "team_uuid_1" },
      issue_id: "issue_uuid_1",
      account_id: "lin_1",
      state: null,
      error: "Linear issue ISSUE_UUID_1 was not found",
      identifier: "ENG-1",
      title: "Rate limit the import",
      linked_at: "2026-01-01T00:00:00Z",
    };
    render(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[linked]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    openBadge();
    expect(screen.getByText("ENG-1 was not found")).toBeTruthy();
    expect(screen.queryByText(/ISSUE_UUID_1/i)).toBeNull();
  });

  it("labels a Linear link made before the identifier was stored", () => {
    const linked: ThreadIssue = {
      scope: { tracker: "linear", team_id: "team_uuid_1" },
      issue_id: "issue_uuid_1",
      account_id: "lin_1",
      state: null,
      error: "the tracker did not answer",
      identifier: null,
      title: null,
      linked_at: "2026-01-01T00:00:00Z",
    };
    render(
      <LinkedPullRequestsMenu
        pulls={[]}
        issues={[linked]}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
      />,
    );
    // Neither the live state nor a stored identifier is available: a plain
    // label, never the provider's internal UUID.
    expect(screen.getAllByText("Linear issue").length).toBeGreaterThan(0);
    expect(screen.queryByText(/issue_uuid_1/)).toBeNull();
    expect(screen.queryByText(/team_uuid_1/)).toBeNull();
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

describe("linked items in another language", () => {
  afterEach(async () => {
    await setLocalePreference("en");
  });

  it("words the badge and the watch and unlink actions in Chinese", async () => {
    await setLocalePreference("zh-CN");
    render(
      <LinkedPullRequestsMenu
        pulls={pulls}
        issues={issues}
        onUnlink={() => undefined}
        onWatch={() => undefined}
        onUnwatch={() => undefined}
        onUnlinkIssue={() => undefined}
      />,
    );
    const trigger = screen.getByRole("button", { name: "关联项" });
    expect(trigger.textContent).toContain("#7 开放");
    fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
    fireEvent.pointerUp(trigger, { button: 0, pointerType: "mouse" });
    fireEvent.click(trigger);
    expect(await screen.findByText("停止关注")).toBeTruthy();
    expect(screen.getByText("关注变更")).toBeTruthy();
    expect(screen.getByText("草稿")).toBeTruthy();
    expect(screen.getAllByText("取消关联").length).toBe(2);
    expect(screen.getByText("取消关联议题")).toBeTruthy();
    // Titles come from the forge and stay as sent.
    expect(screen.getByText("Add PR watch")).toBeTruthy();
  });
});
