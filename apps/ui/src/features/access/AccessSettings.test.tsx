// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OutsideAgentView, ResponseBody } from "@/gen";

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request: vi.fn() }),
  pushToast: vi.fn(),
  useApp: (selector: (state: unknown) => unknown) => selector({ connection: { status: "online" } }),
}));

import { AccessSettings } from "./AccessSettings";
import { resetAccessStore, setAccessRequesterForTests } from "./store";

const agent: OutsideAgentView = {
  client_id: "h4rn_client_1",
  name: "Claude Code",
  level: "read_only",
  connected_at: "2026-11-22T10:00:00.000Z",
  last_used_at: "2026-11-22T11:00:00.000Z",
  expires_at: "2026-12-22T10:00:00.000Z",
};

function ok(response: ResponseBody): Promise<ResponseBody> {
  return Promise.resolve(response);
}

beforeEach(() => {
  resetAccessStore();
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: { writeText: vi.fn(() => Promise.resolve()) },
  });
  setAccessRequesterForTests((body) => {
    if (body.type !== "mcp") throw new Error("unexpected request");
    switch (body.request.type) {
      case "list_outside_agents":
        return ok({
          type: "outside_agents",
          agents: [agent],
          mcp_url: "http://127.0.0.1:47800/mcp/outside",
        });
      case "revoke_outside_agent":
        return ok({ type: "ack" });
      case "agent_tools":
        return ok({
          type: "agent_tools",
          settings: { enabled: { by_harness: [], by_machine: [] } },
        });
      default:
        throw new Error(`unexpected MCP request ${body.request.type}`);
    }
  });
});

afterEach(() => {
  cleanup();
  resetAccessStore();
  setAccessRequesterForTests(null);
});

describe("the Sharing & access page", () => {
  it("names a connected agent and its level", async () => {
    render(<AccessSettings />);
    expect(await screen.findByText("Claude Code")).toBeTruthy();
    expect(document.body.textContent).toContain("Read only");
    expect(document.body.textContent).toContain("nothing changes");
  });

  it("copies the outside-agent URL", async () => {
    render(<AccessSettings />);
    await screen.findByText("Claude Code");
    fireEvent.click(screen.getByRole("button", { name: "Copy the outside-agent MCP URL" }));
    await waitFor(() =>
      expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
        "http://127.0.0.1:47800/mcp/outside",
      ),
    );
    expect(
      screen.getByRole("button", { name: "Copy the outside-agent MCP URL" }).textContent,
    ).toContain("Copied");
  });

  it("revokes an agent after confirmation", async () => {
    render(<AccessSettings />);
    await screen.findByText("Claude Code");
    fireEvent.click(screen.getByRole("button", { name: "Revoke Claude Code" }));
    const confirm = screen.getAllByRole("button", { name: "Revoke" }).at(-1);
    expect(confirm).toBeTruthy();
    fireEvent.click(confirm as HTMLElement);
    await waitFor(() =>
      expect(document.body.textContent).toContain("No outside agent is connected"),
    );
  });
});
