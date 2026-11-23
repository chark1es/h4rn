/**
 * Settings → Sharing & access.
 *
 * Two groups: outside agents that signed in through OAuth, and the switches
 * that decide which harnesses get h4rn's own tools. The daemon owns the state;
 * this page is the view and the two actions people need (copy the URL, revoke).
 */
import { CheckIcon, CopyIcon, KeyRoundIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import type { HarnessKind, OutsideAgentView } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { absoluteDateTime, relativeAge } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import {
  levelDescription,
  levelLabel,
  loadAccess,
  revokeAccessAgent,
  setAgentToolsEnabled,
  useAccess,
} from "./store";

const TOOL_HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity", "acp"];

/** A copy button that confirms in place instead of opening a toast. */
function CopyButton({ value, label }: { value: string; label: string }) {
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
      {copied ? "Copied" : "Copy MCP URL"}
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
          Connected {relativeAge(agent.connected_at)} ago
          {agent.last_used_at ? ` · Last used ${relativeAge(agent.last_used_at)} ago` : ""}
          {" · Sign-in until "}
          <span title={absoluteDateTime(agent.expires_at)}>
            {absoluteDateTime(agent.expires_at)}
          </span>
        </div>
      </div>
      <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
        <button
          type="button"
          className="q-btn secondary"
          disabled={busy}
          onClick={onRevoke}
          aria-label={`Revoke ${agent.name}`}
        >
          Revoke
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

export function AccessSettings() {
  const online = useApp((state) => state.connection.status) === "online";
  const agents = useAccess((state) => state.agents);
  const mcpUrl = useAccess((state) => state.mcpUrl);
  const settings = useAccess((state) => state.settings);
  const loading = useAccess((state) => state.loading);
  const error = useAccess((state) => state.error);
  const pending = useAccess((state) => state.pending);
  const [revoking, setRevoking] = useState<OutsideAgentView | null>(null);

  useEffect(() => {
    if (!online) return;
    void loadAccess();
  }, [online]);

  if (!online) {
    return (
      <div className="q-form">
        <h1>Sharing &amp; access</h1>
        <p className="lead">
          Outside agents connect to this machine. Reconnect to see who is connected and copy the MCP
          URL.
        </p>
      </div>
    );
  }

  return (
    <div className="q-form">
      <h1>Sharing &amp; access</h1>
      <p className="lead">
        Let an agent you did not start work with h4rn, and decide what h4rn's own tools may do
        inside the agents h4rn runs.
      </p>

      <div className="q-group">
        <h2>Outside agents</h2>
        <p className="q-faint" style={{ marginTop: -4, marginBottom: 12 }}>
          Add this URL to Claude Code, Codex or another MCP client that supports OAuth. The client
          signs in once; you choose its level on the approval page.
        </p>
        <div className="q-field">
          <div className="lab" style={{ flex: 1, minWidth: 0 }}>
            <div className="t">MCP URL</div>
            <div className="s">
              <span className="q-mono" style={{ wordBreak: "break-all" }}>
                {mcpUrl || "Waiting for the daemon to bind its listener"}
              </span>
            </div>
          </div>
          <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
            <CopyButton value={mcpUrl} label="Copy the outside-agent MCP URL" />
          </div>
        </div>

        {loading ? (
          <p className="q-faint" style={{ marginTop: 12 }}>
            Reading connected agents…
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
              <div className="t">No outside agent is connected</div>
              <div className="s">
                Nothing can read or start threads from outside h4rn until you approve a sign-in.
              </div>
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

      <div className="q-group">
        <h2>h4rn tools in your agents</h2>
        <p className="q-faint" style={{ marginTop: -4, marginBottom: 12 }}>
          Agents h4rn runs can link pull requests to their thread and read its state. Turn the tools
          off per harness if you would rather they did not.
        </p>
        {TOOL_HARNESSES.map((harness) => {
          const enabled = harnessEnabled(settings?.enabled ?? null, harness);
          const busy = Boolean(pending[`harness:${harness}`]);
          return (
            <div className="q-field" key={harness}>
              <div className="lab" style={{ flex: 1 }}>
                <div className="t">{HARNESS_LABELS[harness]}</div>
                <div className="s">
                  {enabled
                    ? "Link pull requests, watch them and read the thread's status."
                    : "No h4rn tools are offered to these sessions."}
                </div>
              </div>
              <div className="ctl" style={{ flex: "0 1 auto", justifyContent: "flex-end" }}>
                <button
                  type="button"
                  className={cn("q-switch", busy && "busy")}
                  role="switch"
                  aria-checked={enabled}
                  aria-label={`h4rn tools in ${HARNESS_LABELS[harness]}`}
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
        title={revoking ? `Revoke ${revoking.name}?` : "Revoke this agent?"}
        description="Its access token and refresh token stop working immediately. It must ask to sign in again to reconnect."
        confirmLabel="Revoke"
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
