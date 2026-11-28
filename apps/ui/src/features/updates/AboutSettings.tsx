import { useTranslation } from "react-i18next";
import { Switch } from "@/components/ui/switch";
import { formatRelativeTime, i18n } from "@/lib/i18n";
import { updateShell } from "@/lib/platform/appUpdate";
import { useApp } from "@/lib/store/appStore";
import {
  appUpdateErrorText,
  checkForAppUpdates,
  setAutoCheck,
  useAppUpdate,
} from "./appUpdateStore";

function checkedLabel(checkedAtMs: number, now = Date.now()): string {
  if (now - checkedAtMs < 60_000) return i18n.t("settings:about.justNow");
  return formatRelativeTime(checkedAtMs, now);
}

/** Settings → General → About: version, update check and the auto-check toggle. */
export function AboutSettings() {
  const { t } = useTranslation(["settings", "common"]);
  const supported = useAppUpdate((state) => state.supported);
  const version = useAppUpdate((state) => state.currentVersion);
  const available = useAppUpdate((state) => state.availableVersion);
  const checkedAt = useAppUpdate((state) => state.checkedAtMs);
  const autoCheck = useAppUpdate((state) => state.autoCheck);
  const checking = useAppUpdate((state) => state.checking);
  const error = useAppUpdate((state) => state.error);
  const daemonVersion = useApp((state) => state.serverVersion);
  const shell = updateShell();

  if (!supported) {
    return (
      <div className="q-group">
        <h2>{t("about.title")}</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("about.daemonVersion")}</div>
          </div>
          <div className="ctl">
            <span className="q-mono q-muted">{daemonVersion ?? "—"}</span>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("about.updates")}</div>
            <div className="s">
              {shell === "mobile" ? t("about.mobileUpdates") : t("about.webUpdates")}
            </div>
          </div>
        </div>
      </div>
    );
  }

  const status = checking
    ? t("common:checking")
    : error
      ? appUpdateErrorText(error)
      : available
        ? t("about.available", { version: available })
        : checkedAt
          ? t("about.upToDate")
          : t("about.notChecked");

  return (
    <div className="q-group">
      <h2>{t("about.title")}</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("about.version")}</div>
          <div className="s">{status}</div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted">{version ?? "—"}</span>
          <span className="flex-1" />
          <button
            type="button"
            className="q-btn secondary"
            disabled={checking}
            onClick={() => void checkForAppUpdates()}
          >
            {t("about.checkForUpdates")}
          </button>
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("about.lastChecked")}</div>
        </div>
        <div className="ctl">
          <span className="q-muted">{checkedAt ? checkedLabel(checkedAt) : t("about.never")}</span>
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("about.checkAutomatically")}</div>
          <div className="s">{t("about.checkAutomaticallyHint")}</div>
        </div>
        <div className="ctl">
          <Switch
            checked={autoCheck}
            aria-label={t("about.checkAutomaticallyLabel")}
            onCheckedChange={() => void setAutoCheck(!autoCheck)}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("about.daemonVersion")}</div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted">{daemonVersion ?? "—"}</span>
        </div>
      </div>
    </div>
  );
}
