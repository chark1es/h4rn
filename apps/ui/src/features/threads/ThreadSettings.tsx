import { useTranslation } from "react-i18next";

import { setDefaultPermissionMode, useThreadSettings } from "@/features/threads/settings";
import type { RuntimeMode } from "@/gen";
import { MODE_CHOICES } from "@/lib/threadControls";

/**
 * Settings → Threads → New threads: the permission mode a new thread starts
 * on, and the mode a thread must differ from to show its settings next to the
 * model.
 *
 * The section renders inside the Threads page, which also owns the title
 * settings; it deliberately carries no page heading of its own.
 */
export function NewThreadDefaults() {
  const { t } = useTranslation(["settings", "common"]);
  const settings = useThreadSettings();
  return (
    <div className="q-group">
      <h2>{t("threads.newThreads")}</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("threads.permissions")}</div>
          <div className="s">{t("threads.permissionsHint")}</div>
        </div>
        <div className="ctl">
          <div className="q-seg" role="radiogroup" aria-label={t("threads.defaultPermissions")}>
            {MODE_CHOICES.map((choice) => (
              // biome-ignore lint/a11y/useSemanticElements: a segmented radio group, not a form radio
              <button
                key={choice.value}
                type="button"
                role="radio"
                aria-checked={settings.defaultPermissionMode === choice.value}
                onClick={() => setDefaultPermissionMode(choice.value as RuntimeMode)}
              >
                {t(`common:permissionModes.${choice.value}`)}
              </button>
            ))}
          </div>
        </div>
      </div>
      <p className="s q-muted">{t("threads.planNote")}</p>
    </div>
  );
}
