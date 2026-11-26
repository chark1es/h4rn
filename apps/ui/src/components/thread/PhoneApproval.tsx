import { ShieldCheckIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import type { ThreadSummary } from "@/gen";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { respondApproval } from "@/lib/store/appStore";
import type { ApprovalItem } from "@/lib/store/projection";

function shortPath(path: string): string {
  return path.replace(/^\/(?:home|Users)\/[^/]+/, "~");
}

/** Compact approval summary for the phone dock; opens the review sheet. */
export function PhoneApprovalSummary({
  thread,
  hostName,
  approval,
  onReview,
}: {
  thread: ThreadSummary;
  hostName: string;
  approval: ApprovalItem;
  onReview: () => void;
}) {
  const { t } = useTranslation("thread");
  const harness = harnessLabel(thread.harness);
  return (
    <section className="q-ph-ask" aria-label={t("approval.label")}>
      <div className="q-ph-ask-h">
        <ShieldCheckIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="t">
          {t(
            approval.approvalKind === "command"
              ? "approval.wants.commandShort"
              : `approval.wants.${approval.approvalKind}`,
            { harness, host: hostName },
          )}
        </span>
      </div>
      <div className="q-ph-ask-cmd">{approval.prompt}</div>
      <div className="q-ph-ask-f">
        <button type="button" className="q-btn secondary sm" onClick={onReview}>
          {t("approval.phone.details")}
        </button>
        <button type="button" className="q-btn primary sm" onClick={onReview}>
          {t("approval.phone.review")}
        </button>
      </div>
    </section>
  );
}

/** Phone review sheet with the real deny/approve actions. */
export function PhoneApprovalSheet({
  thread,
  hostName,
  approval,
  onClose,
}: {
  thread: ThreadSummary;
  hostName: string;
  approval: ApprovalItem;
  onClose: () => void;
}) {
  const { t } = useTranslation("thread");
  const harness = harnessLabel(thread.harness);
  return (
    <>
      <button
        type="button"
        className="q-ph-scrim"
        aria-label={t("approval.phone.close")}
        onClick={onClose}
      />
      <div
        className="q-ph-sheet"
        role="dialog"
        aria-modal="true"
        aria-label={t("approval.phone.sheet")}
      >
        <div className="q-grab" />
        <div className="q-ph-sheet-head">
          <EntityAvatar
            seed={harnessSeed(thread.host, thread.harness)}
            kind="harness"
            label={harness}
            size={22}
          />
          <div className="grow">
            <div className="t">
              {approval.approvalKind === "command"
                ? t("approval.phone.runCommandOn", { host: hostName })
                : t("approval.phone.reviewRequestOn", { host: hostName })}
            </div>
            <div className="s">
              {harness} · {thread.title}
            </div>
          </div>
        </div>
        <div className="q-cmd q-ph-sheet-cmd">{approval.prompt}</div>
        <div className="q-ph-group">
          <div className="q-ph-li">
            <span className="grow q-muted">{t("approval.phone.folder")}</span>
            <span className="q-mono">{shortPath(thread.workspace_path)}</span>
          </div>
          <div className="q-ph-li">
            <span className="grow q-muted">{t("approval.phone.asked")}</span>
            <span>{t("approval.now")}</span>
          </div>
          <div className="q-ph-li">
            <span className="grow">{t("approval.phone.alwaysAllow")}</span>
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={false}
              disabled
              title={t("approval.notAvailable")}
              aria-label={t("approval.phone.alwaysAllow")}
            />
          </div>
        </div>
        <div className="q-ph-sheet-actions">
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => {
              void respondApproval(thread.thread_id, approval.approvalId, "deny");
              onClose();
            }}
          >
            {t("approval.deny")}
          </button>
          <button
            type="button"
            className="q-btn primary"
            onClick={() => {
              void respondApproval(thread.thread_id, approval.approvalId, "approve");
              onClose();
            }}
          >
            {t("approval.approve")}
          </button>
        </div>
      </div>
    </>
  );
}
