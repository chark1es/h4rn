import { CheckIcon, ShieldCheckIcon, XIcon } from "lucide-react";

import { ENTER, Kbd, MOD } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import { respondApproval, useApp } from "@/lib/store/appStore";

export function StandbyBar({ threadId }: { threadId: string }) {
  const log = useApp((state) => state.logs[threadId]);
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

  return (
    <div className="shrink-0 border-t border-cue/30 bg-cue-tint/80">
      <div className="mx-auto flex w-full max-w-[760px] items-start gap-2 px-3 py-2 sm:items-center sm:px-6">
        <span aria-hidden className="mt-0.5 h-8 w-0.5 shrink-0 rounded-sm bg-cue sm:mt-0" />
        <ShieldCheckIcon className="mt-0.5 size-4 shrink-0 text-cue-ink sm:mt-0" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="line-clamp-2 text-micro leading-snug text-cue-ink sm:line-clamp-1 sm:text-compact">
            <span className="font-semibold">Waiting for approval:</span> {first.prompt}
          </p>
          <p className="hidden text-micro text-cue-ink/70 sm:block">
            Go runs it on the host; Hold ends the turn without it.
            {more > 0 ? ` +${more} more waiting.` : ""}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          <Button
            size="sm"
            className="h-9 px-2.5 sm:h-[26px]"
            disabled={status === "sending"}
            onClick={() => void respondApproval(threadId, first.approvalId, "approve")}
          >
            <CheckIcon aria-hidden />
            Go
            <Kbd className="hidden border-ink/20 bg-ink/10 text-ink sm:inline-flex">
              {MOD} {ENTER}
            </Kbd>
          </Button>
          <Button
            size="sm"
            variant="secondary"
            className="h-9 px-2.5 sm:h-[26px]"
            disabled={status === "sending"}
            onClick={() => void respondApproval(threadId, first.approvalId, "deny")}
          >
            <XIcon aria-hidden />
            Hold
          </Button>
        </div>
      </div>
    </div>
  );
}
