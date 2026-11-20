/**
 * Settings → Hosts → This computer: the daemon's direct network listener.
 *
 * This is the offline way another device reaches this machine: no relay, no
 * account. The daemon owns the listener, the addresses and the one-time
 * pairing secret; this panel mirrors them and never decides them itself.
 *
 * It is only rendered for a local daemon. A remote machine is administered
 * through its own Settings, not by toggling the machine this app happens to
 * be driving.
 */

import { CheckIcon, CopyIcon, LoaderIcon, QrCodeIcon, RefreshCwIcon } from "lucide-react";
import { useEffect, useState } from "react";

import type { PairScope } from "@/gen";
import { useNow } from "@/hooks/use-now";
import { useApp } from "@/lib/store/appStore";
import {
  cancelDirectPairing,
  createDirectPairing,
  type DirectPairingView,
  loadNetworkAccess,
  setNetworkAccess,
  useNetworkAccess,
} from "./networkAccessStore";
import { DEFAULT_PAIR_SCOPES, SCOPE_OPTIONS, toggleScope } from "./pairingScopes";
import { QrCode } from "./QrCode";
import { shellKind, useRemote } from "./remoteStore";

export type Shell = "desktop" | "mobile" | "web";

/**
 * Whether this machine's network access belongs on screen.
 *
 * A remote machine would be the one toggled otherwise, and a phone has no
 * daemon of its own on loopback, so both hide it.
 */
export function networkAccessVisible(
  transport: string,
  remoteActive: boolean,
  shell: Shell = shellKind(),
): boolean {
  if (shell === "mobile") return false;
  if (remoteActive) return false;
  return transport !== "relay" && transport !== "tailscale";
}

function Switch({
  checked,
  disabled,
  label,
  onChange,
}: {
  checked: boolean;
  disabled: boolean;
  label: string;
  onChange: (next: boolean) => void;
}) {
  return (
    <button
      type="button"
      className="q-switch"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    />
  );
}

function remainingText(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1_000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

/**
 * The QR, its expiry and the scopes the new device will hold.
 *
 * Exported so the pairing presentation can be rendered without a live daemon.
 */
export function DirectPairingBlock({
  offer,
  scopes,
  hostName,
  now,
  onToggleScope,
  onNewCode,
  onCancel,
}: {
  offer: DirectPairingView;
  scopes: readonly PairScope[];
  hostName: string | null;
  now: number;
  onToggleScope: (scope: PairScope) => void;
  onNewCode: () => void;
  onCancel: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const remaining = offer.expiresAt - now;
  const expired = remaining <= 0;
  const admin = scopes.includes("admin");

  return (
    <div
      data-testid="direct-pairing"
      style={{
        display: "flex",
        gap: 28,
        padding: "18px 0 4px",
        borderTop: "1px solid var(--line)",
      }}
    >
      <div className="q-qr">
        <QrCode
          value={offer.uri}
          label={`Direct pairing QR code${hostName ? ` for ${hostName}` : ""}`}
          size={148}
        />
      </div>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div className="q-muted" style={{ fontSize: 12 }}>
          {expired
            ? "This code has expired."
            : "Scan with the h4rn app on the other device, or copy the link."}
        </div>
        <div className="q-faint q-num" style={{ fontSize: 12, marginTop: 4 }}>
          {expired ? (
            "Generate a new code to pair."
          ) : (
            <>
              One device, once. Expires in{" "}
              <span data-countdown="direct-pair" className="q-num">
                {remainingText(remaining)}
              </span>
              .
            </>
          )}
        </div>
        <div className="q-pair-scopes" style={{ marginTop: 12 }}>
          {SCOPE_OPTIONS.map((scope) => {
            const on = scope.locked || scopes.includes(scope.id);
            return (
              <label key={scope.id} className="q-pair-scope">
                <input
                  type="checkbox"
                  className="sr-only"
                  checked={on}
                  disabled={scope.locked}
                  onChange={() => onToggleScope(scope.id)}
                />
                <span className={`q-check${on ? " on" : ""}`} aria-hidden>
                  {on ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
                </span>
                <span>
                  <span style={{ fontWeight: 500 }}>{scope.label}</span>
                  {scope.sub ? <span className="q-faint sub">{scope.sub}</span> : null}
                </span>
              </label>
            );
          })}
        </div>
        {admin ? (
          <div className="q-faint" style={{ fontSize: 12, marginTop: 8 }}>
            Admin lets that device change hosts, devices and this setting on{" "}
            {hostName ?? "this computer"}.
          </div>
        ) : null}
        <div style={{ display: "flex", gap: 6, marginTop: 12, flexWrap: "wrap" }}>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => {
              void navigator.clipboard?.writeText(offer.uri).then(() => {
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
          <button type="button" className="q-btn ghost" onClick={onNewCode}>
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            New code
          </button>
          <button type="button" className="q-btn ghost" onClick={onCancel}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

/** The "This computer" group on Settings → Hosts. */
export function NetworkAccessSettings() {
  const transport = useApp((state) => state.connection.transport);
  const status = useApp((state) => state.connection.status);
  const hostName = useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? null) : null,
  );
  const active = useRemote((state) => state.active);
  const state = useNetworkAccess();
  const [open, setOpen] = useState(false);
  const [scopes, setScopes] = useState<PairScope[]>(() => [...DEFAULT_PAIR_SCOPES]);
  const now = useNow(1_000);
  const visible = networkAccessVisible(transport, active !== null);

  const access = state.access;
  const enabled = access?.enabled ?? false;
  const listening = access?.listening ?? false;
  const addresses = access?.addresses ?? [];
  const ready = enabled && listening && addresses.length > 0;
  const offer = state.offer;
  const accessError = access?.error ?? state.error;

  useEffect(() => {
    if (!visible || status !== "online") return;
    void loadNetworkAccess();
  }, [visible, status]);

  // Turning the listener off closes the daemon's pairing window too.
  useEffect(() => {
    if (!enabled) setOpen(false);
  }, [enabled]);

  useEffect(() => {
    if (!open || !ready || offer) return;
    void createDirectPairing(scopes);
  }, [open, ready, offer, scopes]);

  // Scopes are baked into the offer when the daemon arms it, so changing a
  // checkbox asks for a new code. The wait keeps a run of clicks to one call.
  const offeredScopes = offer?.scopes.join(",");
  useEffect(() => {
    if (!open || !ready || !offer || !offeredScopes) return;
    if (offeredScopes === scopes.join(",")) return;
    const timer = window.setTimeout(() => void createDirectPairing(scopes), 350);
    return () => window.clearTimeout(timer);
  }, [open, ready, offer, offeredScopes, scopes]);

  if (!visible) return null;

  return (
    <div className="q-group">
      <h2>This computer</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">Allow devices on this network</div>
          <div className="s">
            Other devices on your Wi-Fi or tailnet can connect after pairing. Traffic is end-to-end
            encrypted.
          </div>
          {accessError ? (
            <div className="s q-danger" role="status">
              {accessError}
            </div>
          ) : null}
        </div>
        <div className="ctl">
          <Switch
            checked={enabled}
            disabled={state.saving || !access}
            label="Allow devices on this network"
            onChange={(next) => void setNetworkAccess(next)}
          />
        </div>
      </div>

      {listening && addresses.length > 0 ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">Addresses</div>
            <div className="s">What another device dials.</div>
          </div>
          <div className="ctl">
            <span className="q-mono" style={{ fontSize: 12, wordBreak: "break-all" }}>
              {addresses.join("  ·  ")}
            </span>
            <button
              type="button"
              className="q-ibtn"
              aria-label="Copy addresses"
              onClick={() => void navigator.clipboard?.writeText(addresses.join(" "))}
            >
              <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          </div>
        </div>
      ) : null}

      <div className="q-field">
        <div className="lab">
          <div className="t">Pair a device</div>
          <div className="s">
            {enabled
              ? listening
                ? "Show a one-time code for a phone or computer on this network."
                : "Waiting for the listener to bind."
              : "Turn on network access first."}
          </div>
          {state.offerError ? (
            <div className="s q-danger" role="status">
              {state.offerError}
            </div>
          ) : null}
        </div>
        <div className="ctl">
          {open ? (
            <span className="q-faint" style={{ fontSize: 12 }}>
              Pairing window open
            </span>
          ) : (
            <button
              type="button"
              className="q-btn secondary"
              disabled={!ready || state.creating || state.saving}
              onClick={() => setOpen(true)}
            >
              {state.creating ? (
                <LoaderIcon className="q-icon s14 q-spin" strokeWidth={1.5} aria-hidden />
              ) : (
                <QrCodeIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              )}
              Pair a device
            </button>
          )}
        </div>
      </div>

      {open && offer ? (
        <DirectPairingBlock
          offer={offer}
          scopes={scopes}
          hostName={hostName}
          now={now}
          onToggleScope={(scope) => setScopes((current) => toggleScope(current, scope))}
          onNewCode={() => void createDirectPairing(scopes)}
          onCancel={() => {
            setOpen(false);
            void cancelDirectPairing();
          }}
        />
      ) : null}
      {open && !offer ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{state.creating ? "Preparing a code…" : "No code yet"}</div>
            {state.offerError ? null : (
              <div className="s">The daemon arms a one-time secret for the code.</div>
            )}
          </div>
          <div className="ctl">
            <button
              type="button"
              className="q-btn ghost"
              onClick={() => {
                setOpen(false);
                void cancelDirectPairing();
              }}
            >
              Cancel
            </button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
