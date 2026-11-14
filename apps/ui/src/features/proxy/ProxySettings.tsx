import {
  CheckIcon,
  ChevronDownIcon,
  KeyRoundIcon,
  PlusIcon,
  RefreshCwIcon,
  TriangleAlertIcon,
} from "lucide-react";
import { Fragment, useEffect, useMemo, useState } from "react";

import type {
  HarnessKind,
  ProxyCapabilities,
  ProxyDiagnostic,
  ProxyModel,
  ProxyProfile,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import {
  addProxyProfile,
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

type ProbeRow = { label: string; value: string; ok: boolean; hint?: string };

function probe(
  label: string,
  value: string | null | undefined,
  ok: boolean,
  hint?: string,
): ProbeRow | null {
  if (value === null || value === undefined) return null;
  return { label, value, ok, ...(hint ? { hint } : {}) };
}

function capabilityRows(capabilities: ProxyCapabilities | null): ProbeRow[] {
  if (!capabilities) return [];
  return [
    probe("Health", capabilities.reachable ? "OK" : "Unreachable", capabilities.reachable),
    probe(
      "Management",
      capabilities.management === "available"
        ? "Available"
        : capabilities.management === "open"
          ? "Open, no key required"
          : capabilities.management === "needs_key"
            ? "Key required"
            : capabilities.management === "disabled"
              ? "Disabled"
              : "Unknown",
      capabilities.management === "available" || capabilities.management === "open",
      capabilities.management === "needs_key"
        ? "Add the management key so h4rn can read models and credentials."
        : capabilities.management === "disabled"
          ? "Enable the management API in CLIProxyAPI's config to read models."
          : undefined,
    ),
    probe("Version", capabilities.version, true),
    probe("Commit", capabilities.commit, true),
    probe(
      "Usage queue",
      capabilities.usage_queue ? "Supported" : "Not available",
      capabilities.usage_queue,
    ),
    probe(
      "Legacy usage",
      capabilities.legacy_usage ? "Supported (≤ 6.9)" : "Not available",
      capabilities.legacy_usage,
    ),
    probe(
      "v8 config",
      capabilities.v8_config ? "Supported" : "Not available",
      capabilities.v8_config,
    ),
    probe(
      "Performance",
      capabilities.performance ? "Fork-only route present" : "Not available",
      capabilities.performance,
    ),
    probe(
      "Credentials verify",
      capabilities.credentials_verify ? "Upstream route present" : "Not available",
      capabilities.credentials_verify,
    ),
    probe(
      "Cooldowns",
      capabilities.cooldowns ? "Reported" : "Not reported",
      capabilities.cooldowns,
    ),
    probe(
      "Quota providers",
      capabilities.quota_providers ? "Reported" : "Not reported",
      capabilities.quota_providers,
    ),
    probe(
      "Model shapes",
      [
        capabilities.openai_models ? "OpenAI" : null,
        capabilities.claude_models ? "Claude" : null,
        capabilities.gemini_models ? "Gemini" : null,
      ]
        .filter(Boolean)
        .join(", ") || "None",
      capabilities.openai_models || capabilities.claude_models || capabilities.gemini_models,
    ),
    probe(
      "Fork enrichment",
      capabilities.fork_model_enrichment ? "Present" : "Not present",
      capabilities.fork_model_enrichment,
    ),
  ].filter((row): row is ProbeRow => row !== null);
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
          <th>Family</th>
          <th>Provenance</th>
          <th className="r">Context</th>
          <th className="r">Max out</th>
        </tr>
      </thead>
      <tbody>
        {models.map((model) => (
          <tr key={`${model.id}:${model.source}`} className="hov">
            <td>
              <span className="q-mono">{model.id}</span>
              {model.alias_of ? (
                <span className="q-faint" style={{ marginLeft: 6, fontSize: 11.5 }}>
                  → {model.alias_of}
                </span>
              ) : null}
            </td>
            <td className="q-faint">{model.shape.replace("_", " ")}</td>
            <td>
              <span className="q-badge" style={{ height: 18 }}>
                {model.source.replace("_", " ")}
              </span>
              {model.fork_enriched ? (
                <span className="q-faint" style={{ marginLeft: 6, fontSize: 11.5 }}>
                  enriched
                </span>
              ) : null}
            </td>
            <td className="r num">
              {model.context_length != null
                ? `${Number(model.context_length).toLocaleString()}`
                : "—"}
            </td>
            <td className="r num">
              {model.max_output_tokens != null
                ? Number(model.max_output_tokens).toLocaleString()
                : "—"}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function AddProfile() {
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
        Keys are written to the daemon's secret store on the host. This app only learns whether one
        exists.
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
            placeholder="Stored on the daemon"
            aria-label="Management key"
            style={{ maxWidth: 300, fontSize: 12 }}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Client API key</div>
          <div className="s">What harnesses send</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
            placeholder="Stored on the daemon"
            aria-label="Client API key"
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
  hasKey,
  onReplace,
  onClear,
}: {
  label: string;
  hasKey: boolean;
  onReplace: (value: string) => void;
  onClear: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  return (
    <div className="q-field">
      <div className="lab">
        <div className="t">{label}</div>
        <div className="s">{hasKey ? "Stored on the daemon" : "Not set"}</div>
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
              {hasKey ? "Stored on the daemon" : "No key stored"}
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
  const override = profile.harnesses.find((candidate) => candidate.harness === harness);
  const enabled = override?.enabled ?? false;
  const catalog = useProxy((state) => state.catalogs[profile.id])?.models ?? [];
  const saving = useProxy((state) => state.overridesSaving[`${profile.id}:${harness}`] ?? false);
  const defaultModel = override?.default_model ?? proxyDefaultModel(harness) ?? "";
  return (
    <div className="q-field">
      <div className="lab">
        <div className="t">{HARNESS_LABELS[harness]}</div>
        <div className="s">{override?.note ?? (enabled ? "Routed" : "Own credential")}</div>
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
        <select
          className="q-input"
          value={defaultModel}
          disabled={!enabled}
          aria-label={`Default model for ${HARNESS_LABELS[harness]}`}
          onChange={(event) =>
            void setHarnessOverride(profile.id, harness, true, event.target.value || null)
          }
          style={{ maxWidth: 220, fontSize: 12 }}
        >
          <option value="">Profile default</option>
          {catalog.map((model) => (
            <option key={model.id} value={model.id}>
              {model.id}
            </option>
          ))}
        </select>
        {override?.verified ? (
          <span className="q-faint" style={{ fontSize: 11.5 }}>
            verified in this repo
          </span>
        ) : null}
        {saving ? <span className="q-faint">Saving…</span> : null}
      </div>
    </div>
  );
}

function ProfileDetail({ profile }: { profile: ProxyProfile }) {
  const report = useProxy((state) => state.reports[profile.id]);
  const testing = useProxy((state) => state.testing[profile.id] ?? false);
  const catalog = useProxy((state) => state.catalogs[profile.id]);
  const limits = useProxy((state) => state.limits[profile.id]);
  const catalogLoading = useProxy((state) => state.catalogLoading);
  const caps = report?.capabilities ?? profile.capabilities;
  const rows = capabilityRows(caps);
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
          label="Client API key"
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
            <div className="t">Test</div>
            <div className="s">
              {caps ? `probed ${relativeAge(caps.detected_at)}` : "not tested yet"}
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
                {testing ? "Testing…" : "Test profile"}
              </button>
              {report ? (
                <span className="q-faint num" style={{ alignSelf: "center", fontSize: 12 }}>
                  {Number(report.latency_ms)} ms round trip · {buildLabel(report.capabilities)}
                </span>
              ) : null}
            </div>
            {rows.length > 0 ? (
              <div className="q-probe" style={{ paddingTop: 2 }}>
                {rows.map((row) => (
                  <Fragment key={row.label}>
                    <span className={row.ok ? "q-ok" : "q-warn"}>
                      {row.ok ? (
                        <CheckIcon className="q-icon s12" strokeWidth={2} aria-hidden />
                      ) : (
                        <TriangleAlertIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                      )}
                    </span>
                    <div className="k">{row.label}</div>
                    <div className={cn("v", !row.ok && "q-faint")} title={row.hint}>
                      {row.value}
                      {!row.ok && row.hint ? <span className="q-faint"> · {row.hint}</span> : null}
                    </div>
                  </Fragment>
                ))}
              </div>
            ) : null}
            {report ? (
              <div style={{ marginTop: 12 }}>
                <Diagnostics diagnostics={report.diagnostics} />
              </div>
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
                      {cooldown
                        ? `${cooldown.reason ?? "cooling down"}${cooldown.remaining_seconds != null ? ` · ${Number(cooldown.remaining_seconds)}s` : ""}`
                        : "—"}
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
  const [pickerOpen, setPickerOpen] = useState(false);

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
          <h1>{selected?.name ?? "CLIProxyAPI"}</h1>
          <p className="lead">
            Routes harnesses through one endpoint with several credentials. Keys stay on the daemon;
            this app only sees whether they exist.
          </p>
        </div>
        {profiles.length > 0 ? (
          <div style={{ position: "relative" }}>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => setPickerOpen((open) => !open)}
            >
              {selected?.name ?? "Profile"}{" "}
              <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            </button>
            {pickerOpen ? (
              <div
                className="q-tip"
                style={{ right: 0, top: 34, position: "absolute", minWidth: 220, zIndex: 10 }}
                role="menu"
              >
                {profiles.map((profile) => (
                  <button
                    key={profile.id}
                    type="button"
                    role="menuitem"
                    className="q-li"
                    style={{ display: "block", width: "100%", textAlign: "left" }}
                    onClick={() => {
                      selectProxyProfile(profile.id);
                      setPickerOpen(false);
                    }}
                  >
                    <span className="t">{profile.name}</span>
                    <span className="s q-faint q-mono">{profile.base_url}</span>
                  </button>
                ))}
                <div style={{ padding: 8 }}>
                  <AddProfile />
                </div>
              </div>
            ) : null}
          </div>
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
