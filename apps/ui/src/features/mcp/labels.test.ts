import { describe, expect, it } from "vitest";

import type { McpServerView } from "@/gen";
import { serverStatus } from "./labels";

function remote(patch: Partial<McpServerView> = {}): McpServerView {
  return {
    id: "remote:fixture",
    name: "fixture",
    transport: { type: "http", url: "h4rn-remote://host_owner/fixture", header_keys: [] },
    tags: [],
    enabled: { by_harness: [], by_machine: [] },
    status: "stopped",
    tool_count: 0,
    last_error: null,
    runs_on: "host_owner",
    shared_to: [],
    imported_from: null,
    updated_at: "2026-11-13T10:00:00.000Z",
    ...patch,
  };
}

describe("serverStatus for a server shared from another machine", () => {
  it("names the owning machine", () => {
    expect(serverStatus(remote())).toEqual({ label: "Shared from host_owner", tone: "off" });
  });

  it("reports an unreachable owner as an error", () => {
    expect(serverStatus(remote({ status: "error" })).label).toBe("Unreachable");
  });
});
