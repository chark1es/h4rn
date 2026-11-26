// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ForgeIssueSummary, LinearIssueSummary } from "@/gen";
import { i18n, setLocalePreference } from "@/lib/i18n";

import {
  forgeIssueRow,
  groupLabel,
  ISSUE_FILTERS,
  issueKey,
  issueStateLabel,
  linearIssueRow,
  relativeIssueAge,
} from "./store";

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
    expect(ISSUE_FILTERS.map((filter) => i18n.t(`issues:filters.${filter}`))).toEqual([
      "Open",
      "Closed",
      "Assigned to me",
      "Opened by me",
      "Mentioning me",
    ]);
  });
});

describe("issue words in another language", () => {
  afterEach(async () => {
    vi.useRealTimers();
    await setLocalePreference("en");
  });

  it("names the state and the age in the active language", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-03-10T12:00:00Z"));
    expect(relativeIssueAge("2026-03-10T11:59:40Z")).toBe("just now");
    expect(relativeIssueAge("2026-03-10T09:00:00Z")).toBe("3 hours ago");
    await setLocalePreference("es");
    expect(issueStateLabel("closed")).toBe("Cerrado");
    expect(relativeIssueAge("2026-03-10T11:59:40Z")).toBe("ahora mismo");
    expect(relativeIssueAge("2026-03-10T09:00:00Z")).toBe("hace 3 horas");
    await setLocalePreference("ja");
    // Ages inside a sentence are written out, never the narrow "3h" of a column.
    expect(relativeIssueAge("2026-03-10T09:00:00Z")).toBe("3 時間前");
    expect(relativeIssueAge("2026-03-08T12:00:00Z")).toBe("一昨日");
    expect(relativeIssueAge("2026-03-07T12:00:00Z")).toBe("3 日前");
    // A clock that runs ahead reads as just now, never "in 3 hours".
    expect(relativeIssueAge("2026-03-10T15:00:00Z")).toBe("たった今");
    await setLocalePreference("zh-CN");
    expect(issueStateLabel("open")).toBe("开放");
    expect(relativeIssueAge("2026-03-10T09:00:00Z")).toBe("3小时前");
    // A month or more reads as the date, in the language's own order.
    expect(relativeIssueAge("2026-01-02T12:00:00Z")).toBe("2026/1/2");
  });
});
