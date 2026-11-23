import { KeyRoundIcon, RefreshCwIcon } from "lucide-react";
import { useCallback, useEffect, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import { useProjects } from "@/features/projects/store";
import type { LinearAccount, LinearProjectTeam, LinearTeam } from "@/gen";
import { relativeAge } from "@/lib/format";
import { getClient, pushToast } from "@/lib/store/appStore";

import { IssueApi, type IssueRequest } from "./store";

const api = (request: IssueRequest = (value) => getClient().request(value)) =>
  new IssueApi(request);

/**
 * Settings → Source control: Linear connections and each project's
 * team. The key is validated by the daemon before it is stored and is never
 * shown again.
 */
export function LinearSettings({ request }: { request?: IssueRequest } = {}) {
  const projects = useProjects((state) => state.projects);
  const [accounts, setAccounts] = useState<LinearAccount[]>([]);
  const [teams, setTeams] = useState<Record<string, LinearTeam[]>>({});
  const [mappings, setMappings] = useState<LinearProjectTeam[]>([]);
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [reload, setReload] = useState(0);

  const load = useCallback(() => setReload((value) => value + 1), []);

  useEffect(() => {
    void reload;
    let cancelled = false;
    setLoading(true);
    Promise.all([api(request).linearAccounts(), api(request).projectTeams()])
      .then(([nextAccounts, nextMappings]) => {
        if (cancelled) return;
        setAccounts(nextAccounts);
        setMappings(nextMappings);
        setLoading(false);
      })
      .catch(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [reload, request]);

  useEffect(() => {
    if (accounts.length === 0) {
      setTeams({});
      return;
    }
    let cancelled = false;
    void Promise.all(
      accounts.map(async (account) => {
        try {
          return [account.account_id, await api(request).linearTeams(account.account_id)] as const;
        } catch {
          return [account.account_id, [] as LinearTeam[]] as const;
        }
      }),
    ).then((entries) => {
      if (!cancelled) setTeams(Object.fromEntries(entries));
    });
    return () => {
      cancelled = true;
    };
  }, [accounts, request]);

  const connect = async () => {
    if (!key.trim()) return;
    setBusy(true);
    try {
      await api(request).connectLinear(key.trim());
      setKey("");
      pushToast({ title: "Linear connected.", variant: "success" });
      load();
    } catch (cause) {
      pushToast({
        title: "Linear rejected the key.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  const disconnect = async (account: LinearAccount) => {
    setBusy(true);
    try {
      await api(request).removeLinear(account.account_id);
      load();
    } catch (cause) {
      pushToast({
        title: "The Linear connection was not removed.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  const chooseTeam = async (projectId: string, value: string) => {
    setBusy(true);
    try {
      if (!value) {
        const next = await api(request).setProjectTeam(projectId, null);
        setMappings(next);
      } else {
        const [accountId, teamId] = value.split("::");
        const team = teams[accountId ?? ""]?.find((candidate) => candidate.id === teamId);
        if (!accountId || !team) return;
        const next = await api(request).setProjectTeam(projectId, {
          project_id: projectId,
          account_id: accountId,
          team_id: team.id,
          team_key: team.key,
          team_name: team.name,
        });
        setMappings(next);
      }
    } catch (cause) {
      pushToast({
        title: "The Linear team was not saved.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="q-group">
      <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}>
        <h2 style={{ flex: 1 }}>Linear</h2>
        <button type="button" className="q-btn ghost" onClick={load} disabled={loading}>
          <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Refresh
        </button>
      </div>
      <p className="desc">
        Connect with a personal API key. The key stays on this host, is never returned to a client,
        and is only used to read your team's issues and post comments.
      </p>
      {accounts.length > 0 ? (
        <table className="q-tbl">
          <thead>
            <tr>
              <th>Workspace</th>
              <th>Account</th>
              <th>Connected</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {accounts.map((account) => (
              <tr key={account.account_id}>
                <td>
                  <span className="q-cell-id">
                    <KeyRoundIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
                    <span style={{ fontWeight: 500 }}>{account.name}</span>
                  </span>
                </td>
                <td className="q-muted">{account.email ?? "Not reported"}</td>
                <td className="q-muted">{relativeAge(account.created_at)}</td>
                <td className="r" style={{ paddingRight: 0 }}>
                  <button
                    type="button"
                    className="q-btn danger"
                    style={{ height: 24 }}
                    disabled={busy}
                    onClick={() => void disconnect(account)}
                  >
                    Remove
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <p className="q-muted" style={{ fontSize: 13, margin: "4px 0 12px" }}>
          No Linear workspace connected yet.
        </p>
      )}
      <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
        <input
          className="q-input"
          type="password"
          aria-label="Linear API key"
          placeholder="Linear personal API key"
          value={key}
          onChange={(event) => setKey(event.target.value)}
          style={{ flex: 1 }}
        />
        <button
          type="button"
          className="q-btn secondary"
          disabled={busy || !key.trim()}
          onClick={() => void connect()}
        >
          {busy ? "Checking…" : "Connect and test"}
        </button>
      </div>
      {accounts.length > 0 && projects.length > 0 ? (
        <div style={{ marginTop: 20 }}>
          <h3 style={{ fontSize: 13, margin: "0 0 8px" }}>Teams per project</h3>
          <p className="desc">
            A project's team appears beside its forge issues on the Issues page.
          </p>
          {projects.map((project) => {
            const mapping = mappings.find((candidate) => candidate.project_id === project.id);
            const value = mapping ? `${mapping.account_id}::${mapping.team_id}` : "";
            return (
              <div
                key={project.id}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 10,
                  padding: "6px 0",
                  fontSize: 13,
                }}
              >
                <span style={{ flex: 1 }}>{project.name}</span>
                <ChipSelect
                  label={`Linear team for ${project.name}`}
                  value={value}
                  disabled={busy}
                  options={[
                    { value: "", label: "No Linear team" },
                    ...accounts.flatMap((account) =>
                      (teams[account.account_id] ?? []).map((team) => ({
                        value: `${account.account_id}::${team.id}`,
                        label: `${accounts.length > 1 ? `${account.name} · ` : ""}${team.key} · ${team.name}`,
                      })),
                    ),
                  ]}
                  onChange={(next) => void chooseTeam(project.id, next)}
                />
              </div>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
