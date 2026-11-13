import {
  CheckIcon,
  ChevronDownIcon,
  CircleCheckIcon,
  CircleDotIcon,
  CopyIcon,
  KeyRoundIcon,
  LaptopIcon,
  MonitorIcon,
  MoreHorizontalIcon,
  PlayIcon,
  QrCodeIcon,
  RefreshCwIcon,
  ShieldCheckIcon,
  SmartphoneIcon,
  WifiOffIcon,
} from "lucide-react";
import type * as React from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ENTER, KeyGlyphs, MOD, SHIFT } from "@/components/common/Kbd";
import { ForgeSettings } from "@/features/forge/ForgeSettings";
import type { HarnessKind } from "@/gen";
import { harnessSeed, machineSeed } from "@/lib/entities";
import { navigate } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

function Health({ tone, children }: { tone: "on" | "off" | "danger"; children: React.ReactNode }) {
  return (
    <span className={cn("q-health", tone === "danger" && "q-danger")}>
      {tone === "danger" ? (
        <WifiOffIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
      ) : (
        <span className={cn("q-dot", tone)} aria-hidden />
      )}
      {children}
    </span>
  );
}

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

/** Deterministic pairing QR stand-in for the static pairing surface. */
function PairingQr() {
  const N = 25;
  let h = 0x9e3779b9;
  for (const char of "pair:7K4Q2M9P") h = (h ^ char.charCodeAt(0)) * 0x01000193;
  const rnd = () => {
    h ^= h << 13;
    h ^= h >>> 17;
    h ^= h << 5;
    return (h >>> 0) / 4294967296;
  };
  const cells: React.ReactNode[] = [];
  const finder = (x: number, y: number, key: string) => {
    cells.push(
      <g key={key}>
        <rect x={x} y={y} width={7} height={7} fill="#111" />
        <rect x={x + 1} y={y + 1} width={5} height={5} fill="#fff" />
        <rect x={x + 2} y={y + 2} width={3} height={3} fill="#111" />
      </g>,
    );
  };
  for (let y = 0; y < N; y += 1) {
    for (let x = 0; x < N; x += 1) {
      const inFinder = (x < 8 && y < 8) || (x > N - 9 && y < 8) || (x < 8 && y > N - 9);
      if (inFinder) continue;
      if (x === 6 || y === 6) {
        if ((x + y) % 2 === 0) {
          cells.push(<rect key={`g${x}-${y}`} x={x} y={y} width={1} height={1} fill="#111" />);
        }
        continue;
      }
      if (rnd() < 0.5) {
        cells.push(<rect key={`c${x}-${y}`} x={x} y={y} width={1} height={1} fill="#111" />);
      }
    }
  }
  finder(0, 0, "f1");
  finder(N - 7, 0, "f2");
  finder(0, N - 7, "f3");
  cells.push(
    <g key="logo">
      <rect x={16} y={16} width={5} height={5} fill="#111" />
      <rect x={17} y={17} width={3} height={3} fill="#fff" />
      <rect x={18} y={18} width={1} height={1} fill="#111" />
    </g>,
  );
  return (
    <svg
      viewBox={`0 0 ${N} ${N}`}
      width="100%"
      height="100%"
      shapeRendering="crispEdges"
      role="img"
      aria-label="Pairing QR code for atlas"
    >
      {cells}
    </svg>
  );
}

function HostsForm() {
  const rows = [
    {
      id: "atlas",
      name: "atlas",
      system: "Windows 11 · this host",
      connection: "Direct",
      presence: <Health tone="on">Online</Health>,
      harnesses: "Claude Code, Codex",
      latency: "12 ms",
      sub: false,
    },
    {
      id: "atlas-wsl",
      name: "Ubuntu-24.04",
      system: "WSL 2 on atlas",
      connection: "Direct",
      presence: <Health tone="on">Online</Health>,
      harnesses: "OpenCode, Codex",
      latency: "18 ms",
      sub: true,
    },
    {
      id: "atlas-debian",
      name: "Debian",
      system: "WSL 2 on atlas",
      connection: "—",
      presence: <Health tone="off">Stopped</Health>,
      harnesses: "None installed",
      latency: "—",
      sub: true,
    },
    {
      id: "orion",
      name: "orion",
      system: "macOS 15 · MacBook Pro",
      connection: "Tailscale",
      presence: <Health tone="on">Online</Health>,
      harnesses: "Codex, Antigravity",
      latency: "42 ms",
      sub: false,
    },
    {
      id: "buildbox",
      name: "buildbox",
      system: "Ubuntu 24.04 server",
      connection: "Relay",
      presence: <Health tone="danger">Offline · 3h</Health>,
      harnesses: "Codex",
      latency: "—",
      sub: false,
    },
  ];

  return (
    <div className="q-form" style={{ maxWidth: 880 }}>
      <div style={{ display: "flex", alignItems: "flex-start", gap: 12 }}>
        <div style={{ flex: 1 }}>
          <h1>Hosts</h1>
          <p className="lead">
            Machines that run agents. WSL distros are separate hosts with their own harnesses,
            credentials and settings.
          </p>
        </div>
        <button type="button" className="q-btn secondary">
          Add host
        </button>
        <button
          type="button"
          className="q-btn primary"
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" })
          }
        >
          <QrCodeIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Pair a device
        </button>
      </div>

      <table className="q-tbl" style={{ marginBottom: 36 }}>
        <thead>
          <tr>
            <th>Host</th>
            <th>System</th>
            <th>Connection</th>
            <th>Presence</th>
            <th>Harnesses</th>
            <th className="r">Latency</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.id} className="hov">
              <td>
                <span className="q-cell-id" style={{ paddingLeft: row.sub ? 16 : 0 }}>
                  <EntityAvatar
                    seed={machineSeed(row.id)}
                    kind="machine"
                    label={row.name}
                    size={row.sub ? 16 : 18}
                  />
                  <span style={{ fontWeight: 500 }}>{row.name}</span>
                </span>
              </td>
              <td className="q-muted">{row.system}</td>
              <td className="q-muted">{row.connection}</td>
              <td>{row.presence}</td>
              <td className="q-muted">{row.harnesses}</td>
              <td className="r q-num q-muted">{row.latency}</td>
              <td className="r" style={{ paddingRight: 0 }}>
                <button type="button" className="q-ibtn" aria-label={`Actions for ${row.name}`}>
                  <MoreHorizontalIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <div style={{ display: "flex", alignItems: "center", gap: 12, marginBottom: 18 }}>
        <EntityAvatar seed={machineSeed("atlas")} kind="machine" label="atlas" size={32} />
        <div style={{ flex: 1 }}>
          <div style={{ fontSize: 15, fontWeight: 600 }}>atlas</div>
          <div className="q-muted" style={{ fontSize: 12 }}>
            Windows 11 Pro · h4rn 0.10.2 · up 6d 4h · CPU 18% · memory 11.2 of 32 GB
          </div>
        </div>
        <button type="button" className="q-btn secondary">
          Restart daemon
        </button>
      </div>

      <div className="q-group">
        <h2>Harnesses</h2>
        <div className="q-field">
          <div className="lab" style={{ display: "flex", gap: 10, alignItems: "center" }}>
            <EntityAvatar
              seed={harnessSeed("atlas", "claude_code")}
              kind="harness"
              label="Claude Code"
              size={18}
            />
            <div>
              <div className="t">Claude Code</div>
              <div className="s">2.1.4 · 3 models</div>
            </div>
          </div>
          <div className="ctl">
            <Health tone="on">Running · 2 threads</Health>
            <span style={{ flex: 1 }} />
            <span className="q-faint" style={{ fontSize: 12 }}>
              Uses CLIProxyAPI
            </span>
            <button type="button" className="q-btn ghost">
              Restart
            </button>
          </div>
        </div>
        <div className="q-field">
          <div className="lab" style={{ display: "flex", gap: 10, alignItems: "center" }}>
            <EntityAvatar
              seed={harnessSeed("atlas", "codex")}
              kind="harness"
              label="Codex"
              size={18}
            />
            <div>
              <div className="t">Codex</div>
              <div className="s">0.98.2 · 4 models</div>
            </div>
          </div>
          <div className="ctl">
            <Health tone="on">Running · idle</Health>
            <span style={{ flex: 1 }} />
            <span className="q-faint" style={{ fontSize: 12 }}>
              Own credential
            </span>
            <button type="button" className="q-btn ghost">
              Restart
            </button>
          </div>
        </div>
        <div className="q-field">
          <div className="lab" style={{ display: "flex", gap: 10, alignItems: "center" }}>
            <EntityAvatar
              seed={harnessSeed("atlas", "opencode")}
              kind="harness"
              label="OpenCode"
              size={18}
            />
            <div>
              <div className="t">OpenCode</div>
              <div className="s">1.14.19</div>
            </div>
          </div>
          <div className="ctl">
            <Health tone="off">Stopped</Health>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn secondary">
              <PlayIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              Start
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

function Scope({ on, label, sub }: { on: boolean; label: string; sub?: string }) {
  return (
    <div style={{ display: "flex", gap: 10, alignItems: "flex-start", padding: "6px 0" }}>
      <span className={cn("q-check", on && "on")} aria-hidden>
        {on ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
      </span>
      <span>
        <span style={{ fontWeight: 500 }}>{label}</span>
        {sub ? (
          <span className="q-faint" style={{ display: "block", fontSize: 12 }}>
            {sub}
          </span>
        ) : null}
      </span>
    </div>
  );
}

function DevicesForm() {
  const devices = [
    {
      icon: LaptopIcon,
      name: "Framework 13",
      meta: "this device",
      scopes: "All",
      seen: "Now",
      online: true,
    },
    {
      icon: SmartphoneIcon,
      name: "iPhone 16 Pro",
      scopes: "View, approve, terminal",
      seen: "2 min ago",
      online: true,
    },
    {
      icon: SmartphoneIcon,
      name: "Pixel 9",
      scopes: "View, approve",
      seen: "Yesterday 21:40",
      online: false,
    },
    { icon: MonitorIcon, name: "Work iMac", scopes: "View", seen: "Oct 28", online: false },
  ];

  return (
    <div className="q-form" style={{ maxWidth: 840 }}>
      <h1>Devices</h1>
      <p className="lead">
        Phones and computers that can reach your hosts. Each device gets its own scopes, and you can
        revoke one without touching the others.
      </p>

      <div className="q-group">
        <h2>
          Pair a device <span className="grow" />
          <span className="q-faint q-num" style={{ fontSize: 12, fontWeight: 400 }}>
            Expires in 9:41
          </span>
        </h2>
        <div
          style={{
            display: "flex",
            gap: 28,
            padding: "18px 0 4px",
            borderTop: "1px solid var(--line)",
          }}
        >
          <div className="q-qr">
            <PairingQr />
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div className="q-muted" style={{ fontSize: 12 }}>
              Scan with the h4rn app, or enter this code
            </div>
            <div className="q-code-big" style={{ margin: "4px 0 2px" }}>
              7K4Q-2M9P
            </div>
            <div className="q-faint" style={{ fontSize: 12, marginBottom: 14 }}>
              Works once, for one device. Pairs with atlas and its WSL distros.
            </div>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "0 24px" }}>
              <Scope on label="View threads" sub="Always included" />
              <Scope on label="Approve and reply" />
              <Scope on={false} label="Terminal" sub="Type into shells on atlas" />
              <Scope on={false} label="Files and edits" />
              <Scope on={false} label="Manage hosts and devices" sub="Admin" />
            </div>
            <div style={{ display: "flex", gap: 6, marginTop: 12 }}>
              <button type="button" className="q-btn secondary">
                <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                Copy link
              </button>
              <button type="button" className="q-btn ghost">
                <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                New code
              </button>
            </div>
          </div>
        </div>
      </div>

      <div className="q-group">
        <h2>Paired devices</h2>
        <table className="q-tbl">
          <thead>
            <tr>
              <th>Device</th>
              <th>Scopes</th>
              <th>Last seen</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {devices.map((device) => (
              <tr key={device.name} className={device.name === "Framework 13" ? undefined : "hov"}>
                <td>
                  <span className="q-cell-id">
                    <device.icon className="q-icon" strokeWidth={1.5} aria-hidden />
                    <span>
                      <span style={{ fontWeight: 500 }}>{device.name}</span>
                      {device.meta ? <span className="q-faint"> · {device.meta}</span> : null}
                    </span>
                  </span>
                </td>
                <td className="q-muted">{device.scopes}</td>
                <td>
                  {device.online ? (
                    <Health tone="on">{device.seen}</Health>
                  ) : (
                    <span className="q-muted">{device.seen}</span>
                  )}
                </td>
                <td className="r" style={{ paddingRight: 0 }}>
                  {device.name === "Framework 13" ? null : (
                    <button type="button" className="q-btn danger" style={{ height: 24 }}>
                      Revoke
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
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

function AccountForm() {
  return (
    <div className="q-form">
      <h1>Account & relay</h1>
      <p className="lead">
        Relay reaches hosts that are not on your network. Nothing is routed until you allow it.
      </p>
      <div className="q-group">
        <h2>Relay</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Relay</div>
            <div className="s">Used for buildbox only</div>
          </div>
          <div className="ctl">
            <Health tone="on">Connected · 64 ms</Health>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn ghost">
              Disconnect
            </button>
          </div>
        </div>
        <div className="q-field">
          <div className="lab">
            <div className="t">Account</div>
          </div>
          <div className="ctl">
            <span className="q-muted">Signed in as you@h4rn.dev</span>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn secondary">
              <ShieldCheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Manage sessions
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

const FORMS: Record<string, () => React.ReactNode> = {
  hosts: HostsForm,
  devices: DevicesForm,
  providers: ProvidersForm,
  forges: ForgeSettings,
  general: GeneralForm,
  appearance: AppearanceForm,
  notifications: NotificationsForm,
  shortcuts: ShortcutsForm,
  account: AccountForm,
};

/** Static settings surfaces, ported from the approved prototype. */
export function SettingsPage({ section }: { section: string }) {
  const key = FORMS[section] ? section : "hosts";
  const Form = FORMS[key] ?? HostsForm;

  return (
    <main className="q-main" aria-label="Settings">
      <div className="q-page">
        <Form />
      </div>
    </main>
  );
}
