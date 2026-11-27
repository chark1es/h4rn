import {
  AlertCircleIcon,
  ArrowRightIcon,
  CopyIcon,
  KeyRoundIcon,
  LoaderIcon,
  LogOutIcon,
} from "lucide-react";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { Collapsible, CollapsiblePanel, CollapsibleTrigger } from "@/components/ui/collapsible";
import type { ProviderInfo } from "@/gen";
import { i18n } from "@/lib/i18n";
import { listProviders } from "./connect";
import { remoteErrorText } from "./remoteError";
import {
  cancelLogin,
  DEFAULT_CONNECT_URL,
  type RemoteBusy,
  setConnectUrl,
  signIn,
  signInWithDeviceCode,
  useRemote,
} from "./remoteStore";

function relayHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/** The store's current step, in the reader's language. */
export function busyText(busy: RemoteBusy): string {
  return busy.kind === "connecting"
    ? i18n.t("settings:connect.busy.connecting", { name: busy.machineName })
    : i18n.t(`settings:connect.busy.${busy.kind}`);
}

function providerLabel(provider: ProviderInfo): string {
  return provider.display_name || provider.id;
}

/**
 * Connect sign-in: choose a deployment, then use OIDC or a device code.
 *
 * The browser redirect, the desktop loopback and the mobile custom scheme all
 * resolve back into `remoteStore`, so this form never handles a token.
 */
export function ConnectSignIn() {
  const { t } = useTranslation(["settings", "common"]);
  const phase = useRemote((state) => state.phase);
  const connectUrl = useRemote((state) => state.connectUrl);
  const busy = useRemote((state) => state.busy);
  const error = useRemote((state) => state.error);
  const login = useRemote((state) => state.login);
  const [draftUrl, setDraftUrl] = useState(connectUrl);
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [providersError, setProvidersError] = useState<string | null>(null);
  const [probing, setProbing] = useState(false);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => setDraftUrl(connectUrl), [connectUrl]);

  // `attempt` is not read in the effect: bumping it re-probes on Retry.
  // biome-ignore lint/correctness/useExhaustiveDependencies: attempt only re-runs the probe
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
  }, [connectUrl, attempt]);

  const primary = providers[0];
  const signingIn = phase === "signing_in" || busy !== null;

  return (
    <div className="q-group">
      <h2>h4rn-connect</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("account.relay")}</div>
          <div className="s">{t("connect.relayHint")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={draftUrl}
            aria-label={t("connect.urlLabel")}
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
            {t("connect.use")}
          </button>
        </div>
      </div>

      {login ? (
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">{t("connect.approvalCode")}</div>
            <div className="s">
              {t("connect.enterAt", { host: new URL(login.verificationUri).host })}
            </div>
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
                {t("connect.openApproval")}
              </a>
              <button
                type="button"
                className="q-btn ghost"
                onClick={() => void navigator.clipboard?.writeText(login.userCode)}
              >
                <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {t("connect.copyCode")}
              </button>
              <button type="button" className="q-btn ghost" onClick={cancelLogin}>
                {t("common:cancel")}
              </button>
            </div>
            <div className="q-faint" style={{ fontSize: 12, marginTop: 8 }}>
              {busy ? busyText(busy) : t("connect.waiting")}
            </div>
          </div>
        </div>
      ) : (
        <>
          {providersError ? (
            <div className="q-banner err wrap q-signin-err" role="alert">
              <AlertCircleIcon className="q-icon s14 ic" strokeWidth={1.5} aria-hidden />
              <div className="grow">
                <Trans
                  t={t}
                  i18nKey="connect.unreachable"
                  values={{ host: relayHost(connectUrl) }}
                  components={{ b: <b /> }}
                />
                <Collapsible className="q-signin-details">
                  <CollapsibleTrigger className="text-xs">
                    {t("proxy.probe.details")}
                  </CollapsibleTrigger>
                  <CollapsiblePanel>
                    <span className="q-mono">{providersError}</span>
                  </CollapsiblePanel>
                </Collapsible>
              </div>
              <button
                type="button"
                className="q-btn secondary"
                disabled={probing}
                onClick={() => setAttempt((count) => count + 1)}
              >
                {probing ? t("connect.retrying") : t("common:retry")}
              </button>
            </div>
          ) : null}
          <div className="q-field">
            <div className="lab">
              <div className="t">{t("connect.signIn")}</div>
              <div className="s">
                {probing
                  ? t("connect.checkingProviders")
                  : primary
                    ? t("connect.providerAt", {
                        provider: providerLabel(primary),
                        host: new URL(primary.issuer).host,
                      })
                    : providersError
                      ? t("connect.availableOnceReachable")
                      : t("connect.noProvider")}
              </div>
            </div>
            <div className="ctl" style={{ flexWrap: "wrap" }}>
              <button
                type="button"
                className="q-btn primary"
                disabled={!primary || signingIn}
                onClick={() => (primary ? void signIn(primary.id) : undefined)}
              >
                {signingIn ? (
                  <LoaderIcon className="q-icon s14 q-spin" strokeWidth={1.5} aria-hidden />
                ) : (
                  <KeyRoundIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                )}
                {primary
                  ? t("connect.signInWith", { provider: providerLabel(primary) })
                  : t("connect.signIn")}
              </button>
              {primary?.device_grant ? (
                <button
                  type="button"
                  className="q-btn secondary"
                  disabled={signingIn}
                  onClick={() => void signInWithDeviceCode(primary.id)}
                >
                  {t("connect.otherDevice")}
                  <ArrowRightIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                </button>
              ) : null}
            </div>
          </div>
        </>
      )}

      {error ? (
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">{t("connect.failed")}</div>
          </div>
          <div className="ctl">
            <span className="q-danger" style={{ fontSize: 12 }}>
              {remoteErrorText(error)}
            </span>
          </div>
        </div>
      ) : null}
    </div>
  );
}

export function SignOutButton({ onDone }: { onDone?: () => void }) {
  const { t } = useTranslation(["settings", "common"]);
  const [confirming, setConfirming] = useState(false);
  const busy = useRemote((state) => state.busy);
  if (!confirming) {
    return (
      <button type="button" className="q-btn ghost" onClick={() => setConfirming(true)}>
        <LogOutIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        {t("devices.signOut")}
      </button>
    );
  }
  return (
    <span style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
      <span className="q-faint" style={{ fontSize: 12 }}>
        {t("connect.forgetDevice")}
      </span>
      <button
        type="button"
        className="q-btn ghost"
        onClick={() => setConfirming(false)}
        disabled={busy !== null}
      >
        {t("common:cancel")}
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
        {t("devices.signOut")}
      </button>
    </span>
  );
}
