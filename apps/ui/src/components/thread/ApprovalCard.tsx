import { ChevronDownIcon, ShieldCheckIcon } from "lucide-react";

import { BACKSPACE, ENTER, KeyGlyphs, MOD } from "@/components/common/Kbd";
import type { ApprovalKind } from "@/gen";
import { harnessLabel } from "@/lib/entities";
import { respondApproval, useApp } from "@/lib/store/appStore";

const KIND_LABEL: Record<ApprovalKind, string> = {
  command: "run a command",
  file_change: "change a file",
  permission: "use a permission",
  plan: "review a plan",
  question: "answer a question",
};

export function ApprovalCard({ threadId }: { threadId: string }) {
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
  const hostName = hosts[summary?.host ?? ""]?.name ?? summary?.host ?? "the host";
  const harness = summary ? harnessLabel(summary.harness) : "Agent";
  const hostOnline = hosts[summary?.host ?? ""]?.online ?? false;
  const blocked = connection !== "online" || !hostOnline;

  return (
    <section className="q-ask" aria-label="Approval request">
      <div className="q-ask-h">
        <ShieldCheckIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="t">
          {harness} on {hostName} wants to {KIND_LABEL[first.approvalKind]}
        </span>
        <span className="q-faint" style={{ fontSize: 12 }}>
          {more > 0 ? `+${more} more` : "now"}
        </span>
      </div>
      <div className="q-ask-sub">
        {first.approvalKind === "command" ? "Command" : "Request"}
        {summary?.workspace_path ? (
          <>
            {" "}
            · in{" "}
            <span className="q-mono" style={{ fontSize: 11.5 }}>
              {summary.workspace_path}
            </span>
          </>
        ) : null}
      </div>
      <div className="q-cmd">{first.prompt}</div>
      {first.options.length > 0 ? (
        <div className="q-ask-f" style={{ marginTop: 8 }}>
          {first.options.map((option) => (
            <button
              key={option.id}
              type="button"
              className="q-btn secondary"
              disabled={blocked || status === "sending"}
              onClick={() => void respondApproval(threadId, first.approvalId, "approve", option.id)}
            >
              {option.label}
            </button>
          ))}
        </div>
      ) : null}
      <div className="q-ask-f">
        {blocked ? (
          <span className="q-faint" style={{ fontSize: 12 }}>
            Reconnect to approve or deny. Nothing runs until you do.
          </span>
        ) : (
          <button type="button" className="q-btn ghost" disabled title="Not available yet">
            Always allow <ChevronDownIcon className="q-icon s12" strokeWidth={1.75} aria-hidden />
          </button>
        )}
        <span className="grow" />
        <button
          type="button"
          className="q-btn secondary"
          disabled={blocked || status === "sending"}
          onClick={() => void respondApproval(threadId, first.approvalId, "deny")}
        >
          Deny{" "}
          <span className="q-kbd">
            <KeyGlyphs value={`${MOD} ${BACKSPACE}`} />
          </span>
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={blocked || status === "sending"}
          onClick={() => void respondApproval(threadId, first.approvalId, "approve")}
        >
          Approve{" "}
          <span className="q-kbd">
            <KeyGlyphs value={`${MOD} ${ENTER}`} />
          </span>
        </button>
      </div>
    </section>
  );
}
