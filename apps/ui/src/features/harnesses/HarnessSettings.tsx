import { PencilIcon, PlusIcon, RefreshCwIcon, Trash2Icon, UnplugIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ChipSelect } from "@/components/common/ChipSelect";
import type { AcpAgent, HarnessKind, HarnessVersionInfo } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { i18n } from "@/lib/i18n";
import { useApp } from "@/lib/store/appStore";

import {
  type AcpAgentForm,
  acpAgentSpecFromForm,
  EMPTY_ACP_FORM,
  formFromAcpAgent,
  validateAcpAgentForm,
} from "./acpForm";
import {
  addAcpAgent,
  loadAcpAgentSpec,
  loadHarnessVersions,
  probeAcpAgent,
  removeAcpAgent,
  updateAcpAgent,
  updateHarness,
  useAcpAgents,
  useHarnesses,
  useHarnessVersions,
} from "./harnessStore";

/** One sentence naming where the row stands; the version numbers sit beside it. */
function statusText(row: HarnessVersionInfo): string {
  if (row.installed === null) return i18n.t("settings:harnesses.status.notInstalled");
  if (row.update_available) return i18n.t("settings:harnesses.status.updateAvailable");
  if (row.latest === null) return i18n.t("settings:harnesses.status.latestUnknown");
  return i18n.t("settings:harnesses.status.upToDate");
}

function HarnessRow({ row }: { row: HarnessVersionInfo }) {
  const { t } = useTranslation(["settings", "common"]);
  const kind: HarnessKind = row.harness;
  const updating = useHarnesses((state) => state.updating[kind] === true);
  const output = useHarnesses((state) => state.output[kind]);
  const result = useHarnesses((state) => state.results[kind]);
  const runnable =
    row.method.kind === "self_update" || row.method.kind === "npm" || row.method.kind === "bun";
  const showOutput = (output?.length ?? 0) > 0 || result !== undefined;

  return (
    <div className="q-harness-row">
      <div className="q-field">
        <div className="lab" style={{ width: 200 }}>
          <div className="t">{HARNESS_LABELS[kind]}</div>
          <div className="s">{statusText(row)}</div>
        </div>
        <div className="ctl">
          <span className="q-mono q-muted">
            {row.installed ?? "—"}
            {row.update_available && row.latest ? ` → ${row.latest}` : ""}
          </span>
          <span style={{ flex: 1 }} />
          {row.update_available && runnable ? (
            <button
              type="button"
              className="q-btn secondary"
              disabled={updating}
              aria-label={t("harnesses.updateLabel", { harness: HARNESS_LABELS[kind] })}
              onClick={() => void updateHarness(kind)}
            >
              {updating ? t("harnesses.updating") : t("harnesses.update")}
            </button>
          ) : null}
        </div>
      </div>
      {row.update_available && row.method.kind === "manual" ? (
        <p className="q-harness-manual">
          <Trans
            t={t}
            i18nKey="harnesses.manual"
            values={{ command: row.method.command }}
            components={{ code: <span className="q-mono" /> }}
          />
        </p>
      ) : null}
      {showOutput ? (
        <div className="q-harness-out" role="status" aria-live="polite">
          {(output?.length ?? 0) > 0 ? <pre className="q-mono">{output?.join("\n")}</pre> : null}
          {result ? (
            <p className={result.ok ? "res" : "res bad"}>
              {result.ok ? "" : `${t("harnesses.failed")} · `}
              {result.message}
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function AgentProbeLine({ id }: { id: string }) {
  const probe = useHarnesses((state) => state.probes[id]);
  const probing = useHarnesses((state) => state.probing[id] === true);
  const { t } = useTranslation(["settings", "common"]);
  if (probing) return <p className="q-harness-manual">{t("harnesses.acp.probing")}</p>;
  if (!probe) return null;
  if (probe.error) {
    return (
      <p className="q-harness-manual" style={{ color: "var(--danger)" }}>
        {probe.error}
      </p>
    );
  }
  const parts = [
    probe.version ? t("harnesses.acp.version", { version: probe.version }) : null,
    probe.models.length > 0
      ? t("harnesses.acp.models", { count: probe.models.length })
      : t("harnesses.acp.noModels"),
    probe.modes.length > 0 ? probe.modes.map((mode) => mode.name).join(", ") : null,
  ].filter(Boolean);
  return <p className="q-harness-manual">{parts.join(" · ")}</p>;
}

function AgentRow({ agent, onEdit }: { agent: AcpAgent; onEdit: () => void }) {
  const { t } = useTranslation(["settings", "common"]);
  const probing = useHarnesses((state) => state.probing[agent.id] === true);
  const [removing, setRemoving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  return (
    <>
      <div className="q-field">
        <div className="lab">
          <div className="t">
            {agent.name} <span className="q-mono q-faint">{agent.id}</span>
          </div>
          <div className="s">
            {agent.installed
              ? `${agent.version ? `${t("harnesses.acp.version", { version: agent.version })} · ` : ""}${agent.path ?? agent.command}`
              : t("harnesses.acp.notFound")}
          </div>
        </div>
        <div className="ctl">
          <span className="q-faint" style={{ fontSize: 12 }}>
            {t("harnesses.acp.managedOutside")}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            disabled={probing}
            onClick={() => void probeAcpAgent(agent.id).then(setError)}
          >
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {probing
              ? t("common:checking")
              : agent.installed
                ? t("harnesses.acp.check")
                : t("harnesses.checkAgain")}
          </button>
          <button
            type="button"
            className="q-btn ghost"
            onClick={onEdit}
            aria-label={t("harnesses.acp.editLabel", { name: agent.name })}
          >
            <PencilIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("common:edit")}
          </button>
          <button
            type="button"
            className="q-btn ghost danger"
            disabled={removing}
            aria-label={t("harnesses.acp.removeLabel", { name: agent.name })}
            onClick={() => {
              setRemoving(true);
              void removeAcpAgent(agent.id).then((message) => {
                setRemoving(false);
                setError(message);
              });
            }}
          >
            <Trash2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("common:remove")}
          </button>
        </div>
      </div>
      <AgentProbeLine id={agent.id} />
      {error ? (
        <p className="q-harness-manual" style={{ color: "var(--danger)" }}>
          {error}
        </p>
      ) : null}
    </>
  );
}

function AgentForm({
  agent,
  existingIds,
  onDone,
}: {
  agent: AcpAgent | "new";
  existingIds: string[];
  onDone: () => void;
}) {
  const { t } = useTranslation(["settings", "common"]);
  const [form, setForm] = useState<AcpAgentForm | null>(agent === "new" ? EMPTY_ACP_FORM : null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // The list masks environment values. The edit form asks the admin-scoped
  // request for the real configuration once it opens.
  useEffect(() => {
    if (agent === "new") return;
    let cancelled = false;
    loadAcpAgentSpec(agent.id)
      .then((spec) => {
        if (!cancelled) setForm(formFromAcpAgent(spec));
      })
      .catch((reason: unknown) => {
        if (!cancelled) {
          setError(
            reason instanceof Error ? reason.message : i18n.t("settings:harnesses.acp.readFailed"),
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [agent]);

  const patch = (next: Partial<AcpAgentForm>) =>
    setForm((current) => (current ? { ...current, ...next } : current));

  const save = async () => {
    if (!form) return;
    const message = validateAcpAgentForm(form, {
      existingIds,
      ...(agent === "new" ? {} : { editingId: agent.id }),
    });
    if (message) {
      setError(message);
      return;
    }
    setSaving(true);
    const spec = acpAgentSpecFromForm(form);
    const failed = agent === "new" ? await addAcpAgent(spec) : await updateAcpAgent(agent.id, spec);
    setSaving(false);
    if (failed) {
      setError(failed);
      return;
    }
    onDone();
  };

  if (!form) {
    return (
      <div className="q-group">
        <h2>
          {agent === "new"
            ? t("harnesses.acp.editAgent")
            : t("harnesses.acp.editLabel", { name: agent.name })}
        </h2>
        {error ? (
          <p className="q-harness-manual" style={{ color: "var(--danger)" }} role="alert">
            {error}
          </p>
        ) : (
          <p className="q-muted">{t("harnesses.acp.reading")}</p>
        )}
        <div style={{ display: "flex", justifyContent: "flex-end", marginTop: 12 }}>
          <button type="button" className="q-btn ghost" onClick={onDone}>
            {t("common:close")}
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="q-group">
      <h2>
        {agent === "new"
          ? t("harnesses.acp.addTitle")
          : t("harnesses.acp.editLabel", { name: agent.name })}
      </h2>
      <p className="desc">{t("harnesses.acp.formHint")}</p>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("harnesses.acp.displayName")}</div>
          <div className="s">{t("harnesses.acp.displayNameHint")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input"
            value={form.name}
            onChange={(event) => patch({ name: event.target.value })}
            aria-label={t("harnesses.acp.displayNameLabel")}
            style={{ maxWidth: 300 }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("harnesses.acp.instanceId")}</div>
          <div className="s">{t("harnesses.acp.instanceIdHint")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={form.id}
            disabled={agent !== "new"}
            onChange={(event) => patch({ id: event.target.value })}
            aria-label={t("harnesses.acp.instanceIdLabel")}
            style={{ maxWidth: 240 }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("harnesses.acp.command")}</div>
          <div className="s">{t("harnesses.acp.commandHint")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={form.command}
            onChange={(event) => patch({ command: event.target.value })}
            aria-label={t("harnesses.acp.commandLabel")}
            placeholder="deepseek-acp"
            style={{ maxWidth: 360 }}
          />
        </div>
      </div>

      <div className="q-field" style={{ alignItems: "flex-start" }}>
        <div className="lab">
          <div className="t">{t("harnesses.acp.arguments")}</div>
          <div className="s">{t("harnesses.acp.argumentsHint")}</div>
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
          {form.args.map((argument, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: rows are ordered form fields, not keyed data
            <div key={index} className="q-acp-row">
              <input
                className="q-input q-mono q-acp-value"
                value={argument}
                onChange={(event) => {
                  const args = [...form.args];
                  args[index] = event.target.value;
                  patch({ args });
                }}
                aria-label={t("harnesses.acp.argumentLabel", { row: index + 1 })}
              />
              <button
                type="button"
                className="q-btn ghost"
                aria-label={t("harnesses.acp.removeArgument", { row: index + 1 })}
                onClick={() => patch({ args: form.args.filter((_, at) => at !== index) })}
              >
                {t("common:remove")}
              </button>
            </div>
          ))}
          <button
            type="button"
            className="q-btn ghost"
            style={{ alignSelf: "flex-start" }}
            onClick={() => patch({ args: [...form.args, ""] })}
          >
            <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("harnesses.acp.addArgument")}
          </button>
        </div>
      </div>

      <div className="q-field" style={{ alignItems: "flex-start" }}>
        <div className="lab">
          <div className="t">{t("harnesses.acp.environment")}</div>
          <div className="s">{t("harnesses.acp.environmentHint")}</div>
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
          {form.env.map((row, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: rows are ordered form fields, not keyed data
            <div key={index} className="q-acp-row">
              <input
                className="q-input q-mono q-acp-name"
                value={row.key}
                onChange={(event) => {
                  const env = [...form.env];
                  env[index] = { ...row, key: event.target.value };
                  patch({ env });
                }}
                aria-label={t("harnesses.acp.envNameLabel", { row: index + 1 })}
                placeholder="NAME"
              />
              <input
                className="q-input q-mono q-acp-value"
                value={row.value}
                onChange={(event) => {
                  const env = [...form.env];
                  env[index] = { ...row, value: event.target.value };
                  patch({ env });
                }}
                aria-label={t("harnesses.acp.envValueLabel", { row: index + 1 })}
              />
              <button
                type="button"
                className="q-btn ghost"
                aria-label={t("harnesses.acp.removeEnv", { row: index + 1 })}
                disabled={form.env.length === 1 && row.key === "" && row.value === ""}
                onClick={() => patch({ env: form.env.filter((_, at) => at !== index) })}
              >
                {t("common:remove")}
              </button>
            </div>
          ))}
          <button
            type="button"
            className="q-btn ghost"
            style={{ alignSelf: "flex-start" }}
            onClick={() => patch({ env: [...form.env, { key: "", value: "" }] })}
          >
            <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("harnesses.acp.addVariable")}
          </button>
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">{t("harnesses.acp.workingDir")}</div>
          <div className="s">{t("harnesses.acp.workingDirHint")}</div>
        </div>
        <div className="ctl" style={{ gap: 8 }}>
          <ChipSelect
            id="acp-working-dir"
            label={t("harnesses.acp.workingDir")}
            value={form.workingDir}
            options={[
              { value: "thread_workspace", label: t("harnesses.acp.threadWorkspace") },
              { value: "fixed_path", label: t("harnesses.acp.fixedPath") },
            ]}
            onChange={(value) => patch({ workingDir: value as "thread_workspace" | "fixed_path" })}
          />
          {form.workingDir === "fixed_path" ? (
            <input
              className="q-input q-mono"
              value={form.path}
              onChange={(event) => patch({ path: event.target.value })}
              aria-label={t("harnesses.acp.fixedPathLabel")}
              placeholder="/home/you/project"
              style={{ maxWidth: 320 }}
            />
          ) : null}
        </div>
      </div>

      {error ? (
        <p className="q-harness-manual" style={{ color: "var(--danger)" }} role="alert">
          {error}
        </p>
      ) : null}

      <div style={{ display: "flex", justifyContent: "flex-end", gap: 8, marginTop: 12 }}>
        <button type="button" className="q-btn ghost" onClick={onDone} disabled={saving}>
          {t("common:cancel")}
        </button>
        <button
          type="button"
          className="q-btn primary"
          onClick={() => void save()}
          disabled={saving}
        >
          {saving
            ? t("common:saving")
            : agent === "new"
              ? t("harnesses.acp.addAgent")
              : t("common:saveChanges")}
        </button>
      </div>
    </div>
  );
}

/** Settings → Harnesses: installed versus latest, with an Update action. */
export function HarnessSettings() {
  const { t } = useTranslation(["settings", "common"]);
  useHarnessVersions();
  useAcpAgents();
  const rows = useHarnesses((state) => state.rows);
  const loaded = useHarnesses((state) => state.loaded);
  const loading = useHarnesses((state) => state.loading);
  const error = useHarnesses((state) => state.error);
  const agents = useHarnesses((state) => state.agents);
  const agentsLoaded = useHarnesses((state) => state.agentsLoaded);
  const agentsError = useHarnesses((state) => state.agentsError);
  const [editing, setEditing] = useState<AcpAgent | "new" | null>(null);
  const connection = useApp((state) => state.connection.status);

  return (
    <div className="q-form">
      <h1>{t("harnesses.title")}</h1>
      <p className="lead">{t("harnesses.lead")}</p>
      <div className="q-group">
        <div className="q-group-head">
          <h2>{t("harnesses.cliVersions")}</h2>
          <button
            type="button"
            className="q-btn ghost"
            disabled={loading || connection !== "online"}
            onClick={() => void loadHarnessVersions()}
          >
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("harnesses.checkAgain")}
          </button>
        </div>
        {!loaded ? (
          <div className="q-skel" style={{ height: 120 }} />
        ) : error ? (
          <p className="q-muted">{error.message}</p>
        ) : (
          rows.map((row) => <HarnessRow key={row.harness} row={row} />)
        )}
      </div>

      <div className="q-group">
        <div className="q-group-head">
          <h2>{t("harnesses.acp.title")}</h2>
          {editing === null ? (
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => setEditing("new")}
              disabled={connection !== "online"}
            >
              <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {t("harnesses.acp.addButton")}
            </button>
          ) : null}
        </div>
        <p className="desc">{t("harnesses.acp.hint")}</p>
        {!agentsLoaded ? (
          <div className="q-skel" style={{ height: 80 }} />
        ) : agentsError ? (
          <p className="q-muted">{agentsError.message}</p>
        ) : agents.length === 0 ? (
          <p className="q-muted">
            <UnplugIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />{" "}
            {t("harnesses.acp.empty")}
          </p>
        ) : (
          agents.map((agent) => (
            <AgentRow key={agent.id} agent={agent} onEdit={() => setEditing(agent)} />
          ))
        )}
        {editing === null ? null : (
          <AgentForm
            key={editing === "new" ? "new" : editing.id}
            agent={editing}
            existingIds={agents.map((agent) => agent.id)}
            onDone={() => setEditing(null)}
          />
        )}
      </div>
    </div>
  );
}
