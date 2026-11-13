import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import { Kbd } from "./Kbd";

/** An empty state teaches what would appear here and offers one next action. */
export function EmptyState({
  icon: Icon,
  title,
  description,
  action,
  shortcut,
  className,
  compact = false,
}: {
  icon: LucideIcon;
  title: string;
  description: string;
  action?: ReactNode;
  shortcut?: string;
  className?: string;
  compact?: boolean;
}) {
  return (
    <div
      className={cn(
        "flex flex-col items-center justify-center px-6 text-center",
        compact ? "gap-2 py-8" : "gap-3 py-12",
        className,
      )}
    >
      <span className="flex size-10 items-center justify-center rounded-md border border-line bg-panel text-ink-muted">
        <Icon className="size-5" strokeWidth={1.5} aria-hidden />
      </span>
      <div className="max-w-[44ch] space-y-1">
        <p className="text-compact font-medium text-ink">{title}</p>
        <p className="text-micro leading-relaxed text-ink-muted">{description}</p>
      </div>
      {action ? <div className="mt-1 flex items-center gap-2">{action}</div> : null}
      {shortcut ? (
        <p className="flex items-center gap-1 text-micro text-ink-faint">
          or press <Kbd>{shortcut}</Kbd>
        </p>
      ) : null}
    </div>
  );
}
