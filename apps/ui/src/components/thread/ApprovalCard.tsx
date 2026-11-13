import { CheckIcon, ShieldCheckIcon, XIcon } from "lucide-react";
import { BACKSPACE, ENTER, Kbd, MOD } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import type { ApprovalKind, ApprovalOption } from "@/gen";
import { respondApproval, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const KIND_LABEL: Record<ApprovalKind, string> = {
  command: "Command approval",
  file_change: "File change",
  permission: "Permission",
  plan: "Plan review",
  question: "Question",
};

const DECISION_WORD = {
  approve: "Approved",
  deny: "Held",
  cancel: "Cancelled",
} as const;

export function ApprovalCard({
  threadId,
  approvalId,
  approvalKind,
  prompt,
  options,
}: {
  threadId: string;
  approvalId: string;
  approvalKind: ApprovalKind;
  prompt: string;
  options: ApprovalOption[];
}) {
  const state = useApp((app) => app.logs[threadId]?.approvals[approvalId]);
  const status = state?.status ?? "pending";
  const resolved = status === "resolved";

  return (
    <div
      className={cn(
        "relative overflow-hidden rounded-md border border-cue/40 bg-cue-tint/60 py-2.5 pr-2.5 pl-3",
        resolved && "border-standby/40 bg-standby-tint/50",
      )}
    >
      <span
        aria-hidden
        className={cn("absolute inset-y-0 left-0 w-0.5 bg-cue", resolved && "bg-standby")}
      />
      <div className="flex items-start gap-2">
        <span
          className={cn(
            "mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-sm bg-cue/15 text-cue-ink",
            resolved && "bg-standby/15 text-standby-ink",
          )}
        >
          {resolved ? <CheckIcon className="size-3.5" /> : <ShieldCheckIcon className="size-3.5" />}
        </span>
        <div className="min-w-0 flex-1 space-y-1.5">
          <div className="flex flex-wrap items-center gap-2">
            <p className="text-micro font-semibold tracking-wide text-cue-ink uppercase">
              {KIND_LABEL[approvalKind]}
            </p>
            {resolved ? (
              <span className="text-micro text-standby-ink">
                {state?.decision ? DECISION_WORD[state.decision] : "Resolved"}
              </span>
            ) : null}
            {status === "sending" ? (
              <span className="text-micro text-cue-ink/80">Sending…</span>
            ) : null}
            {status === "error" ? (
              <span className="text-micro text-strike-ink">Not sent · still pending</span>
            ) : null}
          </div>

          <p className="text-compact leading-relaxed whitespace-pre-wrap text-ink">{prompt}</p>

          {options.length > 0 ? (
            <div className="flex flex-wrap gap-1.5 pt-0.5">
              {options.map((option) => (
                <Button
                  key={option.id}
                  size="sm"
                  variant="secondary"
                  disabled={status === "sending"}
                  onClick={() => void respondApproval(threadId, approvalId, "approve", option.id)}
                >
                  {option.label}
                </Button>
              ))}
            </div>
          ) : null}

          {resolved ? (
            <p className="text-micro text-ink-muted">
              The harness continues from this decision. The cue stays in the stream.
            </p>
          ) : (
            <div className="flex flex-wrap items-center gap-2 pt-0.5">
              <Button
                size="sm"
                data-approval-action="approve"
                disabled={status === "sending"}
                onClick={() => void respondApproval(threadId, approvalId, "approve")}
              >
                <CheckIcon aria-hidden />
                Go
                <Kbd className="ml-0.5 border-ink/20 bg-ink/10 text-ink">
                  {MOD} {ENTER}
                </Kbd>
              </Button>
              <Button
                size="sm"
                variant="secondary"
                disabled={status === "sending"}
                onClick={() => void respondApproval(threadId, approvalId, "deny")}
              >
                <XIcon aria-hidden />
                Hold
                <Kbd className="ml-0.5 text-ink-muted">
                  {MOD} {BACKSPACE}
                </Kbd>
              </Button>
              <span className="text-micro text-ink-muted">
                Go runs it on the host; Hold ends the turn without it.
              </span>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
