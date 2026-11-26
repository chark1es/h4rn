import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Trans, useTranslation } from "react-i18next";

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
  const { t } = useTranslation("common");
  return (
    <div
      className={cn(
        "flex flex-col items-center justify-center px-6 text-center",
        compact ? "gap-2 py-8" : "gap-3 py-12",
        className,
      )}
    >
      <span className="flex size-10 items-center justify-center rounded-md border border-line bg-bg-side text-fg-2">
        <Icon className="size-5" strokeWidth={1.5} aria-hidden />
      </span>
      <div className="max-w-[44ch] space-y-1">
        <p className="text-compact font-medium text-fg">{title}</p>
        <p className="text-small leading-relaxed text-fg-2">{description}</p>
      </div>
      {action ? <div className="mt-1 flex items-center gap-2">{action}</div> : null}
      {shortcut ? (
        <p className="flex items-center gap-1 text-small text-fg-3">
          <Trans t={t} i18nKey="orPress" components={{ kbd: <Kbd>{shortcut}</Kbd> }} />
        </p>
      ) : null}
    </div>
  );
}
