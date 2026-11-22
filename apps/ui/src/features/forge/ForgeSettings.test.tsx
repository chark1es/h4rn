// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { ClientRequest, ForgeRemote, ResponseBody } from "@/gen";

const mock = vi.hoisted(() => {
  const state = {
    threads: {
      thr_a: { workspace_path: "/work/alpha" },
      thr_b: { workspace_path: "/work/beta" },
    },
  };
  return {
    state,
    requests: [] as ClientRequest[],
    pushToast: vi.fn(),
    // A workspace remote for each of the two checkouts.
    client: {
      request: vi.fn(async (request: ClientRequest): Promise<ResponseBody> => {
        mock.requests.push(request);
        switch (request.type) {
          case "forge_list_accounts":
            return { type: "forge_accounts", accounts: [], capabilities: [] };
          case "forge_detect_repo": {
            const workspace = request.workspace;
            const remote: ForgeRemote = {
              remote: "origin",
              repo:
                workspace === "/work/alpha"
                  ? {
                      forge: "bitbucket",
                      base_url: "https://bitbucket.org",
                      owner: "alpha",
                      repo: "widget",
                    }
                  : {
                      forge: "bitbucket",
                      base_url: "https://bitbucket.org",
                      owner: "beta",
                      repo: "gizmo",
                    },
              url: `git@bitbucket.org:${workspace === "/work/alpha" ? "alpha/widget" : "beta/gizmo"}.git`,
            };
            return { type: "forge_repos", repos: [remote] };
          }
          case "forge_add_account":
            return {
              type: "forge_account",
              account: {
                account_id: "acct_1",
                forge: request.forge,
                base_url: request.base_url,
                username: null,
                display_name: "Acme",
                scopes: [],
                source: "bitbucket_access_token",
                created_at: "2026-01-01T00:00:00Z",
              },
            };
          default:
            return { type: "ack" };
        }
      }),
    },
  };
});

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getClient: () => mock.client,
  pushToast: mock.pushToast,
}));

import { ForgeSettings } from "./ForgeSettings";

/** Opens a ChipSelect's menu; Radix listens for the pointer down, not the click. */
function openChip(trigger: HTMLElement) {
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.click(trigger);
}

beforeAll(() => {
  // Radix's dropdown expects these pointer-capture APIs, which jsdom lacks.
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
});

afterEach(() => {
  cleanup();
  mock.requests.length = 0;
  mock.client.request.mockClear();
  mock.pushToast.mockClear();
});

describe("Bitbucket token validation repository", () => {
  it("defaults to the first detected remote and can pick the second", async () => {
    render(<ForgeSettings />);

    expect(screen.getByRole("button", { name: "Forge: GitHub" })).toBeTruthy();
    openChip(screen.getByRole("button", { name: "Forge: GitHub" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: /Bitbucket Cloud/ }));

    // The token help matches docs/forges.md.
    expect(screen.getByText(/Pull requests: read and write/)).toBeTruthy();

    // Two checkouts, so the form defaults to the first detected repository.
    const picker = await screen.findByRole("button", {
      name: "Bitbucket repository to validate against: alpha/widget",
    });
    openChip(picker);
    fireEvent.click(await screen.findByRole("menuitem", { name: /beta\/gizmo/ }));
    await waitFor(() =>
      expect(
        screen.getByRole("button", {
          name: "Bitbucket repository to validate against: beta/gizmo",
        }),
      ).toBeTruthy(),
    );

    fireEvent.change(screen.getByLabelText("Forge token"), {
      target: { value: "ATBB-access" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Add account/ }));

    await waitFor(() => {
      const added = mock.requests.find((request) => request.type === "forge_add_account");
      expect(added).toBeDefined();
      expect(added).toMatchObject({
        type: "forge_add_account",
        forge: "bitbucket",
        repository: {
          forge: "bitbucket",
          owner: "beta",
          repo: "gizmo",
        },
      });
    });
  });
});
