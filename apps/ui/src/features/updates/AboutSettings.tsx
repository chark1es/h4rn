import { relativeAge } from "@/lib/format";
import { updateShell } from "@/lib/platform/appUpdate";
import { useApp } from "@/lib/store/appStore";

import { checkForAppUpdates, setAutoCheck, useAppUpdate } from "./appUpdateStore";

function checkedLabel(checkedAtMs: number): string {
  const age = relativeAge(new Date(checkedAtMs).toISOString());
  return age === "now" ? "Just now" : `${age} ago`;
}

/** Settings → General → About: version, update check and the auto-check toggle. */
export function AboutSettings() {
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
        <h2>About</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Daemon version</div>
          </div>
          <div className="ctl">
            <span className="q-mono q-muted">{daemonVersion ?? "—"}</span>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Updates</div>
            <div className="s">
              {shell === "mobile"
                ? "Mobile updates come through the App Store and Google Play."
                : "The web build updates when its host serves a new version."}
            </div>
          </div>
        </div>
      </div>
    );
  }

  const status = checking
    ? "Checking…"
    : error
      ? error
      : available
        ? `h4rn ${available} is available`
        : checkedAt
          ? "You're up to date"
          : "Not checked yet";

  return (
    <div className="q-group">
      <h2>About</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">Version</div>
          <div className="s">{status}</div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted">{version ?? "—"}</span>
          <span style={{ flex: 1 }} />
          <button
            type="button"
            className="q-btn secondary"
            disabled={checking}
            onClick={() => void checkForAppUpdates()}
          >
            Check for updates
          </button>
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Last checked</div>
        </div>
        <div className="ctl">
          <span className="q-muted">{checkedAt ? checkedLabel(checkedAt) : "Never"}</span>
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Check automatically</div>
          <div className="s">On launch, then every 6 hours</div>
        </div>
        <div className="ctl">
          <button
            type="button"
            className="q-switch"
            role="switch"
            aria-checked={autoCheck}
            aria-label="Check for updates automatically"
            onClick={() => void setAutoCheck(!autoCheck)}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Daemon version</div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted">{daemonVersion ?? "—"}</span>
        </div>
      </div>
    </div>
  );
}
