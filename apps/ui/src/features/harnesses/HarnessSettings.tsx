import { PencilIcon, PlusIcon, RefreshCwIcon, Trash2Icon, UnplugIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import type { AcpAgent, HarnessKind, HarnessVersionInfo } from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
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
  if (row.installed === null) return "Not installed";
  if (row.update_available) return "Update available";
  if (row.latest === null) return "Latest version unknown";
  return "Up to date";
}

function HarnessRow({ row }: { row: HarnessVersionInfo }) {
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
              aria-label={`Update ${HARNESS_LABELS[kind]}`}
              onClick={() => void updateHarness(kind)}
            >
              {updating ? "Updating…" : "Update"}
            </button>
          ) : null}
        </div>
      </div>
      {row.update_available && row.method.kind === "manual" ? (
        <p className="q-harness-manual">
          h4rn won't change this install. Run <span className="q-mono">{row.method.command}</span>{" "}
          in a terminal.
        </p>
      ) : null}
      {showOutput ? (
        <div className="q-harness-out" role="status" aria-live="polite">
          {(output?.length ?? 0) > 0 ? <pre className="q-mono">{output?.join("\n")}</pre> : null}
          {result ? (
            <p className={result.ok ? "res" : "res bad"}>
              {result.ok ? "" : "Failed · "}
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
  if (probing)
    return <p className="q-harness-manual">Starting the agent once for its ACP handshake…</p>;
  if (!probe) return null;
  if (probe.error) {
    return (
      <p className="q-harness-manual" style={{ color: "var(--danger)" }}>
        {probe.error}
      </p>
    );
  }
  const parts = [
    probe.version ? `version ${probe.version}` : null,
    probe.models.length > 0
      ? `${probe.models.length} model${probe.models.length === 1 ? "" : "s"}`
      : "no models advertised",
    probe.modes.length > 0 ? probe.modes.map((mode) => mode.name).join(", ") : null,
  ].filter(Boolean);
  return <p className="q-harness-manual">{parts.join(" · ")}</p>;
}

function AgentRow({ agent, onEdit }: { agent: AcpAgent; onEdit: () => void }) {
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
              ? `${agent.version ? `version ${agent.version} · ` : ""}${agent.path ?? agent.command}`
              : "Not installed · command not found"}
          </div>
        </div>
        <div className="ctl">
          <span className="q-faint" style={{ fontSize: 12 }}>
            Managed outside h4rn
          </span>
          <button
            type="button"
            className="q-btn ghost"
            disabled={probing}
            onClick={() => void probeAcpAgent(agent.id).then(setError)}
          >
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {probing ? "Checking…" : agent.installed ? "Check" : "Check again"}
          </button>
          <button
            type="button"
            className="q-btn ghost"
            onClick={onEdit}
            aria-label={`Edit ${agent.name}`}
          >
            <PencilIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Edit
          </button>
          <button
            type="button"
            className="q-btn ghost danger"
            disabled={removing}
            aria-label={`Remove ${agent.name}`}
            onClick={() => {
              setRemoving(true);
              void removeAcpAgent(agent.id).then((message) => {
                setRemoving(false);
                setError(message);
              });
            }}
          >
            <Trash2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Remove
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
          setError(reason instanceof Error ? reason.message : "Could not read this agent.");
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
        <h2>Edit {agent === "new" ? "agent" : agent.name}</h2>
        {error ? (
          <p className="q-harness-manual" style={{ color: "var(--danger)" }} role="alert">
            {error}
          </p>
        ) : (
          <p className="q-muted">Reading the agent's configuration…</p>
        )}
        <div style={{ display: "flex", justifyContent: "flex-end", marginTop: 12 }}>
          <button type="button" className="q-btn ghost" onClick={onDone}>
            Close
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="q-group">
      <h2>{agent === "new" ? "Add a local ACP agent" : `Edit ${agent.name}`}</h2>
      <p className="desc">
        h4rn runs this command directly, with the arguments below and no shell expansion. It runs
        with this computer's privileges. The agent manages its own updates and sign-in.
      </p>

      <div className="q-field">
        <div className="lab">
          <div className="t">Display name</div>
          <div className="s">Shown in this list and the new-thread picker.</div>
        </div>
        <div className="ctl">
          <input
            className="q-input"
            value={form.name}
            onChange={(event) => patch({ name: event.target.value })}
            aria-label="ACP agent display name"
            style={{ maxWidth: 300 }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Instance id</div>
          <div className="s">Lowercase slug, unique on this host.</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={form.id}
            disabled={agent !== "new"}
            onChange={(event) => patch({ id: event.target.value })}
            aria-label="ACP agent instance id"
            style={{ maxWidth: 240 }}
          />
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Command</div>
          <div className="s">An executable name on PATH, or an absolute path.</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            value={form.command}
            onChange={(event) => patch({ command: event.target.value })}
            aria-label="ACP agent command"
            placeholder="deepseek-acp"
            style={{ maxWidth: 360 }}
          />
        </div>
      </div>

      <div className="q-field" style={{ alignItems: "flex-start" }}>
        <div className="lab">
          <div className="t">Arguments</div>
          <div className="s">One literal argument per row. No quotes or shell syntax.</div>
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
          {form.args.map((argument, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: rows are ordered form fields, not keyed data
            <div key={index} style={{ display: "flex", gap: 6 }}>
              <input
                className="q-input q-mono"
                value={argument}
                onChange={(event) => {
                  const args = [...form.args];
                  args[index] = event.target.value;
                  patch({ args });
                }}
                aria-label={`Argument ${index + 1}`}
                style={{ flex: 1 }}
              />
              <button
                type="button"
                className="q-btn ghost"
                aria-label={`Remove argument ${index + 1}`}
                onClick={() => patch({ args: form.args.filter((_, at) => at !== index) })}
              >
                Remove
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
            Add argument
          </button>
        </div>
      </div>

      <div className="q-field" style={{ alignItems: "flex-start" }}>
        <div className="lab">
          <div className="t">Environment</div>
          <div className="s">Extra variables for this command only.</div>
        </div>
        <div className="ctl" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
          {form.env.map((row, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: rows are ordered form fields, not keyed data
            <div key={index} style={{ display: "flex", gap: 6 }}>
              <input
                className="q-input q-mono"
                value={row.key}
                onChange={(event) => {
                  const env = [...form.env];
                  env[index] = { ...row, key: event.target.value };
                  patch({ env });
                }}
                aria-label={`Environment name ${index + 1}`}
                placeholder="NAME"
                style={{ flex: "0 0 180px" }}
              />
              <input
                className="q-input q-mono"
                value={row.value}
                onChange={(event) => {
                  const env = [...form.env];
                  env[index] = { ...row, value: event.target.value };
                  patch({ env });
                }}
                aria-label={`Environment value ${index + 1}`}
                style={{ flex: 1 }}
              />
              <button
                type="button"
                className="q-btn ghost"
                aria-label={`Remove environment row ${index + 1}`}
                disabled={form.env.length === 1 && row.key === "" && row.value === ""}
                onClick={() => patch({ env: form.env.filter((_, at) => at !== index) })}
              >
                Remove
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
            Add variable
          </button>
        </div>
      </div>

      <div className="q-field">
        <div className="lab">
          <div className="t">Working directory</div>
          <div className="s">Where the agent starts; the thread workspace by default.</div>
        </div>
        <div className="ctl" style={{ gap: 8 }}>
          <ChipSelect
            id="acp-working-dir"
            label="Working directory"
            value={form.workingDir}
            options={[
              { value: "thread_workspace", label: "Thread workspace" },
              { value: "fixed_path", label: "Fixed path" },
            ]}
            onChange={(value) => patch({ workingDir: value as "thread_workspace" | "fixed_path" })}
          />
          {form.workingDir === "fixed_path" ? (
            <input
              className="q-input q-mono"
              value={form.path}
              onChange={(event) => patch({ path: event.target.value })}
              aria-label="Fixed working directory"
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
          Cancel
        </button>
        <button
          type="button"
          className="q-btn primary"
          onClick={() => void save()}
          disabled={saving}
        >
          {saving ? "Saving…" : agent === "new" ? "Add agent" : "Save changes"}
        </button>
      </div>
    </div>
  );
}

/** Settings → Harnesses: installed versus latest, with an Update action. */
export function HarnessSettings() {
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
      <h1>Harnesses</h1>
      <p className="lead">
        The agent CLIs h4rn runs on this host. Latest versions come from the npm registry and are
        checked every few hours.
      </p>
      <div className="q-group">
        <div className="q-group-head">
          <h2>CLI versions</h2>
          <button
            type="button"
            className="q-btn ghost"
            disabled={loading || connection !== "online"}
            onClick={() => void loadHarnessVersions()}
          >
            <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Check again
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
          <h2>Local ACP agents</h2>
          {editing === null ? (
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => setEditing("new")}
              disabled={connection !== "online"}
            >
              <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Add ACP agent
            </button>
          ) : null}
        </div>
        <p className="desc">
          Any command that speaks the Agent Client Protocol over stdio. h4rn launches it with the
          arguments you write, literally, and no shell. It runs with this computer's privileges;
          install, sign in and update it yourself.
        </p>
        {!agentsLoaded ? (
          <div className="q-skel" style={{ height: 80 }} />
        ) : agentsError ? (
          <p className="q-muted">{agentsError.message}</p>
        ) : agents.length === 0 ? (
          <p className="q-muted">
            <UnplugIcon className="q-icon s14" strokeWidth={1.5} aria-hidden /> No local ACP agents
            yet.
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
