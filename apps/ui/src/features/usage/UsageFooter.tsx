import { useTranslation } from "react-i18next";

import { useIsPhone } from "@/hooks/use-media-query";
import { formatCost, formatTokens } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import { firstToken, formatPercent, formatRate, formatSpan } from "./presentation";
import { useUsage } from "./usageStore";

/**
 * The quiet footer under the composer. A live tick wins over the summary so
 * the numbers move while a turn runs; the context bar appears only when the
 * proxy catalogue reported a window for this model.
 */
export function UsageFooter({ threadId }: { threadId: string }) {
  const summary = useApp((state) => state.threads[threadId]);
  const live = useUsage((state) => state.live[threadId]);
  const isPhone = useIsPhone();
  const { t } = useTranslation("usage");

  const usage = live?.usage ?? summary?.usage;
  if (!usage) return null;

  const total = usage.input_tokens + usage.output_tokens;
  const tps = live?.tps ?? usage.tps;
  const ttft = live?.ttft_ms ?? summary?.ttft_ms ?? 0;
  const active = live?.elapsed_ms ?? usage.duration_ms;
  const filled = summary?.context_fill_percent ?? null;

  return (
    <div className="q-usage-foot q-num">
      <span>{t("footer.tokens", { tokens: formatTokens(total) })}</span>
      <span>{t("footer.cost", { cost: formatCost(usage.cost_micros) })}</span>
      <span>{t("footer.rate", { rate: formatRate(tps) })}</span>
      {isPhone ? null : <span>{t("footer.ttft", { time: firstToken(ttft) })}</span>}
      {isPhone ? null : <span>{t("footer.active", { duration: formatSpan(active) })}</span>}
      {isPhone ? null : <span className="grow" />}
      {filled !== null ? (
        <span>
          <span className="q-ctxbar" aria-hidden>
            <i style={{ width: `${filled}%` }} />
          </span>
          {t("footer.context", { percent: formatPercent(filled) })}
        </span>
      ) : null}
    </div>
  );
}
