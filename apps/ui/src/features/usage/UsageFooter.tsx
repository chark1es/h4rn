import { useIsPhone } from "@/hooks/use-media-query";
import { cost, duration, tokens, tokensPerSecond } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";

import { firstToken } from "./presentation";
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

  const usage = live?.usage ?? summary?.usage;
  if (!usage) return null;

  const total = usage.input_tokens + usage.output_tokens;
  const tps = live?.tps ?? usage.tps;
  const ttft = live?.ttft_ms ?? summary?.ttft_ms ?? 0;
  const active = live?.elapsed_ms ?? usage.duration_ms;
  const filled = summary?.context_fill_percent ?? null;

  return (
    <div className="q-usage-foot q-num">
      <span>{tokens(total)} tokens</span>
      <span>{cost(usage.cost_micros)} est.</span>
      <span>{tokensPerSecond(tps)} tok/s</span>
      {isPhone ? null : <span>TTFT {firstToken(ttft)}</span>}
      {isPhone ? null : <span>{duration(active)} active</span>}
      {isPhone ? null : <span className="grow" />}
      {filled !== null ? (
        <span>
          <span className="q-ctxbar" aria-hidden>
            <i style={{ width: `${filled}%` }} />
          </span>
          {filled}% context
        </span>
      ) : null}
    </div>
  );
}
