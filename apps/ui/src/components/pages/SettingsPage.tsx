import type * as React from "react";
import { useEffect, useMemo, useState } from "react";

import { ShortcutCaps } from "@/components/common/Kbd";
import { AccessSettings } from "@/features/access/AccessSettings";
import { ForgeSettings } from "@/features/forge/ForgeSettings";
import { GuestSettingsForm } from "@/features/guest/GuestSettings";
import { HarnessSettings } from "@/features/harnesses/HarnessSettings";
import {
  agentAlertsAvailabilityText,
  agentAlertsSwitchEnabled,
  loadAgentAlertSettings,
  setAgentAlertsEnabled,
  useAgentAlerts,
} from "@/features/notifications/agentAlerts";
import {
  type NotificationPermissionState,
  notificationChannel,
  useBrowserNotificationsUnavailable,
} from "@/features/notifications/channel";
import {
  type DeviceAlertsState,
  deviceAvailabilityNote,
  deviceLiveUpdatesEnabled,
  deviceLiveUpdatesSupported,
  deviceStatusLine,
  deviceToggleUsable,
  disableDeviceAlerts,
  enableDeviceAlerts,
  readDeviceAlerts,
  setDeviceLiveUpdates,
} from "@/features/notifications/devicePush";
import {
  displayedNotificationSetting,
  hostOfflineVisibility,
  type NotificationSettingKey,
  setNotificationSetting,
  useNotificationSettings,
} from "@/features/notifications/settings";
import { ProxySettings } from "@/features/proxy/ProxySettings";
import { useRemote } from "@/features/remote/remoteStore";
import { LiveAccountForm, LiveHostsForm } from "@/features/remote/SettingsRemote";
import { SchedulesSettings } from "@/features/schedules/SchedulesSettings";
import { SourceControlSettings } from "@/features/source-control/SourceControlSettings";
import {
  TextGeneratorsSettings,
  ThreadSettings,
} from "@/features/text-generation/TextGenerationSettings";
import { NewThreadDefaults } from "@/features/threads/ThreadSettings";
import { AboutSettings } from "@/features/updates/AboutSettings";
import { AutoResumeSettings } from "@/features/usage/AutoResumeSettings";
import { WorktreeSettings } from "@/features/worktrees/WorktreeSettings";
import {
  setFollowUpBehavior,
  setMultilineEnter,
  useFollowUpBehavior,
  useMultilineEnter,
} from "@/lib/chatSettings";
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
import { deviceAlertsShell } from "@/lib/platform/push";
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
  return (
    <div className="q-form">
      <h1>General</h1>
      <p className="lead">How h4rn starts and where it keeps its local state.</p>
      <StartupSettings />
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

/**
 * What the OS says about the login item.
 *
 * `unsupported` is not an error: a desktop build without the autostart plugin
 * has no real state to show, so the row is left out instead of offering a
 * switch that cannot work. Every other read failure is a real problem and is
 * shown as an error line with the row, rather than hidden as if the plugin
 * were missing.
 */
type AutostartState =
  | { status: "loading" }
  | { status: "ready"; enabled: boolean }
  | { status: "unsupported" }
  | { status: "error"; error: string };

/**
 * Whether an autostart read failed because this shell simply has no plugin.
 *
 * Only the shell's own "no such plugin/command" answer counts. A permission
 * error, a refused command or anything else the OS reports is a failure the
 * person should see, not a reason to pretend the setting does not exist.
 */
export function autostartUnsupported(message: string): boolean {
  // A missing plugin or command is the only shape that means "this build
  // cannot offer the setting". A permission or capability denial ("not
  // allowed") comes from a plugin that is present, so the row must stay and
  // say what failed instead of vanishing.
  const missing =
    /(?:plugin[: ][\w-]+|unknown plugin|plugin)\b[^\n]*\b(?:not found|not installed|does not exist|unknown)\b/i;
  return (
    missing.test(message) ||
    /(?:command|plugin) not found/i.test(message) ||
    /no such (?:command|plugin)/i.test(message)
  );
}

/**
 * Settings → General → Startup: the login item and the tray.
 *
 * Both switches read the real OS state. The autostart row appears only after
 * that state is known, and stays hidden when the plugin is unavailable; a
 * fake "off" during the read would claim a setting the person never chose.
 */
export function StartupSettings() {
  const shell = useMemo(desktopShellBridge, []);
  const [autostart, setAutostart] = useState<AutostartState>({ status: "loading" });
  const [shellSettings, setShellSettings] = useState<ShellSettings | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!shell) return;
    let cancelled = false;
    void shell
      .autostartEnabled()
      .then((enabled) => {
        if (!cancelled) setAutostart({ status: "ready", enabled });
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        const reason = describeError(cause);
        if (autostartUnsupported(reason)) {
          // A shell without the plugin is not a failed read; there is simply
          // no login item to control here.
          setAutostart({ status: "unsupported" });
          return;
        }
        // Anything else (a blocked permission, a refused command) is a real
        // failure: keep the row and say what went wrong. The reason lives on
        // this row's state, so toggling the tray or a later autostart write
        // does not erase why the read failed.
        setAutostart({ status: "error", error: reason });
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
    if (!shell || autostart.status !== "ready") return;
    setError(null);
    try {
      await shell.setAutostart(next);
      setAutostart({ status: "ready", enabled: await shell.autostartEnabled() });
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

  if (!shell) return null;

  const trayAvailable = shellSettings?.tray_available === true;
  const keepInTray = trayAvailable && shellSettings?.keep_in_tray === true;

  return (
    <div className="q-group">
      <h2>Startup</h2>
      {autostart.status !== "unsupported" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">Open at login</div>
            {autostart.status === "error" ? (
              <div className="s" role="alert" style={{ color: "var(--danger)" }}>
                {autostart.error}
              </div>
            ) : (
              <div className="s">Start h4rn when you sign in to this computer</div>
            )}
          </div>
          <div className="ctl">
            {autostart.status === "loading" ? (
              <span className="q-faint">Checking…</span>
            ) : autostart.status === "ready" ? (
              <Toggle
                checked={autostart.enabled}
                label="Open at login"
                onChange={(next) => void changeAutostart(next)}
              />
            ) : null}
          </div>
        </div>
      ) : null}
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
  );
}

function ChatForm() {
  const followUp = useFollowUpBehavior();
  const multiline = useMultilineEnter();
  return (
    <div className="q-form">
      <h1>Chat</h1>
      <p className="lead">
        How a reply behaves while the agent is already working. These choices apply to this device.
      </p>
      <div className="q-group">
        <h2>Follow-up behaviour</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t" id="follow-up-label">
              Send while the agent is working
            </div>
            <div className="s">
              Queue waits for the next turn. Steer goes into the running turn when its agent can
              accept it.
            </div>
          </div>
          <div className="ctl">
            <fieldset className="q-seg inline q-seg-fs" aria-labelledby="follow-up-label">
              <button
                type="button"
                aria-pressed={followUp === "queue"}
                onClick={() => setFollowUpBehavior("queue")}
              >
                Queue
              </button>
              <button
                type="button"
                aria-pressed={followUp === "steer"}
                onClick={() => setFollowUpBehavior("steer")}
              >
                Steer
              </button>
            </fieldset>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Enter inserts a new line</div>
            <div className="s">
              {multiline
                ? "Cmd/Ctrl+Enter sends; add Shift for the other follow-up action."
                : "Enter sends; Cmd/Ctrl+Enter uses the other follow-up action."}
            </div>
          </div>
          <div className="ctl">
            <Toggle
              checked={multiline}
              label="Enter inserts a new line"
              onChange={setMultilineEnter}
            />
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
  const phase = useRemote((state) => state.phase);
  const signedIn = useRemote((state) => state.user !== null);
  const linkedSupported = useRemote((state) => state.linkedSupported);
  const linkedMachineCount = useRemote((state) => state.linkedMachines.length);
  // Host presence comes from the relay for the machines this device is linked
  // to. A direct-only session has no signal for other hosts, so the row is
  // hidden rather than shown as a dead switch. Before the account has loaded
  // nothing is known yet, so neither the row nor the explanation appears (no
  // flash of "offline" before the first relay answer).
  const hostOffline = hostOfflineVisibility({
    phase,
    signedIn,
    linkedSupported,
    linkedMachineCount,
  });
  const relayPresence = hostOffline.kind === "available";
  const pageUnavailable = useBrowserNotificationsUnavailable();
  const [permission, setPermission] = useState<NotificationPermissionState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const agentAlerts = useAgentAlerts();
  const daemonMachineId = useApp((state) => state.daemonMachineId);
  const daemonPaired = daemonMachineId !== null;
  const [device, setDevice] = useState<DeviceAlertsState | null>(null);
  const [deviceBusy, setDeviceBusy] = useState(false);
  const [liveUpdates, setLiveUpdates] = useState(deviceLiveUpdatesEnabled);
  const alertsShell = deviceAlertsShell();
  const liveShell = deviceLiveUpdatesSupported();

  useEffect(() => {
    void loadAgentAlertSettings();
  }, []);

  useEffect(() => {
    if (!alertsShell) return;
    if (!signedIn) {
      setDevice(null);
      return;
    }
    let cancelled = false;
    void readDeviceAlerts().then((state) => {
      if (!cancelled) setDevice(state);
    });
    return () => {
      cancelled = true;
    };
  }, [alertsShell, signedIn]);

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

  const changeAgentAlerts = async (next: boolean) => {
    if (next && !signedIn && !daemonPaired) return;
    await setAgentAlertsEnabled(next);
  };

  const changeLiveUpdates = (next: boolean) => {
    setLiveUpdates(next);
    void setDeviceLiveUpdates(next);
  };

  const changeDeviceAlerts = async (next: boolean) => {
    if (deviceBusy) return;
    setDeviceBusy(true);
    try {
      setDevice(next ? await enableDeviceAlerts() : await disableDeviceAlerts());
    } catch (cause) {
      // Never leave the row frozen: a failed turn-on reports why and stays off.
      const state = await readDeviceAlerts();
      setDevice({ ...state, enabled: false, error: describeError(cause) });
    } finally {
      setDeviceBusy(false);
    }
  };

  const rows = relayPresence
    ? NOTIFICATION_ROWS
    : NOTIFICATION_ROWS.filter((row) => row.key !== "hostOffline");
  const availabilityNote = deviceAvailabilityNote(device);

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
          {hostOffline.kind === "no-links" || hostOffline.kind === "relay-too-old" ? (
            <p className="s q-muted">
              {hostOffline.kind === "relay-too-old"
                ? "This relay does not report which machines this device is linked to. Update the relay to get host offline alerts."
                : "Host offline alerts need a host linked through the relay. This device has none, so there is no other host to watch."}
            </p>
          ) : null}
          {channel.kind === "browser" && pageUnavailable ? (
            <p className="s q-muted" role="status">
              Notifications stopped working on this page. Reload h4rn to try again.
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
      <div className="q-group">
        <h2>Phone alerts</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Send agent alerts to my phone through Connect</div>
            <div className="s">
              Finished, failed, approval and question alerts for this machine.
            </div>
          </div>
          <div className="ctl">
            <Toggle
              checked={agentAlerts.enabled === true}
              label="Send agent alerts to my phone through Connect"
              disabled={
                !agentAlertsSwitchEnabled(
                  agentAlerts.enabled,
                  agentAlerts.saving,
                  signedIn || daemonPaired,
                )
              }
              onChange={(next) => void changeAgentAlerts(next)}
            />
          </div>
        </div>
        <p className="s q-muted">{agentAlertsAvailabilityText(daemonPaired)}</p>
        {agentAlerts.error ? (
          <p className="s" role="alert" style={{ color: "var(--danger)" }}>
            {agentAlerts.error}
          </p>
        ) : null}
        {alertsShell ? (
          <>
            <div className="q-field">
              <div className="lab">
                <div className="t">Device notifications</div>
                <div className="s">{deviceStatusLine(device, signedIn)}</div>
              </div>
              <div className="ctl">
                <Toggle
                  checked={device?.enabled === true && device?.permission === "granted"}
                  label="Device notifications"
                  disabled={!deviceToggleUsable(device, signedIn, deviceBusy)}
                  onChange={(next) => void changeDeviceAlerts(next)}
                />
              </div>
            </div>
            {liveShell ? (
              <div className="q-field">
                <div className="lab">
                  <div className="t">Show running agents as live notifications</div>
                  <div className="s">
                    An ongoing notification names the thread and project while an agent works, waits
                    for an approval or asks you something. The finish, failure or limit alert
                    replaces it.
                  </div>
                </div>
                <div className="ctl">
                  <Toggle
                    checked={liveUpdates}
                    label="Show running agents as live notifications"
                    onChange={changeLiveUpdates}
                  />
                </div>
              </div>
            ) : null}
            {device?.permission === "denied" ? (
              <p className="s q-muted">
                Blocked in system settings. Allow notifications for h4rn there, then reopen this
                page.
              </p>
            ) : null}
            {device?.available === false ? (
              <p className="s q-muted">This build cannot receive alerts.</p>
            ) : null}
            {availabilityNote ? <p className="s q-muted">{availabilityNote}</p> : null}
            {device?.error ? (
              <p className="s" role="alert" style={{ color: "var(--danger)" }}>
                {device.error}
              </p>
            ) : null}
          </>
        ) : null}
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
  threads: () => (
    <ThreadSettings>
      <NewThreadDefaults />
    </ThreadSettings>
  ),
  // One page title (the proxy form owns it) and one column: text generators
  // render as a group under the same page padding.
  providers: () => (
    <>
      <ProxySettings />
      <div className="q-form" style={{ paddingTop: 0 }}>
        <TextGeneratorsSettings />
      </div>
    </>
  ),
  harnesses: HarnessSettings,
  worktrees: WorktreeSettings,
  schedules: SchedulesSettings,
  resume: AutoResumeSettings,
  forges: ForgeSettings,
  access: AccessSettings,
  "source-control": SourceControlSettings,
  general: GeneralForm,
  appearance: AppearanceForm,
  notifications: NotificationsForm,
  chat: ChatForm,
  shortcuts: ShortcutsForm,
  account: LiveAccountForm,
  labs: GuestSettingsForm,
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
