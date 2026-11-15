/**
 * A shared MCP tool call waiting for this machine's decision (ADR-037).
 *
 * The card mirrors the in-thread `ApprovalCard` so an owner recognises the
 * interaction: the same `.q-ask` shape, a plain-language action line and one
 * primary button. It is global because the call belongs to no local thread;
 * it can arrive while the owner is anywhere in the app.
 */

import { ShieldCheckIcon } from "lucide-react";
import { useState } from "react";

import { pendingMcpApprovals, respondMcpApproval, useMcp } from "@/features/mcp/store";
import { useApp } from "@/lib/store/appStore";

export function McpApprovalCard() {
  const approvals = useMcp((state) => state.approvals);
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection.status);
  const [sending, setSending] = useState(false);

  const pending = pendingMcpApprovals(approvals);
  const first = pending[0];
  if (!first) return null;

  const machineName = hosts[first.machine]?.name ?? first.machine;
  const more = pending.length - 1;
  const blocked = connection !== "online" || sending;
  const expiresIn = Math.max(0, Math.ceil((Date.parse(first.expires_at) - Date.now()) / 60_000));

  const answer = (decision: "approve" | "deny") => {
    if (blocked) return;
    setSending(true);
    void respondMcpApproval(first.approval_id, decision).finally(() => setSending(false));
  };

  return (
    <section className="q-ask" aria-label="Shared tool approval">
      <div className="q-ask-h">
        <ShieldCheckIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="t">
          {machineName} wants to run{" "}
          <span className="q-mono">
            {first.server}__{first.tool}
          </span>
        </span>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {more > 0 ? `+${more} more` : "Needs you"}
        </span>
      </div>
      <div className="q-ask-sub">
        <span>
          Shared MCP server · {expiresIn > 0 ? `expires in ${expiresIn} min` : "expiring now"}
        </span>
      </div>
      <div className="q-cmd">{first.arguments_preview.trim() || "No arguments"}</div>
      <div className="q-ask-f">
        {blocked ? (
          <span className="q-faint" style={{ fontSize: 12 }}>
            Reconnect to answer. Nothing runs until you approve.
          </span>
        ) : null}
        <span className="grow" />
        <button
          type="button"
          className="q-btn secondary"
          disabled={blocked}
          onClick={() => answer("deny")}
        >
          Deny
        </button>
        <button
          type="button"
          className="q-btn primary"
          data-approval-action="approve"
          disabled={blocked}
          onClick={() => answer("approve")}
        >
          {sending ? "Answering…" : "Approve once"}
        </button>
      </div>
    </section>
  );
}
