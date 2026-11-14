import {
  CheckIcon,
  CircleCheckIcon,
  CircleDotIcon,
  KeyRoundIcon,
  PlusIcon,
  RefreshCwIcon,
  TriangleAlertIcon,
} from "lucide-react";
import { Fragment, useEffect, useMemo, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import type {
  AgentState,
  HarnessKind,
  ProxyCapabilities,
  ProxyDiagnostic,
  ProxyModel,
  ProxyProfile,
  ProxyProfileId,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import {
  addProxyProfile,
  applyProxyOverrides,
  loadProxyProfiles,
  proxyDefaultModel,
  selectProxyProfile,
  setHarnessOverride,
  testProxyProfile,
  updateProxyProfile,
  useProxy,
  useProxyCatalog,
} from "./proxyStore";

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

function buildLabel(capabilities: ProxyCapabilities | null): string {
  if (!capabilities) return "Not tested";
  const fork =
    capabilities.performance ||
    capabilities.fork_model_enrichment ||
    capabilities.usage_queue ||
    capabilities.cooldowns;
  if (fork) return "Fork build";
  if (capabilities.credentials_verify || capabilities.legacy_usage) return "Upstream build";
  return "Build not identified";
}

type ProbeRow = { label: string; value: string; ok: boolean | "neutral"; hint?: string };

function managementValue(capabilities: ProxyCapabilities): string {
  const state =
    capabilities.management === "available"
      ? "Enabled"
      : capabilities.management === "open"
        ? "Open, no key required"
        : capabilities.management === "needs_key"
          ? "Key required"
          : capabilities.management === "disabled"
            ? "Disabled"
            : "Unknown";
  return capabilities.version ? `${state} · ${capabilities.version}` : state;
}

/** The four lines a person needs from a probe. */
function summaryRows(capabilities: ProxyCapabilities | null): ProbeRow[] {
  if (!capabilities) return [];
  const shapes = [
    capabilities.claude_models ? "Claude" : null,
    capabilities.openai_models ? "OpenAI" : null,
    capabilities.gemini_models ? "Gemini" : null,
  ].filter(Boolean);
  const usageKnown = capabilities.usage_queue || capabilities.legacy_usage;
  return [
    {
      label: "Health",
      value: capabilities.reachable ? "OK" : "Unreachable",
      ok: capabilities.reachable,
    },
    {
      label: "Management",
      value: managementValue(capabilities),
      ok: capabilities.management === "available" || capabilities.management === "open",
      ...(capabilities.management === "needs_key"
        ? { hint: "Add the management key so h4rn can read models and credentials." }
        : capabilities.management === "disabled"
          ? { hint: "Enable the management API in CLIProxyAPI's config to read models." }
          : {}),
    },
    {
      label: "Model shape",
      value: shapes.length > 0 ? shapes.join(", ") : "None detected",
      ok: shapes.length > 0,
    },
    {
      label: "Usage history",
      value: usageKnown ? "Read from the proxy" : "Not available · h4rn counts usage itself",
      ok: usageKnown ? true : "neutral",
    },
  ];
}

const yesNo = (value: boolean, yes: string, no: string): ProbeRow["value"] => (value ? yes : no);

/** Everything else the probe learned, behind a disclosure. */
function detailRows(capabilities: ProxyCapabilities | null): ProbeRow[] {
  if (!capabilities) return [];
  const row = (label: string, value: string, ok: boolean): ProbeRow => ({ label, value, ok });
  return [
    row("Build", buildLabel(capabilities), true),
    ...(capabilities.commit ? [row("Commit", capabilities.commit, true)] : []),
    row(
      "Usage queue",
      yesNo(capabilities.usage_queue, "Supported", "Not available"),
      capabilities.usage_queue,
    ),
    row(
      "Legacy usage",
      yesNo(capabilities.legacy_usage, "Supported (≤ 6.9)", "Not available"),
      capabilities.legacy_usage,
    ),
    row(
      "Config v8",
      yesNo(capabilities.v8_config, "Supported", "Not available"),
      capabilities.v8_config,
    ),
    row(
      "Performance",
      yesNo(capabilities.performance, "Route present", "Not available"),
      capabilities.performance,
    ),
    row(
      "Credentials verify",
      yesNo(capabilities.credentials_verify, "Route present", "Not available"),
      capabilities.credentials_verify,
    ),
    row(
      "Cooldowns",
      yesNo(capabilities.cooldowns, "Reported", "Not reported"),
      capabilities.cooldowns,
    ),
    row(
      "Quota providers",
      yesNo(capabilities.quota_providers, "Reported", "Not reported"),
      capabilities.quota_providers,
    ),
  ];
}

function useHostName(): string {
  return useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? "this host") : "this host",
  );
}

/** "480" → "8 min": a cooldown a person can plan around. */
function cooldownText(seconds: bigint | number | null | undefined): string {
  if (seconds == null) return "Cooling down";
  const value = Number(seconds);
  if (value < 60) return `Cooling down · ${Math.max(1, Math.round(value))} s`;
  if (value < 3_600) return `Cooling down · ${Math.round(value / 60)} min`;
  return `Cooling down · ${Math.round(value / 3_600)} h`;
}

function Diagnostics({ diagnostics }: { diagnostics: ProxyDiagnostic[] }) {
  if (diagnostics.length === 0) {
    return (
      <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
        No trouble found. h4rn can read models and usage from this profile.
      </p>
    );
  }
  return (
    <div className="q-list">
      {diagnostics.map((diagnostic) => (
        <div
          key={`${diagnostic.code}:${diagnostic.message}`}
          className="q-li"
          style={{ display: "block" }}
        >
          <div style={{ display: "flex", gap: 6, alignItems: "flex-start" }}>
            {diagnostic.level === "error" ? (
              <TriangleAlertIcon className="q-icon s12 q-danger" strokeWidth={1.5} aria-hidden />
            ) : null}
            <span style={{ fontWeight: 500 }}>{diagnostic.message}</span>
          </div>
          {diagnostic.hint ? (
            <div className="q-faint" style={{ fontSize: 12, marginTop: 3 }}>
              {diagnostic.hint}
            </div>
          ) : null}
        </div>
      ))}
    </div>
  );
}

function ModelTable({ models }: { models: ProxyModel[] }) {
  if (models.length === 0) {
    return (
      <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
        No models read yet. Test the profile, or open its management API.
      </p>
    );
  }
  return (
    <table className="q-tbl">
      <thead>
        <tr>
          <th>Model</th>
          <th>Maps to</th>
          <th className="r">Context</th>
        </tr>
      </thead>
      <tbody>
        {models.map((model) => (
          <tr key={`${model.id}:${model.source}`} className="hov">
            <td>
              <span className="q-mono">{model.id}</span>
              {model.alias_of ? (
                <span className="q-badge" style={{ height: 18, marginLeft: 6 }}>
                  Custom
                </span>
              ) : null}
            </td>
            <td className="q-faint">
              {model.alias_of ? (
                <>
                  <span className="q-mono">{model.alias_of}</span> · keeps original id
                </>
              ) : (
                "—"
              )}
            </td>
            <td className="r num">
              {model.context_length != null ? formatContext(Number(model.context_length)) : "—"}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function formatContext(tokens: number): string {
  return tokens >= 1_000_000
    ? `${+(tokens / 1_000_000).toFixed(1)}M`
    : tokens >= 1_000
      ? `${Math.round(tokens / 1_000)}k`
      : String(tokens);
}

function AddProfile() {
  const host = useHostName();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("CLIProxyAPI");
  const [baseUrl, setBaseUrl] = useState("http://127.0.0.1:8317");
  const [managementKey, setManagementKey] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [isDefault, setIsDefault] = useState(false);
  const [saving, setSaving] = useState(false);

  if (!open) {
    return (
      <button type="button" className="q-btn secondary" onClick={() => setOpen(true)}>
        <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden /> Add profile
      </button>
    );
  }

  return (
    <div className="q-group">
      <h2>Add a profile</h2>
      <p className="desc">
        Keys are written to the secret store on {host}. This app only learns whether one exists.
      </p>
      <div className="q-field">
        <div className="lab">
          <div className="t">Label</div>
        </div>
        <div className="ctl">
          <input
            className="q-input"
            value={name}
            onChange={(event) => setName(event.target.value)}
            aria-label="Profile label"
            style={{ maxWidth: 260 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Base URL</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            aria-label="Proxy base URL"
            style={{ maxWidth: 300, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Management key</div>
          <div className="s">Write-only</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={managementKey}
            onChange={(event) => setManagementKey(event.target.value)}
            placeholder={`Stored on ${host}`}
            aria-label="Management key"
            style={{ maxWidth: 300, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Access key</div>
          <div className="s">What harnesses send</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
            placeholder={`Stored on ${host}`}
            aria-label="Access key"
            style={{ maxWidth: 300, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Default</div>
        </div>
        <div className="ctl">
          <button
            type="button"
            className={cn("q-check", isDefault && "on")}
            role="switch"
            aria-checked={isDefault}
            aria-label="Make this the default profile"
            onClick={() => setIsDefault((value) => !value)}
          >
            {isDefault ? <CheckIcon className="q-icon s12" strokeWidth={2} aria-hidden /> : null}
          </button>
        </div>
      </div>
      <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
        <button type="button" className="q-btn ghost" onClick={() => setOpen(false)}>
          Cancel
        </button>
        <button
          type="button"
          className="q-btn primary"
          disabled={saving || !name.trim() || !baseUrl.trim()}
          onClick={() => {
            setSaving(true);
            void addProxyProfile({
              name: name.trim(),
              base_url: baseUrl.trim(),
              management_key: managementKey.trim() || null,
              api_key: apiKey.trim() || null,
              default: isDefault,
            }).then((profile) => {
              setSaving(false);
              if (profile) setOpen(false);
            });
          }}
        >
          {saving ? "Saving…" : "Save profile"}
        </button>
      </div>
    </div>
  );
}

function ProfileField({
  label,
  value,
  onSave,
}: {
  label: string;
  value: string;
  onSave: (next: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  return (
    <div className="q-field">
      <div className="lab">
        <div className="t">{label}</div>
      </div>
      <div className="ctl">
        <input
          className="q-input q-mono"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          aria-label={label}
          style={{ maxWidth: 300, fontSize: 12 }}
        />
        <button
          type="button"
          className="q-btn secondary"
          disabled={draft === value}
          onClick={() => onSave(draft.trim())}
        >
          Save
        </button>
      </div>
    </div>
  );
}

function SecretField({
  label,
  sub,
  hasKey,
  onReplace,
  onClear,
}: {
  label: string;
  sub?: string;
  hasKey: boolean;
  onReplace: (value: string) => void;
  onClear: () => void;
}) {
  const host = useHostName();
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  return (
    <div className="q-field">
      <div className="lab">
        <div className="t">{label}</div>
        {sub ? <div className="s">{sub}</div> : null}
      </div>
      <div className="ctl">
        {editing ? (
          <>
            <input
              className="q-input q-mono"
              type="password"
              value={value}
              onChange={(event) => setValue(event.target.value)}
              placeholder="New key"
              aria-label={`New ${label}`}
              style={{ maxWidth: 240, fontSize: 12 }}
            />
            <button
              type="button"
              className="q-btn primary"
              disabled={!value.trim()}
              onClick={() => {
                onReplace(value.trim());
                setValue("");
                setEditing(false);
              }}
            >
              Save
            </button>
            <button type="button" className="q-btn ghost" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </>
        ) : (
          <>
            <span className="q-secret">
              <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              {hasKey ? `Stored on ${host}` : "No key stored"}
            </span>
            <span style={{ flex: 1 }} />
            <button type="button" className="q-btn ghost" onClick={() => setEditing(true)}>
              Replace
            </button>
            {hasKey ? (
              <button type="button" className="q-btn ghost" onClick={onClear}>
                Remove
              </button>
            ) : null}
          </>
        )}
      </div>
    </div>
  );
}

function HarnessRow({ profile, harness }: { profile: ProxyProfile; harness: HarnessKind }) {
  const host = useHostName();
  const override = profile.harnesses.find((candidate) => candidate.harness === harness);
  const enabled = override?.enabled ?? false;
  const catalog = useProxy((state) => state.catalogs[profile.id])?.models ?? [];
  const saving = useProxy((state) => state.overridesSaving[`${profile.id}:${harness}`] ?? false);
  const defaultModel = override?.default_model ?? proxyDefaultModel(harness) ?? "";
  return (
    <>
      <div className="q-field q-harness">
        <div className="lab">
          <div className="t">{HARNESS_LABELS[harness]}</div>
          <div className="s">{enabled ? "Routed" : "Own credential"}</div>
        </div>
        <div className="ctl">
          <button
            type="button"
            className="q-switch"
            role="switch"
            aria-checked={enabled}
            aria-label={`Route ${HARNESS_LABELS[harness]} through CLIProxyAPI`}
            disabled={saving}
            onClick={() =>
              void setHarnessOverride(
                profile.id,
                harness,
                !enabled,
                !enabled ? (override?.default_model ?? catalog[0]?.id ?? null) : null,
              )
            }
          />
          <ChipSelect
            label={`Default model for ${HARNESS_LABELS[harness]}`}
            width={190}
            disabled={!enabled}
            value={defaultModel}
            options={[
              { value: "", label: "Profile default" },
              ...catalog.map((model) => ({ value: model.id, label: model.id })),
            ]}
            onChange={(value) => void setHarnessOverride(profile.id, harness, true, value || null)}
          />
          <span className="q-mono q-faint q-cfgpath" title={override?.config_path}>
            {override?.config_path}
          </span>
        </div>
      </div>
      {enabled && override && override.preview.length > 0 ? (
        <section className="q-envprev" aria-label={`${HARNESS_LABELS[harness]} settings preview`}>
          {override.preview.map((line) => (
            <div key={line.name} className="ln">
              <span className="sg">+</span>
              <span className="q-mono">
                {line.name}:{" "}
                {line.value === null ? (
                  <span className="q-faint">stored on {host}</span>
                ) : (
                  line.value
                )}
              </span>
            </div>
          ))}
        </section>
      ) : null}
    </>
  );
}

const RUNNING_STATES: ReadonlySet<AgentState> = new Set([
  "thinking",
  "searching",
  "reading",
  "editing",
  "running_tool",
  "awaiting_approval",
]);

/** The footer of the override group: what applying will interrupt, and the button. */
function ApplyBar({ profile }: { profile: ProxyProfile }) {
  const host = useHostName();
  const dirty = useProxy((state) => state.overridesDirty[profile.id] ?? false);
  const applying = useProxy((state) => state.applying[profile.id] ?? false);
  const applied = useProxy((state) => state.applied[profile.id]);
  const threads = useApp((state) => state.threads);
  const localHostId = useApp((state) => state.localHostId);
  const routed = new Set(profile.harnesses.filter((row) => row.enabled).map((row) => row.harness));
  const live = Object.values(threads).filter(
    (thread) =>
      thread.host === localHostId &&
      routed.has(thread.harness) &&
      RUNNING_STATES.has(thread.agent_state),
  );
  const note =
    live.length === 1 ? (
      <>
        Interrupts <em>{live[0]?.title}</em> on {host}.
      </>
    ) : live.length > 1 ? (
      `Interrupts ${live.length} running turns on ${host}.`
    ) : applied && !dirty ? (
      applied.restarted > 0 ? (
        `Restarted ${applied.restarted} ${applied.restarted === 1 ? "session" : "sessions"}.`
      ) : (
        "Applied. Sessions you start next use it."
      )
    ) : (
      "Sessions already running keep their old routing until restarted."
    );
  return (
    <div className="q-applybar">
      <span className="q-faint note" role="status">
        {note}
      </span>
      <button
        type="button"
        className="q-btn primary"
        disabled={!dirty || applying}
        onClick={() => void applyProxyOverrides(profile.id)}
      >
        {applying ? "Restarting…" : "Apply and restart"}
      </button>
    </div>
  );
}

function ProfileDetail({ profile }: { profile: ProxyProfile }) {
  const report = useProxy((state) => state.reports[profile.id]);
  const testing = useProxy((state) => state.testing[profile.id] ?? false);
  const catalog = useProxy((state) => state.catalogs[profile.id]);
  const limits = useProxy((state) => state.limits[profile.id]);
  const catalogLoading = useProxy((state) => state.catalogLoading);
  const host = useHostName();
  const caps = report?.capabilities ?? profile.capabilities;
  const rows = summaryRows(caps);
  const details = detailRows(caps);
  const models = catalog?.models ?? [];

  return (
    <>
      <div className="q-group">
        <h2>
          Connection
          <span className="grow" />
          <span className="q-faint" style={{ fontSize: 12, fontWeight: 400 }}>
            {buildLabel(caps)}
          </span>
        </h2>
        <ProfileField
          label="Base URL"
          value={profile.base_url}
          onSave={(next) =>
            void updateProxyProfile(profile.id, {
              name: null,
              base_url: next,
              management_key: null,
              clear_management_key: false,
              api_key: null,
              clear_api_key: false,
              default: null,
            })
          }
        />
        <SecretField
          label="Management key"
          hasKey={profile.has_management_key}
          onReplace={(value) =>
            void updateProxyProfile(profile.id, {
              name: null,
              base_url: null,
              management_key: value,
              clear_management_key: false,
              api_key: null,
              clear_api_key: false,
              default: null,
            })
          }
          onClear={() =>
            void updateProxyProfile(profile.id, {
              name: null,
              base_url: null,
              management_key: null,
              clear_management_key: true,
              api_key: null,
              clear_api_key: false,
              default: null,
            })
          }
        />
        <SecretField
          label="Access key"
          sub="What harnesses send"
          hasKey={profile.has_api_key}
          onReplace={(value) =>
            void updateProxyProfile(profile.id, {
              name: null,
              base_url: null,
              management_key: null,
              clear_management_key: false,
              api_key: value,
              clear_api_key: false,
              default: null,
            })
          }
          onClear={() =>
            void updateProxyProfile(profile.id, {
              name: null,
              base_url: null,
              management_key: null,
              clear_management_key: false,
              api_key: null,
              clear_api_key: true,
              default: null,
            })
          }
        />
        <div className="q-field" style={{ alignItems: "flex-start" }}>
          <div className="lab">
            <div className="t">Probe</div>
            <div className="s">
              {caps ? `${relativeAge(caps.detected_at)} from ${host}` : "not probed yet"}
            </div>
          </div>
          <div className="ctl" style={{ display: "block" }}>
            <div style={{ display: "flex", gap: 8, marginBottom: rows.length ? 12 : 0 }}>
              <button
                type="button"
                className="q-btn secondary"
                disabled={testing}
                onClick={() => void testProxyProfile(profile.id)}
              >
                <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                {testing ? "Probing…" : "Probe"}
              </button>
              {report ? (
                <span className="q-faint num" style={{ alignSelf: "center", fontSize: 12 }}>
                  {Number(report.latency_ms)} ms round trip
                </span>
              ) : null}
            </div>
            {rows.length > 0 ? (
              <div className="q-probe" style={{ paddingTop: 2 }}>
                {rows.map((row) => (
                  <Fragment key={row.label}>
                    <span
                      className={row.ok === true ? "q-ok" : row.ok === false ? "q-warn" : "q-faint"}
                    >
                      {row.ok === true ? (
                        <CircleCheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                      ) : row.ok === false ? (
                        <TriangleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                      ) : (
                        <CircleDotIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                      )}
                    </span>
                    <div className="k">{row.label}</div>
                    <div className={cn("v", row.ok === false && "q-faint")} title={row.hint}>
                      {row.value}
                      {row.ok === false && row.hint ? (
                        <span className="q-faint"> · {row.hint}</span>
                      ) : null}
                    </div>
                  </Fragment>
                ))}
              </div>
            ) : null}
            {details.length > 0 || report ? (
              <details className="q-details">
                <summary>Details</summary>
                {details.length > 0 ? (
                  <div className="q-probe">
                    {details.map((row) => (
                      <Fragment key={row.label}>
                        <span className={row.ok ? "q-ok" : "q-faint"}>
                          {row.ok ? (
                            <CheckIcon className="q-icon s12" strokeWidth={2} aria-hidden />
                          ) : null}
                        </span>
                        <div className="k">{row.label}</div>
                        <div className={cn("v", !row.ok && "q-faint")}>{row.value}</div>
                      </Fragment>
                    ))}
                  </div>
                ) : null}
                {report ? (
                  <div style={{ marginTop: 12 }}>
                    <Diagnostics diagnostics={report.diagnostics} />
                  </div>
                ) : null}
              </details>
            ) : null}
          </div>
        </div>
      </div>

      <div className="q-group">
        <h2>
          Models
          <span className="grow" />
          <span className="q-faint" style={{ fontSize: 12, fontWeight: 400 }}>
            {catalogLoading
              ? "Reading…"
              : `${models.length} from ${catalog?.aliases_merged ? "responses + aliases" : "responses"}`}
          </span>
        </h2>
        <ModelTable models={models} />
      </div>

      <div className="q-group">
        <h2>Per-harness override</h2>
        <p className="desc">
          Off means the harness keeps its own credential. Nothing changes for a harness you don't
          turn on.
        </p>
        {HARNESSES.map((harness) => (
          <HarnessRow key={harness} profile={profile} harness={harness} />
        ))}
        <ApplyBar profile={profile} />
      </div>

      <div className="q-group">
        <h2>Credential health</h2>
        {!limits ? (
          <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
            No health snapshot yet.
          </p>
        ) : !limits.available ? (
          <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
            {limits.note ?? "The management API could not be read."}
          </p>
        ) : limits.credentials.length === 0 ? (
          <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
            No credentials reported.
          </p>
        ) : (
          <table className="q-tbl">
            <thead>
              <tr>
                <th>Credential</th>
                <th>Provider</th>
                <th>Status</th>
                <th className="r">OK</th>
                <th className="r">Failed</th>
                <th>Cooldown</th>
              </tr>
            </thead>
            <tbody>
              {limits.credentials.map((credential) => {
                const cooldown = credential.cooldowns[0];
                return (
                  <tr
                    key={credential.auth_index ?? credential.name ?? "credential"}
                    className="hov"
                  >
                    <td className="q-mono">{credential.name ?? "—"}</td>
                    <td>{credential.provider ?? "—"}</td>
                    <td className={credential.disabled ? "q-danger" : undefined}>
                      {credential.disabled ? "Disabled" : (credential.status ?? "Ready")}
                    </td>
                    <td className="r num">
                      {credential.success != null ? Number(credential.success) : "—"}
                    </td>
                    <td className="r num">
                      {credential.failed != null ? Number(credential.failed) : "—"}
                    </td>
                    <td className="q-faint">
                      {cooldown ? cooldownText(cooldown.remaining_seconds) : "—"}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}

export function ProxySettings() {
  useProxyCatalog();
  const profiles = useProxy((state) => state.profiles);
  const selectedId = useProxy((state) => state.selectedId);
  const loading = useProxy((state) => state.loading);
  const error = useProxy((state) => state.error);
  const saveError = useProxy((state) => state.saveError);
  const connection = useApp((state) => state.connection.status);
  const host = useHostName();

  useEffect(() => {
    if (connection === "online") void loadProxyProfiles();
  }, [connection]);

  const selected = useMemo(
    () => profiles.find((profile) => profile.id === selectedId) ?? profiles[0] ?? null,
    [profiles, selectedId],
  );

  if (loading && profiles.length === 0) {
    return (
      <div className="q-form">
        <h1>CLIProxyAPI</h1>
        <div className="q-skel" style={{ height: 12, width: 260 }} />
        <div className="q-skel" style={{ height: 120, marginTop: 18 }} />
      </div>
    );
  }

  if (error && profiles.length === 0) {
    return (
      <div className="q-form">
        <h1>CLIProxyAPI</h1>
        <div className="q-empty">
          <p style={{ margin: 0 }}>{error.message}</p>
          <p className="q-faint" style={{ margin: "4px 0 12px" }}>
            {error.hint ?? "Start the daemon on a host that can reach the proxy."}
          </p>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void loadProxyProfiles()}
          >
            Try again
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="q-form">
      <div className="q-form-head">
        <div style={{ flex: 1 }}>
          <h1>
            {selected?.name ?? "CLIProxyAPI"}
            {selected && !selected.name.includes(host) ? ` on ${host}` : ""}
          </h1>
          <p className="lead">
            Routes harnesses on {host} through one endpoint with several credentials. Keys stay on{" "}
            {host}; this app only sees whether they exist.
          </p>
        </div>
        {profiles.length > 1 ? (
          <ChipSelect
            label="Profile"
            width={160}
            value={selected?.id ?? ""}
            options={profiles.map((profile) => ({
              value: profile.id,
              label: profile.name,
              hint: profile.base_url,
            }))}
            onChange={(id) => selectProxyProfile(id as ProxyProfileId)}
          />
        ) : null}
      </div>

      {profiles.length === 0 ? (
        <div className="q-group">
          <h2>No profile yet</h2>
          <p className="desc">
            Add the base URL of the CLIProxyAPI instance and the keys it expects. h4rn probes it
            before routing anything.
          </p>
          <AddProfile />
        </div>
      ) : null}

      {selected ? <ProfileDetail profile={selected} /> : null}
      {profiles.length > 0 ? (
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 12 }}>
          <AddProfile />
        </div>
      ) : null}

      {saveError ? (
        <p className="q-danger" style={{ fontSize: 12, marginTop: 12 }}>
          {saveError.message}
          {saveError.hint ? ` ${saveError.hint}` : ""}
        </p>
      ) : null}
    </div>
  );
}
