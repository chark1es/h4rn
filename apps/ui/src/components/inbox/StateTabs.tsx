import { useRef } from "react";
import { useTranslation } from "react-i18next";

import type { ThreadTab } from "@/gen";
import { useSegmentFit } from "@/hooks/use-segment-fit";
import { formatNumber, useActiveLocale } from "@/lib/i18n";
import { TAB_ORDER } from "@/lib/inbox";
import { cn } from "@/lib/utils";

/**
 * The Working / Done / Settled segment above the thread list.
 *
 * It always stays on one line. When a language's labels are too long for the
 * sidebar's width, the labels shrink with an ellipsis (the counts stay) and
 * each tab's full name moves into its tooltip.
 */
export function StateTabs({
  tab,
  counts,
  loading,
  onSelect,
}: {
  tab: ThreadTab;
  counts: Record<ThreadTab, number>;
  loading: boolean;
  onSelect: (tab: ThreadTab) => void;
}) {
  const { t } = useTranslation(["sidebar", "common"]);
  const locale = useActiveLocale();
  const ref = useRef<HTMLDivElement | null>(null);
  const shown = TAB_ORDER.map((value) => (loading ? "—" : formatNumber(counts[value])));
  const fit = useSegmentFit(ref, `${locale}|${shown.join("|")}`);
  return (
    <div ref={ref} className={cn("q-seg", fit && "fit")} role="tablist" aria-label={t("stateTabs")}>
      {TAB_ORDER.map((value, index) => {
        const label = t(`common:states.${value}`);
        return (
          <button
            key={value}
            type="button"
            role="tab"
            aria-selected={tab === value}
            title={fit ? label : undefined}
            onClick={() => onSelect(value)}
          >
            <span className="l">{label}</span> <span className="c q-num">{shown[index]}</span>
          </button>
        );
      })}
    </div>
  );
}
