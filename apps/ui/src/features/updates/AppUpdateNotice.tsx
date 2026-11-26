import { DownloadIcon } from "lucide-react";
import { useEffect } from "react";
import { Trans, useTranslation } from "react-i18next";

import { formatNumber } from "@/lib/i18n";

import {
  installAppUpdate,
  noticeVisible,
  snoozeAppUpdate,
  startAppUpdates,
  useAppUpdate,
} from "./appUpdateStore";

/**
 * A quiet, non-modal notice that a newer h4rn is available.
 *
 * It renders only inside the desktop shell. Neutral on purpose: an update is
 * not something an agent is waiting on, so it does not borrow the accent.
 */
export function AppUpdateNotice({ inline = false }: { inline?: boolean }) {
  const { t } = useTranslation(["shell", "common"]);
  useEffect(() => {
    void startAppUpdates();
  }, []);
  const visible = useAppUpdate(noticeVisible);
  const version = useAppUpdate((state) => state.availableVersion);
  const progress = useAppUpdate((state) => state.progress);

  if (!visible || !version) return null;

  const busy = progress?.phase === "downloading" || progress?.phase === "installing";
  const percent =
    progress?.phase === "downloading" && progress.total
      ? Math.min(100, Math.round((progress.downloaded / progress.total) * 100))
      : null;

  return (
    <div
      role="status"
      aria-live="polite"
      className="q-banner q-update-notice"
      style={inline ? { flex: "none", margin: "0 12px 8px" } : { flex: "none", marginTop: 8 }}
    >
      <span className="ic">
        <DownloadIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        {progress?.phase === "failed" ? (
          // The shell's own sentence is shown as sent; null is the client's.
          (progress.message ?? t("update.installFailed"))
        ) : progress?.phase === "installing" ? (
          <Trans t={t} i18nKey="update.installing" values={{ version }} components={{ b: <b /> }} />
        ) : busy ? (
          <>
            <b>{t("update.downloading", { version })}</b>
            {percent !== null ? ` · ${formatNumber(percent / 100, { style: "percent" })}` : ""}
          </>
        ) : (
          <Trans t={t} i18nKey="update.available" values={{ version }} components={{ b: <b /> }} />
        )}
      </span>
      {progress?.phase === "downloading" && percent !== null ? (
        <span className="bar" aria-hidden>
          <span style={{ width: `${percent}%` }} />
        </span>
      ) : null}
      {busy ? null : (
        <>
          <button type="button" className="q-btn secondary" onClick={() => void installAppUpdate()}>
            {progress?.phase === "failed" ? t("common:tryAgain") : t("update.restart")}
          </button>
          <button type="button" className="q-btn ghost" onClick={snoozeAppUpdate}>
            {t("update.later")}
          </button>
        </>
      )}
    </div>
  );
}
