import { ChevronRightIcon } from "lucide-react";
import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
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
        disabled={locked}
        onClick={onToggle}
      />
    </div>
  );
}

/** Static Phase 4 pairing flow ported from the approved phone prototype. */
export function PairingPage() {
  const [scopes, setScopes] = useState({
    reply: true,
    terminal: false,
    files: false,
    manage: false,
  });
  const toggle = (key: keyof typeof scopes) =>
    setScopes((current) => ({ ...current, [key]: !current[key] }));

  return (
    <main className="q-main" aria-label="Pair with atlas">
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
        <span className="ttl">Pair with atlas</span>
        <span style={{ width: 60 }} />
      </div>
      <div className="q-ph-body q-pair-body">
        <div className="q-pair-hero">
          <EntityAvatar seed={machineSeed("atlas")} kind="machine" label="atlas" size={56} />
          <div className="ttl">atlas</div>
          <div className="sub">Windows 11 · 192.168.1.24 · via relay</div>
          <div className="note">Signed in to h4rn-connect as dev@example.com</div>
        </div>

        <div className="q-ph-gh">This phone can</div>
        <div className="q-ph-group">
          <ScopeRow label="View threads" sub="Always included" on locked />
          <ScopeRow label="Approve and reply" on={scopes.reply} onToggle={() => toggle("reply")} />
          <ScopeRow
            label="Terminal"
            sub="Type into shells on atlas"
            on={scopes.terminal}
            onToggle={() => toggle("terminal")}
          />
          <ScopeRow label="Files and edits" on={scopes.files} onToggle={() => toggle("files")} />
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
              seed={machineSeed("atlas-wsl")}
              kind="machine"
              label="Ubuntu-24.04"
              size={22}
            />
            <div className="grow">
              Ubuntu-24.04<div className="s">WSL 2 on atlas</div>
            </div>
          </div>
        </div>
        <p className="q-pair-note">
          Credentials stay on atlas. You can revoke this phone from any other device.
        </p>
      </div>
      <div className="q-pair-foot">
        <div className="q-faint num">Link expires in 9:41</div>
        <button
          type="button"
          className="q-btn primary"
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" })
          }
        >
          Pair this phone
          <ChevronRightIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </button>
      </div>
    </main>
  );
}
