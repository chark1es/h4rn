import {
  CheckIcon,
  CopyIcon,
  LaptopIcon,
  LoaderIcon,
  MonitorIcon,
  PencilIcon,
  PlusIcon,
  QrCodeIcon,
  RefreshCwIcon,
  ServerIcon,
  SmartphoneIcon,
  WifiOffIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ChipSelect } from "@/components/common/ChipSelect";
import type {
  ConnectDevice,
  HostSummary,
  MachineSummary,
  PairingConfirmation,
  PairScope,
} from "@/gen";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, machineSeed } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { getClient, navigate, pushToast, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { ConnectSignIn, SignOutButton } from "./ConnectSignIn";
import {
  DEFAULT_PAIR_SCOPES,
  formatShortCode,
  orderedScopes,
  SCOPE_OPTIONS,
  scopesLabel,
  toggleScope,
} from "./pairingScopes";
import { QrCode } from "./QrCode";
import {
  activateMachine,
  createPairing,
  dismissPairing,
  forgetMachine,
  refresh,
  renameDevice,
  revokeDevice,
  useRemote,
} from "./remoteStore";

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

function pathLabel(transport: string): string {
  if (transport === "mock") return "Direct";
  if (transport === "tailscale") return "Tailscale";
  if (transport === "relay") return "Relay";
  return "Direct";
}

function latencyText(ms: number | null | undefined): string {
  return typeof ms === "number" && Number.isFinite(ms) ? `${Math.round(ms)} ms` : "—";
}

function relativeSeen(iso: string | null | undefined, now: number): string {
  if (!iso) return "Never";
  const value = relativeAge(iso, now);
  return value === "now" ? "Just now" : `${value} ago`;
}

function signedIn(user: unknown): boolean {
  return user !== null;
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

function MachineRows({
  machines,
  activeId,
  connection,
  busy,
  now,
}: {
  machines: MachineSummary[];
  activeId: string | null;
  connection: { status: string; latencyMs: number | null; transport: string };
  busy: string | null;
  now: number;
}) {
  return (
    <div className="q-group">
      <h2>
        Machines <span className="grow" />
        <button
          type="button"
          className="q-btn ghost"
          style={{ fontWeight: 500 }}
          onClick={() => void refresh()}
        >
          <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Refresh
        </button>
      </h2>
      <table className="q-tbl">
        <thead>
          <tr>
            <th>Machine</th>
            <th>Hosts</th>
            <th>Connection</th>
            <th>Presence</th>
            <th className="r">Latency</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {machines.map((machine) => {
            const active = machine.id === activeId;
            const online = active ? connection.status === "online" : machine.online;
            return (
              <tr key={machine.id} className="hov" aria-current={active ? "true" : undefined}>
                <td>
                  <span className="q-cell-id">
                    <EntityAvatar
                      seed={machineSeed(machine.id)}
                      kind="machine"
                      label={machine.name}
                      size={18}
                    />
                    <span style={{ fontWeight: 500 }}>{machine.name}</span>
                  </span>
                </td>
                <td className="q-muted">
                  {machine.hosts.length === 0
                    ? "No hosts"
                    : machine.hosts.map((host) => host.name).join(", ")}
                </td>
                <td className="q-muted">
                  {active ? pathLabel(connection.transport) : machine.online ? "Reachable" : "—"}
                </td>
                <td>
                  {online ? (
                    <Health tone="on">Online</Health>
                  ) : (
                    <Health tone="danger">Offline · {relativeSeen(machine.last_seen, now)}</Health>
                  )}
                </td>
                <td className="r q-num q-muted">
                  {active ? latencyText(connection.latencyMs) : "—"}
                </td>
                <td className="r" style={{ paddingRight: 0 }}>
                  {active ? (
                    <button
                      type="button"
                      className="q-btn ghost"
                      style={{ height: 24 }}
                      onClick={forgetMachine}
                    >
                      Disconnect
                    </button>
                  ) : (
                    <button
                      type="button"
                      className="q-btn secondary"
                      style={{ height: 24 }}
                      disabled={!machine.online || busy !== null}
                      onClick={() => void activateMachine(machine.id)}
                    >
                      {busy === `Connecting to ${machine.name}` ? (
                        <LoaderIcon className="q-icon s14 q-spin" strokeWidth={1.5} aria-hidden />
                      ) : null}
                      Connect
                    </button>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/**
 * Hosts and the devices that supervise them, on one page. `/settings/devices`
 * opens it with the pairing code showing.
 */
export function LiveHostsForm({ pairing: pairFirst = false }: { pairing?: boolean }) {
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection);
  const phase = useRemote((state) => state.phase);
  const machines = useRemote((state) => state.machines);
  const activeId = useRemote((state) => state.activeMachineId);
  const busy = useRemote((state) => state.busy);
  const user = useRemote((state) => state.user);
  const connectUrl = useRemote((state) => state.connectUrl);
  const now = useNow();
  const rows = useMemo(
    () =>
      Object.values(hosts).sort(
        (a, b) =>
          Number(b.online) - Number(a.online) ||
          a.platform.localeCompare(b.platform) ||
          a.name.localeCompare(b.name),
      ),
    [hosts],
  );
  const isSignedIn = signedIn(user);
  const hostId = rows[0]?.host_id;
  const [pairOpen, setPairOpen] = useState(pairFirst);
  useEffect(() => setPairOpen(pairFirst), [pairFirst]);

  return (
    <div className="q-form" style={{ maxWidth: 880 }}>
      <div className="q-form-head">
        <div style={{ flex: 1 }}>
          <h1>Hosts & devices</h1>
          <p className="lead">
            Hosts are machines that run agents; WSL distros are separate hosts with their own
            harnesses and credentials. Devices are the phones and computers you supervise them from.
          </p>
        </div>
        <button
          type="button"
          className="q-btn secondary"
          onClick={() =>
            pushToast({
              variant: "info",
              title: "Add a machine with h4rnd login.",
              description: "Run `h4rnd login --connect <url>` on it, then refresh.",
            })
          }
        >
          <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Add host
        </button>
        <button
          type="button"
          className="q-btn primary"
          aria-expanded={pairOpen}
          aria-controls="pair-device"
          onClick={() => {
            if (!isSignedIn) {
              pushToast({
                variant: "info",
                title: "Sign in to pair a device.",
                description: "Pairing goes through h4rn-connect. Sign in under Remote access.",
              });
              return;
            }
            setPairOpen(true);
            requestAnimationFrame(() =>
              document.getElementById("pair-device")?.scrollIntoView({ block: "nearest" }),
            );
          }}
        >
          <QrCodeIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Pair a device
        </button>
      </div>

      {!isSignedIn ? (
        <div className="q-group">
          <h2>Remote access</h2>
          <p className="desc">
            Sign in to h4rn-connect to reach machines outside this network, or keep working with the
            local daemon.
          </p>
          <ConnectSignIn compact />
        </div>
      ) : null}

      {isSignedIn && pairOpen ? <PairDeviceGroup onClose={() => setPairOpen(false)} /> : null}
      {isSignedIn ? <PairingConfirmations /> : null}

      {isSignedIn ? (
        machines.length > 0 ? (
          <MachineRows
            machines={machines}
            activeId={activeId}
            connection={connection}
            busy={busy}
            now={now}
          />
        ) : (
          <div className="q-group">
            <h2>Machines</h2>
            <p className="desc">
              No machine has logged in to this relay yet. Run{" "}
              <span className="q-mono">h4rnd login --connect {connectUrl}</span> on a host.
            </p>
          </div>
        )
      ) : null}

      <div className="q-group" style={{ marginBottom: 0 }}>
        <h2>Hosts</h2>
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
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={6} className="q-muted" style={{ padding: "18px 0" }}>
                {phase === "loading"
                  ? "Loading hosts…"
                  : "No hosts answer yet. Start the daemon, or connect a remote machine above."}
              </td>
            </tr>
          ) : null}
          {rows.map((host: HostSummary) => {
            const online = host.online && connection.status === "online";
            const isActiveHost = host.host_id === hostId;
            return (
              <tr key={host.host_id} className="hov">
                <td>
                  <span className="q-cell-id">
                    <EntityAvatar
                      seed={machineSeed(host.host_id)}
                      kind="machine"
                      label={host.name}
                      size={host.platform.startsWith("wsl:") ? 16 : 18}
                    />
                    <span style={{ fontWeight: 500 }}>{host.name}</span>
                  </span>
                </td>
                <td className="q-muted">{host.platform}</td>
                <td className="q-muted">
                  {online ? pathLabel(host.path ?? connection.transport) : "—"}
                </td>
                <td>
                  {online ? (
                    <Health tone="on">Online</Health>
                  ) : (
                    <Health tone="off">
                      {connection.status === "connecting" ? "Connecting…" : "Offline"}
                    </Health>
                  )}
                </td>
                <td className="q-muted">
                  {host.harnesses.length === 0
                    ? "None installed"
                    : host.harnesses.map(harnessLabel).join(", ")}
                </td>
                <td className="r q-num q-muted">
                  {online
                    ? latencyText(host.latency_ms ?? (isActiveHost ? connection.latencyMs : null))
                    : "—"}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>

      {isSignedIn ? <PairedDevicesGroup /> : null}

      <div className="q-group">
        <h2>Connection</h2>
        <div className="q-field">
          <div className="lab">
            <div className="t">Transport</div>
            <div className="s">Chosen by the Rust transport; TypeScript only renders it.</div>
          </div>
          <div className="ctl">
            {connection.status === "online" ? (
              <Health tone="on">
                {pathLabel(connection.transport)} · {latencyText(connection.latencyMs)} · encrypted
              </Health>
            ) : connection.status === "connecting" ? (
              <span className="q-muted">Connecting…</span>
            ) : (
              <Health tone="danger">Offline</Health>
            )}
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn secondary" onClick={() => void refresh()}>
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Refresh
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Devices + pairing
// ---------------------------------------------------------------------------

function deviceIcon(device: ConnectDevice) {
  if (device.kind === "mobile") return SmartphoneIcon;
  if (device.kind === "daemon") return ServerIcon;
  if (device.platform === "macos" || device.platform === "windows" || device.platform === "linux") {
    return LaptopIcon;
  }
  return MonitorIcon;
}

function deviceKindLabel(device: ConnectDevice): string {
  if (device.kind === "daemon") return "Daemon";
  if (device.kind === "mobile") return "Phone";
  return "Computer";
}

function DeviceRow({
  device,
  current,
  now,
  onRename,
  onRevoke,
}: {
  device: ConnectDevice;
  current: boolean;
  now: number;
  onRename: (name: string) => Promise<boolean>;
  onRevoke: () => Promise<boolean>;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(device.name);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const Icon = deviceIcon(device);

  const nameInput = useRef<HTMLInputElement>(null);

  useEffect(() => {
    setDraft(device.name);
    setEditing(false);
  }, [device.name]);

  useEffect(() => {
    if (editing) nameInput.current?.focus();
  }, [editing]);

  return (
    <tr className={current ? undefined : "hov"}>
      <td>
        <span className="q-cell-id">
          <Icon className="q-icon" strokeWidth={1.5} aria-hidden />
          {editing ? (
            <span style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
              <input
                className="q-input"
                value={draft}
                aria-label={`Rename ${device.name}`}
                style={{ height: 26, maxWidth: 180 }}
                ref={nameInput}
                onChange={(event) => setDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    void onRename(draft).then((ok) => ok && setEditing(false));
                  }
                  if (event.key === "Escape") setEditing(false);
                }}
              />
              <button
                type="button"
                className="q-ibtn"
                aria-label="Save name"
                onClick={() => void onRename(draft).then((ok) => ok && setEditing(false))}
              >
                <CheckIcon className="q-icon s14" strokeWidth={2} aria-hidden />
              </button>
              <button
                type="button"
                className="q-btn ghost"
                style={{ height: 24 }}
                onClick={() => setEditing(false)}
              >
                Cancel
              </button>
            </span>
          ) : (
            <span>
              <span style={{ fontWeight: 500 }}>{device.name}</span>
              {current ? <span className="q-faint"> · this device</span> : null}
            </span>
          )}
        </span>
      </td>
      <td className="q-muted">{deviceKindLabel(device)}</td>
      <td className="q-muted">{scopesLabel(device.scopes)}</td>
      <td>
        {device.last_seen ? (
          <Health tone="on">{relativeSeen(device.last_seen, now)}</Health>
        ) : (
          <span className="q-muted">Never connected</span>
        )}
      </td>
      <td className="r" style={{ paddingRight: 0 }}>
        {editing ? null : confirming ? (
          <span style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
            <span className="q-faint" style={{ fontSize: 12 }}>
              {current
                ? "Sign out of this device?"
                : device.kind === "daemon"
                  ? "Disconnect this host?"
                  : "Revoke?"}
            </span>
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 24 }}
              onClick={() => setConfirming(false)}
            >
              Cancel
            </button>
            <button
              type="button"
              className="q-btn danger"
              style={{ height: 24 }}
              disabled={busy}
              onClick={() => {
                setBusy(true);
                void onRevoke().then((ok) => {
                  setBusy(false);
                  if (!ok) setConfirming(false);
                });
              }}
            >
              {current ? "Sign out" : "Revoke"}
            </button>
          </span>
        ) : (
          <span style={{ display: "inline-flex", gap: 4, alignItems: "center" }}>
            <button
              type="button"
              className="q-ibtn"
              aria-label={`Rename ${device.name}`}
              onClick={() => setEditing(true)}
            >
              <PencilIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
            <button
              type="button"
              className="q-btn danger"
              style={{ height: 24 }}
              onClick={() => setConfirming(true)}
            >
              {current ? "Sign out" : "Revoke"}
            </button>
          </span>
        )}
      </td>
    </tr>
  );
}

/**
 * Typed-code pairings the daemon is waiting on a person to check. The code is
 * derived by the daemon in Rust from the keys it holds; this only displays it.
 */
function usePairingConfirmations(enabled: boolean): {
  entries: PairingConfirmation[];
  decide: (deviceId: string, confirm: boolean) => Promise<void>;
} {
  const [entries, setEntries] = useState<PairingConfirmation[]>([]);
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const load = async () => {
      try {
        const response = await getClient().request({ type: "pairing_confirmations" });
        if (!cancelled && response.type === "pairing_confirmations") {
          setEntries(response.confirmations);
        }
      } catch {
        // A daemon without the request (or a revoked session) just shows none.
      }
    };
    void load();
    const timer = window.setInterval(() => void load(), 2_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled]);

  const decide = async (deviceId: string, confirm: boolean) => {
    // Hide the card immediately; the next poll confirms the daemon's answer.
    setEntries((current) => current.filter((entry) => entry.device_id !== deviceId));
    try {
      await getClient().request({ type: "confirm_pairing", device_id: deviceId, confirm });
      pushToast({
        variant: "success",
        title: confirm ? "Device approved." : "Device rejected.",
        description: confirm
          ? "It can now reach this host with the scopes it was granted."
          : "Its key was revoked and never reached this host.",
      });
    } catch (error) {
      pushToast({
        variant: "error",
        title: "Could not record the pairing decision.",
        description: error instanceof Error ? error.message : String(error),
      });
      const response = await getClient()
        .request({ type: "pairing_confirmations" })
        .catch(() => null);
      if (response?.type === "pairing_confirmations") setEntries(response.confirmations);
    }
  };

  return { entries, decide };
}

/** Incoming pairing requests waiting for the owner to compare codes. */
function PairingConfirmations() {
  const isSignedIn = useRemote((state) => state.user !== null);
  const { entries: confirmations, decide: decidePairing } = usePairingConfirmations(isSignedIn);
  if (confirmations.length === 0) return null;
  return (
    <div className="q-group">
      {confirmations.map((entry) => (
        <fieldset key={entry.device_id} className="q-sas-host">
          <legend className="q-muted" style={{ fontSize: 12 }}>
            {entry.device_name} wants to connect. Its screen shows
          </legend>
          <div className="q-sas-code num" data-testid="host-sas">
            {entry.sas}
          </div>
          <div className="q-faint" style={{ fontSize: 12 }}>
            Approve only if both codes match. Until then the device cannot reach this host.
          </div>
          <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
            <button
              type="button"
              className="q-btn primary"
              data-testid="confirm-pairing"
              onClick={() => void decidePairing(entry.device_id, true)}
            >
              Confirm device
            </button>
            <button
              type="button"
              className="q-btn danger"
              data-testid="reject-pairing"
              onClick={() => void decidePairing(entry.device_id, false)}
            >
              Reject
            </button>
          </div>
        </fieldset>
      ))}
    </div>
  );
}

/** QR code and short code for adding a phone or computer. */
function PairDeviceGroup({ onClose }: { onClose: () => void }) {
  const user = useRemote((state) => state.user);
  const machines = useRemote((state) => state.machines);
  const activeId = useRemote((state) => state.activeMachineId);
  const pairing = useRemote((state) => state.pairing);
  const phase = useRemote((state) => state.phase);
  const now = useNow(1_000);
  const isSignedIn = user !== null;
  const available = machines;
  const [machineId, setMachineId] = useState(() => activeId ?? available[0]?.id ?? "");
  const [copied, setCopied] = useState(false);
  const [scopes, setScopes] = useState<PairScope[]>(() => [...DEFAULT_PAIR_SCOPES]);
  const expiring = useRef(false);

  useEffect(() => {
    if (!machineId && available[0]) setMachineId(available[0].id);
  }, [available, machineId]);

  useEffect(() => {
    if (!isSignedIn || pairing || !machineId) return;
    void createPairing(machineId, scopes);
  }, [isSignedIn, pairing, machineId, scopes]);

  // The scopes are baked into the pairing when connect creates it, so changing
  // a checkbox asks for a new code. The wait keeps a run of clicks to one call.
  const offeredScopes = pairing?.scopes.join(",");
  useEffect(() => {
    if (!pairing || !machineId || offeredScopes === orderedScopes(scopes).join(",")) return;
    const timer = window.setTimeout(() => void createPairing(machineId, scopes), 350);
    return () => window.clearTimeout(timer);
  }, [scopes, offeredScopes, pairing, machineId]);

  const pairingExpired = pairing !== null && pairing.expiresAt <= now;

  useEffect(() => {
    if (!pairing) {
      expiring.current = false;
      return;
    }
    if (!pairingExpired || expiring.current) return;
    expiring.current = true;
    void createPairing(machineId, scopes);
  }, [pairing, pairingExpired, machineId, scopes]);

  const pairingMachine = available.find((machine) => machine.id === machineId);
  const remaining = pairing ? Math.max(0, pairing.expiresAt - now) : 0;
  const remainingText = `${Math.floor(remaining / 60_000)}:${String(
    Math.floor((remaining % 60_000) / 1_000),
  ).padStart(2, "0")}`;

  return (
    <div className="q-group" id="pair-device">
      <h2>
        Pair a device <span className="grow" />
        {pairing ? (
          <span className="q-faint q-num" style={{ fontSize: 12, fontWeight: 400 }}>
            Expires in <span data-countdown="pair">{remainingText}</span>
          </span>
        ) : null}
        <button
          type="button"
          className="q-btn ghost"
          style={{ height: 24, marginLeft: 8 }}
          onClick={onClose}
        >
          Done
        </button>
      </h2>
      <div style={{ paddingTop: 12 }}>
        <div className="q-field" style={{ borderTop: "1px solid var(--line)" }}>
          <div className="lab">
            <div className="t">Machine</div>
            <div className="s">The paired device can reach this machine's hosts.</div>
          </div>
          <div className="ctl">
            <ChipSelect
              label="Machine to pair with"
              value={machineId}
              placeholder="No machines"
              width={200}
              options={available.map((machine) => ({
                value: machine.id,
                label: machine.name,
              }))}
              onChange={(next) => {
                setMachineId(next);
                dismissPairing();
              }}
            />
          </div>
        </div>
        {pairing ? (
          <div
            style={{
              display: "flex",
              gap: 28,
              padding: "18px 0 4px",
              borderTop: "1px solid var(--line)",
            }}
          >
            <div className="q-qr">
              <QrCode
                value={pairing.qr}
                label={`Pairing QR code for ${pairingMachine?.name ?? "this machine"}`}
                size={148}
              />
            </div>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div className="q-muted" style={{ fontSize: 12 }}>
                Scan with the h4rn app, or enter this code
              </div>
              <div className="q-code-big" style={{ margin: "4px 0 2px" }}>
                {formatShortCode(pairing.shortCode)}
              </div>
              <div className="q-faint" style={{ fontSize: 12, marginBottom: 14 }}>
                Works once, for one device. Pairs with {pairingMachine?.name ?? "this machine"}.
              </div>
              <div className="q-pair-scopes">
                {SCOPE_OPTIONS.map((scope) => (
                  <label key={scope.id} className="q-pair-scope">
                    <input
                      type="checkbox"
                      className="sr-only"
                      checked={scope.locked || scopes.includes(scope.id)}
                      disabled={scope.locked}
                      onChange={() => setScopes((current) => toggleScope(current, scope.id))}
                    />
                    <span
                      className={`q-check${scope.locked || scopes.includes(scope.id) ? " on" : ""}`}
                      aria-hidden
                    >
                      {scope.locked || scopes.includes(scope.id) ? (
                        <CheckIcon className="q-icon s12" strokeWidth={2} />
                      ) : null}
                    </span>
                    <span>
                      <span style={{ fontWeight: 500 }}>{scope.label}</span>
                      {scope.sub ? <span className="q-faint sub">{scope.sub}</span> : null}
                    </span>
                  </label>
                ))}
              </div>
              <div style={{ display: "flex", gap: 6, marginTop: 12, flexWrap: "wrap" }}>
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={() => {
                    void navigator.clipboard?.writeText(pairing.qr).then(() => {
                      setCopied(true);
                      window.setTimeout(() => setCopied(false), 1_500);
                    });
                  }}
                >
                  {copied ? (
                    <CheckIcon className="q-icon s14" strokeWidth={2} aria-hidden />
                  ) : (
                    <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  )}
                  {copied ? "Copied" : "Copy link"}
                </button>
                <button
                  type="button"
                  className="q-btn ghost"
                  onClick={() => void createPairing(machineId, scopes)}
                >
                  <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  New code
                </button>
              </div>
            </div>
          </div>
        ) : (
          <div className="q-field">
            <div className="lab">
              <div className="t">
                {phase === "signing_in" ? "Preparing a code…" : "No code yet"}
              </div>
            </div>
            <div className="ctl">
              <button
                type="button"
                className="q-btn secondary"
                disabled={!machineId}
                onClick={() => void createPairing(machineId, scopes)}
              >
                Generate a pairing code
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

/** Phones and computers that supervise the hosts. */
function PairedDevicesGroup() {
  const device = useRemote((state) => state.device);
  const devices = useRemote((state) => state.devices);
  const now = useNow();
  const liveDevices = devices.filter((candidate) => !candidate.revoked_at);
  return (
    <div className="q-group">
      <h2>
        Devices{" "}
        <span className="q-faint q-num" style={{ fontSize: 12, fontWeight: 400 }}>
          {liveDevices.length}
        </span>
      </h2>
      <p className="desc">
        Phones and computers that can reach your hosts. Each gets its own scopes, and you can revoke
        one without touching the others.
      </p>
      <table className="q-tbl">
        <thead>
          <tr>
            <th>Device</th>
            <th>Kind</th>
            <th>Scopes</th>
            <th>Last seen</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {liveDevices.length === 0 ? (
            <tr>
              <td colSpan={5} className="q-muted" style={{ padding: "16px 0" }}>
                No devices paired yet.
              </td>
            </tr>
          ) : null}
          {liveDevices.map((candidate) => (
            <DeviceRow
              key={candidate.id}
              device={candidate}
              current={candidate.id === device?.id}
              now={now}
              onRename={(name) => renameDevice(candidate.id, name)}
              onRevoke={() => revokeDevice(candidate.id)}
            />
          ))}
        </tbody>
      </table>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

export function LiveAccountForm() {
  const user = useRemote((state) => state.user);
  const device = useRemote((state) => state.device);
  const connectUrl = useRemote((state) => state.connectUrl);
  const connection = useApp((state) => state.connection);
  const hostName = useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? null) : null,
  );
  const machines = useRemote((state) => state.machines);

  return (
    <div className="q-form">
      <h1>Account & relay</h1>
      <p className="lead">
        Relay reaches hosts that are not on your network. Nothing is routed until you allow it.
      </p>

      {user ? (
        <>
          <div className="q-group">
            <h2>Relay</h2>
            <div className="q-field">
              <div className="lab">
                <div className="t">Connection</div>
              </div>
              <div className="ctl">
                {connection.status === "online" ? (
                  <Health tone="on">
                    {pathLabel(connection.transport)}
                    {hostName ? ` to ${hostName}` : ""} · {latencyText(connection.latencyMs)} ·
                    encrypted
                  </Health>
                ) : connection.status === "connecting" ? (
                  <span className="q-muted">Connecting…</span>
                ) : (
                  <Health tone="danger">Offline</Health>
                )}
              </div>
            </div>
            <div className="q-field">
              <div className="lab">
                <div className="t">Relay URL</div>
              </div>
              <div className="ctl">
                <span className="q-faint q-mono" style={{ fontSize: 11.5 }}>
                  {connectUrl}
                </span>
              </div>
            </div>
            <div className="q-field">
              <div className="lab">
                <div className="t">Paired machines</div>
                <div className="s">Reached through h4rn-connect</div>
              </div>
              <div className="ctl">
                <span className="q-muted q-num">
                  {machines.filter((machine) => machine.online).length} online of {machines.length}
                </span>
                <span style={{ flex: 1 }} />
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={() =>
                    navigate({
                      kind: "placeholder",
                      surface: "settings",
                      path: "/settings/hosts",
                    })
                  }
                >
                  Manage hosts
                </button>
              </div>
            </div>
          </div>

          <div className="q-group">
            <h2>Account</h2>
            <div className="q-field">
              <div className="lab">
                <div className="t">{user.display_name ?? "Signed in"}</div>
                <div className="s">{user.email ?? user.id}</div>
              </div>
              <div className="ctl">
                <span style={{ flex: 1 }} />
                <SignOutButton
                  onDone={() =>
                    navigate({ kind: "placeholder", surface: "settings", path: "/settings/hosts" })
                  }
                />
              </div>
            </div>
            <div className="q-field">
              <div className="lab">
                <div className="t">This device</div>
                <div className="s">{device?.name ?? "Unknown device"}</div>
              </div>
              <div className="ctl">
                <span className="q-faint" style={{ fontSize: 12 }}>
                  {device?.platform ?? "web"} · {device?.kind ?? "web"}
                </span>
                <span style={{ flex: 1 }} />
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={() =>
                    navigate({
                      kind: "placeholder",
                      surface: "settings",
                      path: "/settings/devices",
                    })
                  }
                >
                  Manage devices
                </button>
              </div>
            </div>
          </div>
        </>
      ) : (
        <ConnectSignIn compact />
      )}
    </div>
  );
}
