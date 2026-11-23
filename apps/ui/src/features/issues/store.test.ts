// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import type { ForgeIssueSummary, LinearIssueSummary } from "@/gen";

import { forgeIssueRow, groupLabel, ISSUE_FILTERS, issueKey, linearIssueRow } from "./store";

const repo = {
  forge: "github" as const,
  base_url: "https://github.com",
  owner: "acme",
  repo: "widget",
};

const forgeIssue: ForgeIssueSummary = {
  repo,
  number: 12,
  title: "Dark mode flickers",
  state: "open",
  author: "alice",
  labels: [{ name: "bug", color: "d73a4a" }],
  assignees: [{ login: "bob", name: "Bob" }],
  comment_count: 2,
  updated_at: "2026-01-02T00:00:00Z",
  web_url: "https://github.com/acme/widget/issues/12",
};

const linearIssue: LinearIssueSummary = {
  id: "iss_1",
  identifier: "ENG-12",
  title: "Rate limit the import",
  state_name: "In Progress",
  state_type: "started",
  team_id: "team_1",
  team_key: "ENG",
  assignees: ["Alice"],
  labels: ["backend"],
  updated_at: "2026-01-03T00:00:00Z",
  url: "https://linear.app/acme/issue/ENG-12",
};

describe("issue rows", () => {
  it("keeps a forge issue's labels, assignees and repository", () => {
    const row = forgeIssueRow("acct_1", forgeIssue);
    expect(row.number).toBe("#12");
    expect(row.repository).toBe("acme/widget");
    expect(row.labels[0]?.name).toBe("bug");
    expect(row.assignees).toEqual(["bob"]);
    expect(row.scope).toEqual({ tracker: "forge", repo });
  });

  it("maps Linear workflow families to open and closed", () => {
    expect(linearIssueRow("lin_1", linearIssue).state).toBe("open");
    expect(linearIssueRow("lin_1", { ...linearIssue, state_type: "completed" }).state).toBe(
      "closed",
    );
    expect(linearIssueRow("lin_1", linearIssue).number).toBe("ENG-12");
  });

  it("uses a stable key per tracker", () => {
    const forge = forgeIssueRow("acct_1", forgeIssue);
    const linear = linearIssueRow("lin_1", linearIssue);
    expect(issueKey(forge.scope, forge.id)).toBe(forge.key);
    expect(issueKey(linear.scope, linear.id)).toBe(linear.key);
  });

  it("labels a group by its forge account", () => {
    expect(
      groupLabel("forge", {
        account_id: "acct_1",
        forge: "github",
        base_url: "https://github.com",
        username: "alice",
        display_name: null,
        scopes: [],
        source: "pat",
        created_at: "2026-01-01T00:00:00Z",
      }),
    ).toBe("GitHub · alice");
    expect(
      groupLabel("linear", {
        account_id: "lin_1",
        name: "Acme",
        email: null,
        created_at: "2026-01-01T00:00:00Z",
      }),
    ).toBe("Linear · Acme");
  });

  it("offers every filter the brief names", () => {
    expect(ISSUE_FILTERS.map((filter) => filter.label)).toEqual([
      "Open",
      "Closed",
      "Assigned to me",
      "Opened by me",
      "Mentioning me",
    ]);
  });
});
