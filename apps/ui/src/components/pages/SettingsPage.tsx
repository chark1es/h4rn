import type * as React from "react";
import { useEffect, useMemo, useState } from "react";

import { ShortcutCaps } from "@/components/common/Kbd";
import { ForgeSettings } from "@/features/forge/ForgeSettings";
import { HarnessSettings } from "@/features/harnesses/HarnessSettings";
import {
  type NotificationPermissionState,
  notificationChannel,
} from "@/features/notifications/channel";
import {
  displayedNotificationSetting,
  type NotificationSettingKey,
  setNotificationSetting,
  useNotificationSettings,
} from "@/features/notifications/settings";
import { ProxySettings } from "@/features/proxy/ProxySettings";
import { useRemote } from "@/features/remote/remoteStore";
import { LiveAccountForm, LiveHostsForm } from "@/features/remote/SettingsRemote";
import {
  TextGeneratorsSettings,
  ThreadSettings,
} from "@/features/text-generation/TextGenerationSettings";
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
import { desktopShellBridge, type ShellSettings } from "@/lib/platform/desktopShell";
import { secretShell } from "@/lib/platform/secrets";
import { setTheme, setUi, type Theme, useApp } from "@/lib/store/appStore";
import { setTerminalMatchesApp, useTerminalMatchesApp } from "@/lib/terminalTheme";

function Toggle({
  checked,
  label,
  onChange,
  disabled = false,
}: {
  checked: boolean;
  label: string;
  onChange?: (next: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      className="q-switch"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      aria-disabled={disabled || undefined}
      disabled={disabled}
      onClick={() => onChange?.(!checked)}
    />
  );
}

const SCHEMES: Array<{ value: Theme; label: string }> = [
  { value: "system", label: "System" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

function describeError(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return "The change did not stick.";
}

function GeneralForm() {
  const shell = useMemo(desktopShellBridge, []);
  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [shellSettings, setShellSettings] = useState<ShellSettings | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!shell) return;
    let cancelled = false;
    void shell
      .autostartEnabled()
      .then((enabled) => {
        if (!cancelled) setAutostart(enabled);
      })
      .catch((cause: unknown) => {
        if (!cancelled) setError(describeError(cause));
      });
    void shell
      .shellSettings()
      .then((settings) => {
        if (!cancelled) setShellSettings(settings);
      })
      .catch((cause: unknown) => {
        if (!cancelled) setError(describeError(cause));
      });
    return () => {
      cancelled = true;
    };
  }, [shell]);

  const changeAutostart = async (next: boolean) => {
    if (!shell) return;
    setError(null);
    try {
      await shell.setAutostart(next);
      setAutostart(await shell.autostartEnabled());
    } catch (cause) {
      setError(describeError(cause));
    }
  };

  const changeTray = async (next: boolean) => {
    if (!shell) return;
    setError(null);
    try {
      setShellSettings(await shell.setKeepInTray(next));
    } catch (cause) {
      setError(describeError(cause));
    }
  };

  const trayAvailable = shellSettings?.tray_available === true;
  const keepInTray = trayAvailable && shellSettings?.keep_in_tray === true;

  return (
    <div className="q-form">
      <h1>General</h1>
      <p className="lead">How h4rn starts and where it keeps its local state.</p>
      {shell ? (
        <div className="q-group">
          <h2>Startup</h2>
          <div className="q-field">
            <div className="lab">
              <div className="t">Open at login</div>
              <div className="s">Start h4rn when you sign in to this computer</div>
            </div>
            <div className="ctl">
              <Toggle
                checked={autostart ?? false}
                disabled={autostart === null}
                label="Open at login"
                onChange={(next) => void changeAutostart(next)}
              />
            </div>
          </div>
          <div className="q-field">
            <div className="lab">
              <div className="t">Keep running in tray</div>
              <div className="s">
                {shellSettings !== null && !trayAvailable
                  ? "No tray icon was created, so closing the window quits h4rn"
                  : keepInTray
                    ? "Closing the window keeps h4rn in the tray — open h4rn again to show it"
                    : "Closing the window quits h4rn"}
              </div>
            </div>
            <div className="ctl">
              <Toggle
                checked={keepInTray}
                disabled={shellSettings === null || !trayAvailable}
                label="Keep running in tray"
                onChange={(next) => void changeTray(next)}
              />
            </div>
          </div>
          {error ? (
            <p className="s" role="alert" style={{ color: "var(--danger)" }}>
              {error}
            </p>
          ) : null}
        </div>
      ) : null}
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

const NOTIFICATION_ROWS: Array<{
  key: NotificationSettingKey;
  title: string;
  subtitle: string;
  label: string;
}> = [
  {
    key: "approvals",
    title: "Approvals",
    subtitle: "A command or question is waiting to run",
    label: "Approval notifications",
  },
  {
    key: "finishedTurns",
    title: "Finished turns",
    subtitle: "A thread lands in Done",
    label: "Finished turn notifications",
  },
  {
    key: "hostOffline",
    title: "Host offline",
    subtitle: "A host becomes unreachable",
    label: "Host offline notifications",
  },
];

function NotificationsForm() {
  const channel = useMemo(notificationChannel, []);
  const settings = useNotificationSettings();
  const shell = secretShell();
  const signedIn = useRemote((state) => state.user !== null);
  const linkedMachineCount = useRemote((state) => state.linkedMachines.length);
  // Host presence comes from the relay for the machines this device is linked
  // to. A direct-only session has no signal for other hosts, so the row is
  // hidden rather than shown as a dead switch.
  const relayPresence = signedIn && linkedMachineCount > 0;
  const [permission, setPermission] = useState<NotificationPermissionState | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!channel) return;
    let cancelled = false;
    void channel
      .permission()
      .then((state) => {
        if (!cancelled) setPermission(state);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setPermission("unknown");
          setError(describeError(cause));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [channel]);

  const change = async (key: NotificationSettingKey, next: boolean) => {
    if (!channel) return;
    setError(null);
    if (next) {
      try {
        const state = await channel.requestPermission();
        setPermission(state);
        // The desktop shell cannot report a permission; the OS still decides
        // whether a notification appears, so the switch is allowed to mean
        // "ask for this" there. A browser that refused keeps the stored
        // preference but shows no switch at all.
        if (state === "granted" || state === "unknown") {
          setNotificationSetting(key, true);
        }
        return;
      } catch (cause) {
        setError(describeError(cause));
        return;
      }
    }
    setNotificationSetting(key, next);
  };

  const rows = relayPresence
    ? NOTIFICATION_ROWS
    : NOTIFICATION_ROWS.filter((row) => row.key !== "hostOffline");

  return (
    <div className="q-form">
      <h1>Notifications</h1>
      <p className="lead">Only things that need you interrupt you.</p>
      {channel ? (
        <div className="q-group">
          <h2>On this device</h2>
          {rows.map((row) => (
            <div className="q-field" key={row.key}>
              <div className="lab">
                <div className="t">{row.title}</div>
                <div className="s">{row.subtitle}</div>
              </div>
              <div className="ctl">
                {permission === null ? (
                  <span className="q-faint">Checking…</span>
                ) : permission === "denied" ? (
                  <span className="q-muted">Blocked in system settings</span>
                ) : (
                  <Toggle
                    checked={displayedNotificationSetting(permission, settings[row.key])}
                    label={row.label}
                    onChange={(next) => void change(row.key, next)}
                  />
                )}
              </div>
            </div>
          ))}
          {!relayPresence ? (
            <p className="s q-muted">
              Host offline alerts need a host linked through the relay. This device has none, so
              there is no other host to watch.
            </p>
          ) : null}
          {error ? (
            <p className="s" role="alert" style={{ color: "var(--danger)" }}>
              {error}
            </p>
          ) : null}
        </div>
      ) : (
        <div className="q-group">
          <h2>On this device</h2>
          <p className="lead">
            {shell === "mobile"
              ? "This build does not send notifications yet. Use the desktop app or a browser."
              : "This browser does not support notifications."}
          </p>
        </div>
      )}
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
  threads: ThreadSettings,
  providers: () => (
    <>
      <TextGeneratorsSettings />
      <ProxySettings />
    </>
  ),
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
