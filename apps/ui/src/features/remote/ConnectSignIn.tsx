import { ArrowRightIcon, CopyIcon, KeyRoundIcon, LoaderIcon, LogOutIcon } from "lucide-react";
import { useEffect, useState } from "react";

import type { ProviderInfo } from "@/gen";
import { listProviders } from "./connect";
import {
  cancelLogin,
  DEFAULT_CONNECT_URL,
  setConnectUrl,
  signIn,
  signInWithDeviceCode,
  useRemote,
} from "./remoteStore";

function providerLabel(provider: ProviderInfo): string {
  return provider.display_name || provider.id;
}

/**
 * Connect sign-in: choose a deployment, then use OIDC or a device code.
 *
 * The browser redirect, the desktop loopback and the mobile custom scheme all
 * resolve back into `remoteStore`, so this form never handles a token.
 */
export function ConnectSignIn({ compact = false }: { compact?: boolean }) {
  const phase = useRemote((state) => state.phase);
  const connectUrl = useRemote((state) => state.connectUrl);
  const busy = useRemote((state) => state.busy);
  const error = useRemote((state) => state.error);
  const login = useRemote((state) => state.login);
  const [draftUrl, setDraftUrl] = useState(connectUrl);
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [providersError, setProvidersError] = useState<string | null>(null);
  const [probing, setProbing] = useState(false);

  useEffect(() => setDraftUrl(connectUrl), [connectUrl]);

  useEffect(() => {
    let cancelled = false;
    setProbing(true);
    setProvidersError(null);
    listProviders(connectUrl)
      .then((list) => {
        if (!cancelled) setProviders(list);
      })
      .catch((failure: unknown) => {
        if (!cancelled) {
          setProviders([]);
          setProvidersError(failure instanceof Error ? failure.message : String(failure));
        }
      })
      .finally(() => {
        if (!cancelled) setProbing(false);
      });
    return () => {
      cancelled = true;
    };
  }, [connectUrl]);

  const primary = providers[0];
  const signingIn = phase === "signing_in" || busy !== null;

  return (
    <div className="q-group">
      <h2>h4rn-connect</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">Relay</div>
          <div className="s">Self-hosted; only machines you pair are reachable.</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={draftUrl}
            aria-label="h4rn-connect URL"
            spellCheck={false}
            style={{ maxWidth: 320, fontSize: 12 }}
            onChange={(event) => setDraftUrl(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && draftUrl.trim()) {
                setConnectUrl(draftUrl.trim() || DEFAULT_CONNECT_URL);
              }
            }}
          />
          <button
            type="button"
            className="q-btn secondary"
            disabled={!draftUrl.trim() || draftUrl.trim() === connectUrl}
            onClick={() => setConnectUrl(draftUrl.trim() || DEFAULT_CONNECT_URL)}
          >
            Use
          </button>
        </div>
      </div>

      {login ? (
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">Approval code</div>
            <div className="s">Enter it at {new URL(login.verificationUri).host}</div>
          </div>
          <div className="ctl" style={{ display: "block" }}>
            <div className="q-code-big" style={{ marginBottom: 8 }}>
              {login.userCode}
            </div>
            <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
              <a
                className="q-btn secondary"
                href={login.verificationUriComplete ?? login.verificationUri}
                target="_blank"
                rel="noreferrer"
              >
                Open approval page
              </a>
              <button
                type="button"
                className="q-btn ghost"
                onClick={() => void navigator.clipboard?.writeText(login.userCode)}
              >
                <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                Copy code
              </button>
              <button type="button" className="q-btn ghost" onClick={cancelLogin}>
                Cancel
              </button>
            </div>
            <div className="q-faint" style={{ fontSize: 12, marginTop: 8 }}>
              {busy ?? "Waiting for approval"}…
            </div>
          </div>
        </div>
      ) : (
        <>
          {providersError ? (
            <div className="q-field" style={{ alignItems: "flex-start" }}>
              <div className="lab">
                <div className="t">Relay unreachable</div>
                <div className="s">{providersError}</div>
              </div>
              <div className="ctl">
                <span className="q-faint" style={{ fontSize: 12 }}>
                  Start h4rn-connect at {connectUrl}, then reload this page.
                </span>
              </div>
            </div>
          ) : null}
          <div className="q-field">
            <div className="lab">
              <div className="t">Sign in</div>
              <div className="s">
                {probing
                  ? "Checking providers…"
                  : primary
                    ? `OrbitID at ${new URL(primary.issuer).host}`
                    : "No provider advertised"}
              </div>
            </div>
            <div className="ctl" style={{ flexWrap: "wrap" }}>
              {primary ? (
                <button
                  type="button"
                  className="q-btn primary"
                  disabled={signingIn}
                  onClick={() => void signIn(primary.id)}
                >
                  {signingIn ? (
                    <LoaderIcon className="q-icon s14 q-spin" strokeWidth={1.5} aria-hidden />
                  ) : (
                    <KeyRoundIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  )}
                  Continue with {providerLabel(primary)}
                </button>
              ) : null}
              {primary?.device_grant ? (
                <button
                  type="button"
                  className="q-btn secondary"
                  disabled={signingIn}
                  onClick={() => void signInWithDeviceCode(primary.id)}
                >
                  Sign in on another device
                  <ArrowRightIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                </button>
              ) : null}
              {!primary && !probing ? (
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={() => setConnectUrl(draftUrl.trim() || DEFAULT_CONNECT_URL)}
                >
                  Probe relay
                </button>
              ) : null}
            </div>
          </div>
        </>
      )}

      {error && !compact ? (
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">Sign-in failed</div>
          </div>
          <div className="ctl">
            <span className="q-danger" style={{ fontSize: 12 }}>
              {error}
            </span>
          </div>
        </div>
      ) : null}
    </div>
  );
}

export function AccountBadge() {
  const user = useRemote((state) => state.user);
  const device = useRemote((state) => state.device);
  if (!user) return null;
  return (
    <div className="q-account-badge">
      <span className="q-account-name">{user.display_name ?? user.email ?? user.id}</span>
      <span className="q-faint" style={{ fontSize: 11 }}>
        {device?.name ?? "This device"}
      </span>
    </div>
  );
}

export function SignOutButton({ onDone }: { onDone?: () => void }) {
  const [confirming, setConfirming] = useState(false);
  const busy = useRemote((state) => state.busy);
  if (!confirming) {
    return (
      <button type="button" className="q-btn ghost" onClick={() => setConfirming(true)}>
        <LogOutIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        Sign out
      </button>
    );
  }
  return (
    <span style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
      <span className="q-faint" style={{ fontSize: 12 }}>
        Forget this device?
      </span>
      <button
        type="button"
        className="q-btn ghost"
        onClick={() => setConfirming(false)}
        disabled={busy !== null}
      >
        Cancel
      </button>
      <button
        type="button"
        className="q-btn danger"
        disabled={busy !== null}
        onClick={async () => {
          const { signOut } = await import("./remoteStore");
          await signOut();
          setConfirming(false);
          onDone?.();
        }}
      >
        Sign out
      </button>
    </span>
  );
}
