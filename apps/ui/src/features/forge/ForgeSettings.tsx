import { KeyRoundIcon, PlusIcon, RefreshCwIcon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ChipSelect } from "@/components/common/ChipSelect";
import { ConfirmDialog } from "@/components/common/ConfirmDialog";
import { type ForgeAccounts, ForgeApi, forgeLabel } from "@/features/forge/store";
import type {
  ForgeAccount,
  ForgeCredentialSource,
  ForgeDeviceFlow,
  ForgeKind,
  ForgeRemote,
  ForgeRepoRef,
} from "@/gen";
import { formatList, formatRelativeTime, i18n } from "@/lib/i18n";
import { getClient, pushToast, useApp } from "@/lib/store/appStore";

const api = () => new ForgeApi((request) => getClient().request(request));

const FORGES: ForgeKind[] = ["github", "gitlab", "gitea", "forgejo", "bitbucket", "azure_devops"];

const DEFAULT_BASE: Record<ForgeKind, string> = {
  github: "https://github.com",
  gitlab: "https://gitlab.com",
  gitea: "https://gitea.example.com",
  forgejo: "https://forgejo.example.com",
  bitbucket: "https://bitbucket.org",
  azure_devops: "https://dev.azure.com",
};

/** OAuth device flow is a GitHub dotcom feature in this build. */
function supportsDeviceFlow(accounts: ForgeAccounts, base: string): boolean {
  const github = accounts.capabilities.find((entry) => entry.forge === "github");
  return (
    Boolean(github?.capabilities.device_flow) && base.replace(/\/$/, "") === "https://github.com"
  );
}

function sourceLabel(source: ForgeCredentialSource): string {
  return i18n.t(`settings:forges.source.${source}`);
}

/** "@name" after a forge, when the account reported one. */
function asUser(username: string | null | undefined): string {
  return username ? i18n.t("settings:forges.asUser", { username }) : "";
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
  const { t } = useTranslation(["settings", "common"]);
  if (accounts.length === 0) {
    return <p className="q-muted text-sm mt-1 mx-0 mb-4">{t("forges.noAccounts")}</p>;
  }
  return (
    <table className="q-tbl">
      <thead>
        <tr>
          <th>{t("forges.table.account")}</th>
          <th>{t("forges.table.host")}</th>
          <th>{t("forges.table.scopes")}</th>
          <th>{t("forges.table.added")}</th>
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
                    <span className="font-medium">
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
                  <span className="q-faint">{t("forges.notReported")}</span>
                ) : broad.length > 0 ? (
                  <span
                    className="q-warn"
                    title={t("forges.broadScopesTitle", { scopes: formatList(broad) })}
                  >
                    {formatList(broad, "conjunction", "narrow")} · {t("forges.broaderThanNeeded")}
                  </span>
                ) : (
                  <span className="q-muted">
                    {formatList(account.scopes, "conjunction", "narrow")}
                  </span>
                )}
              </td>
              <td className="q-muted">
                {formatRelativeTime(account.created_at, Date.now(), {
                  numeric: "auto",
                  style: "short",
                })}
              </td>
              <td className="r pr-0">
                <button
                  type="button"
                  className="q-btn danger"
                  aria-label={t("forges.removeLabel", {
                    name: account.display_name ?? account.username ?? forgeLabel(account.forge),
                  })}
                  onClick={() => onRemove(account)}
                >
                  {t("common:remove")}
                </button>
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function AddAccountForm({
  onAdded,
  repositories,
}: {
  onAdded: () => void;
  repositories: ForgeRemote[];
}) {
  const { t } = useTranslation(["settings", "common"]);
  const [forge, setForge] = useState<ForgeKind>("github");
  const [baseUrl, setBaseUrl] = useState(DEFAULT_BASE.github);
  const [username, setUsername] = useState("");
  const [token, setToken] = useState("");
  const [repositoryKey, setRepositoryKey] = useState("");
  const [busy, setBusy] = useState(false);

  const changeForge = (next: ForgeKind) => {
    // Keep a deliberate self-hosted URL, replace a default one.
    setBaseUrl((current) =>
      current === DEFAULT_BASE[forge] || current.trim() === "" ? DEFAULT_BASE[next] : current,
    );
    setForge(next);
  };

  const missingOrganization =
    forge === "azure_devops" && baseUrl.trim().replace(/\/$/, "") === "https://dev.azure.com";

  // The detected workspace remotes for this host. Bitbucket access tokens need
  // one because they cannot read `/user` or `/user/workspaces`; with several
  // checkouts the person picks which repository proves the token.
  const normalizedBase = baseUrl.trim().replace(/\/$/, "");
  const candidateRepos: ForgeRepoRef[] =
    forge === "bitbucket"
      ? repositories
          .filter(
            (remote) =>
              remote.repo.forge === "bitbucket" &&
              remote.repo.base_url.replace(/\/$/, "") === normalizedBase,
          )
          .map((remote) => remote.repo)
      : [];
  const repositoryName = (repo: ForgeRepoRef) => `${repo.owner}/${repo.repo}`;
  const detectedRepo: ForgeRepoRef | null =
    candidateRepos.find((repo) => repositoryName(repo) === repositoryKey) ??
    candidateRepos[0] ??
    null;
  const accessTokenWithoutRepository =
    forge === "bitbucket" && username.trim() === "" && detectedRepo === null;

  const submit = async () => {
    if (
      token.trim() === "" ||
      baseUrl.trim() === "" ||
      busy ||
      missingOrganization ||
      accessTokenWithoutRepository
    ) {
      return;
    }
    setBusy(true);
    try {
      const account = await api().addAccount(
        forge,
        normalizedBase,
        token.trim(),
        forge === "bitbucket" ? username.trim() || null : null,
        detectedRepo,
      );
      // The token is written once and never read back, and the email is
      // cleared so the next account starts from an empty form.
      setToken("");
      setUsername("");
      pushToast({
        title: t("forges.toast.added", {
          forge: forgeLabel(forge),
          user: asUser(account.username),
        }),
        variant: "success",
      });
      onAdded();
    } catch (cause) {
      pushToast({
        title: t("forges.toast.addFailed"),
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="q-group">
      <h2>{t("forges.add.title")}</h2>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("forges.add.forge")}</div>
          <div className="s">
            {t(`forges.scopes.${forge}`)}
            {forge === "bitbucket" ? ` ${t("forges.add.bitbucketNote")}` : null}
          </div>
        </div>
        <div className="ctl">
          <ChipSelect
            label={t("forges.add.forge")}
            value={forge}
            options={FORGES.map((kind) => ({ value: kind, label: forgeLabel(kind) }))}
            onChange={changeForge}
          />
        </div>
      </div>
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("forges.add.baseUrl")}</div>
          <div className="s">
            {forge === "azure_devops"
              ? t("forges.add.baseUrlAzure")
              : forge === "bitbucket"
                ? t("forges.add.baseUrlBitbucket")
                : t("forges.add.baseUrlSelfHosted")}
          </div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            aria-label={t("forges.add.baseUrlLabel")}
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            placeholder={DEFAULT_BASE[forge]}
          />
        </div>
      </div>
      {forge === "bitbucket" ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("forges.add.email")}</div>
            <div className="s">{t("forges.add.emailHint")}</div>
          </div>
          <div className="ctl">
            <input
              className="q-input q-mono"
              autoComplete="username"
              aria-label={t("forges.add.email")}
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              placeholder="you@example.com"
            />
            {accessTokenWithoutRepository ? (
              <span className="q-warn text-xs block">{t("forges.add.needsRepository")}</span>
            ) : null}
          </div>
        </div>
      ) : null}
      {forge === "bitbucket" && candidateRepos.length > 1 ? (
        <div className="q-field">
          <div className="lab">
            <div className="t">{t("forges.add.validateAgainst")}</div>
            <div className="s">{t("forges.add.validateAgainstHint")}</div>
          </div>
          <div className="ctl">
            <ChipSelect
              label={t("forges.add.validateAgainstLabel")}
              value={detectedRepo ? repositoryName(detectedRepo) : ""}
              options={candidateRepos.map((repo) => ({
                value: repositoryName(repo),
                label: repositoryName(repo),
              }))}
              onChange={setRepositoryKey}
            />
          </div>
        </div>
      ) : null}
      <div className="q-field">
        <div className="lab">
          <div className="t">{t("forges.add.token")}</div>
          <div className="s">{t("forges.add.tokenHint")}</div>
        </div>
        <div className="ctl">
          <input
            className="q-input q-mono"
            type="password"
            autoComplete="off"
            aria-label={t("forges.add.tokenLabel")}
            value={token}
            onChange={(event) => setToken(event.target.value)}
            placeholder={t("forges.add.tokenPlaceholder")}
          />
          <button
            type="button"
            className="q-btn primary"
            disabled={
              busy ||
              token.trim() === "" ||
              baseUrl.trim() === "" ||
              missingOrganization ||
              accessTokenWithoutRepository
            }
            onClick={() => void submit()}
          >
            <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {busy ? t("common:checking") : t("forges.add.button")}
          </button>
          {missingOrganization ? (
            <span className="q-warn text-xs">{t("forges.add.missingOrganization")}</span>
          ) : null}
        </div>
      </div>
    </div>
  );
}

function DeviceFlow({ baseUrl, onComplete }: { baseUrl: string; onComplete: () => void }) {
  const { t } = useTranslation(["settings", "common"]);
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
        title: t("forges.device.startFailed"),
        description: cause instanceof Error ? cause.message : t("forges.device.needsClientId"),
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
            setStatus(i18n.t("settings:forges.device.slowDown"));
            timer.current = setTimeout(() => void poll(), delay);
            break;
          case "complete":
            setFlow(null);
            pushToast({
              title: i18n.t("settings:forges.toast.connected", {
                forge: forgeLabel(result.account.forge),
                user: asUser(result.account.username),
              }),
              variant: "success",
            });
            onComplete();
            break;
          case "expired":
            setFlow(null);
            setStatus(i18n.t("settings:forges.device.expired"));
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
      <h2>{t("forges.device.title")}</h2>
      {flow ? (
        <>
          <p className="desc">
            <Trans
              t={t}
              i18nKey="forges.device.enterCode"
              count={Math.ceil(flow.expires_in / 60)}
              values={{ uri: flow.verification_uri }}
              components={{
                link: (
                  // biome-ignore lint/a11y/useAnchorContent: Trans fills the link with the URI
                  <a
                    className="q-accent"
                    href={flow.verification_uri}
                    target="_blank"
                    rel="noopener noreferrer"
                  />
                ),
              }}
            />
          </p>
          <div className="q-code-big mt-1 mx-0 mb-2.5">{flow.user_code}</div>
          <div className="flex gap-1.5 items-center">
            <span className="q-faint" style={{ fontSize: 12 }}>
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />{" "}
              {t("forges.device.waiting")}
            </span>
            <button type="button" className="q-btn ghost" onClick={() => setFlow(null)}>
              {t("forges.device.stop")}
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="desc">{t("forges.device.hint")}</p>
          <div className="flex gap-2 items-center">
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy}
              onClick={() => void start()}
            >
              {busy ? t("forges.device.starting") : t("forges.device.start")}
            </button>
            {status ? <span className="q-warn text-xs">{status}</span> : null}
          </div>
        </>
      )}
    </div>
  );
}

export function ForgeSettings() {
  const { t } = useTranslation(["settings", "common"]);
  const threads = useApp((state) => state.threads);
  const [repositories, setRepositories] = useState<ForgeRemote[]>([]);
  const [data, setData] = useState<ForgeAccounts>({
    accounts: [],
    capabilities: [],
    linear: null,
  });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [removing, setRemoving] = useState<ForgeAccount | null>(null);
  const [importForge, setImportForge] = useState<ForgeKind | null>(null);
  const [importing, setImporting] = useState(false);

  const load = useCallback(() => setReload((value) => value + 1), []);

  const workspaceKey = useMemo(
    () =>
      [
        ...new Set(
          Object.values(threads)
            .map((thread) => thread.workspace_path)
            .filter(Boolean),
        ),
      ]
        .sort()
        .join("\n"),
    [threads],
  );

  // The detected workspace remotes let a Bitbucket access token be validated
  // against its repository even though it cannot read the account.
  useEffect(() => {
    if (!workspaceKey) {
      setRepositories([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      const found: ForgeRemote[] = [];
      for (const workspace of workspaceKey.split("\n")) {
        try {
          found.push(...(await api().detectRepo(workspace)));
        } catch {
          // A workspace without a forge remote is not an error.
        }
      }
      if (cancelled) return;
      setRepositories([
        ...new Map(
          found.map((remote) => [
            `${remote.repo.forge}:${remote.repo.base_url}:${remote.repo.owner}/${remote.repo.repo}`,
            remote,
          ]),
        ).values(),
      ]);
    })();
    return () => {
      cancelled = true;
    };
  }, [workspaceKey]);

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
        title: t("forges.toast.removed", {
          name: account.display_name ?? account.username ?? forgeLabel(account.forge),
        }),
        variant: "success",
      });
      load();
    } catch (cause) {
      pushToast({
        title: t("forges.toast.removeFailed"),
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
        title: t("forges.toast.imported", {
          forge: forgeLabel(forge),
          user: asUser(account.username),
        }),
        variant: "success",
      });
      load();
    } catch (cause) {
      pushToast({
        title: t("forges.toast.importFailed", { cli: forge === "github" ? "gh" : "glab" }),
        description: cause instanceof Error ? cause.message : String(cause),
        variant: "error",
      });
    } finally {
      setImporting(false);
    }
  };

  const deviceFlow = supportsDeviceFlow(data, DEFAULT_BASE.github);

  return (
    <div className="q-form wide">
      <div className="q-form-head">
        <div className="flex-1">
          <h1>{t("forges.title")}</h1>
          <p className="lead">{t("forges.lead")}</p>
        </div>
        <button type="button" className="q-btn secondary" onClick={load} disabled={loading}>
          <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          {t("common:refresh")}
        </button>
      </div>

      {error ? (
        <div className="q-group">
          <p className="q-warn">{error}</p>
        </div>
      ) : null}

      <div className="q-group">
        <h2>{t("forges.accounts")}</h2>
        {loading ? (
          <div aria-busy="true">
            {[0, 1].map((row) => (
              <div key={row} className="q-skel h-10 mb-2" />
            ))}
          </div>
        ) : (
          <AccountsTable accounts={data.accounts} onRemove={setRemoving} />
        )}
      </div>

      <AddAccountForm onAdded={load} repositories={repositories} />

      <div className="q-group">
        <h2>{t("forges.import.title")}</h2>
        <p className="desc">
          <Trans
            t={t}
            i18nKey="forges.import.hint"
            components={{ code: <span className="q-mono" /> }}
          />
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            className="q-btn secondary"
            disabled={importing}
            onClick={() => setImportForge("github")}
          >
            {t("forges.import.fromGh")}
          </button>
          <button
            type="button"
            className="q-btn secondary"
            disabled={importing}
            onClick={() => setImportForge("gitlab")}
          >
            {t("forges.import.fromGlab")}
          </button>
        </div>
      </div>

      {deviceFlow ? <DeviceFlow baseUrl={DEFAULT_BASE.github} onComplete={load} /> : null}

      <div className="q-group">
        <h2>{t("forges.leastPrivilege")}</h2>
        <p className="desc">{t("forges.leastPrivilegeHint")}</p>
      </div>

      <ConfirmDialog
        open={removing !== null}
        title={t("forges.remove.title", { forge: removing ? forgeLabel(removing.forge) : "" })}
        description={removing ? t("forges.remove.description", { url: removing.base_url }) : ""}
        confirmLabel={t("forges.remove.button")}
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
        title={t("forges.import.confirmTitle", { cli: importForge === "github" ? "gh" : "glab" })}
        description={
          importForge
            ? t("forges.import.confirmDescription", {
                cli: importForge === "github" ? "gh" : "glab",
              })
            : ""
        }
        confirmLabel={t("forges.import.confirmButton")}
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
