import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { pairMachine, pendingPairing, useRemote } from "@/features/remote/remoteStore";
import { useNow } from "@/hooks/use-now";
import { machineSeed } from "@/lib/entities";
import { navigate } from "@/lib/store/appStore";

function ScopeRow({
  label,
  sub,
  on,
  locked = false,
  onToggle,
}: {
  label: string;
  sub?: string;
  on: boolean;
  locked?: boolean;
  onToggle?: () => void;
}) {
  return (
    <div className="q-ph-li">
      <div className="grow">
        {label}
        {sub ? <div className="s">{sub}</div> : null}
      </div>
      <button
        type="button"
        className="q-switch"
        role="switch"
        aria-checked={on}
        aria-label={label}
        aria-disabled={locked || undefined}
        disabled={locked}
        onClick={onToggle}
      />
    </div>
  );
}

function formatRemaining(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1_000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

/**
 * Phone pairing confirmation.
 *
 * The QR or deep link already carries the connect URL, machine id and the
 * daemon's Noise key; this screen asks the person to claim it. Scopes are
 * shown as the fixed connect grant: every paired device sees threads and can
 * approve, and the additional scopes stay host-local.
 */
export function PairingPage() {
  const [scopes, setScopes] = useState({
    reply: true,
    terminal: false,
    files: false,
    manage: false,
  });
  const [claiming, setClaiming] = useState(false);
  const user = useRemote((state) => state.user);
  const active = useRemote((state) => state.active);
  const busy = useRemote((state) => state.busy);
  const error = useRemote((state) => state.error);
  const machines = useRemote((state) => state.machines);
  const now = useNow(1_000);
  const pending = pendingPairing();
  const toggle = (key: keyof typeof scopes) =>
    setScopes((current) => ({ ...current, [key]: !current[key] }));

  const pendingMachine = pending
    ? machines.find((machine) => machine.id === pending.payload.machineId)
    : null;
  const machineName =
    pendingMachine?.name ??
    (pending ? pending.payload.machineId : (active?.machineName ?? "atlas"));
  const alreadyPaired =
    pending !== null && active !== null && active.machineId === pending.payload.machineId;
  const expired =
    pending !== null && pending.payload.expiresAt > 0 && pending.payload.expiresAt < now;

  const claim = async () => {
    if (!pending) {
      navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" });
      return;
    }
    setClaiming(true);
    const ok = await pairMachine(pending.raw);
    setClaiming(false);
    if (ok) navigate({ kind: "inbox" });
  };

  return (
    <main className="q-main" aria-label={`Pair with ${machineName}`}>
      <div className="q-ph-nav">
        <button
          type="button"
          className="navbtn"
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" })
          }
        >
          Cancel
        </button>
        <span className="ttl">Pair with {machineName}</span>
        <span style={{ width: 60 }} />
      </div>
      <div className="q-ph-body q-pair-body">
        <div className="q-pair-hero">
          <EntityAvatar
            seed={machineSeed(pending?.payload.machineId ?? machineName)}
            kind="machine"
            label={machineName}
            size={48}
          />
          <div className="ttl">{machineName}</div>
          <div className="sub">
            {pending
              ? `${pending.payload.connectUrl.replace(/^https?:\/\//, "")} · via relay`
              : "No pairing link on this device"}
          </div>
          <div className="note">
            {user
              ? `Signed in to h4rn-connect as ${user.email ?? user.display_name ?? user.id}`
              : "Sign in to h4rn-connect before pairing"}
          </div>
        </div>

        {pending ? (
          <>
            <div className="q-ph-gh">This phone can</div>
            <div className="q-ph-group">
              <ScopeRow label="View threads" sub="Always included" on locked />
              <ScopeRow
                label="Approve and reply"
                on={scopes.reply}
                onToggle={() => toggle("reply")}
              />
              <ScopeRow
                label="Terminal"
                sub={`Type into shells on ${machineName}`}
                on={scopes.terminal}
                onToggle={() => toggle("terminal")}
              />
              <ScopeRow
                label="Files and edits"
                on={scopes.files}
                onToggle={() => toggle("files")}
              />
              <ScopeRow
                label="Manage hosts and devices"
                on={scopes.manage}
                onToggle={() => toggle("manage")}
              />
            </div>
            <div className="q-ph-gh">Also included</div>
            <div className="q-ph-group">
              <div className="q-ph-li">
                <EntityAvatar
                  seed={machineSeed(`${machineName}-hosts`)}
                  kind="machine"
                  label="Hosts"
                  size={22}
                />
                <div className="grow">
                  Every host on {machineName}
                  <div className="s">WSL distros included</div>
                </div>
              </div>
            </div>
          </>
        ) : (
          <div className="q-empty q-sub-empty">
            <h3>Scan a pairing code</h3>
            <p>
              Open the code on the host from Settings → Devices, then scan it with this device. The
              link brings you back here to confirm.
            </p>
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
              Open Devices
            </button>
          </div>
        )}

        <p className="q-pair-note">
          Credentials stay on {machineName}. You can revoke this phone from any other device.
        </p>
        {error || expired ? (
          <p className="q-pair-note q-danger">
            {expired ? "That pairing code has expired. Generate a new one on the host." : error}
          </p>
        ) : null}
      </div>
      <div className="q-pair-foot">
        <div className="q-faint num">
          {pending
            ? expired
              ? "Link expired"
              : `Link expires in ${formatRemaining(pending.payload.expiresAt - now)}`
            : "Waiting for a code"}
        </div>
        <button
          type="button"
          className="q-btn primary"
          disabled={claiming || busy !== null || (pending !== null && (expired || alreadyPaired))}
          onClick={() => (alreadyPaired ? navigate({ kind: "inbox" }) : void claim())}
        >
          {alreadyPaired ? "Open threads" : claiming || busy ? "Pairing…" : "Pair this phone"}
        </button>
      </div>
    </main>
  );
}
