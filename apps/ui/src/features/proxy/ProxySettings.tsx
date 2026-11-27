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
import { Trans, useTranslation } from "react-i18next";

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
import { formatList, formatNumber, formatRelativeTime, i18n } from "@/lib/i18n";
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

const p = () => i18n.getFixedT(null, "settings", "proxy");

function buildLabel(capabilities: ProxyCapabilities | null): string {
  const t = p();
  if (!capabilities) return t("build.notTested");
  const fork =
    capabilities.performance ||
    capabilities.fork_model_enrichment ||
    capabilities.usage_queue ||
    capabilities.cooldowns;
  if (fork) return t("build.fork");
  if (capabilities.credentials_verify || capabilities.legacy_usage) return t("build.upstream");
  return t("build.unknown");
}

type ProbeRow = { label: string; value: string; ok: boolean | "neutral"; hint?: string };

function managementValue(capabilities: ProxyCapabilities): string {
  const t = p();
  const state =
    capabilities.management === "available"
      ? t("management.available")
      : capabilities.management === "open"
        ? t("management.open")
        : capabilities.management === "needs_key"
          ? t("management.needsKey")
          : capabilities.management === "disabled"
            ? t("management.disabled")
            : t("management.unknown");
  return capabilities.version ? `${state} · ${capabilities.version}` : state;
}

/** The four lines a person needs from a probe. */
function summaryRows(capabilities: ProxyCapabilities | null): ProbeRow[] {
  if (!capabilities) return [];
  const t = p();
  const shapes = [
    capabilities.claude_models ? "Claude" : null,
    capabilities.openai_models ? "OpenAI" : null,
    capabilities.gemini_models ? "Gemini" : null,
  ].filter((shape): shape is string => shape !== null);
  const usageKnown = capabilities.usage_queue || capabilities.legacy_usage;
  return [
    {
      label: t("summary.health"),
      value: capabilities.reachable ? t("summary.ok") : t("summary.unreachable"),
      ok: capabilities.reachable,
    },
    {
      label: t("summary.management"),
      value: managementValue(capabilities),
      ok: capabilities.management === "available" || capabilities.management === "open",
      ...(capabilities.management === "needs_key"
        ? { hint: t("summary.needsKeyHint") }
        : capabilities.management === "disabled"
          ? { hint: t("summary.disabledHint") }
          : {}),
    },
    {
      label: t("summary.modelShape"),
      value:
        shapes.length > 0 ? formatList(shapes, "conjunction", "narrow") : t("summary.noneDetected"),
      ok: shapes.length > 0,
    },
    {
      label: t("summary.usageHistory"),
      value: usageKnown ? t("summary.usageFromProxy") : t("summary.usageLocal"),
      ok: usageKnown ? true : "neutral",
    },
  ];
}

const yesNo = (value: boolean, yes: string, no: string): ProbeRow["value"] => (value ? yes : no);

/** Everything else the probe learned, behind a disclosure. */
function detailRows(capabilities: ProxyCapabilities | null): ProbeRow[] {
  if (!capabilities) return [];
  const t = p();
  const row = (label: string, value: string, ok: boolean): ProbeRow => ({ label, value, ok });
  const supported = t("detail.supported");
  const missing = t("detail.notAvailable");
  const route = t("detail.routePresent");
  const reported = t("detail.reported");
  const notReported = t("detail.notReported");
  return [
    row(t("detail.build"), buildLabel(capabilities), true),
    ...(capabilities.commit ? [row(t("detail.commit"), capabilities.commit, true)] : []),
    row(
      t("detail.usageQueue"),
      yesNo(capabilities.usage_queue, supported, missing),
      capabilities.usage_queue,
    ),
    row(
      t("detail.legacyUsage"),
      yesNo(capabilities.legacy_usage, t("detail.supportedLegacy"), missing),
      capabilities.legacy_usage,
    ),
    row(
      t("detail.configV8"),
      yesNo(capabilities.v8_config, supported, missing),
      capabilities.v8_config,
    ),
    row(
      t("detail.performance"),
      yesNo(capabilities.performance, route, missing),
      capabilities.performance,
    ),
    row(
      t("detail.credentialsVerify"),
      yesNo(capabilities.credentials_verify, route, missing),
      capabilities.credentials_verify,
    ),
    row(
      t("detail.cooldowns"),
      yesNo(capabilities.cooldowns, reported, notReported),
      capabilities.cooldowns,
    ),
    row(
      t("detail.quotaProviders"),
      yesNo(capabilities.quota_providers, reported, notReported),
      capabilities.quota_providers,
    ),
  ];
}

function useHostName(): string {
  const { t } = useTranslation(["settings", "common"]);
  const name = useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? null) : null,
  );
  return name ?? t("textGeneration.thisHost");
}

/** "480" → "8 min": a cooldown a person can plan around. */
function cooldownText(seconds: bigint | number | null | undefined): string {
  const t = p();
  if (seconds == null) return t("cooldown.plain");
  const value = Number(seconds);
  const unit = (amount: number, name: "second" | "minute" | "hour") =>
    formatNumber(amount, { style: "unit", unit: name, unitDisplay: "short" });
  const left =
    value < 60
      ? unit(Math.max(1, Math.round(value)), "second")
      : value < 3_600
        ? unit(Math.round(value / 60), "minute")
        : unit(Math.round(value / 3_600), "hour");
  return t("cooldown.left", { left });
}

function Diagnostics({ diagnostics }: { diagnostics: ProxyDiagnostic[] }) {
  const { t } = useTranslation(["settings", "common"]);
  if (diagnostics.length === 0) {
    return (
      <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
        {t("proxy.noTrouble")}
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
          <div className="flex gap-1.5 items-start">
            {diagnostic.level === "error" ? (
              <TriangleAlertIcon className="q-icon s12 q-danger" strokeWidth={1.5} aria-hidden />
            ) : null}
            <span className="font-medium">{diagnostic.message}</span>
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
  const { t } = useTranslation(["settings", "common"]);
  if (models.length === 0) {
    return (
      <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
        {t("proxy.models.empty")}
      </p>
    );
  }
  return (
    <table className="q-tbl">
      <thead>
        <tr>
          <th>{t("proxy.models.model")}</th>
          <th>{t("proxy.models.mapsTo")}</th>
          <th className="r">{t("proxy.models.context")}</th>
        </tr>
      </thead>
      <tbody>
        {models.map((model) => (
          <tr key={`${model.id}:${model.source}`} className="hov">
            <td>
              <span className="q-mono">{model.id}</span>
              {model.alias_of ? (
                <span className="q-badge" style={{ height: 18, marginLeft: 6 }}>
                  {t("proxy.models.custom")}
                </span>
              ) : null}
            </td>
            <td className="q-faint">
              {model.alias_of ? (
                <>
                  <span className="q-mono">{model.alias_of}</span> · {t("proxy.models.keepsId")}
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
  return formatNumber(tokens, { notation: "compact", maximumFractionDigits: 1 });
}

function AddProfile() {
  const { t } = useTranslation(["settings", "common"]);
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
        <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden /> {t("proxy.add.button")}
      </button>
    );
  }

  return (
    <div className="q-group">
      <h2>{t("proxy.add.title")}</h2>
      <p className="desc">{t("proxy.add.hint", { host })}</p>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("proxy.add.label")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input"
            value={name}
            onChange={(event) => setName(event.target.value)}
            aria-label={t("proxy.add.labelLabel")}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("forges.add.baseUrl")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            aria-label={t("proxy.add.baseUrlLabel")}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("proxy.managementKey")}</div>
          <div className="s">{t("textGeneration.writeOnly")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={managementKey}
            onChange={(event) => setManagementKey(event.target.value)}
            placeholder={t("proxy.storedOn", { host })}
            aria-label={t("proxy.managementKey")}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("proxy.accessKey")}</div>
          <div className="s">{t("proxy.accessKeyHint")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
            placeholder={t("proxy.storedOn", { host })}
            aria-label={t("proxy.accessKey")}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("common:default")}</div>
        </div>
        <div className="ctl">
          <button
            type="button"
            className={cn("q-check", isDefault && "on")}
            role="switch"
            aria-checked={isDefault}
            aria-label={t("proxy.add.defaultLabel")}
            onClick={() => setIsDefault((value) => !value)}
          >
            {isDefault ? <CheckIcon className="q-icon s12" strokeWidth={2} aria-hidden /> : null}
          </button>
        </div>
      </div>
      <div className="flex gap-2 justify-end">
        <button type="button" className="q-btn ghost" onClick={() => setOpen(false)}>
          {t("common:cancel")}
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
          {saving ? t("common:saving") : t("proxy.add.save")}
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
  const { t } = useTranslation(["settings", "common"]);
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
        />
        <button
          type="button"
          className="q-btn secondary"
          disabled={draft === value}
          onClick={() => onSave(draft.trim())}
        >
          {t("common:save")}
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
  const { t } = useTranslation(["settings", "common"]);
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
              placeholder={t("proxy.newKey")}
              aria-label={t("proxy.newKeyLabel", { label })}
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
              {t("common:save")}
            </button>
            <button type="button" className="q-btn ghost" onClick={() => setEditing(false)}>
              {t("common:cancel")}
            </button>
          </>
        ) : (
          <>
            <span className="q-secret">
              <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              {hasKey ? t("proxy.storedOn", { host }) : t("proxy.noKeyStored")}
            </span>
            <span className="flex-1" />
            <button type="button" className="q-btn secondary" onClick={() => setEditing(true)}>
              {t("proxy.replace")}
            </button>
            {hasKey ? (
              <button type="button" className="q-btn danger" onClick={onClear}>
                {t("common:remove")}
              </button>
            ) : null}
          </>
        )}
      </div>
    </div>
  );
}

function HarnessRow({ profile, harness }: { profile: ProxyProfile; harness: HarnessKind }) {
  const { t } = useTranslation(["settings", "common"]);
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
          <div className="s">{enabled ? t("proxy.harness.routed") : t("proxy.harness.own")}</div>
        </div>
        <div className="ctl">
          <button
            type="button"
            className="q-switch"
            role="switch"
            aria-checked={enabled}
            aria-label={t("proxy.harness.routeLabel", { harness: HARNESS_LABELS[harness] })}
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
            label={t("proxy.harness.defaultModel", { harness: HARNESS_LABELS[harness] })}
            disabled={!enabled}
            value={defaultModel}
            options={[
              { value: "", label: t("proxy.harness.profileDefault") },
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
        <section
          className="q-envprev"
          aria-label={t("proxy.harness.preview", { harness: HARNESS_LABELS[harness] })}
        >
          {override.preview.map((line) => (
            <div key={line.name} className="ln">
              <span className="sg">+</span>
              <span className="q-mono">
                {line.name}:{" "}
                {line.value === null ? (
                  <span className="q-faint">{t("proxy.harness.storedOn", { host })}</span>
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
  const { t } = useTranslation(["settings", "common"]);
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
      <Trans
        t={t}
        i18nKey="proxy.apply.interruptsOne"
        values={{ title: live[0]?.title ?? "", host }}
        components={{ em: <em /> }}
      />
    ) : live.length > 1 ? (
      t("proxy.apply.interruptsMany", { count: live.length, host })
    ) : applied && !dirty ? (
      applied.restarted > 0 ? (
        t("proxy.apply.restarted", { count: applied.restarted })
      ) : (
        t("proxy.apply.applied")
      )
    ) : (
      t("proxy.apply.keepOld")
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
        {applying ? t("proxy.apply.restarting") : t("proxy.apply.button")}
      </button>
    </div>
  );
}

function ProfileDetail({ profile }: { profile: ProxyProfile }) {
  const { t } = useTranslation(["settings", "common"]);
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
          {t("proxy.connection")}
          <span className="grow" />
          <span className="q-faint" style={{ fontSize: 12, fontWeight: 400 }}>
            {buildLabel(caps)}
          </span>
        </h2>
        <ProfileField
          label={t("forges.add.baseUrl")}
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
          label={t("proxy.managementKey")}
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
          label={t("proxy.accessKey")}
          sub={t("proxy.accessKeyHint")}
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
            <div className="t">{t("proxy.probe.title")}</div>
            <div className="s">
              {caps
                ? t("proxy.probe.when", { when: formatRelativeTime(caps.detected_at), host })
                : t("proxy.probe.never")}
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
                {testing ? t("proxy.probe.probing") : t("proxy.probe.title")}
              </button>
              {report ? (
                <span className="q-faint num" style={{ alignSelf: "center", fontSize: 12 }}>
                  {t("proxy.probe.roundTrip", {
                    latency: formatNumber(Number(report.latency_ms), {
                      style: "unit",
                      unit: "millisecond",
                      unitDisplay: "short",
                    }),
                  })}
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
                <summary>{t("proxy.probe.details")}</summary>
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
                  <div className="mt-3">
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
          {t("proxy.models.title")}
          <span className="grow" />
          <span className="q-faint" style={{ fontSize: 12, fontWeight: 400 }}>
            {catalogLoading
              ? t("proxy.models.reading")
              : catalog?.aliases_merged
                ? t("proxy.models.fromAliases", { count: models.length })
                : t("proxy.models.fromResponses", { count: models.length })}
          </span>
        </h2>
        <ModelTable models={models} />
      </div>

      <div className="q-group">
        <h2>{t("proxy.harness.title")}</h2>
        <p className="desc">{t("proxy.harness.hint")}</p>
        {HARNESSES.map((harness) => (
          <HarnessRow key={harness} profile={profile} harness={harness} />
        ))}
        <ApplyBar profile={profile} />
      </div>

      <div className="q-group">
        <h2>{t("proxy.health.title")}</h2>
        {!limits ? (
          <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
            {t("proxy.health.none")}
          </p>
        ) : !limits.available ? (
          <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
            {limits.note ?? t("proxy.health.unreadable")}
          </p>
        ) : limits.credentials.length === 0 ? (
          <p className="q-faint" style={{ fontSize: 12, margin: 0 }}>
            {t("proxy.health.noCredentials")}
          </p>
        ) : (
          <table className="q-tbl">
            <thead>
              <tr>
                <th>{t("proxy.health.credential")}</th>
                <th>{t("proxy.health.provider")}</th>
                <th>{t("proxy.health.status")}</th>
                <th className="r">{t("proxy.summary.ok")}</th>
                <th className="r">{t("proxy.health.failed")}</th>
                <th>{t("proxy.health.cooldown")}</th>
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
                      {credential.disabled
                        ? t("proxy.management.disabled")
                        : (credential.status ?? t("proxy.health.ready"))}
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
  const { t } = useTranslation(["settings", "common"]);
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
        <div className="q-skel h-3 w-65" />
        <div className="q-skel h-30 mt-4" />
      </div>
    );
  }

  if (error && profiles.length === 0) {
    return (
      <div className="q-form">
        <h1>CLIProxyAPI</h1>
        <div className="q-empty">
          <p className="m-0">{error.message}</p>
          <p className="q-faint" style={{ margin: "4px 0 12px" }}>
            {error.hint ?? t("proxy.startDaemon")}
          </p>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void loadProxyProfiles()}
          >
            {t("common:tryAgain")}
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="q-form">
      <div className="q-form-head">
        <div className="flex-1">
          <h1>
            {selected?.name ?? "CLIProxyAPI"}
            {selected && !selected.name.includes(host) ? ` ${t("proxy.onHost", { host })}` : ""}
          </h1>
          <p className="lead">{t("proxy.lead", { host })}</p>
        </div>
        {profiles.length > 1 ? (
          <ChipSelect
            label={t("proxy.profile")}
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
          <h2>{t("proxy.noProfile")}</h2>
          <p className="desc">{t("proxy.noProfileHint")}</p>
          <AddProfile />
        </div>
      ) : null}

      {selected ? <ProfileDetail profile={selected} /> : null}
      {profiles.length > 0 ? (
        <div className="flex gap-2 justify-end mt-3">
          <AddProfile />
        </div>
      ) : null}

      {saveError ? (
        <p className="q-danger text-xs mt-3">
          {saveError.message}
          {saveError.hint ? ` ${saveError.hint}` : ""}
        </p>
      ) : null}
    </div>
  );
}
