import { CheckIcon } from "lucide-react";
import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { isCompleteShortCode, maskShortCode, SCOPE_OPTIONS } from "@/features/remote/pairingScopes";
import { pairByCode, pairMachine, pendingPairing, useRemote } from "@/features/remote/remoteStore";
import { useNow } from "@/hooks/use-now";
import { machineSeed } from "@/lib/entities";
import { navigate } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

function ScopeRow({ label, sub, on }: { label: string; sub?: string; on: boolean }) {
  return (
    <div className={cn("q-ph-li", !on && "off")}>
      <span className="q-scope-mark" aria-hidden>
        {on ? <CheckIcon className="q-icon s14" strokeWidth={2} /> : null}
      </span>
      <div className="grow">
        {label}
        {sub ? <div className="s">{sub}</div> : null}
      </div>
      <span className="sr-only">{on ? "Included" : "Not included"}</span>
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
 * daemon's Noise key; this screen asks the person to claim it. The scopes are
 * chosen on the host when it makes the code, so they are shown, not toggled:
 * the link carries them as a hint and connect enforces the real set. Without a
 * link, the 8-character code from the host does the same job.
 */
export function PairingPage() {
  const [code, setCode] = useState("");
  const [claiming, setClaiming] = useState(false);
  const user = useRemote((state) => state.user);
  const active = useRemote((state) => state.active);
  const busy = useRemote((state) => state.busy);
  const error = useRemote((state) => state.error);
  const machines = useRemote((state) => state.machines);
  const now = useNow(1_000);
  const pending = pendingPairing();

  const pendingMachine = pending
    ? machines.find((machine) => machine.id === pending.payload.machineId)
    : null;
  // A typed code could be for any host, so no host is named until a link says which.
  const machineName = pending ? (pendingMachine?.name ?? pending.payload.machineId) : null;
  const hostLabel = machineName ?? "the host";
  const alreadyPaired =
    pending !== null && active !== null && active.machineId === pending.payload.machineId;
  const expired =
    pending !== null && pending.payload.expiresAt > 0 && pending.payload.expiresAt < now;

  const claim = async () => {
    setClaiming(true);
    const ok = pending ? await pairMachine(pending.raw) : await pairByCode(code);
    setClaiming(false);
    if (ok) navigate({ kind: "inbox" });
  };

  return (
    <main className="q-main" aria-label={machineName ? `Pair with ${machineName}` : "Pair a host"}>
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
        <span className="ttl">{machineName ? `Pair with ${machineName}` : "Pair a host"}</span>
        <span style={{ width: 60 }} />
      </div>
      <div className="q-ph-body q-pair-body">
        <div className="q-pair-hero">
          {pending && machineName ? (
            <>
              <EntityAvatar
                seed={machineSeed(pending.payload.machineId)}
                kind="machine"
                label={machineName}
                size={56}
              />
              <div className="ttl">{machineName}</div>
              <div className="sub">
                {`${pending.payload.connectUrl.replace(/^https?:\/\//, "")} · via relay`}
              </div>
            </>
          ) : null}
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
              {SCOPE_OPTIONS.map((option) => (
                <ScopeRow
                  key={option.id}
                  label={option.label}
                  sub={option.id === "terminal" ? `Type into shells on ${hostLabel}` : option.sub}
                  on={option.locked || pending.payload.scopes.includes(option.id)}
                />
              ))}
            </div>
            <p className="q-pair-note">Set on {hostLabel}.</p>
            <div className="q-ph-gh">Also included</div>
            <div className="q-ph-group">
              <div className="q-ph-li">
                <EntityAvatar
                  seed={machineSeed(`${hostLabel}-hosts`)}
                  kind="machine"
                  label="Hosts"
                  size={22}
                />
                <div className="grow">
                  Every host on {hostLabel}
                  <div className="s">WSL distros included</div>
                </div>
              </div>
            </div>
          </>
        ) : (
          <div className="q-pair-empty">
            <h3>Scan a pairing code</h3>
            <p>
              Open the code on the host from Settings → Devices, then scan it with this device. The
              link brings you back here to confirm.
            </p>
            <label className="q-pair-code">
              <span className="q-muted">Or type the code under the QR</span>
              <input
                className="q-input"
                inputMode="text"
                autoCapitalize="characters"
                autoComplete="off"
                autoCorrect="off"
                spellCheck={false}
                placeholder="ABCD-EFGH"
                aria-label="Pairing code"
                value={code}
                onChange={(event) => setCode(maskShortCode(event.target.value))}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && isCompleteShortCode(code) && !claiming) {
                    void claim();
                  }
                }}
              />
            </label>
          </div>
        )}

        <p className="q-pair-note">
          Credentials stay on {hostLabel}. You can revoke this phone from any other device.
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
            : "Codes work once and expire in 5 minutes"}
        </div>
        <button
          type="button"
          className="q-btn primary"
          disabled={
            claiming ||
            busy !== null ||
            (pending !== null ? expired || alreadyPaired : !isCompleteShortCode(code))
          }
          onClick={() => (alreadyPaired ? navigate({ kind: "inbox" }) : void claim())}
        >
          {alreadyPaired
            ? "Open threads"
            : claiming || busy
              ? "Pairing…"
              : pending
                ? "Pair this phone"
                : "Pair with code"}
        </button>
      </div>
    </main>
  );
}
