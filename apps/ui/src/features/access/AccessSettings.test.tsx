// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OutsideAgentSignIn, OutsideAgentView, ResponseBody } from "@/gen";

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
      case "outside_agent_settings":
        return ok({ type: "outside_agent_settings", settings: { enabled: true } });
      case "list_outside_agents":
        return ok({
          type: "outside_agents",
          agents: [agent],
          mcp_url: "http://127.0.0.1:47800/mcp/outside",
        });
      case "revoke_outside_agent":
        return ok({ type: "ack" });
      case "list_outside_agent_sign_ins":
        return ok({ type: "outside_agent_sign_ins", sign_ins: [] });
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
  window.localStorage.clear();
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

describe("when outside agents are off in Labs", () => {
  it("hides the sign-in section and says where the feature lives", async () => {
    setAccessRequesterForTests((body) => {
      if (body.type !== "mcp") throw new Error("unexpected request");
      switch (body.request.type) {
        case "outside_agent_settings":
          return ok({ type: "outside_agent_settings", settings: { enabled: false } });
        case "list_outside_agents":
          return ok({ type: "outside_agents", agents: [], mcp_url: "http://127.0.0.1:1/mcp" });
        case "list_outside_agent_sign_ins":
          return ok({ type: "outside_agent_sign_ins", sign_ins: [pendingSignIn] });
        default:
          return ok({
            type: "agent_tools",
            settings: { enabled: { by_harness: [], by_machine: [] } },
          });
      }
    });
    render(<AccessSettings />);
    expect(await screen.findByText(/Outside agents are in Labs/)).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Outside agents" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Copy the outside-agent MCP URL" })).toBeNull();
    expect(screen.queryByText("Codex wants to connect")).toBeNull();
  });
});

const pendingSignIn: OutsideAgentSignIn = {
  request_id: "h4rn_auth_1",
  client_name: "Codex",
  redirect_uri: "http://localhost:1455/cb",
  redirect_origin: "http://localhost:1455",
  requested_at: "2026-11-22T10:00:00.000Z",
  expires_at: "2026-11-22T10:10:00.000Z",
};

describe("a sign-in waiting for the person", () => {
  function withSignIn() {
    setAccessRequesterForTests((body) => {
      if (body.type !== "mcp") throw new Error("unexpected request");
      switch (body.request.type) {
        case "outside_agent_settings":
          return ok({ type: "outside_agent_settings", settings: { enabled: true } });
        case "list_outside_agents":
          return ok({ type: "outside_agents", agents: [], mcp_url: "http://127.0.0.1:1/mcp" });
        case "list_outside_agent_sign_ins":
          return ok({ type: "outside_agent_sign_ins", sign_ins: [pendingSignIn] });
        case "respond_outside_agent_sign_in":
          return ok({ type: "ack" });
        default:
          return ok({
            type: "agent_tools",
            settings: { enabled: { by_harness: [], by_machine: [] } },
          });
      }
    });
  }

  it("shows the callback, calls the name unverified and needs a level", async () => {
    withSignIn();
    render(<AccessSettings />);
    expect(await screen.findByText("Codex wants to connect")).toBeTruthy();
    expect(document.body.textContent).toContain("Unverified name");
    expect(document.body.textContent).toContain("http://localhost:1455");
    const approve = screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement;
    expect(approve.disabled).toBe(true);
    fireEvent.click(screen.getByRole("radio", { name: /Supervised/ }));
    await waitFor(() => expect(approve.disabled).toBe(false));
  });

  it("answers the sign-in with the chosen level", async () => {
    const answered: unknown[] = [];
    setAccessRequesterForTests((body) => {
      if (body.type !== "mcp") throw new Error("unexpected request");
      switch (body.request.type) {
        case "outside_agent_settings":
          return ok({ type: "outside_agent_settings", settings: { enabled: true } });
        case "list_outside_agents":
          return ok({ type: "outside_agents", agents: [], mcp_url: "http://127.0.0.1:1/mcp" });
        case "list_outside_agent_sign_ins":
          return ok({ type: "outside_agent_sign_ins", sign_ins: [pendingSignIn] });
        case "respond_outside_agent_sign_in":
          answered.push(body.request);
          return ok({ type: "ack" });
        default:
          return ok({
            type: "agent_tools",
            settings: { enabled: { by_harness: [], by_machine: [] } },
          });
      }
    });
    render(<AccessSettings />);
    await screen.findByText("Codex wants to connect");
    fireEvent.click(screen.getByRole("radio", { name: /Read only/ }));
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    await waitFor(() =>
      expect(answered).toEqual([
        {
          type: "respond_outside_agent_sign_in",
          request_id: "h4rn_auth_1",
          decision: "approve",
          level: "read_only",
        },
      ]),
    );
    await waitFor(() => expect(screen.queryByText("Codex wants to connect")).toBeNull());
  });
});

describe("the MCP URL when this app is a remote client", () => {
  it("does not hand out a loopback URL as if it worked from here", async () => {
    window.localStorage.setItem(
      "h4rn.remote.active",
      JSON.stringify({
        connectUrl: "https://connect.example",
        machineId: "host_1",
        machineName: "Studio",
        daemonNoiseKey: "0".repeat(64),
        addresses: [],
        online: true,
        relay: true,
      }),
    );
    render(<AccessSettings />);
    await waitFor(() => expect(document.body.textContent).toContain("Studio"));
    expect(screen.queryByRole("button", { name: "Copy the outside-agent MCP URL" })).toBeNull();
    expect(document.body.textContent).toContain("cannot reach it yet");
  });
});
