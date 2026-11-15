import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it } from "vitest";

import { getState, store } from "@/lib/store/appStore";
import { McpApprovalCard } from "./McpApprovalCard";
import { applyMcpEvent, getMcpState, resetMcpStore } from "./store";

const approval = {
  approval_id: "mcp-1",
  machine: "atlas",
  server: "github",
  tool: "write_file",
  arguments_preview: '{"path":"/tmp/report.md","body":"hello"}',
  requested_at: "2026-11-14T10:00:00.000Z",
  expires_at: "2099-11-14T10:02:00.000Z",
};

function render(): string {
  return renderToStaticMarkup(createElement(McpApprovalCard));
}

beforeEach(() => {
  resetMcpStore();
  store.setState({
    ...getState(),
    connection: { ...getState().connection, status: "online" },
    hosts: {},
  });
});

describe("McpApprovalCard", () => {
  it("names the shared tool and previews its arguments", () => {
    applyMcpEvent({ type: "mcp_share_approval_requested", approval });
    const html = render();
    expect(html).toContain("Shared tool approval");
    expect(html).toContain("github__write_file");
    expect(html).toContain("atlas");
    expect(html).toContain("/tmp/report.md");
    expect(html).toContain("Needs you");
    expect(html).toContain("Approve once");
    expect(html).toContain("Deny");
  });

  it("removes the card when the daemon reports the decision", () => {
    applyMcpEvent({ type: "mcp_share_approval_requested", approval });
    applyMcpEvent({
      type: "mcp_share_approval_resolved",
      approval_id: approval.approval_id,
      decision: "deny",
    });
    expect(render()).toBe("");
  });

  it("removes the card when the caller drops and the daemon cancels it", () => {
    // I2: a tunnel that closed withdrew its approvals with `cancel`, and the
    // owner must not be left with a card for a caller that is gone.
    applyMcpEvent({ type: "mcp_share_approval_requested", approval });
    applyMcpEvent({
      type: "mcp_share_approval_resolved",
      approval_id: approval.approval_id,
      decision: "cancel",
    });
    expect(render()).toBe("");
    expect(getMcpState().approvals).toEqual([]);
  });

  it("hides approvals that have already expired", () => {
    applyMcpEvent({
      type: "mcp_share_approval_requested",
      approval: { ...approval, expires_at: "2000-01-01T00:00:00.000Z" },
    });
    expect(render()).toBe("");
  });
});
