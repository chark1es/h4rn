import { ShieldCheckIcon } from "lucide-react";
import { useTranslation } from "react-i18next";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { buttonVariants } from "@/components/ui/button";
import {
  Sheet,
  SheetBody,
  SheetClose,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Tip } from "@/components/ui/tooltip";
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
  const respond = (decision: "approve" | "deny") => () => {
    void respondApproval(thread.thread_id, approval.approvalId, decision);
  };
  return (
    <Sheet defaultOpen onOpenChangeComplete={(open) => (open ? undefined : onClose())}>
      <SheetContent>
        <SheetHeader className="items-center gap-2.5">
          <EntityAvatar
            seed={harnessSeed(thread.host, thread.harness)}
            kind="harness"
            label={harness}
            size={24}
          />
          <div className="flex min-w-0 flex-1 flex-col gap-0.5">
            <SheetTitle>
              {approval.approvalKind === "command"
                ? t("approval.phone.runCommandOn", { host: hostName })
                : t("approval.phone.reviewRequestOn", { host: hostName })}
            </SheetTitle>
            <SheetDescription className="truncate">
              {harness} · {thread.title}
            </SheetDescription>
          </div>
        </SheetHeader>
        <SheetBody className="px-0">
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
              <Tip label={t("approval.notAvailable")}>
                <button
                  type="button"
                  className="q-switch"
                  role="switch"
                  aria-checked={false}
                  disabled
                  aria-label={t("approval.phone.alwaysAllow")}
                />
              </Tip>
            </div>
          </div>
        </SheetBody>
        <SheetFooter>
          <SheetClose className={buttonVariants({ variant: "secondary" })} action={respond("deny")}>
            {t("approval.deny")}
          </SheetClose>
          <SheetClose className={buttonVariants()} action={respond("approve")}>
            {t("approval.approve")}
          </SheetClose>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
