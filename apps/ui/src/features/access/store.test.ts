import { afterEach, describe, expect, it, vi } from "vitest";

import type { AgentToolSettings, OutsideAgentView, ResponseBody } from "@/gen";
import {
  getAccessState,
  loadAccess,
  resetAccessStore,
  revokeAccessAgent,
  setAccessRequesterForTests,
  setAgentToolsEnabled,
} from "./store";

function agent(patch: Partial<OutsideAgentView> = {}): OutsideAgentView {
  return {
    client_id: "h4rn_client_1",
    name: "Claude Code",
    level: "read_only",
    connected_at: "2026-11-22T10:00:00.000Z",
    last_used_at: "2026-11-22T11:00:00.000Z",
    expires_at: "2026-12-22T10:00:00.000Z",
    ...patch,
  };
}

const settings: AgentToolSettings = {
  enabled: {
    by_harness: [{ harness: "codex", enabled: false }],
    by_machine: [],
  },
};

function ok(response: ResponseBody): Promise<ResponseBody> {
  return Promise.resolve(response);
}

afterEach(() => {
  setAccessRequesterForTests(null);
  resetAccessStore();
  vi.restoreAllMocks();
});

describe("Sharing & access store", () => {
  it("loads connected agents, the MCP URL and the tool switches", async () => {
    setAccessRequesterForTests((body) => {
      if (body.type !== "mcp") throw new Error("unexpected request");
      if (body.request.type === "list_outside_agents") {
        return ok({
          type: "outside_agents",
          agents: [agent()],
          mcp_url: "http://127.0.0.1:47800/mcp/outside",
        });
      }
      return ok({ type: "agent_tools", settings });
    });
    await loadAccess();
    expect(getAccessState().agents).toHaveLength(1);
    expect(getAccessState().mcpUrl).toBe("http://127.0.0.1:47800/mcp/outside");
    expect(getAccessState().settings?.enabled.by_harness[0]).toEqual({
      harness: "codex",
      enabled: false,
    });
  });

  it("removes an agent immediately and puts it back when revocation fails", async () => {
    setAccessRequesterForTests(() =>
      ok({
        type: "outside_agents",
        agents: [agent()],
        mcp_url: "http://127.0.0.1:47800/mcp/outside",
      }),
    );
    await loadAccess();

    const gate: { reject?: (error: Error) => void } = {};
    setAccessRequesterForTests(
      () =>
        new Promise((_, reject) => {
          gate.reject = reject;
        }),
    );
    const pending = revokeAccessAgent(agent());
    expect(getAccessState().agents).toHaveLength(0);
    gate.reject?.(new Error("the daemon is offline"));
    expect(await pending).toBe(false);
    expect(getAccessState().agents).toHaveLength(1);
  });

  it("turns a harness switch off and keeps it when the daemon refuses", async () => {
    setAccessRequesterForTests((body) => {
      if (body.type !== "mcp") throw new Error("unexpected request");
      if (body.request.type === "list_outside_agents") {
        return ok({
          type: "outside_agents",
          agents: [],
          mcp_url: "http://127.0.0.1:1/mcp/outside",
        });
      }
      return ok({ type: "agent_tools", settings: { enabled: { by_harness: [], by_machine: [] } } });
    });
    await loadAccess();
    setAccessRequesterForTests(() => Promise.reject(new Error("the daemon refused")));
    expect(await setAgentToolsEnabled("claude_code", false)).toBe(false);
    expect(
      getAccessState().settings?.enabled.by_harness.find(
        (entry) => entry.harness === "claude_code",
      ),
    ).toBeUndefined();
  });
});
