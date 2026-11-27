import { ChevronDownIcon, ShieldCheckIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ShortcutCaps } from "@/components/ui/kbd";
import { harnessLabel } from "@/lib/entities";
import { tildePath } from "@/lib/format";
import { respondApproval, useApp } from "@/lib/store/appStore";

export function ApprovalCard({ threadId }: { threadId: string }) {
  const { t } = useTranslation("thread");
  const log = useApp((state) => state.logs[threadId]);
  const summary = useApp((state) => state.threads[threadId]);
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection.status);
  const pending = (log?.turns ?? [])
    .flatMap((turn) => turn.items)
    .filter((item) => item.kind === "approval")
    .filter((item) => {
      const status = log?.approvals?.[item.approvalId]?.status ?? "pending";
      return status === "pending" || status === "sending" || status === "error";
    });

  const first = pending[0];
  if (first?.kind !== "approval") return null;

  const status = log?.approvals?.[first.approvalId]?.status ?? "pending";
  const more = pending.length - 1;
  const hostName = hosts[summary?.host ?? ""]?.name ?? summary?.host ?? t("approval.theHost");
  const harness = summary ? harnessLabel(summary.harness) : t("approval.agent");
  const hostOnline = hosts[summary?.host ?? ""]?.online ?? false;
  const blocked = connection !== "online" || !hostOnline;
  const sending = status === "sending";
  const waiting = blocked || sending;

  const respond = (kind: "approve" | "deny", optionId?: string) => {
    if (waiting) return;
    void respondApproval(threadId, first.approvalId, kind, optionId);
  };

  return (
    <section className="q-ask" aria-label={t("approval.label")}>
      <div className="q-ask-h">
        <ShieldCheckIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="t">
          {t(`approval.wants.${first.approvalKind}`, { harness, host: hostName })}
        </span>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {more > 0 ? t("approval.more", { count: more }) : t("approval.now")}
        </span>
      </div>
      {summary?.workspace_path ? (
        <div className="q-ask-sub">
          <span className="q-mono" style={{ fontSize: 11.5 }}>
            {tildePath(summary.workspace_path)}
          </span>
        </div>
      ) : null}
      <div className="q-cmd">{first.prompt}</div>
      {first.options.length > 0 ? (
        <div className="q-ask-f" style={{ marginTop: 8 }}>
          {first.options.map((option) => (
            <button
              key={option.id}
              type="button"
              className="q-btn secondary"
              disabled={blocked}
              aria-disabled={waiting || undefined}
              onClick={() => respond("approve", option.id)}
            >
              {option.label}
            </button>
          ))}
        </div>
      ) : null}
      <div className="q-ask-f">
        {blocked ? (
          <span className="q-faint" style={{ fontSize: 12 }}>
            {t("approval.reconnect")}
          </span>
        ) : (
          <button
            type="button"
            className="q-btn ghost"
            aria-disabled="true"
            title={t("approval.notAvailable")}
            onClick={(event) => event.preventDefault()}
          >
            {t("approval.alwaysAllow")}{" "}
            <ChevronDownIcon className="q-icon s12" strokeWidth={1.75} aria-hidden />
          </button>
        )}
        <span className="grow" />
        <button
          type="button"
          className="q-btn secondary"
          disabled={blocked}
          aria-disabled={waiting || undefined}
          onClick={() => respond("deny")}
        >
          {t("approval.deny")} <ShortcutCaps command="approval.deny" />
        </button>
        <button
          type="button"
          className="q-btn primary"
          data-approval-action="approve"
          disabled={blocked}
          aria-disabled={waiting || undefined}
          onClick={() => respond("approve")}
        >
          {sending ? (
            t("approval.approving")
          ) : (
            <>
              {t("approval.approve")} <ShortcutCaps command="approval.approve" />
            </>
          )}
        </button>
      </div>
    </section>
  );
}
