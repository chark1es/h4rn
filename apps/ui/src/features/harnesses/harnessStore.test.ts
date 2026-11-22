import { beforeEach, describe, expect, it, vi } from "vitest";

import type { HarnessVersionInfo } from "@/gen";
import { H4rnRequestError } from "@/lib/client/errors";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  useApp: () => undefined,
}));

import {
  applyHarnessEvent,
  getHarnessState,
  harnessUpdateCount,
  loadHarnessVersions,
  probeAcpAgent,
  resetHarnessStoreForTests,
  updateAcpAgent,
  updateHarness,
} from "./harnessStore";

function row(overrides: Partial<HarnessVersionInfo> = {}): HarnessVersionInfo {
  return {
    harness: "codex",
    installed: "0.168.0",
    latest: "0.169.0",
    update_available: true,
    method: { kind: "npm", package: "@openai/codex", command: "npm i -g @openai/codex@latest" },
    path: "/usr/lib/node_modules/@openai/codex/bin/codex",
    ...overrides,
  };
}

describe("harness versions store", () => {
  beforeEach(() => {
    resetHarnessStoreForTests();
    request.mockReset();
  });

  it("loads rows and counts the ones with an update", async () => {
    request.mockResolvedValueOnce({
      type: "harness_versions",
      harnesses: [
        row(),
        row({ harness: "claude_code", update_available: false, latest: "2.1.323" }),
        row({ harness: "antigravity", latest: null, update_available: false }),
      ],
    });
    await loadHarnessVersions();
    expect(getHarnessState().loaded).toBe(true);
    expect(getHarnessState().rows).toHaveLength(3);
    expect(harnessUpdateCount(getHarnessState())).toBe(1);
  });

  it("reports a failed read as an error, not a thrown rejection", async () => {
    request.mockRejectedValueOnce(new Error("socket closed"));
    await loadHarnessVersions();
    expect(getHarnessState().loaded).toBe(true);
    expect(getHarnessState().error).not.toBeNull();
    expect(getHarnessState().rows).toEqual([]);
  });

  it("streams output, then records the result and re-reads the versions", async () => {
    request.mockResolvedValueOnce({ type: "ack" });
    await updateHarness("codex");
    expect(getHarnessState().updating.codex).toBe(true);

    applyHarnessEvent({ type: "output", harness: "codex", stream: "stdout", line: "added 1" });
    applyHarnessEvent({ type: "output", harness: "codex", stream: "stderr", line: "warn" });
    expect(getHarnessState().output.codex).toEqual(["added 1", "warn"]);

    request.mockResolvedValueOnce({
      type: "harness_versions",
      harnesses: [row({ installed: "0.169.0", update_available: false })],
    });
    applyHarnessEvent({
      type: "finished",
      harness: "codex",
      ok: true,
      message: "Updated Codex from 0.168.0 to 0.169.0.",
      from_version: "0.168.0",
      to_version: "0.169.0",
    });
    expect(getHarnessState().updating.codex).toBe(false);
    expect(getHarnessState().results.codex).toEqual({
      ok: true,
      message: "Updated Codex from 0.168.0 to 0.169.0.",
    });
    await vi.waitFor(() => expect(getHarnessState().rows[0]?.installed).toBe("0.169.0"));
  });

  it("surfaces a refused update with its hint", async () => {
    request.mockRejectedValueOnce(
      new H4rnRequestError({
        code: "conflict",
        message: "Codex is working on a turn.",
        hint: "Finish or stop the turn, then try again.",
      }),
    );
    await updateHarness("codex");
    expect(getHarnessState().updating.codex).toBe(false);
    expect(getHarnessState().results.codex?.ok).toBe(false);
    expect(getHarnessState().results.codex?.message).toContain("working on a turn");
  });

  it("keeps only the most recent output lines", () => {
    for (let index = 0; index < 250; index += 1) {
      applyHarnessEvent({
        type: "output",
        harness: "codex",
        stream: "stdout",
        line: `line ${index}`,
      });
    }
    const lines = getHarnessState().output.codex ?? [];
    expect(lines).toHaveLength(200);
    expect(lines.at(-1)).toBe("line 249");
  });

  it("remembers a refused probe instead of retrying it", async () => {
    request.mockRejectedValueOnce(
      new H4rnRequestError({
        code: "forbidden",
        message: "This device cannot start the agent.",
        hint: "Pair an admin device to check it.",
      }),
    );
    const message = await probeAcpAgent("demo");
    expect(message).toContain("start the agent");
    expect(getHarnessState().probing.demo).toBe(false);
    expect(getHarnessState().probeErrors.demo).toContain("admin");
  });

  it("drops a cached probe when the agent is edited", async () => {
    request.mockResolvedValueOnce({
      type: "acp_agent_probe",
      probe: { id: "demo", installed: true, models: [], modes: [] },
    });
    request.mockResolvedValueOnce({ type: "acp_agents", agents: [] });
    await probeAcpAgent("demo");
    expect(getHarnessState().probes.demo).toBeDefined();

    request.mockResolvedValueOnce({ type: "acp_agent", agent: { id: "demo" } });
    request.mockResolvedValueOnce({ type: "acp_agents", agents: [] });
    await updateAcpAgent("demo", {
      id: "demo",
      name: "Demo",
      command: "demo-agent",
      args: [],
      env: {},
      working_dir: { kind: "thread_workspace" },
    });
    expect(getHarnessState().probes.demo).toBeUndefined();
  });
});
