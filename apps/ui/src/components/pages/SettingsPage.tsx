import type * as React from "react";

import { ENTER, KeyGlyphs, MOD, SHIFT } from "@/components/common/Kbd";
import { ForgeSettings } from "@/features/forge/ForgeSettings";
import { HarnessSettings } from "@/features/harnesses/HarnessSettings";
import { ProxySettings } from "@/features/proxy/ProxySettings";
import { LiveAccountForm, LiveDevicesForm, LiveHostsForm } from "@/features/remote/SettingsRemote";
import { AboutSettings } from "@/features/updates/AboutSettings";
import { AutoResumeSettings } from "@/features/usage/AutoResumeSettings";
import { setTheme, type Theme, useApp } from "@/lib/store/appStore";
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

function ShortcutsForm() {
  const rows = [
    { label: "New thread", keys: `${MOD} ${SHIFT} N` },
    { label: "Search or run", keys: `${MOD} K` },
    { label: "Open terminal", keys: "T" },
    { label: "Show subagents", keys: `${MOD} J` },
    { label: "Interrupt turn", keys: `${MOD} .` },
    { label: "Send message", keys: ENTER },
    { label: "Settle thread", keys: "S" },
  ];
  return (
    <div className="q-form">
      <h1>Shortcuts</h1>
      <p className="lead">Shortcuts work anywhere in the app.</p>
      <div className="q-group">
        <h2>Keyboard</h2>
        {rows.map((row) => (
          <div className="q-field" key={row.label}>
            <div className="lab" style={{ width: "auto", flex: 1 }}>
              <div className="t" style={{ fontWeight: 400 }}>
                {row.label}
              </div>
            </div>
            <div className="ctl" style={{ flex: "none" }}>
              {row.keys.split(" ").map((key) => (
                <span className="q-kbd" key={`${row.label}-${key}`}>
                  <KeyGlyphs value={key} />
                </span>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

const FORMS: Record<string, () => React.ReactNode> = {
  hosts: LiveHostsForm,
  devices: LiveDevicesForm,
  providers: ProxySettings,
  harnesses: HarnessSettings,
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
  const Form = FORMS[key] ?? LiveHostsForm;

  return (
    <main className="q-main" aria-label="Settings">
      <div className="q-page">
        <Form />
      </div>
    </main>
  );
}
