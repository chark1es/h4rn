import { FlaskConicalIcon } from "lucide-react";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { errorWords } from "@/lib/client/errors";
import { loadOutsideAgentSettings, setOutsideAgentEnabled, useAccess } from "../access/store";
import { loadRewindSettings, setRewindFilesEnabled, useRewindSettings } from "../rewind/settings";
import { guestErrorText, loadGuestSettings, setGuestLinksEnabled, useGuest } from "./guestStore";

/**
 * Settings → Labs → Guest links.
 *
 * One setting for this machine, owned by the daemon. Off ends every live link
 * and deletes the messages waiting for the owner, so turning it back on does
 * not revive them: the owner shares again. The page says what changes.
 */
export function GuestSettingsForm() {
  const { t } = useTranslation(["settings", "common"]);
  const { settingsEnabled, settingsLoading, settingsSaving, error } = useGuest();
  const outsideEnabled = useAccess((state) => state.outsideEnabled);
  const outsideSaving = useAccess((state) => Boolean(state.pending["outside:switch"]));
  const outsideError = useAccess((state) => state.error);
  const {
    filesEnabled: rewindFilesEnabled,
    loading: rewindLoading,
    saving: rewindSaving,
    error: rewindError,
  } = useRewindSettings();

  useEffect(() => {
    void loadGuestSettings();
    void loadOutsideAgentSettings();
    void loadRewindSettings();
  }, []);

  return (
    <div className="q-form">
      <h1>{t("labs.title")}</h1>
      <p className="lead">{t("labs.lead")}</p>
      <div className="q-group">
        <h2>{t("labs.threads")}</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("labs.rewindFiles")}</div>
            <div className="s">{t("labs.rewindFilesHint")}</div>
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={rewindFilesEnabled === true}
              aria-label={t("labs.rewindFiles")}
              disabled={rewindLoading || rewindSaving || rewindFilesEnabled === null}
              onClick={() => void setRewindFilesEnabled(rewindFilesEnabled !== true)}
            />
          </div>
        </div>
        {rewindError ? (
          <p className="q-danger" role="alert" style={{ margin: "8px 0 0", fontSize: 12 }}>
            {errorWords(rewindError).message}
          </p>
        ) : null}
        <p className="q-muted" style={{ fontSize: 13 }}>
          {t("labs.rewindOffNote")}
        </p>
      </div>
      <div className="q-group">
        <h2>{t("labs.sharing")}</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("labs.guestLinks")}</div>
            <div className="s">{t("labs.guestLinksHint")}</div>
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={settingsEnabled === true}
              aria-label={t("labs.guestLinks")}
              disabled={settingsLoading || settingsSaving || settingsEnabled === null}
              onClick={() => void setGuestLinksEnabled(settingsEnabled !== true)}
            />
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("labs.guestSees")}</div>
            <div className="s">{t("labs.guestSeesHint")}</div>
          </div>
          <div className="ctl">
            <FlaskConicalIcon className="q-icon s16 q-faint" strokeWidth={1.5} aria-hidden />
          </div>
        </div>
        {error ? (
          <p className="q-danger" role="alert" style={{ margin: "8px 0 0", fontSize: 12 }}>
            {guestErrorText(error)}
          </p>
        ) : null}
        <p className="q-muted" style={{ fontSize: 13 }}>
          {t("labs.guestOffNote")}
        </p>
      </div>
      <div className="q-group">
        <h2>{t("labs.outsideAgents")}</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("labs.outsideAgentsOAuth")}</div>
            <div className="s">{t("labs.outsideAgentsHint")}</div>
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={outsideEnabled === true}
              aria-label={t("labs.outsideAgentsOAuth")}
              disabled={outsideSaving || outsideEnabled === null}
              onClick={() => void setOutsideAgentEnabled(outsideEnabled !== true)}
            />
          </div>
        </div>
        {outsideError ? (
          <p className="q-danger" role="alert" style={{ margin: "8px 0 0", fontSize: 12 }}>
            {outsideError.message}
            {outsideError.hint ? ` ${outsideError.hint}` : ""}
          </p>
        ) : null}
        <p className="q-faint" style={{ margin: "8px 0 0", fontSize: 13 }}>
          {t("labs.outsideOffNote")}
        </p>
      </div>
    </div>
  );
}
