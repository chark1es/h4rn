import { CheckIcon } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import {
  isCompleteShortCode,
  maskShortCode,
  SCOPE_OPTIONS,
  scopeLabel,
  scopeSub,
} from "@/features/remote/pairingScopes";
import {
  confirmPairingSas,
  pairByCode,
  pairMachine,
  pendingPairing,
  rejectPairingSas,
  useRemote,
} from "@/features/remote/remoteStore";
import { useNow } from "@/hooks/use-now";
import { machineSeed } from "@/lib/entities";
import { formatNumber } from "@/lib/i18n";
import { navigate } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

/** How long a typed pairing code lives, as the host mints it. */
const CODE_TTL_MINUTES = 5;

/** The typed code's shape. It is the same in every language, so it is not translated. */
const CODE_SHAPE = "ABCD-EFGH";

function ScopeRow({ label, sub, on }: { label: string; sub?: string; on: boolean }) {
  const { t } = useTranslation("pairing");
  return (
    <div className={cn("q-ph-li", !on && "off")}>
      <span className="q-scope-mark" aria-hidden>
        {on ? <CheckIcon className="q-icon s14" strokeWidth={2} /> : null}
      </span>
      <div className="grow">
        {label}
        {sub ? <div className="s">{sub}</div> : null}
      </div>
      <span className="sr-only">{on ? t("scopes.included") : t("scopes.notIncluded")}</span>
    </div>
  );
}

/** A countdown as `m:ss`, with the minutes in the active language's digits. */
function formatRemaining(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1_000));
  const minutes = formatNumber(Math.floor(seconds / 60));
  const rest = formatNumber(seconds % 60, { minimumIntegerDigits: 2 });
  return `${minutes}:${rest}`;
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
  const { t } = useTranslation(["pairing", "common", "sidebar"]);
  const [code, setCode] = useState("");
  const [claiming, setClaiming] = useState(false);
  const user = useRemote((state) => state.user);
  const active = useRemote((state) => state.active);
  const busy = useRemote((state) => state.busy);
  const sasConfirm = useRemote((state) => state.sasConfirm);
  const error = useRemote((state) => state.error);
  const machines = useRemote((state) => state.machines);
  const now = useNow(1_000);
  const pending = pendingPairing();

  const pendingMachine = pending
    ? machines.find((machine) => machine.id === pending.payload.machineId)
    : null;
  // A typed code could be for any host, so no host is named until a link says which.
  const machineName = pending ? (pendingMachine?.name ?? pending.payload.machineId) : null;
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

  if (sasConfirm) {
    const waiting = sasConfirm.waiting;
    return (
      <main
        className="q-main"
        aria-label={
          waiting
            ? t("sas.waitingLabel", { name: sasConfirm.machineName })
            : t("sas.checkLabel", { name: sasConfirm.machineName })
        }
      >
        <div className="q-ph-nav">
          <span style={{ width: 60 }} />
          <span className="ttl">{waiting ? t("sas.waitingTitle") : t("sas.checkTitle")}</span>
          <span style={{ width: 60 }} />
        </div>
        <div className="q-ph-body q-pair-body">
          <div className="q-pair-hero">
            <div className="ttl">{sasConfirm.machineName}</div>
            <div className="note">
              {waiting
                ? t("sas.mustApprove", { name: sasConfirm.machineName })
                : sasConfirm.method === "qr"
                  ? t("sas.scanned")
                  : t("sas.typed")}
            </div>
          </div>
          <div className="q-sas">
            <div className="q-sas-code num" data-testid="pair-sas">
              {sasConfirm.sas}
            </div>
            <p className="q-pair-note">
              {waiting
                ? t("sas.waitingNote", { name: sasConfirm.machineName })
                : t("sas.compareNote", { name: sasConfirm.machineName })}
            </p>
            {waiting ? (
              <div className="q-sas-waiting" data-testid="pair-waiting" role="status">
                {t("sas.waitingFor", { name: sasConfirm.machineName })}
              </div>
            ) : (
              <p className="q-pair-note">
                {t("sas.approvalRequired", { name: sasConfirm.machineName })}
              </p>
            )}
            {error ? <p className="q-pair-note q-danger">{error}</p> : null}
          </div>
        </div>
        <div className="q-pair-foot">
          <button type="button" className="q-btn secondary" onClick={() => void rejectPairingSas()}>
            {t("sas.differ")}
          </button>
          <button
            type="button"
            className="q-btn primary"
            disabled={waiting}
            onClick={() => {
              void confirmPairingSas().then((ok) => {
                if (ok) navigate({ kind: "inbox" });
              });
            }}
          >
            {waiting ? t("sas.waitingHost") : t("sas.match")}
          </button>
        </div>
      </main>
    );
  }

  const title = machineName ? t("pairWith", { name: machineName }) : t("title");
  return (
    <main className="q-main" aria-label={title}>
      <div className="q-ph-nav">
        <button
          type="button"
          className="navbtn"
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" })
          }
        >
          {t("common:cancel")}
        </button>
        <span className="ttl">{title}</span>
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
                {t("viaRelay", {
                  address: pending.payload.connectUrl.replace(/^https?:\/\//, ""),
                })}
              </div>
            </>
          ) : null}
          <div className="note">
            {user
              ? t("signedInAs", { account: user.email ?? user.display_name ?? user.id })
              : t("signInFirst")}
          </div>
        </div>

        {pending && machineName ? (
          <>
            <div className="q-ph-gh">{t("thisPhoneCan")}</div>
            <div className="q-ph-group">
              {SCOPE_OPTIONS.map((option) => (
                <ScopeRow
                  key={option.id}
                  label={scopeLabel(option.id)}
                  sub={
                    option.id === "terminal"
                      ? t("scopes.terminalOn", { host: machineName })
                      : scopeSub(option.id)
                  }
                  on={option.locked || pending.payload.scopes.includes(option.id)}
                />
              ))}
            </div>
            <p className="q-pair-note">{t("scopes.setOn", { host: machineName })}</p>
            <div className="q-ph-gh">{t("alsoIncluded")}</div>
            <div className="q-ph-group">
              <div className="q-ph-li">
                <EntityAvatar
                  seed={machineSeed(`${machineName}-hosts`)}
                  kind="machine"
                  label={t("sidebar:hosts.title")}
                  size={22}
                />
                <div className="grow">
                  {t("everyHost", { host: machineName })}
                  <div className="s">{t("wslIncluded")}</div>
                </div>
              </div>
            </div>
          </>
        ) : (
          <div className="q-pair-empty">
            <h3>{t("empty.title")}</h3>
            <p>{t("empty.body")}</p>
            <label className="q-pair-code">
              <span className="q-muted">{t("empty.typeCode")}</span>
              <input
                className="q-input"
                inputMode="text"
                autoCapitalize="characters"
                autoComplete="off"
                autoCorrect="off"
                spellCheck={false}
                placeholder={CODE_SHAPE}
                aria-label={t("empty.codeLabel")}
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
          {machineName ? t("credentials", { host: machineName }) : t("credentialsUnnamed")}
        </p>
        {error || expired ? (
          <p className="q-pair-note q-danger">{expired ? t("errors.expired") : error}</p>
        ) : null}
      </div>
      <div className="q-pair-foot">
        <div className="q-faint num">
          {pending
            ? expired
              ? t("foot.linkExpired")
              : t("foot.expiresIn", { time: formatRemaining(pending.payload.expiresAt - now) })
            : t("foot.codesOnce", { count: CODE_TTL_MINUTES })}
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
            ? t("actions.openThreads")
            : claiming || busy
              ? t("actions.pairing")
              : pending
                ? t("actions.pairPhone")
                : t("actions.pairCode")}
        </button>
      </div>
    </main>
  );
}
