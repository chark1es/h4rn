import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { McpRequest, McpServerView, ResponseBody } from "@/gen";
import { H4rnRequestError, protoError } from "@/lib/client/errors";
import { getState, store } from "@/lib/store/appStore";
import {
  applyMcpEvent,
  getMcpState,
  loadMcpServers,
  removeMcpServer,
  resetMcpStore,
  setMcpEnabled,
  setMcpRequesterForTests,
} from "./store";

function server(patch: Partial<McpServerView> = {}): McpServerView {
  return {
    id: "srv_1",
    name: "playwright",
    transport: {
      type: "stdio",
      command: "npx",
      args: ["-y", "@playwright/mcp"],
      cwd: null,
      env_keys: ["GITHUB_TOKEN"],
    },
    tags: [],
    enabled: { by_harness: [{ harness: "claude_code", enabled: true }], by_machine: [] },
    status: "ready",
    tool_count: 5,
    last_error: null,
    runs_on: null,
    shared_to: [],
    imported_from: "user",
    updated_at: "2026-11-13T10:00:00.000Z",
    ...patch,
  };
}

function ok(response: ResponseBody): Promise<ResponseBody> {
  return Promise.resolve(response);
}

beforeEach(() => {
  resetMcpStore();
  store.setState({ ...getState(), localHostId: "atlas" });
});

afterEach(() => {
  setMcpRequesterForTests(null);
  vi.restoreAllMocks();
});

describe("MCP store", () => {
  it("loads and sorts servers", async () => {
    setMcpRequesterForTests(() =>
      ok({
        type: "mcp_servers",
        servers: [server({ id: "srv_2", name: "sentry" }), server()],
      }),
    );
    await loadMcpServers();
    expect(getMcpState().servers.map((entry) => entry.name)).toEqual(["playwright", "sentry"]);
    expect(getMcpState().checkedAt).not.toBeNull();
  });

  it("moves a harness switch before the daemon answers", async () => {
    resetMcpStore();
    setMcpRequesterForTests(() => ok({ type: "mcp_servers", servers: [server()] }));
    await loadMcpServers();

    const gate: { release?: () => void } = {};
    setMcpRequesterForTests(
      () =>
        new Promise((resolve) => {
          gate.release = () =>
            resolve({
              type: "mcp_server",
              server: server({
                enabled: {
                  by_harness: [{ harness: "codex", enabled: false }],
                  by_machine: [],
                },
              }),
            });
        }),
    );

    const pending = setMcpEnabled("playwright", { harness: "codex" }, false);
    const optimistic = getMcpState().servers[0]?.enabled.by_harness.find(
      (entry) => entry.harness === "codex",
    );
    expect(optimistic?.enabled).toBe(false);
    expect(getMcpState().pending["playwright:toggle:codex"]).toBe(true);

    gate.release?.();
    await pending;
    expect(
      getMcpState().servers[0]?.enabled.by_harness.find((entry) => entry.harness === "codex")
        ?.enabled,
    ).toBe(false);
    expect(getMcpState().pending["playwright:toggle:codex"]).toBeUndefined();
  });

  it("rolls a rejected toggle back and explains what happened", async () => {
    setMcpRequesterForTests(() => ok({ type: "mcp_servers", servers: [server()] }));
    await loadMcpServers();
    setMcpRequesterForTests(() =>
      Promise.reject(
        new H4rnRequestError(protoError("internal", "The hub did not answer.", "Try again.")),
      ),
    );

    const result = await setMcpEnabled("playwright", { harness: "claude_code" }, false);

    expect(result).toBe(false);
    expect(
      getMcpState().servers[0]?.enabled.by_harness.find((entry) => entry.harness === "claude_code")
        ?.enabled,
    ).toBe(true);
    const toast = getState().toasts.at(-1);
    expect(toast?.title).toBe("Couldn't turn off playwright.");
    expect(toast?.description).toContain("The switch is back where it was");
  });

  it("restores a server when removal fails", async () => {
    setMcpRequesterForTests(() => ok({ type: "mcp_servers", servers: [server()] }));
    await loadMcpServers();
    setMcpRequesterForTests(() =>
      Promise.reject(new H4rnRequestError(protoError("conflict", "In use.", null))),
    );

    const removed = await removeMcpServer("playwright");

    expect(removed).toBe(false);
    expect(getMcpState().servers.map((entry) => entry.name)).toEqual(["playwright"]);
    expect(getState().toasts.at(-1)?.description).toContain("still configured");
  });

  it("keeps live tool calls newest first and capped", () => {
    for (let index = 0; index < 42; index += 1) {
      applyMcpEvent({
        type: "mcp_tool_called",
        server: "playwright",
        tool: "open_page",
        qualified_name: "playwright__open_page",
        caller: { kind: "client" },
        ok: true,
        duration_ms: BigInt(index) as unknown as bigint,
        error: null,
        at: new Date(2026, 10, 13, 10, 0, index).toISOString(),
      });
    }
    const activity = getMcpState().activity;
    expect(activity).toHaveLength(40);
    expect(activity[0]?.durationMs).toBe(41);
  });

  it("does not send a toggle before the list arrives", async () => {
    const requests: McpRequest[] = [];
    setMcpRequesterForTests((request) => {
      requests.push(request);
      return ok({ type: "mcp_servers", servers: [] });
    });
    expect(await setMcpEnabled("missing", { harness: "codex" }, false)).toBe(false);
    expect(requests).toHaveLength(0);
  });
});
