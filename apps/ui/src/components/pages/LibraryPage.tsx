import {
  CheckCircle2Icon,
  KeyRoundIcon,
  MoreHorizontalIcon,
  PlugIcon,
  PlusIcon,
  RefreshCwIcon,
  Share2Icon,
  TriangleAlertIcon,
} from "lucide-react";
import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { harnessSeed, machineSeed } from "@/lib/entities";

const SERVERS = [
  {
    id: "playwright",
    transport: "http",
    summary: "5 targets · 3 hosts",
    health: "healthy" as const,
  },
  { id: "linear", transport: "http", summary: "3 targets · 2 hosts", health: "healthy" as const },
  {
    id: "postgres-dev",
    transport: "stdio",
    summary: "Shared from orion · 2 targets",
    health: "healthy" as const,
    shared: true,
  },
  { id: "sentry", transport: "http", summary: "1 target", health: "error" as const },
  {
    id: "github",
    transport: "stdio",
    summary: "4 targets",
    health: "credential" as const,
  },
];

function Health({ health }: { health: "healthy" | "error" | "credential" }) {
  if (health === "healthy")
    return (
      <span className="q-health">
        <CheckCircle2Icon className="q-icon s12" aria-hidden /> Healthy
      </span>
    );
  if (health === "error")
    return (
      <span className="q-health q-danger">
        <TriangleAlertIcon className="q-icon s12" aria-hidden /> Error
      </span>
    );
  return (
    <span className="q-health q-warn">
      <KeyRoundIcon className="q-icon s12" aria-hidden /> Needs credential
    </span>
  );
}

function Toggle({ on, disabled }: { on: boolean; disabled?: boolean }) {
  return (
    <button
      type="button"
      className="q-switch"
      role="switch"
      aria-checked={on}
      aria-disabled={disabled ? "true" : undefined}
      disabled={disabled}
    />
  );
}

/** Static Phase 4 surface: MCP servers & skills list/detail from the prototype. */
export function LibraryPage() {
  const [selected, setSelected] = useState("playwright");

  return (
    <>
      <section className="q-pane" aria-label="Library">
        <div className="q-pane-head">
          <h2>MCP &amp; skills</h2>
          <button type="button" className="q-ibtn" aria-label="Add">
            <PlusIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
        <div className="q-pane-sub">
          <div className="q-seg" role="tablist">
            <button type="button" role="tab" aria-selected>
              MCP servers <span className="c">5</span>
            </button>
            <button type="button" role="tab" aria-selected={false}>
              Skills <span className="c">9</span>
            </button>
          </div>
        </div>
        <div className="q-scroll">
          <div className="q-rows">
            {SERVERS.map((server) => (
              <button
                key={server.id}
                type="button"
                className={`q-mrow ${selected === server.id ? "sel" : ""}`}
                onClick={() => setSelected(server.id)}
              >
                <span style={{ paddingTop: 1, color: "var(--fg-2)" }}>
                  {server.shared ? (
                    <Share2Icon className="q-icon" strokeWidth={1.5} aria-hidden />
                  ) : (
                    <PlugIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                  )}
                </span>
                <span className="grow">
                  <span className="t">
                    {server.id}
                    <span className="q-faint" style={{ fontWeight: 400, fontSize: 12 }}>
                      {server.transport}
                    </span>
                  </span>
                  <span className="s">{server.summary}</span>
                </span>
                <Health health={server.health} />
              </button>
            ))}
          </div>
        </div>
      </section>

      <main className="q-main" aria-label="MCP server detail">
        <div className="q-page">
          <div className="q-wrap" style={{ maxWidth: 860 }}>
            <div style={{ display: "flex", alignItems: "flex-start", gap: 16, marginBottom: 22 }}>
              <div style={{ flex: 1 }}>
                <h1
                  style={{
                    margin: 0,
                    fontSize: 20,
                    lineHeight: "26px",
                    fontWeight: 600,
                    letterSpacing: "-0.015em",
                  }}
                >
                  playwright
                </h1>
                <div className="q-muted" style={{ marginTop: 4 }}>
                  HTTP · <span className="mono">http://127.0.0.1:8931/mcp</span> · 21 tools ·
                  checked 12 s ago
                </div>
              </div>
              <button type="button" className="q-btn secondary">
                <RefreshCwIcon className="q-icon s14" aria-hidden /> Test
              </button>
              <button type="button" className="q-btn secondary">
                Edit
              </button>
              <button type="button" className="q-ibtn" aria-label="More">
                <MoreHorizontalIcon className="q-icon" aria-hidden />
              </button>
            </div>

            <div className="q-field" style={{ borderTop: "1px solid var(--line)" }}>
              <div className="lab">
                <div className="t">Credential</div>
                <div className="s">Never leaves the host</div>
              </div>
              <div className="ctl">
                <span className="q-secret">
                  <KeyRoundIcon className="q-icon s12" aria-hidden /> Stored on atlas
                </span>
                <span className="q-secret">
                  <KeyRoundIcon className="q-icon s12" aria-hidden /> Stored on orion
                </span>
                <span className="q-faint" style={{ fontSize: 12 }}>
                  Ubuntu-24.04 uses atlas’s server
                </span>
                <span style={{ flex: 1 }} />
                <button type="button" className="q-btn ghost">
                  Replace
                </button>
              </div>
            </div>

            <div className="q-sec" style={{ marginTop: 28 }}>
              <div className="q-sec-h">
                <h2>Where it runs</h2>
                <span className="aside">Each target receives the server its own way</span>
              </div>
              <table className="q-tbl">
                <thead>
                  <tr>
                    <th style={{ width: "24%" }}>Host</th>
                    <th style={{ width: "22%" }}>Harness</th>
                    <th>Delivered as</th>
                    <th>Status</th>
                    <th className="r" style={{ paddingRight: 0 }}>
                      On
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {[
                    {
                      host: "atlas",
                      harness: "claude_code",
                      how: "Host config file",
                      on: true,
                      note: "21 tools",
                    },
                    {
                      host: "atlas",
                      harness: "codex",
                      how: "Session request",
                      on: true,
                      note: "21 tools",
                    },
                    {
                      host: "atlas-wsl",
                      harness: "opencode",
                      how: "Harness API",
                      on: false,
                      note: "",
                    },
                    {
                      host: "orion",
                      harness: "codex",
                      how: "Session request",
                      on: true,
                      note: "Name conflict",
                      warn: true,
                    },
                    {
                      host: "orion",
                      harness: "antigravity",
                      how: "Not supported",
                      on: false,
                      note: "Needs the stdio bridge",
                      disabled: true,
                    },
                  ].map((target) => (
                    <tr key={`${target.host}-${target.harness}`} className="hov">
                      <td>
                        <span className="q-cell-id">
                          <EntityAvatar
                            seed={machineSeed(target.host)}
                            kind="machine"
                            label={target.host}
                            size={14}
                          />
                          {target.host === "atlas-wsl" ? "Ubuntu-24.04" : target.host}
                        </span>
                      </td>
                      <td>
                        <span className="q-cell-id">
                          <EntityAvatar
                            seed={harnessSeed(target.host, target.harness as never)}
                            kind="harness"
                            label={target.harness}
                            size={14}
                          />
                          {target.harness === "claude_code"
                            ? "Claude Code"
                            : target.harness === "codex"
                              ? "Codex"
                              : target.harness === "opencode"
                                ? "OpenCode"
                                : "Antigravity"}
                        </span>
                      </td>
                      <td className={target.disabled ? "q-faint" : "q-muted"}>{target.how}</td>
                      <td>
                        {target.note ? (
                          <span
                            className={`q-health ${target.warn ? "q-warn" : ""} ${target.disabled ? "q-faint" : ""}`}
                          >
                            {target.warn ? (
                              <TriangleAlertIcon className="q-icon s12" aria-hidden />
                            ) : target.disabled ? null : (
                              <CheckCircle2Icon className="q-icon s12" aria-hidden />
                            )}
                            {target.note}
                          </span>
                        ) : (
                          <span className="q-faint">Off</span>
                        )}
                      </td>
                      <td className="r" style={{ paddingRight: 0 }}>
                        <Toggle on={target.on} disabled={target.disabled ?? false} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <div className="q-conflict">
                <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
                <div className="grow">
                  <div className="t">orion already has a server named playwright</div>
                  <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
                    Codex on orion loads its own playwright 0.0.41 from{" "}
                    <span className="mono">~/.codex/config.toml</span>. Pick which one Codex should
                    see.
                  </div>
                </div>
                <button type="button" className="q-btn secondary">
                  Resolve…
                </button>
              </div>
            </div>

            <div className="q-sec">
              <div className="q-sec-h">
                <h2>Shared with you</h2>
                <span className="aside">Defined on another host</span>
              </div>
              <div className="q-list">
                <div className="q-li">
                  <EntityAvatar
                    seed={machineSeed("orion")}
                    kind="machine"
                    label="orion"
                    size={18}
                  />
                  <div className="grow">
                    <div className="t">
                      postgres-dev{" "}
                      <span className="q-faint" style={{ fontWeight: 400 }}>
                        from orion
                      </span>
                    </div>
                    <div className="s">
                      stdio · <span className="mono">mcp-postgres --readonly</span> · runs on orion,
                      reached by atlas over Tailscale
                    </div>
                  </div>
                  <span className="q-secret">
                    <KeyRoundIcon className="q-icon s12" aria-hidden /> Stored on orion
                  </span>
                  <Toggle on />
                </div>
              </div>
            </div>
          </div>
        </div>
      </main>
    </>
  );
}
