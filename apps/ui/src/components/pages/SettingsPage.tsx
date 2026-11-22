import type * as React from "react";
import { useEffect, useState } from "react";

import { ShortcutCaps } from "@/components/common/Kbd";
import { ForgeSettings } from "@/features/forge/ForgeSettings";
import { HarnessSettings } from "@/features/harnesses/HarnessSettings";
import { ProxySettings } from "@/features/proxy/ProxySettings";
import { LiveAccountForm, LiveHostsForm } from "@/features/remote/SettingsRemote";
import { AboutSettings } from "@/features/updates/AboutSettings";
import { AutoResumeSettings } from "@/features/usage/AutoResumeSettings";
import { WorktreeSettings } from "@/features/worktrees/WorktreeSettings";
import {
  COMMAND_GROUPS,
  COMMANDS,
  type CommandId,
  type CommandSpec,
  chordFromEvent,
  chordText,
  formatChord,
  isApplePlatform,
  isCustomized,
  rebind,
  resetAllBindings,
  resetBinding,
  shortcutText,
  useKeybindingsVersion,
} from "@/lib/keybindings";
import { setTheme, setUi, type Theme, useApp } from "@/lib/store/appStore";
import { setTerminalMatchesApp, useTerminalMatchesApp } from "@/lib/terminalTheme";

function Toggle({
  checked,
  label,
  onChange,
}: {
  checked: boolean;
  label: string;
  onChange?: (next: boolean) => void;
}) {
  return (
    <button
      type="button"
      className="q-switch"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange?.(!checked)}
    />
  );
}

const SCHEMES: Array<{ value: Theme; label: string }> = [
  { value: "system", label: "System" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

function GeneralForm() {
  return (
    <div className="q-form">
      <h1>General</h1>
      <p className="lead">How h4rn starts and where it keeps its local state.</p>
      <div className="q-group">
        <h2>Startup</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Open at login</div>
            <div className="s">Start the tray helper with your session</div>
          </div>
          <div className="ctl">
            <Toggle checked label="Open at login" />
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Keep running in tray</div>
            <div className="s">Closing the window leaves agents running</div>
          </div>
          <div className="ctl">
            <Toggle checked label="Keep running in tray" />
          </div>
        </div>
      </div>
      <AboutSettings />
      <div className="q-group">
        <h2>Daemon</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Data directory</div>
          </div>
          <div className="ctl">
            <span className="q-mono q-muted">~/.h4rn</span>
          </div>
        </div>
      </div>
    </div>
  );
}

function AppearanceForm() {
  const theme = useApp((state) => state.ui.theme);
  const terminalMatches = useTerminalMatchesApp();
  return (
    <div className="q-form">
      <h1>Appearance</h1>
      <p className="lead">h4rn follows your system by default. The theme applies to this device.</p>
      <div className="q-group">
        <h2>Theme</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t" id="color-scheme-label">
              Color scheme
            </div>
            <div className="s">
              {theme === "system" ? "Follows your system setting" : `Always ${theme}`}
            </div>
          </div>
          <div className="ctl">
            <fieldset className="q-seg inline q-seg-fs" aria-labelledby="color-scheme-label">
              {SCHEMES.map((scheme) => (
                <button
                  key={scheme.value}
                  type="button"
                  aria-pressed={theme === scheme.value}
                  onClick={() => setTheme(scheme.value)}
                >
                  {scheme.label}
                </button>
              ))}
            </fieldset>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Terminal</div>
            <div className="s">{terminalMatches ? "Matches the app theme" : "Always dark"}</div>
          </div>
          <div className="ctl">
            <Toggle
              checked={terminalMatches}
              label="Terminal matches the app theme"
              onChange={setTerminalMatchesApp}
            />
          </div>
        </div>
      </div>
    </div>
  );
}

function NotificationsForm() {
  return (
    <div className="q-form">
      <h1>Notifications</h1>
      <p className="lead">Only things that need you interrupt you.</p>
      <div className="q-group">
        <h2>Desktop</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Approvals</div>
            <div className="s">A command is waiting to run</div>
          </div>
          <div className="ctl">
            <Toggle checked label="Approval notifications" />
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Finished turns</div>
            <div className="s">A thread lands in Done</div>
          </div>
          <div className="ctl">
            <Toggle checked label="Finished turn notifications" />
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Host offline</div>
            <div className="s">A host becomes unreachable</div>
          </div>
          <div className="ctl">
            <Toggle checked label="Host offline notifications" />
          </div>
        </div>
      </div>
    </div>
  );
}

function ShortcutRow({
  command,
  recording,
  onRecord,
}: {
  command: CommandSpec;
  recording: boolean;
  onRecord: (id: CommandId | null) => void;
}) {
  useKeybindingsVersion();
  const [error, setError] = useState<string | null>(null);
  const customized = isCustomized(command.id);

  useEffect(() => {
    if (!recording) return;
    setError(null);
    setUi({ recordingShortcut: true });
    const onKeyDown = (event: KeyboardEvent) => {
      event.preventDefault();
      event.stopPropagation();
      if (event.key === "Escape" && !event.metaKey && !event.ctrlKey) {
        onRecord(null);
        return;
      }
      if (["Control", "Meta", "Shift", "Alt", "AltGraph"].includes(event.key)) return;
      const chord = chordFromEvent(event);
      if (!chord) {
        setError(`Hold ${isApplePlatform ? "Cmd" : "Ctrl"} with the key.`);
        return;
      }
      const result = rebind(command.id, formatChord(chord));
      if (result.ok) onRecord(null);
      else setError(result.reason);
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      setUi({ recordingShortcut: false });
    };
  }, [recording, command.id, onRecord]);

  return (
    <div className="q-field">
      <div className="lab">
        <div className="t" style={{ fontWeight: 400 }}>
          {command.label}
        </div>
        {error ? (
          <div className="s" role="alert" style={{ color: "var(--danger)" }}>
            {error}
          </div>
        ) : recording ? (
          <div className="s">Press the new shortcut. Esc cancels.</div>
        ) : null}
      </div>
      <div className="ctl">
        {customized && !recording ? (
          <button
            type="button"
            className="q-btn ghost"
            style={{ height: 24 }}
            onClick={() => resetBinding(command.id)}
            aria-label={`Reset ${command.label} to ${chordText(command.binding)}`}
            title={`Reset to ${chordText(command.binding)}`}
          >
            Reset
          </button>
        ) : null}
        <button
          type="button"
          className={`q-keybind${recording ? " on" : ""}`}
          aria-label={
            recording
              ? `Recording a shortcut for ${command.label}`
              : `Change shortcut for ${command.label}, now ${shortcutText(command.id)}`
          }
          aria-pressed={recording}
          onClick={() => onRecord(recording ? null : command.id)}
        >
          {recording ? (
            <span className="q-faint">Press keys…</span>
          ) : (
            <ShortcutCaps command={command.id} />
          )}
        </button>
      </div>
    </div>
  );
}

function ShortcutsForm() {
  useKeybindingsVersion();
  const [recording, setRecording] = useState<CommandId | null>(null);
  const anyCustom = COMMANDS.some((command) => isCustomized(command.id));
  return (
    <div className="q-form">
      <div className="q-form-head">
        <div>
          <h1>Shortcuts</h1>
          <p className="lead">
            Every shortcut is {isApplePlatform ? "Cmd" : "Ctrl"} plus a key, so typing never runs a
            command. Click a shortcut to change it. Changes apply to this device.
          </p>
        </div>
        {anyCustom ? (
          <div>
            <button type="button" className="q-btn secondary" onClick={resetAllBindings}>
              Reset all
            </button>
          </div>
        ) : null}
      </div>
      {COMMAND_GROUPS.map((group) => (
        <div className="q-group" key={group}>
          <h2>{group}</h2>
          {COMMANDS.filter((command) => command.group === group).map((command) => (
            <ShortcutRow
              key={command.id}
              command={command}
              recording={recording === command.id}
              onRecord={setRecording}
            />
          ))}
        </div>
      ))}
    </div>
  );
}

const FORMS: Record<string, () => React.ReactNode> = {
  hosts: () => <LiveHostsForm />,
  devices: () => <LiveHostsForm pairing />,
  providers: ProxySettings,
  harnesses: HarnessSettings,
  worktrees: WorktreeSettings,
  resume: AutoResumeSettings,
  forges: ForgeSettings,
  general: GeneralForm,
  appearance: AppearanceForm,
  notifications: NotificationsForm,
  shortcuts: ShortcutsForm,
  account: LiveAccountForm,
};

/** Static settings surfaces, ported from the approved prototype. */
export function SettingsPage({ section }: { section: string }) {
  const key = FORMS[section] ? section : "hosts";
  const Form = FORMS[key] ?? (() => <LiveHostsForm />);

  return (
    <main className="q-main" aria-label="Settings">
      <div className="q-page">
        <Form />
      </div>
    </main>
  );
}
