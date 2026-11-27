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

import {
  CheckIcon,
  CopyIcon,
  LoaderIcon,
  QrCodeIcon,
  RefreshCwIcon,
  SmartphoneIcon,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import { Switch } from "@/components/ui/switch";
import type { DirectDevice, PairScope } from "@/gen";
import { useNow } from "@/hooks/use-now";
import { rememberNeighbours } from "@/lib/focus";
import { formatRelativeTime, i18n } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";
import {
  cancelDirectPairing,
  createDirectPairing,
  type DirectPairingView,
  loadDirectDevices,
  loadNetworkAccess,
  revokeDirectDevice,
  setNetworkAccess,
  useNetworkAccess,
} from "./networkAccessStore";
import {
  DEFAULT_PAIR_SCOPES,
  SCOPE_OPTIONS,
  scopeLabel,
  scopeSub,
  scopesLabel,
  toggleScope,
} from "./pairingScopes";
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
  const { t } = useTranslation(["settings", "common"]);
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
          label={hostName ? t("network.qrLabelFor", { host: hostName }) : t("network.qrLabel")}
          size={148}
        />
      </div>
      <div className="flex-1 min-w-0">
        <div className="q-muted text-xs">{expired ? t("network.expired") : t("network.scan")}</div>
        <div className="q-faint q-num" style={{ fontSize: 12, marginTop: 4 }}>
          {expired ? (
            t("network.generateNew")
          ) : (
            <Trans
              t={t}
              i18nKey="network.oneDevice"
              values={{ time: remainingText(remaining) }}
              components={{ countdown: <span data-countdown="direct-pair" className="q-num" /> }}
            />
          )}
        </div>
        <div className="q-pair-scopes mt-3">
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
                  {on ? <CheckIcon className="q-icon s14" strokeWidth={2} /> : null}
                </span>
                <span>
                  <span className="font-medium">{scopeLabel(scope.id)}</span>
                  {scopeSub(scope.id) ? (
                    <span className="q-faint sub">{scopeSub(scope.id)}</span>
                  ) : null}
                </span>
              </label>
            );
          })}
        </div>
        {admin ? (
          <div className="q-faint" style={{ fontSize: 12, marginTop: 8 }}>
            {t("network.adminNote", { host: hostName ?? t("network.thisComputerLower") })}
          </div>
        ) : null}
        <div className="flex gap-1.5 mt-3 flex-wrap">
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
            {copied ? t("common:copied") : t("devices.pairing.copyLink")}
          </button>
          <button type="button" className="q-btn secondary" onClick={onNewCode}>
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("devices.pairing.newCode")}
          </button>
          <button type="button" className="q-btn ghost" onClick={onCancel}>
            {t("common:cancel")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** A short, stable name for a device that only has a key: its first 8 hex digits. */
export function directDeviceName(key: string): string {
  return i18n.t("settings:network.deviceName", { id: key.slice(0, 8).toUpperCase() });
}

function DirectDeviceRow({
  device,
  now,
  busy,
  onRevoke,
}: {
  device: DirectDevice;
  now: number;
  busy: boolean;
  onRevoke: () => Promise<boolean>;
}) {
  const { t } = useTranslation(["settings", "common"]);
  const [confirming, setConfirming] = useState(false);
  const restoreFocus = useRef<(() => Promise<void>) | null>(null);
  const name = directDeviceName(device.key);
  return (
    <tr className="hov">
      <td>
        <span className="q-cell-id">
          <SmartphoneIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          <span className="q-num font-medium" title={device.key}>
            {name}
          </span>
        </span>
      </td>
      <td className="q-muted">{scopesLabel(device.scopes)}</td>
      <td className="q-muted">
        {device.paired_at
          ? now - Date.parse(device.paired_at) < 60_000
            ? t("about.justNow")
            : formatRelativeTime(device.paired_at, now)
          : "—"}
      </td>
      <td>
        {device.connected ? (
          <span className="q-health">
            <span className="q-dot on" aria-hidden />
            {t("network.connected")}
          </span>
        ) : (
          <span className="q-muted">{t("network.notConnected")}</span>
        )}
      </td>
      <td className="r pr-0">
        <button
          type="button"
          className="q-btn danger"
          aria-label={t("access.revokeLabel", { name })}
          onClick={(event) => {
            restoreFocus.current = rememberNeighbours(event.currentTarget);
            setConfirming(true);
          }}
        >
          {t("common:revoke")}
        </button>
        <ConfirmDialog
          open={confirming}
          title={t("devices.confirmRevoke", { name })}
          description={t("devices.confirmDescription.direct", { name })}
          confirmLabel={t("common:revoke")}
          onOpenChange={setConfirming}
          onConfirm={() => {
            if (busy) return;
            void onRevoke().finally(() => void restoreFocus.current?.());
          }}
        />
      </td>
    </tr>
  );
}

/**
 * Devices paired with this computer's own QR. They never went through the
 * relay, so this is the only place they can be seen or removed.
 */
export function DirectDevicesTable({
  devices,
  now,
  revoking,
  onRevoke,
}: {
  devices: readonly DirectDevice[];
  now: number;
  revoking: string | null;
  onRevoke: (key: string) => Promise<boolean>;
}) {
  const { t } = useTranslation(["settings", "common"]);
  return (
    <table className="q-tbl" data-testid="direct-devices">
      <thead>
        <tr>
          <th>{t("devices.table.device")}</th>
          <th>{t("devices.table.scopes")}</th>
          <th>{t("network.paired")}</th>
          <th>{t("proxy.health.status")}</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {devices.length === 0 ? (
          <tr>
            <td colSpan={5} className="q-muted py-4 px-0">
              {t("network.empty")}
            </td>
          </tr>
        ) : null}
        {devices.map((device) => (
          <DirectDeviceRow
            key={device.key}
            device={device}
            now={now}
            busy={revoking === device.key}
            onRevoke={() => onRevoke(device.key)}
          />
        ))}
      </tbody>
    </table>
  );
}

/** The "This computer" group on Settings → Hosts. */
export function NetworkAccessSettings() {
  const { t } = useTranslation(["settings", "common"]);
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
    void loadDirectDevices();
  }, [visible, status]);

  // While a code is showing, watch for the device that scans it. A new key
  // means the one-time secret was spent, so the QR closes on its own.
  const devices = state.devices;
  const knownAtOpen = useRef<number | null>(null);
  useEffect(() => {
    if (!open) {
      knownAtOpen.current = null;
      return;
    }
    knownAtOpen.current ??= devices?.length ?? 0;
    const timer = window.setInterval(() => void loadDirectDevices(), 3_000);
    return () => window.clearInterval(timer);
  }, [open, devices?.length]);
  useEffect(() => {
    if (!open || knownAtOpen.current === null || !devices) return;
    if (devices.length > knownAtOpen.current) {
      setOpen(false);
      void cancelDirectPairing();
    }
  }, [open, devices]);

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
      <h2>{t("network.title")}</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("network.allow")}</div>
          <div className="s">{t("network.allowHint")}</div>
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
            aria-label={t("network.allow")}
            onCheckedChange={(next) => void setNetworkAccess(next)}
          />
        </div>
      </div>

      {listening && addresses.length > 0 ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("network.addresses")}</div>
            <div className="s">{t("network.addressesHint")}</div>
          </div>
          <div className="ctl">
            <span className="q-mono" style={{ fontSize: 12, wordBreak: "break-all" }}>
              {addresses.join("  ·  ")}
            </span>
            <button
              type="button"
              className="q-ibtn"
              aria-label={t("network.copyAddresses")}
              onClick={() => void navigator.clipboard?.writeText(addresses.join(" "))}
            >
              <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          </div>
        </div>
      ) : null}

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("network.pair")}</div>
          <div className="s">
            {enabled
              ? listening
                ? t("network.pairHint")
                : t("network.waitingListener")
              : t("network.turnOnFirst")}
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
              {t("network.windowOpen")}
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
              {t("network.pairButton")}
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
            <div className="t">
              {state.creating ? t("devices.pairing.preparing") : t("devices.pairing.noCode")}
            </div>
            {state.offerError ? null : <div className="s">{t("network.arms")}</div>}
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
              {t("common:cancel")}
            </button>
          </div>
        </div>
      ) : null}

      {devices && (devices.length > 0 || enabled) ? (
        <>
          <DirectDevicesTable
            devices={devices}
            now={now}
            revoking={state.revoking}
            onRevoke={revokeDirectDevice}
          />
          {state.devicesError ? (
            <div className="q-danger text-xs mt-2" role="status">
              {state.devicesError}
            </div>
          ) : null}
        </>
      ) : null}
    </div>
  );
}
