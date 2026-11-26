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
import { Trans, useTranslation } from "react-i18next";

import { pendingMcpApprovals, respondMcpApproval, useMcp } from "@/features/mcp/store";
import { formatRelativeTime } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";

export function McpApprovalCard() {
  const approvals = useMcp((state) => state.approvals);
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection.status);
  const [sending, setSending] = useState(false);
  const { t } = useTranslation(["library", "common"]);

  const pending = pendingMcpApprovals(approvals);
  const first = pending[0];
  if (!first) return null;

  const machineName = hosts[first.machine]?.name ?? first.machine;
  const more = pending.length - 1;
  const blocked = connection !== "online" || sending;
  const now = Date.now();
  const expiresAt = Date.parse(first.expires_at);
  const expiring = !(expiresAt - now > 0);

  const answer = (decision: "approve" | "deny") => {
    if (blocked) return;
    setSending(true);
    void respondMcpApproval(first.approval_id, decision).finally(() => setSending(false));
  };

  return (
    <section className="q-ask" aria-label={t("mcp.approval.label")}>
      <div className="q-ask-h">
        <ShieldCheckIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="t">
          <Trans
            t={t}
            i18nKey="mcp.approval.wantsToRun"
            values={{ machine: machineName, tool: `${first.server}__${first.tool}` }}
            components={{ tool: <span className="q-mono" /> }}
          />
        </span>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {more > 0 ? t("mcp.approval.more", { count: more }) : t("mcp.approval.needsYou")}
        </span>
      </div>
      <div className="q-ask-sub">
        <span>
          {expiring
            ? t("mcp.approval.expiring")
            : t("mcp.approval.expires", { when: formatRelativeTime(expiresAt, now) })}
        </span>
      </div>
      <div className="q-cmd">{first.arguments_preview.trim() || t("mcp.approval.noArgs")}</div>
      <div className="q-ask-f">
        {blocked ? (
          <span className="q-faint" style={{ fontSize: 12 }}>
            {t("mcp.approval.reconnect")}
          </span>
        ) : null}
        <span className="grow" />
        <button
          type="button"
          className="q-btn secondary"
          disabled={blocked}
          onClick={() => answer("deny")}
        >
          {t("mcp.approval.deny")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          data-approval-action="approve"
          disabled={blocked}
          onClick={() => answer("approve")}
        >
          {sending ? t("mcp.approval.answering") : t("mcp.approval.approveOnce")}
        </button>
      </div>
    </section>
  );
}
