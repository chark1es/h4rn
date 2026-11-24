// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ClientRequest, LinearAccount, ResponseBody } from "@/gen";

import { LinearSettings } from "./LinearSettings";
import type { IssueRequest } from "./store";

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request: vi.fn() }),
  pushToast: vi.fn(),
}));

vi.mock("@/features/projects/store", () => ({
  useProjects: (selector: (state: { projects: unknown[] }) => unknown) =>
    selector({
      projects: [
        {
          id: "proj_1",
          host: "host_local",
          path: "/work/widget",
          name: "widget",
          created_at: "2026-01-01T00:00:00Z",
          git: true,
        },
      ],
    }),
}));

const account: LinearAccount = {
  account_id: "lin_1",
  name: "Acme",
  email: "alice@example.test",
  created_at: "2026-01-01T00:00:00Z",
};

/** Routes each request to the handler the test supplies. */
function request(
  handlers: Partial<Record<ClientRequest["type"], () => ResponseBody>>,
): IssueRequest {
  return async (value) => {
    const handler = handlers[value.type];
    if (!handler) throw new Error(`unexpected request ${value.type}`);
    return handler();
  };
}

afterEach(() => {
  cleanup();
});

describe("Linear settings", () => {
  it("keeps an account load failure on the page", async () => {
    render(
      <LinearSettings request={() => Promise.reject(new Error("the relay is unreachable"))} />,
    );
    expect(await screen.findByText("the relay is unreachable")).toBeTruthy();
    // The failure is not mistaken for a workspace that was never connected.
    expect(screen.queryByText("No Linear workspace connected yet.")).toBeNull();
  });

  it("keeps a team load failure, named by its workspace", async () => {
    render(
      <LinearSettings
        request={request({
          linear_list_accounts: () => ({ type: "linear_accounts", accounts: [account] }),
          linear_project_teams: () => ({ type: "linear_project_teams", teams: [] }),
          linear_list_teams: () => {
            throw new Error("Linear is rate limiting requests: try again later");
          },
        })}
      />,
    );
    expect(
      await screen.findByText("Acme: Linear is rate limiting requests: try again later"),
    ).toBeTruthy();
  });
});
