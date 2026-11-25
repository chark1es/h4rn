import { FlaskConicalIcon } from "lucide-react";
import { useEffect } from "react";
import { loadOutsideAgentSettings, setOutsideAgentEnabled, useAccess } from "../access/store";
import { loadRewindSettings, setRewindFilesEnabled, useRewindSettings } from "../rewind/settings";
import { loadGuestSettings, setGuestLinksEnabled, useGuest } from "./guestStore";

/**
 * Settings → Labs → Guest links.
 *
 * One setting for this machine, owned by the daemon. Off ends every live link
 * and deletes the messages waiting for the owner, so turning it back on does
 * not revive them: the owner shares again. The page says what changes.
 */
export function GuestSettingsForm() {
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
      <h1>Labs</h1>
      <p className="lead">Features that are complete but still settling in.</p>
      <div className="q-group">
        <h2>Threads</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Rewind files with Edit from here</div>
            <div className="s">
              Put a thread&rsquo;s own worktree back to the checkpoint taken before an edited
              message. Off by default while the file restore is still being hardened; the
              conversation rewind works either way.
            </div>
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={rewindFilesEnabled === true}
              aria-label="Rewind files with Edit from here"
              disabled={rewindLoading || rewindSaving || rewindFilesEnabled === null}
              onClick={() => void setRewindFilesEnabled(rewindFilesEnabled !== true)}
            />
          </div>
        </div>
        {rewindError ? (
          <p className="q-danger" role="alert" style={{ margin: "8px 0 0", fontSize: 12 }}>
            {rewindError}
          </p>
        ) : null}
        <p className="q-muted" style={{ fontSize: 12.5 }}>
          While this is off, editing an earlier prompt still removes that message and everything
          after it from the thread and the harness&rsquo;s history; only the workspace files stay as
          they are.
        </p>
      </div>
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
        <p className="q-muted" style={{ fontSize: 12.5 }}>
          Turning guest links off ends every existing guest link on this machine and deletes the
          messages waiting for you; turning it back on does not revive them. Settling a thread,
          creating a new link or choosing Stop sharing also ends a link for good.
        </p>
      </div>
      <div className="q-group">
        <h2>Outside agents</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Outside agents (OAuth)</div>
            <div className="s">
              Let Claude Code, Codex or another local MCP client sign in over OAuth and work with
              your projects and threads. Off by default. Turning it off revokes every grant and
              drops every sign-in waiting for you.
            </div>
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-switch"
              role="switch"
              aria-checked={outsideEnabled === true}
              aria-label="Outside agents (OAuth)"
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
        <p className="q-faint" style={{ margin: "8px 0 0", fontSize: 12.5 }}>
          While this is off, h4rn&rsquo;s OAuth routes do not exist, outside-agent tokens are
          refused, and no sign-in can be created. Agents h4rn runs keep their own tools.
        </p>
      </div>
    </div>
  );
}
