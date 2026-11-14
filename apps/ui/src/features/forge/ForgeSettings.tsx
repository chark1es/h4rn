import { KeyRoundIcon, PlusIcon, RefreshCwIcon } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import { type ForgeAccounts, ForgeApi, forgeLabel } from "@/features/forge/store";
import type { ForgeAccount, ForgeCredentialSource, ForgeDeviceFlow, ForgeKind } from "@/gen";
import { relativeAge } from "@/lib/format";
import { getClient, pushToast } from "@/lib/store/appStore";

const api = () => new ForgeApi((request) => getClient().request(request));

const FORGES: ForgeKind[] = ["github", "gitlab", "gitea", "forgejo"];

const DEFAULT_BASE: Record<ForgeKind, string> = {
  github: "https://github.com",
  gitlab: "https://gitlab.com",
  gitea: "https://gitea.example.com",
  forgejo: "https://forgejo.example.com",
};

/** The forge's smallest useful token, in the words of its own settings page. */
const SCOPES: Record<ForgeKind, string> = {
  github: "a fine-grained token with Contents: read, Pull requests: write and Checks: read",
  gitlab: "a project access token with read_api and write to merge requests",
  gitea: "a token with repo scope on the repositories you want to see",
  forgejo: "a token with repo scope on the repositories you want to see",
};

/** OAuth device flow is a GitHub dotcom feature in this build. */
function supportsDeviceFlow(accounts: ForgeAccounts, base: string): boolean {
  const github = accounts.capabilities.find((entry) => entry.forge === "github");
  return (
    Boolean(github?.capabilities.device_flow) && base.replace(/\/$/, "") === "https://github.com"
  );
}

function sourceLabel(source: ForgeCredentialSource): string {
  switch (source) {
    case "pat":
      return "Token";
    case "o_auth_device":
      return "GitHub device flow";
    case "gh_cli":
      return "Imported from gh";
    case "glab_cli":
      return "Imported from glab";
  }
}

/** Scopes that reach beyond the pull requests this surface needs. */
function broadScopes(scopes: string[]): string[] {
  return scopes.filter((scope) =>
    ["repo", "admin:org", "write:org", "api", "sudo", "delete_repo"].includes(scope),
  );
}

function AccountsTable({
  accounts,
  onRemove,
}: {
  accounts: ForgeAccount[];
  onRemove: (account: ForgeAccount) => void;
}) {
  if (accounts.length === 0) {
    return (
      <p className="q-muted" style={{ fontSize: 13, margin: "4px 0 16px" }}>
        No forge accounts yet. Add a token below, or import one your CLI already stores.
      </p>
    );
  }
  return (
    <table className="q-tbl">
      <thead>
        <tr>
          <th>Account</th>
          <th>Host</th>
          <th>Scopes</th>
          <th>Added</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {accounts.map((account) => {
          const broad = broadScopes(account.scopes);
          return (
            <tr key={account.account_id}>
              <td>
                <span className="q-cell-id">
                  <KeyRoundIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
                  <span>
                    <span style={{ fontWeight: 500 }}>
                      {account.display_name ?? account.username ?? forgeLabel(account.forge)}
                    </span>
                    <span className="q-faint"> · {sourceLabel(account.source)}</span>
                    {account.username && account.display_name ? (
                      <span className="q-faint"> · @{account.username}</span>
                    ) : null}
                  </span>
                </span>
              </td>
              <td>
                <span className="q-mono q-muted" style={{ fontSize: 12 }}>
                  {account.base_url}
                </span>
                <span className="q-faint"> · {forgeLabel(account.forge)}</span>
              </td>
              <td>
                {account.scopes.length === 0 ? (
                  <span className="q-faint">Not reported</span>
                ) : broad.length > 0 ? (
                  <span className="q-warn" title={`Broad scopes: ${broad.join(", ")}`}>
                    {broad.join(", ")} · broader than needed
                  </span>
                ) : (
                  <span className="q-muted">{account.scopes.join(", ")}</span>
                )}
              </td>
              <td className="q-muted">{relativeAge(account.created_at)}</td>
              <td className="r" style={{ paddingRight: 0 }}>
                <button
                  type="button"
                  className="q-btn danger"
                  style={{ height: 24 }}
                  aria-label={`Remove ${account.display_name ?? account.username ?? forgeLabel(account.forge)}`}
                  onClick={() => onRemove(account)}
                >
                  Remove
                </button>
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function AddAccountForm({ onAdded }: { onAdded: () => void }) {
  const [forge, setForge] = useState<ForgeKind>("github");
  const [baseUrl, setBaseUrl] = useState(DEFAULT_BASE.github);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);

  const changeForge = (next: ForgeKind) => {
    // Keep a deliberate self-hosted URL, replace a default one.
    setBaseUrl((current) =>
      current === DEFAULT_BASE[forge] || current.trim() === "" ? DEFAULT_BASE[next] : current,
    );
    setForge(next);
  };

  const submit = async () => {
    if (token.trim() === "" || baseUrl.trim() === "" || busy) return;
    setBusy(true);
    try {
      const account = await api().addAccount(
        forge,
        baseUrl.trim().replace(/\/$/, ""),
        token.trim(),
      );
      // The token is written once and never read back.
      setToken("");
      pushToast({
        title: `Added ${forgeLabel(forge)}${account.username ? ` as @${account.username}` : ""}.`,
        variant: "success",
      });
      onAdded();
    } catch (cause) {
      pushToast({
        title: "The account was not added.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="q-group">
      <h2>Add an account</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">Forge</div>
        </div>
        <div className="ctl">
          <ChipSelect
            label="Forge"
            value={forge}
            width={160}
            options={FORGES.map((kind) => ({ value: kind, label: forgeLabel(kind) }))}
            onChange={changeForge}
          />
          <span className="q-faint" style={{ fontSize: 12 }}>
            Use {SCOPES[forge]}.
          </span>
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Base URL</div>
          <div className="s">Point a self-hosted forge at its own host</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            style={{ maxWidth: 300, fontSize: 12 }}
            aria-label="Forge base URL"
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            placeholder={DEFAULT_BASE[forge]}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">Token</div>
          <div className="s">Stored on this host, never shown again</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            style={{ maxWidth: 320, fontSize: 12 }}
            type="password"
            autoComplete="off"
            aria-label="Forge token"
            value={token}
            onChange={(event) => setToken(event.target.value)}
            placeholder="Paste a token"
          />
          <button
            type="button"
            className="q-btn primary"
            disabled={busy || token.trim() === "" || baseUrl.trim() === ""}
            onClick={() => void submit()}
          >
            <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {busy ? "Checking…" : "Add account"}
          </button>
        </div>
      </div>
    </div>
  );
}

function DeviceFlow({ baseUrl, onComplete }: { baseUrl: string; onComplete: () => void }) {
  const [flow, setFlow] = useState<ForgeDeviceFlow | null>(null);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cancelled = useRef(false);

  useEffect(() => {
    cancelled.current = false;
    return () => {
      cancelled.current = true;
      if (timer.current) clearTimeout(timer.current);
    };
  }, []);

  const start = async () => {
    setBusy(true);
    setStatus(null);
    try {
      setFlow(await api().startDeviceFlow(baseUrl));
    } catch (cause) {
      pushToast({
        title: "The device flow did not start.",
        description:
          cause instanceof Error
            ? cause.message
            : "Set H4RN_GITHUB_CLIENT_ID on the daemon to enable it.",
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  // Poll on the interval the provider asked for. The flow keeps going only
  // while this component is mounted, so closing Settings stops the polling.
  useEffect(() => {
    if (!flow) return;
    let delay = Math.max(1, flow.interval) * 1000;
    const poll = async () => {
      if (cancelled.current) return;
      try {
        const result = await api().pollDeviceFlow(flow.flow_id);
        if (cancelled.current) return;
        switch (result.state) {
          case "pending":
            delay = Math.max(1, result.interval) * 1000;
            timer.current = setTimeout(() => void poll(), delay);
            break;
          case "slow_down":
            delay = Math.max(1, result.interval) * 1000;
            setStatus("GitHub asked us to slow down; still waiting…");
            timer.current = setTimeout(() => void poll(), delay);
            break;
          case "complete":
            setFlow(null);
            pushToast({
              title: `Connected ${forgeLabel(result.account.forge)}${
                result.account.username ? ` as @${result.account.username}` : ""
              }.`,
              variant: "success",
            });
            onComplete();
            break;
          case "expired":
            setFlow(null);
            setStatus("The code expired. Start again.");
            break;
          case "denied":
            setFlow(null);
            setStatus(result.message);
            break;
        }
      } catch (cause) {
        if (cancelled.current) return;
        setFlow(null);
        setStatus(cause instanceof Error ? cause.message : String(cause));
      }
    };
    timer.current = setTimeout(() => void poll(), delay);
    return () => {
      if (timer.current) clearTimeout(timer.current);
    };
  }, [flow, onComplete]);

  return (
    <div className="q-group">
      <h2>GitHub device flow</h2>
      {flow ? (
        <>
          <p className="desc">
            Enter this code at{" "}
            <a
              className="q-accent"
              href={flow.verification_uri}
              target="_blank"
              rel="noopener noreferrer"
            >
              {flow.verification_uri}
            </a>
            . The code expires in {Math.ceil(flow.expires_in / 60)} minutes.
          </p>
          <div className="q-code-big" style={{ margin: "4px 0 10px" }}>
            {flow.user_code}
          </div>
          <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
            <span className="q-faint" style={{ fontSize: 12 }}>
              <RefreshCwIcon className="q-icon s12" strokeWidth={1.5} aria-hidden /> Waiting for
              authorization…
            </span>
            <button type="button" className="q-btn ghost" onClick={() => setFlow(null)}>
              Stop
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="desc">
            Authorize once in the browser; the daemon keeps the resulting token. Needs
            H4RN_GITHUB_CLIENT_ID on the host.
          </p>
          <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy}
              onClick={() => void start()}
            >
              {busy ? "Starting…" : "Start device flow"}
            </button>
            {status ? (
              <span className="q-warn" style={{ fontSize: 12 }}>
                {status}
              </span>
            ) : null}
          </div>
        </>
      )}
    </div>
  );
}

export function ForgeSettings() {
  const [data, setData] = useState<ForgeAccounts>({ accounts: [], capabilities: [] });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [removing, setRemoving] = useState<ForgeAccount | null>(null);
  const [importForge, setImportForge] = useState<ForgeKind | null>(null);
  const [importing, setImporting] = useState(false);

  const load = useCallback(() => setReload((value) => value + 1), []);

  useEffect(() => {
    void reload;
    let cancelled = false;
    setLoading(true);
    api()
      .accounts()
      .then((result) => {
        if (cancelled) return;
        setData(result);
        setError(null);
        setLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(cause instanceof Error ? cause.message : String(cause));
        setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [reload]);

  const remove = async (account: ForgeAccount) => {
    try {
      await api().removeAccount(account.account_id);
      pushToast({
        title: `Removed ${account.display_name ?? account.username ?? forgeLabel(account.forge)}.`,
        variant: "success",
      });
      load();
    } catch (cause) {
      pushToast({
        title: "The account was not removed.",
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    }
  };

  const importCli = async (forge: ForgeKind) => {
    setImporting(true);
    try {
      const account = await api().importCli(forge, null);
      pushToast({
        title: `Imported ${forgeLabel(forge)}${
          account.username ? ` as @${account.username}` : ""
        }.`,
        variant: "success",
      });
      load();
    } catch (cause) {
      pushToast({
        title: `No ${forge === "github" ? "gh" : "glab"} credential was imported.`,
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setImporting(false);
    }
  };

  const deviceFlow = supportsDeviceFlow(data, DEFAULT_BASE.github);

  return (
    <div className="q-form" style={{ maxWidth: 940 }}>
      <div style={{ display: "flex", alignItems: "flex-start", gap: 12 }}>
        <div style={{ flex: 1 }}>
          <h1>Forges</h1>
          <p className="lead">
            GitHub, GitLab, Gitea and Forgejo accounts used for pull requests, checks and reviews.
            Tokens stay on this host and every call goes through the daemon.
          </p>
        </div>
        <button type="button" className="q-btn secondary" onClick={load} disabled={loading}>
          <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Refresh
        </button>
      </div>

      {error ? (
        <div className="q-group">
          <p className="q-warn">{error}</p>
        </div>
      ) : null}

      <div className="q-group">
        <h2>Accounts</h2>
        {loading ? (
          <div aria-busy="true">
            {[0, 1].map((row) => (
              <div key={row} className="q-skel" style={{ height: 40, marginBottom: 8 }} />
            ))}
          </div>
        ) : (
          <AccountsTable accounts={data.accounts} onRemove={setRemoving} />
        )}
      </div>

      <AddAccountForm onAdded={load} />

      <div className="q-group">
        <h2>Import from a CLI</h2>
        <p className="desc">
          Reads the token {""}
          <span className="q-mono">gh</span> or <span className="q-mono">glab</span> already stored
          for the current user. Nothing is imported until you confirm.
        </p>
        <div style={{ display: "flex", gap: 8 }}>
          <button
            type="button"
            className="q-btn secondary"
            disabled={importing}
            onClick={() => setImportForge("github")}
          >
            Import from gh
          </button>
          <button
            type="button"
            className="q-btn secondary"
            disabled={importing}
            onClick={() => setImportForge("gitlab")}
          >
            Import from glab
          </button>
        </div>
      </div>

      {deviceFlow ? <DeviceFlow baseUrl={DEFAULT_BASE.github} onComplete={load} /> : null}

      <div className="q-group">
        <h2>Least privilege</h2>
        <p className="desc">
          The pull request surface reads contents, checks and reviews, and writes comments and
          replies. It never needs repository administration, secrets or organization write access; a
          token that reports those scopes is flagged above.
        </p>
      </div>

      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing ? forgeLabel(removing.forge) : ""} account?`}
        description={
          removing
            ? `The token for ${removing.base_url} is deleted from this host. Pull requests for that host stop loading until you add an account again.`
            : ""
        }
        confirmLabel="Remove account"
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        onConfirm={() => {
          if (removing) void remove(removing);
          setRemoving(null);
        }}
      />

      <ConfirmDialog
        open={importForge !== null}
        title={importForge === "github" ? "Import the gh token?" : "Import the glab token?"}
        description={
          importForge
            ? `h4rn reads the credential ${importForge === "github" ? "gh" : "glab"} stores for the signed-in user and saves it on this host. Only do this if you trust this machine.`
            : ""
        }
        confirmLabel="Import token"
        onOpenChange={(open) => {
          if (!open) setImportForge(null);
        }}
        onConfirm={() => {
          if (importForge) void importCli(importForge);
          setImportForge(null);
        }}
      />
    </div>
  );
}
