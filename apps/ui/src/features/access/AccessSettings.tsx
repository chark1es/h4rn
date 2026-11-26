/**
 * Settings → Sharing & access.
 *
 * Two groups: outside agents that signed in through OAuth, and the switches
 * that decide which harnesses get h4rn's own tools. The daemon owns the state;
 * this page is the view and the two actions people need (copy the URL, revoke).
 */
import { CheckIcon, CopyIcon, KeyRoundIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import { readActiveRemote } from "@/features/remote/active";
import type { HarnessKind, OutsideAgentLevel, OutsideAgentSignIn, OutsideAgentView } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { absoluteDateTime } from "@/lib/format";
import { formatRelativeTime } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import {
  levelDescription,
  levelLabel,
  loadAccess,
  respondSignIn,
  revokeAccessAgent,
  setAgentToolsEnabled,
  useAccess,
} from "./store";

const TOOL_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity", "acp"];

const LEVELS: OutsideAgentLevel[] = ["read_only", "supervised", "full_access"];

/** A copy button that confirms in place instead of opening a toast. */
function CopyButton({ value, label }: { value: string; label: string }) {
  const { t } = useTranslation(["settings", "common"]);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 1600);
    return () => window.clearTimeout(timer);
  }, [copied]);
  const disabled = !value || !navigator.clipboard;
  return (
    <button
      type="button"
      className="q-btn secondary"
      disabled={disabled}
      aria-label={label}
      onClick={() => {
        if (disabled) return;
        void navigator.clipboard.writeText(value).then(() => setCopied(true));
      }}
    >
      {copied ? (
        <CheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      ) : (
        <CopyIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      )}
      {copied ? t("common:copied") : t("access.copyMcpUrl")}
    </button>
  );
}

function AgentRow({
  agent,
  busy,
  onRevoke,
}: {
  agent: OutsideAgentView;
  busy: boolean;
  onRevoke: () => void;
}) {
  const { t } = useTranslation(["settings", "common"]);
  return (
    <div className="q-field">
      <div className="lab" style={{ width: "auto", flex: 1, minWidth: 0 }}>
        <div className="t" style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
          {agent.name}
        </div>
        <div className="s">
          {levelLabel(agent.level)} · {levelDescription(agent.level)}
        </div>
        <div className="q-faint" style={{ fontSize: 12 }}>
          {t("access.connected", { when: formatRelativeTime(agent.connected_at) })}
          {agent.last_used_at
            ? ` · ${t("access.lastUsed", { when: formatRelativeTime(agent.last_used_at) })}`
            : ""}
          {" · "}
          <span title={absoluteDateTime(agent.expires_at)}>
            {t("access.signInUntil", { when: absoluteDateTime(agent.expires_at) })}
          </span>
        </div>
      </div>
      <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
        <button
          type="button"
          className="q-btn secondary"
          disabled={busy}
          onClick={onRevoke}
          aria-label={t("access.revokeLabel", { name: agent.name })}
        >
          {t("common:revoke")}
        </button>
      </div>
    </div>
  );
}

function harnessEnabled(
  settings: { by_harness: Array<{ harness: HarnessKind; enabled: boolean }> } | null,
  harness: HarnessKind,
) {
  if (!settings) return true;
  return settings.by_harness.find((entry) => entry.harness === harness)?.enabled ?? true;
}

/**
 * One sign-in waiting for the person.
 *
 * The client's browser cannot grant anything; this card is the grant. The name
 * is the client's own choice, so it is labelled as unverified, and no level is
 * preselected: approving needs an explicit choice.
 */
function SignInCard({ signIn }: { signIn: OutsideAgentSignIn }) {
  const { t } = useTranslation(["settings", "common"]);
  const [level, setLevel] = useState<OutsideAgentLevel | null>(null);
  const busy = useAccess((state) => Boolean(state.pending[signIn.request_id]));
  return (
    <div className="q-field" style={{ flexDirection: "column", alignItems: "stretch", gap: 10 }}>
      <div className="lab" style={{ width: "auto" }}>
        <div className="t">{t("access.wantsToConnect", { name: signIn.client_name })}</div>
        <div className="s">
          <Trans
            t={t}
            i18nKey="access.unverifiedName"
            values={{ origin: signIn.redirect_origin }}
            components={{ code: <span className="q-mono" /> }}
          />
        </div>
      </div>
      <div role="radiogroup" aria-label={t("access.mayDo", { name: signIn.client_name })}>
        {LEVELS.map((entry) => (
          <label key={entry} className="q-resolve-opt">
            <input
              type="radio"
              name={`outside-level-${signIn.request_id}`}
              className="sr-only"
              checked={level === entry}
              onChange={() => setLevel(entry)}
            />
            <span className={cn("q-radio", level === entry && "on")} aria-hidden />
            <span>
              <span style={{ fontWeight: 500 }}>{levelLabel(entry)}</span>
              <span className="q-faint sub">{levelDescription(entry)}</span>
            </span>
          </label>
        ))}
      </div>
      <div style={{ display: "flex", justifyContent: "flex-end", gap: 6 }}>
        <button
          type="button"
          className="q-btn ghost"
          disabled={busy}
          onClick={() => void respondSignIn(signIn, "deny", null)}
        >
          {t("access.deny")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={busy || !level}
          onClick={() => void respondSignIn(signIn, "approve", level)}
        >
          {t("access.approve")}
        </button>
      </div>
    </div>
  );
}

export function AccessSettings() {
  const { t } = useTranslation(["settings", "common"]);
  const online = useApp((state) => state.connection.status) === "online";
  const agents = useAccess((state) => state.agents);
  const signIns = useAccess((state) => state.signIns);
  const mcpUrl = useAccess((state) => state.mcpUrl);
  const settings = useAccess((state) => state.settings);
  const outsideEnabled = useAccess((state) => state.outsideEnabled);
  const loading = useAccess((state) => state.loading);
  const error = useAccess((state) => state.error);
  const pending = useAccess((state) => state.pending);
  const [revoking, setRevoking] = useState<OutsideAgentView | null>(null);
  // A client's page says "approve this in h4rn"; this page polls so the request
  // is already here by the time the person looks.
  const [remote] = useState(() => readActiveRemote());

  useEffect(() => {
    if (!online) return;
    void loadAccess();
    const timer = window.setInterval(() => void loadAccess(true), 3000);
    return () => window.clearInterval(timer);
  }, [online]);

  if (!online) {
    return (
      <div className="q-form">
        <h1>{t("access.title")}</h1>
        <p className="lead">{t("access.offlineLead")}</p>
      </div>
    );
  }

  return (
    <div className="q-form">
      <h1>{t("access.title")}</h1>
      <p className="lead">{t("access.lead")}</p>

      {outsideEnabled === true ? (
        <div className="q-group">
          <h2>{t("access.outsideAgents")}</h2>
          <p className="q-faint" style={{ marginTop: -4, marginBottom: 12 }}>
            {t("access.outsideAgentsHint")}
          </p>
          <div className="q-field">
            <div className="lab" style={{ flex: 1, minWidth: 0 }}>
              <div className="t">{t("access.mcpUrl")}</div>
              <div className="s">
                <span className="q-mono" style={{ wordBreak: "break-all" }}>
                  {mcpUrl || t("access.needsLoopback")}
                </span>
              </div>
            </div>
            <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
              {remote ? null : <CopyButton value={mcpUrl} label={t("access.copyMcpUrlLabel")} />}
            </div>
          </div>
          {remote ? (
            <p className="q-faint" style={{ marginTop: 8 }}>
              {t("access.remoteNote", { machine: remote.machineName })}
            </p>
          ) : null}

          {signIns.length > 0 ? (
            <div style={{ marginTop: 16 }}>
              <h3 style={{ margin: "0 0 8px", fontSize: 13, fontWeight: 600 }}>
                {t("access.waiting")}
              </h3>
              {signIns.map((signIn) => (
                <SignInCard key={signIn.request_id} signIn={signIn} />
              ))}
            </div>
          ) : null}

          {loading ? (
            <p className="q-faint" style={{ marginTop: 12 }}>
              {t("access.readingAgents")}
            </p>
          ) : error ? (
            <p className="q-faint" style={{ marginTop: 12 }}>
              {error.message}
              {error.hint ? ` ${error.hint}` : ""}
            </p>
          ) : agents.length === 0 ? (
            <div className="q-empty" style={{ marginTop: 12 }}>
              <KeyRoundIcon className="q-icon" strokeWidth={1.5} aria-hidden />
              <div>
                <div className="t">{t("access.noneConnected")}</div>
                <div className="s">{t("access.noneConnectedHint")}</div>
              </div>
            </div>
          ) : (
            agents.map((agent) => (
              <AgentRow
                key={agent.client_id}
                agent={agent}
                busy={Boolean(pending[agent.client_id])}
                onRevoke={() => setRevoking(agent)}
              />
            ))
          )}
        </div>
      ) : outsideEnabled === false ? (
        <p className="q-faint">{t("access.inLabs")}</p>
      ) : (
        <p className="q-faint">{t("access.readingSetting")}</p>
      )}

      <div className="q-group">
        <h2>{t("access.tools")}</h2>
        <p className="q-faint" style={{ marginTop: -4, marginBottom: 12 }}>
          {t("access.toolsHint")}
        </p>
        {TOOL_HARNESSES.map((harness) => {
          const enabled = harnessEnabled(settings?.enabled ?? null, harness);
          const busy = Boolean(pending[`harness:${harness}`]);
          return (
            <div className="q-field" key={harness}>
              <div className="lab" style={{ flex: 1 }}>
                <div className="t">{HARNESS_LABELS[harness]}</div>
                <div className="s">{enabled ? t("access.toolsOn") : t("access.toolsOff")}</div>
              </div>
              <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
                <button
                  type="button"
                  className={cn("q-switch", busy && "busy")}
                  role="switch"
                  aria-checked={enabled}
                  aria-label={t("access.toolsIn", { harness: HARNESS_LABELS[harness] })}
                  aria-disabled={busy || undefined}
                  disabled={busy}
                  onClick={() => void setAgentToolsEnabled(harness, !enabled)}
                />
              </div>
            </div>
          );
        })}
      </div>

      <ConfirmDialog
        open={Boolean(revoking)}
        title={revoking ? t("access.revokeTitle", { name: revoking.name }) : t("access.revokeThis")}
        description={t("access.revokeDescription")}
        confirmLabel={t("common:revoke")}
        onOpenChange={(open) => {
          if (!open) setRevoking(null);
        }}
        onConfirm={() => {
          if (!revoking) return;
          void revokeAccessAgent(revoking).finally(() => setRevoking(null));
        }}
      />
    </div>
  );
}
