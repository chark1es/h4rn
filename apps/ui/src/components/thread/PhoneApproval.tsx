import { ShieldCheckIcon } from "lucide-react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import type { ApprovalKind, ThreadSummary } from "@/gen";
import { harnessLabel, harnessSeed } from "@/lib/entities";
import { respondApproval } from "@/lib/store/appStore";
import type { ApprovalItem } from "@/lib/store/projection";

const SUMMARY_KIND_LABEL: Record<ApprovalKind, string> = {
  command: "run",
  file_change: "change a file",
  permission: "use a permission",
  plan: "review a plan",
  question: "answer a question",
};

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
  const harness = harnessLabel(thread.harness);
  return (
    <section className="q-ph-ask" aria-label="Approval request">
      <div className="q-ph-ask-h">
        <ShieldCheckIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="t">
          {harness} on {hostName} wants to {SUMMARY_KIND_LABEL[approval.approvalKind]}
        </span>
      </div>
      <div className="q-ph-ask-cmd">{approval.prompt}</div>
      <div className="q-ph-ask-f">
        <button type="button" className="q-btn secondary sm" onClick={onReview}>
          Details
        </button>
        <button type="button" className="q-btn primary sm" onClick={onReview}>
          Review
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
  const harness = harnessLabel(thread.harness);
  return (
    <>
      <button
        type="button"
        className="q-ph-scrim"
        aria-label="Close approval review"
        onClick={onClose}
      />
      <div className="q-ph-sheet" role="dialog" aria-modal="true" aria-label="Approve command">
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
              {approval.approvalKind === "command" ? "Run a command" : "Review a request"} on{" "}
              {hostName}
            </div>
            <div className="s">
              {harness} · subagent of {thread.title}
            </div>
          </div>
        </div>
        <div className="q-cmd q-ph-sheet-cmd">{approval.prompt}</div>
        <div className="q-ph-group">
          <div className="q-ph-li">
            <span className="grow q-muted">Folder</span>
            <span className="q-mono">{shortPath(thread.workspace_path)}</span>
          </div>
          <div className="q-ph-li">
            <span className="grow q-muted">Asked</span>
            <span>now</span>
          </div>
          <div className="q-ph-li">
            <span className="grow">Always allow in this thread</span>
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={false}
              disabled
              title="Not available yet"
              aria-label="Always allow in this thread"
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
            Deny
          </button>
          <button
            type="button"
            className="q-btn primary"
            onClick={() => {
              void respondApproval(thread.thread_id, approval.approvalId, "approve");
              onClose();
            }}
          >
            Approve
          </button>
        </div>
      </div>
    </>
  );
}
