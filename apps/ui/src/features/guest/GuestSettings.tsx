import { FlaskConicalIcon } from "lucide-react";
import { useEffect } from "react";

import { loadGuestSettings, setGuestLinksEnabled, useGuest } from "./guestStore";

/**
 * Settings → Labs → Guest links.
 *
 * One setting for this machine, owned by the daemon. Off stops every live link
 * without deleting it; on makes unexpired links work again. Links that were
 * created while it was off do not exist, so the page says what changes.
 */
export function GuestSettingsForm() {
  const { settingsEnabled, settingsLoading, settingsSaving, error } = useGuest();

  useEffect(() => {
    void loadGuestSettings();
  }, []);

  return (
    <div className="q-form">
      <h1>Labs</h1>
      <p className="lead">Features that are complete but still settling in.</p>
      <div className="q-group">
        <h2>Sharing</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Guest links</div>
            <div className="s">
              Let one teammate follow one thread in a browser, with no account. Every message they
              write waits for you.
            </div>
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={settingsEnabled === true}
              aria-label="Guest links"
              disabled={settingsLoading || settingsSaving || settingsEnabled === null}
              onClick={() => void setGuestLinksEnabled(settingsEnabled !== true)}
            />
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">What a guest can see</div>
            <div className="s">
              One thread&rsquo;s messages, the agent&rsquo;s replies, its reasoning, plans and tool
              output, including anything the agent printed such as a value from a file. Never diffs,
              other threads, files, terminals or settings.
            </div>
          </div>
          <div className="ctl">
            <FlaskConicalIcon className="q-icon s16 q-faint" strokeWidth={1.5} aria-hidden />
          </div>
        </div>
        {error ? (
          <p className="q-danger" role="alert" style={{ margin: "8px 0 0", fontSize: 12 }}>
            {error}
          </p>
        ) : null}
      </div>
      <p className="q-muted" style={{ fontSize: 12.5 }}>
        While this is off, every existing guest link on this machine stops working. Turn it back on
        and links that have not expired work again. Settling a thread, creating a new link or
        choosing Stop sharing ends a link for good.
      </p>
    </div>
  );
}
