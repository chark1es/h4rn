import { WifiOffIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { errorWords } from "@/lib/client/errors";
import { absoluteTime } from "@/lib/format";
import { formatNumber } from "@/lib/i18n";
import { joinSentences } from "@/lib/i18n/sentences";
import { getClient, useApp } from "@/lib/store/appStore";

export function OfflineBanner({ inline = false }: { inline?: boolean }) {
  const status = useApp((state) => state.connection.status);
  const attempt = useApp((state) => state.connection.attempt);
  const error = useApp((state) => state.connection.error);
  const lastSyncAt = useApp((state) => state.lastSyncAt);
  const { t } = useTranslation("shell");

  // The first handshake is represented by the skeleton screens, not a banner.
  if (status === "online" || status === "connecting") return null;

  const cached = lastSyncAt !== null ? absoluteTime(new Date(lastSyncAt).toISOString()) : null;
  // A client-written error is worded again in the active language.
  const words = error ? errorWords(error) : null;

  return (
    <div
      role="status"
      className="q-banner"
      style={inline ? { flex: "none", margin: "0 12px 8px" } : { flex: "none", marginTop: 8 }}
      title={words ? joinSentences([words.message, words.hint]) : undefined}
    >
      <span className="ic">
        <WifiOffIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        {status === "degraded" ? (
          <>
            <b>{t("offline.reconnecting")}</b>
            {attempt > 0 ? ` · ${t("offline.attempt", { attempt: formatNumber(attempt) })}` : ""}
            {cached ? ` · ${t("offline.cached", { time: cached })}` : ""}
          </>
        ) : (
          <>
            <b>{t("offline.offline")}</b>
            {` · ${cached ? t("offline.cached", { time: cached }) : t("offline.noCache")}`}
          </>
        )}
      </span>
      <button
        type="button"
        className="q-btn ghost q-offline-action"
        onClick={() => getClient().reconnectNow()}
      >
        {t("offline.reconnect")}
      </button>
    </div>
  );
}
