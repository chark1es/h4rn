import {
  CheckIcon,
  ChevronDownIcon,
  CircleCheckIcon,
  CircleDotIcon,
  KeyRoundIcon,
  MoreHorizontalIcon,
  RefreshCwIcon,
} from "lucide-react";
import type * as React from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ENTER, KeyGlyphs, MOD, SHIFT } from "@/components/common/Kbd";
import { LiveAccountForm, LiveDevicesForm, LiveHostsForm } from "@/features/remote/SettingsRemote";
import type { HarnessKind } from "@/gen";
import { harnessSeed } from "@/lib/entities";
import { cn } from "@/lib/utils";

function Toggle({ checked, label }: { checked: boolean; label: string }) {
  return (
    <button
      type="button"
      className="q-switch"
      role="switch"
      aria-checked={checked}
      aria-label={label}
    />
  );
}

function Probe({ status, label, value }: { status: 1 | 2 | 3; label: string; value: string }) {
  return (
    <>
      <span className={status === 1 ? "q-ok" : status === 2 ? "q-faint" : "q-warn"}>
        {status === 1 ? (
          <CircleCheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        ) : status === 2 ? (
          <CircleDotIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        ) : (
          <span aria-hidden>!</span>
        )}
      </span>
      <span className="k">{label}</span>
      <span className="v">{value}</span>
    </>
  );
}

function ProviderRow({
  harness,
  label,
  on,
  model,
  path,
}: {
  harness: HarnessKind;
  label: string;
  on: boolean;
  model: string;
  path: string;
}) {
  return (
    <div className="q-field">
      <div className="lab" style={{ display: "flex", gap: 10, alignItems: "center", width: 180 }}>
        <EntityAvatar seed={harnessSeed("atlas", harness)} kind="harness" label={label} size={16} />
        <div className="t">{label}</div>
      </div>
      <div className="ctl">
        <Toggle checked={on} label={`Route ${label} through CLIProxyAPI`} />
        <span style={{ width: 160 }} className={on ? undefined : "q-faint"}>
          {on ? model : "Own credential"}
        </span>
        <span
          className="q-mono q-faint"
          style={{
            flex: 1,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
            fontSize: 11.5,
          }}
        >
          {path}
        </span>
      </div>
    </div>
  );
}

function ProvidersForm() {
  return (
    <div className="q-form">
      <div style={{ display: "flex", alignItems: "flex-start", gap: 12 }}>
        <div style={{ flex: 1 }}>
          <h1>CLIProxyAPI on atlas</h1>
          <p className="lead">
            Routes harnesses on atlas through one endpoint with several credentials. Keys stay on
            atlas; this app only sees whether they exist.
          </p>
        </div>
        <button type="button" className="q-btn secondary">
          atlas <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
        </button>
      </div>

      <div className="q-group">
        <h2>Connection</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Base URL</div>
          </div>
          <div className="ctl">
            <input
              className="q-input q-mono"
              defaultValue="http://127.0.0.1:8317"
              aria-label="Base URL"
              style={{ maxWidth: 280, fontSize: 12 }}
            />
            <button type="button" className="q-btn secondary">
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Probe
            </button>
          </div>
        </div>
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">Probe</div>
            <div className="s">12:20 from atlas</div>
          </div>
          <div className="ctl" style={{ display: "block" }}>
            <div className="q-probe" style={{ paddingTop: 2 }}>
              <Probe status={1} label="Health" value="OK" />
              <Probe status={1} label="Management" value="Enabled · v8.0.69" />
              <Probe status={1} label="Model shape" value="Claude, detected from headers" />
              <Probe
                status={2}
                label="Usage history"
                value="Not available on ≥ 6.10 · h4rn counts usage itself"
              />
            </div>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Management key</div>
          </div>
          <div className="ctl">
            <span className="q-secret">
              <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              Stored on atlas
            </span>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn ghost">
              Replace
            </button>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Access key</div>
            <div className="s">What harnesses send</div>
          </div>
          <div className="ctl">
            <span className="q-secret">
              <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              Stored on atlas
            </span>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn ghost">
              Replace
            </button>
          </div>
        </div>
      </div>

      <div className="q-group">
        <h2>
          Models <span className="grow" />
          <button type="button" className="q-btn ghost" style={{ fontWeight: 500 }}>
            <span aria-hidden>+</span> Custom model
          </button>
        </h2>
        <table className="q-tbl">
          <thead>
            <tr>
              <th style={{ width: 28 }} />
              <th>Model</th>
              <th>Alias</th>
              <th className="r">Context</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {[
              { model: "claude-sonnet-5-5", alias: "sonnet", on: true },
              { model: "claude-opus-5-5", alias: "opus", on: true },
              { model: "claude-haiku-5-5", alias: "haiku", on: false },
            ].map((row) => (
              <tr key={row.model}>
                <td>
                  <span className={cn("q-check", row.on && "on")}>
                    {row.on ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
                  </span>
                </td>
                <td className={cn("q-mono", !row.on && "q-faint")}>{row.model}</td>
                <td className={row.on ? undefined : "q-faint"}>{row.alias}</td>
                <td className={cn("r q-num", !row.on && "q-faint")}>200k</td>
                <td />
              </tr>
            ))}
            <tr>
              <td>
                <span className="q-check on">
                  <CheckIcon className="q-icon s12" strokeWidth={2} />
                </span>
              </td>
              <td>
                <span className="q-mono">fast</span>
                <span className="q-badge" style={{ height: 18, marginLeft: 6 }}>
                  Custom
                </span>
              </td>
              <td className="q-muted">
                maps to <span className="q-mono">claude-haiku-5-5</span> · keeps original id
              </td>
              <td className="r q-num">200k</td>
              <td className="r" style={{ paddingRight: 0 }}>
                <button type="button" className="q-ibtn" aria-label="Custom model actions">
                  <MoreHorizontalIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <div className="q-group">
        <h2>Per-harness override</h2>
        <p className="desc">
          Off means the harness keeps its own credential. Nothing changes for a harness you don’t
          turn on.
        </p>
        <ProviderRow
          harness="claude_code"
          label="Claude Code"
          on
          model="sonnet"
          path="~/.claude/settings.json"
        />
        <div className="q-preview" style={{ margin: "0 0 6px 196px" }}>
          <div className="q-ln a">
            <span className="n">+</span>
            <span className="sg" />
            <span className="cx">"ANTHROPIC_BASE_URL": "http://127.0.0.1:8317",</span>
          </div>
          <div className="q-ln a">
            <span className="n">+</span>
            <span className="sg" />
            <span className="cx">
              "ANTHROPIC_AUTH_TOKEN": <span className="q-faint">stored on atlas</span>,
            </span>
          </div>
          <div className="q-ln a">
            <span className="n">+</span>
            <span className="sg" />
            <span className="cx">"ANTHROPIC_MODEL": "sonnet"</span>
          </div>
        </div>
        <ProviderRow
          harness="codex"
          label="Codex"
          on={false}
          model=""
          path="~/.codex/config.toml"
        />
        <ProviderRow
          harness="opencode"
          label="OpenCode"
          on
          model="fast"
          path="~/.config/opencode/opencode.jsonc"
        />
        <ProviderRow
          harness="antigravity"
          label="Antigravity"
          on={false}
          model=""
          path="~/.gemini/settings.json"
        />
      </div>

      <div className="q-group">
        <h2>Verify</h2>
        <div className="q-field" style={{ borderTop: "1px solid var(--line)" }}>
          <div className="lab">
            <div className="t">Test request</div>
            <div className="s">Claude Code route</div>
          </div>
          <div className="ctl">
            <span className="q-ok">
              <CheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </span>
            <span>
              Resolved <span className="q-mono">claude-sonnet-5-5</span> · TTFT 412 ms · 61 tok/s
            </span>
          </div>
        </div>
        <div
          style={{
            display: "flex",
            gap: 8,
            justifyContent: "flex-end",
            marginTop: 16,
            alignItems: "center",
          }}
        >
          <span className="q-faint" style={{ fontSize: 12, marginRight: "auto" }}>
            Claude Code reads this at startup. Restarting interrupts 1 running turn on atlas.
          </span>
          <button type="button" className="q-btn ghost">
            Cancel
          </button>
          <button type="button" className="q-btn primary">
            Apply and restart
          </button>
        </div>
      </div>
    </div>
  );
}

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
      <div className="q-group">
        <h2>Daemon</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Version</div>
          </div>
          <div className="ctl">
            <span className="q-mono q-muted">0.10.2</span>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn secondary">
              Check for updates
            </button>
          </div>
        </div>
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
  return (
    <div className="q-form">
      <h1>Appearance</h1>
      <p className="lead">
        Quiet native follows the system by default. Theme applies to this device.
      </p>
      <div className="q-group">
        <h2>Theme</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Color scheme</div>
          </div>
          <div className="ctl">
            <div className="q-seg inline" role="tablist" aria-label="Color scheme">
              <button type="button" role="tab" aria-selected>
                System
              </button>
              <button type="button" role="tab" aria-selected={false}>
                Light
              </button>
              <button type="button" role="tab" aria-selected={false}>
                Dark
              </button>
            </div>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Terminal</div>
            <div className="s">Match the app theme</div>
          </div>
          <div className="ctl">
            <Toggle checked label="Match the app theme" />
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
  providers: ProvidersForm,
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
