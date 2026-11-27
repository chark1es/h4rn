import { useEffect, useState } from "react";

import type { HarnessKind, HostId } from "@/gen";
import { getClient } from "@/lib/store/appStore";

export function useThreadModels(host: HostId, harness: HarnessKind, enabled: boolean) {
  const [catalog, setCatalog] = useState<{
    host: HostId;
    harness: HarnessKind;
    models: string[];
  } | null>(null);

  useEffect(() => {
    setCatalog(null);
    if (!enabled || !host || harness === "acp") return;
    let cancelled = false;
    void getClient()
      .request({ type: "list_harness_models", host, harness }, { timeoutMs: 12_000 })
      .then((response) => {
        if (!cancelled && response?.type === "harness_models") {
          setCatalog({ host, harness, models: response.models });
        }
      })
      .catch(() => {
        // Older or offline hosts can continue with the harness-specific fallback.
      });
    return () => {
      cancelled = true;
    };
  }, [enabled, host, harness]);

  return enabled && catalog?.host === host && catalog.harness === harness ? catalog.models : null;
}
