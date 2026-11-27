import {
  CircleDotIcon,
  ExternalLinkIcon,
  LinkIcon,
  LoaderCircleIcon,
  MessageSquareIcon,
  SearchIcon,
  UnlinkIcon,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ChipSelect } from "@/components/common/ChipSelect";
import { Markdown } from "@/components/thread/Markdown";
import {
  Menu,
  MenuCheckboxItem,
  MenuContent,
  MenuFooter,
  MenuGroup,
  MenuRadioGroup,
  MenuRadioItem,
  MenuTrigger,
} from "@/components/ui/menu";
import { accountsForRepo, forgeLabel } from "@/features/forge/store";
import { useProjects } from "@/features/projects/store";
import type {
  ForgeAccount,
  ForgeCapabilities,
  ForgeCapabilityEntry,
  ForgeIssue,
  ForgeIssueCapabilities,
  ForgeIssueComment,
  ForgeIssueFilter,
  ForgeIssueState,
  ForgeIssueUpdate,
  ForgeRepoRef,
  LinearAccount,
  LinearIssue,
  LinearProjectTeam,
  ThreadIssue,
} from "@/gen";
import { formatList, formatNumber } from "@/lib/i18n";
import { getClient, pushToast, useApp } from "@/lib/store/appStore";

import {
  forgeIssueRow,
  groupLabel,
  ISSUE_FILTERS,
  IssueApi,
  type IssueRow,
  issueCapabilitiesFor,
  issueError,
  issueKey,
  issueStateLabel,
  linearIssueRow,
  linearStateIsClosed,
  relativeIssueAge,
  type TrackerGroup,
  trackerKey,
} from "./store";
import { notifyThreadIssueLinks } from "./threadLinks";

const api = () => new IssueApi((request) => getClient().request(request));

const HIDDEN_PROJECTS = "h4rn:issues:hidden-projects";
const PROJECT_ACCOUNTS = "h4rn:issues:project-accounts";

function readHidden(): Set<string> {
  if (typeof window === "undefined") return new Set();
  try {
    const raw = window.localStorage.getItem(HIDDEN_PROJECTS);
    return new Set(raw ? (JSON.parse(raw) as string[]) : []);
  } catch {
    return new Set();
  }
}

function writeHidden(hidden: Set<string>): void {
  try {
    window.localStorage.setItem(HIDDEN_PROJECTS, JSON.stringify([...hidden]));
  } catch {
    // A blocked storage still leaves the toggle working for this session.
  }
}

function readProjectAccounts(): Record<string, string> {
  if (typeof window === "undefined") return {};
  try {
    const raw = window.localStorage.getItem(PROJECT_ACCOUNTS);
    return raw ? (JSON.parse(raw) as Record<string, string>) : {};
  } catch {
    return {};
  }
}

function writeProjectAccounts(choices: Record<string, string>): void {
  try {
    window.localStorage.setItem(PROJECT_ACCOUNTS, JSON.stringify(choices));
  } catch {
    // A blocked storage still leaves the choice working for this session.
  }
}

type Source =
  | {
      kind: "forge";
      accountId: string;
      repo: ForgeRepoRef;
      page: number;
      hasMore: boolean;
    }
  | {
      kind: "linear";
      accountId: string;
      projectId: string;
      /** The team the project's issues belong to; part of the listing's identity. */
      teamId: string;
      cursor: string | null;
      hasMore: boolean;
    };

type Group = TrackerGroup & {
  sources: Source[];
  capabilities: ForgeIssueCapabilities;
  /** The listing generation this group's rows came from. */
  generation: number;
};

function sortRows(rows: IssueRow[]): IssueRow[] {
  return [...rows].sort(
    (a, b) => new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
  );
}

function mergeRows(existing: IssueRow[], incoming: IssueRow[]): IssueRow[] {
  const seen = new Set(existing.map((row) => row.key));
  return sortRows([...existing, ...incoming.filter((row) => !seen.has(row.key))]);
}

/** Whether a filter means the same thing to every tracker currently shown. */
function filterSupported(groups: Group[], filter: ForgeIssueFilter): boolean {
  if (filter === "open" || filter === "closed" || filter === "all") return true;
  if (groups.length === 0) return true;
  return groups.every((group) => {
    switch (filter) {
      case "assigned":
        return group.capabilities.assigned_filter;
      case "created":
        return group.capabilities.created_filter;
      case "mentioning":
        return group.capabilities.mentioning_filter;
      default:
        return true;
    }
  });
}

/**
 * Issues from every project's tracker, grouped by account.
 *
 * Every row comes from the daemon's tracker read. A failed read is shown in
 * its group and never replaced by remembered rows. A tracker the daemon says
 * it cannot read is not queried, and filters or actions it cannot honour are
 * not offered.
 */
export function IssuesPage() {
  const { t } = useTranslation(["issues", "common"]);
  const projects = useProjects((state) => state.projects);
  const selectedThreadId = useApp((state) => state.ui.selectedThreadId);
  const connection = useApp((state) => state.connection.status);

  const [accounts, setAccounts] = useState<ForgeAccount[]>([]);
  const [accountsReady, setAccountsReady] = useState(false);
  // Whether the forge account list actually arrived. A failed load must not
  // turn into "no account matches this repository": that note is only true
  // when the list was read.
  const [forgeAccountsKnown, setForgeAccountsKnown] = useState(false);
  const [accountsError, setAccountsError] = useState<string | null>(null);
  const [forgeCapabilities, setForgeCapabilities] = useState<ForgeCapabilityEntry[]>([]);
  // Linear is not a forge, so its capabilities come in their own field.
  const [linearCapabilities, setLinearCapabilities] = useState<ForgeCapabilities | null>(null);
  const [linearAccounts, setLinearAccounts] = useState<LinearAccount[]>([]);
  const [mappings, setMappings] = useState<LinearProjectTeam[]>([]);
  const [repos, setRepos] = useState<Record<string, ForgeRepoRef>>({});
  const [hidden, setHidden] = useState<Set<string>>(() => readHidden());
  const [accountChoices, setAccountChoices] = useState<Record<string, string>>(() =>
    readProjectAccounts(),
  );
  const [filter, setFilter] = useState<ForgeIssueFilter>("open");
  const [searchInput, setSearchInput] = useState("");
  const [search, setSearch] = useState("");
  const [groups, setGroups] = useState<Group[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // The chosen row keeps the capabilities it was opened with, so an issue
  // that a filter change or a close removes from the list keeps its comment
  // and edit actions.
  const [selected, setSelected] = useState<{
    row: IssueRow;
    capabilities: ForgeIssueCapabilities;
  } | null>(null);
  const [linked, setLinked] = useState<Set<string>>(new Set());
  const [refresh, setRefresh] = useState(0);
  // Bumped whenever the query changes, so a page that arrives late for a
  // superseded search or filter is discarded instead of spliced in.
  const queryToken = useRef(0);
  // The generation the rendered groups belong to, in state so the Load more
  // buttons disable as soon as a listing is superseded.
  const [generation, setGeneration] = useState(0);
  // The set of sources the current pages were read from. A page that arrives
  // after a project was hidden or an account changed belongs to a listing that
  // no longer exists and is dropped.
  const sourceSignature = useRef<string | null>(null);

  // Choosing the filter or search already applied is not a new query: it
  // must leave the list alone instead of blanking it with no effect to rerun.
  const changeFilter = (next: ForgeIssueFilter) => {
    if (next === filter) return;
    queryToken.current += 1;
    setGeneration(queryToken.current);
    setGroups([]);
    setFilter(next);
  };
  const runSearch = (term: string) => {
    if (term === search) return;
    queryToken.current += 1;
    setGeneration(queryToken.current);
    setGroups([]);
    setSearch(term);
  };

  // A cold deep link mounts before the socket handshake finishes.
  useEffect(() => {
    if (connection === "connecting") return;
    let cancelled = false;
    setAccountsError(null);
    // Each discovery can fail on its own: a Linear sign-in that expired must
    // not hide the forge accounts that did load, and vice versa.
    Promise.allSettled([api().forgeAccounts(), api().linearAccounts(), api().projectTeams()]).then(
      ([forge, linear, teams]) => {
        if (cancelled) return;
        const failures: string[] = [];
        if (forge.status === "fulfilled") {
          setAccounts(forge.value.accounts);
          setForgeCapabilities(forge.value.capabilities);
          setLinearCapabilities(forge.value.linear);
          setForgeAccountsKnown(true);
        } else {
          failures.push(issueError(forge.reason));
        }
        if (linear.status === "fulfilled") {
          setLinearAccounts(linear.value);
        } else {
          failures.push(issueError(linear.reason));
        }
        if (teams.status === "fulfilled") {
          setMappings(teams.value);
        } else {
          failures.push(issueError(teams.reason));
        }
        setAccountsReady(true);
        setAccountsError(failures[0] ?? null);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [connection]);

  // Detect one forge remote per project; a project without one simply has no
  // forge group.
  useEffect(() => {
    if (connection === "connecting" || projects.length === 0) return;
    let cancelled = false;
    void (async () => {
      const found: Record<string, ForgeRepoRef> = {};
      for (const project of projects) {
        try {
          const repo = await api().detectRepo(project.path);
          if (repo) found[project.id] = repo;
        } catch {
          // A workspace without a forge remote is not an error.
        }
      }
      if (!cancelled) setRepos(found);
    })();
    return () => {
      cancelled = true;
    };
  }, [projects, connection]);

  useEffect(() => {
    void refresh;
    if (!selectedThreadId) {
      setLinked(new Set());
      return;
    }
    let cancelled = false;
    api()
      .threadIssues(selectedThreadId)
      .then((issues: ThreadIssue[]) => {
        if (!cancelled) {
          setLinked(new Set(issues.map((issue) => issueKey(issue.scope, issue.issue_id))));
        }
      })
      .catch(() => {
        if (!cancelled) setLinked(new Set());
      });
    return () => {
      cancelled = true;
    };
  }, [selectedThreadId, refresh]);

  // Build the account groups from visible projects, then read page one.
  useEffect(() => {
    void refresh;
    if (connection === "connecting") return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    const visible = projects.filter((project) => !hidden.has(project.id));
    const forgeSources: Source[] = [];
    for (const project of visible) {
      const repo = repos[project.id];
      if (!repo) continue;
      // Two accounts on one host are both valid; the person's choice for the
      // project decides, and only the named account is read.
      const candidates = accountsForRepo(accounts, repo);
      const account =
        candidates.find((candidate) => candidate.account_id === accountChoices[project.id]) ??
        candidates[0];
      if (!account) continue;
      const capabilities = issueCapabilitiesFor(
        "forge",
        account,
        forgeCapabilities,
        linearCapabilities,
      );
      if (!capabilities.read) continue;
      forgeSources.push({
        kind: "forge",
        accountId: account.account_id,
        repo,
        page: 1,
        hasMore: false,
      });
    }
    const linearSources: Source[] = [];
    // Without Linear's capabilities there is nothing safe to ask it for: the
    // field is absent unless the daemon knows Linear can read issues.
    if (linearCapabilities?.issues.read) {
      for (const project of visible) {
        const mapping = mappings.find((candidate) => candidate.project_id === project.id);
        if (mapping) {
          linearSources.push({
            kind: "linear",
            accountId: mapping.account_id,
            projectId: project.id,
            teamId: mapping.team_id,
            cursor: null,
            hasMore: false,
          });
        }
      }
    }
    const sources = [...forgeSources, ...linearSources];
    // A different set of sources starts a new generation. Pages still in
    // flight for the old set are discarded when they land, so a hidden
    // project's delayed page cannot add its rows back to the list. The
    // signature names the account and, for Linear, the team as well: either
    // one changing means the rows on screen belong to a listing that no
    // longer exists.
    const signature = sources.map(sourceSignatureKey).join("|");
    if (sourceSignature.current !== signature) {
      sourceSignature.current = signature;
      queryToken.current += 1;
      setGeneration(queryToken.current);
    }
    const token = queryToken.current;
    if (sources.length === 0) {
      setGroups([]);
      setLoading(false);
      return;
    }
    void Promise.all(
      sources.map(async (source) => {
        try {
          if (source.kind === "forge") {
            const result = await api().listForgeIssues(
              source.accountId,
              source.repo,
              filter,
              1,
              search || null,
            );
            return {
              source: { ...source, hasMore: result.hasMore },
              rows: result.issues.map((issue) => forgeIssueRow(source.accountId, issue)),
              readOnly: result.readOnly,
              error: null as string | null,
            };
          }
          const result = await api().listLinearIssues(
            source.projectId,
            filter,
            null,
            search || null,
          );
          return {
            source: {
              ...source,
              cursor: result.nextCursor,
              hasMore: result.nextCursor !== null,
            },
            rows: result.issues.map((issue) => linearIssueRow(source.accountId, issue)),
            readOnly: result.readOnly,
            error: null as string | null,
          };
        } catch (cause) {
          return {
            source,
            rows: [] as IssueRow[],
            readOnly: true,
            error: issueError(cause),
          };
        }
      }),
    ).then((results) => {
      if (cancelled || token !== queryToken.current) return;
      const byAccount = new Map<string, Group>();
      for (const result of results) {
        const kind = result.source.kind;
        const key = trackerKey(kind, result.source.accountId);
        const account =
          kind === "forge"
            ? accounts.find((candidate) => candidate.account_id === result.source.accountId)
            : linearAccounts.find((candidate) => candidate.account_id === result.source.accountId);
        if (!account) continue;
        const group =
          byAccount.get(key) ??
          ({
            key,
            label: groupLabel(kind, account),
            accountId: result.source.accountId,
            kind,
            rows: [],
            hasMore: false,
            loading: false,
            error: null,
            sources: [],
            generation: token,
            capabilities: issueCapabilitiesFor(
              kind,
              account,
              forgeCapabilities,
              linearCapabilities,
            ),
          } satisfies Group);
        group.rows = mergeRows(group.rows, result.rows);
        group.hasMore = group.hasMore || result.source.hasMore;
        group.error = group.error ?? result.error;
        group.sources.push(result.source);
        byAccount.set(key, group);
      }
      setGroups([...byAccount.values()]);
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [
    accounts,
    forgeCapabilities,
    linearCapabilities,
    linearAccounts,
    mappings,
    projects,
    repos,
    hidden,
    accountChoices,
    filter,
    search,
    refresh,
    connection,
  ]);

  // A filter one of the shown trackers cannot honour is not offered; if it
  // was selected before the tracker changed, fall back to open.
  const canMention = filterSupported(groups, "mentioning");
  const canAssign = filterSupported(groups, "assigned");
  const canCreate = filterSupported(groups, "created");
  useEffect(() => {
    if (
      (filter === "mentioning" && !canMention) ||
      (filter === "assigned" && !canAssign) ||
      (filter === "created" && !canCreate)
    ) {
      queryToken.current += 1;
      setGeneration(queryToken.current);
      setGroups([]);
      setFilter("open");
    }
  }, [filter, canMention, canAssign, canCreate]);

  const loadMore = useCallback(
    async (group: Group) => {
      // A group from an earlier listing is superseded: page two of it would
      // belong to rows the page no longer shows.
      if (group.generation !== queryToken.current) return;
      const token = queryToken.current;
      const active = group.sources.filter((source) => source.hasMore);
      if (active.length === 0) return;
      setGroups((current) =>
        current.map((item) => (item.key === group.key ? { ...item, loading: true } : item)),
      );
      const additions = await Promise.all(
        active.map(async (source) => {
          try {
            if (source.kind === "forge") {
              const next = source.page + 1;
              const result = await api().listForgeIssues(
                source.accountId,
                source.repo,
                filter,
                next,
                search || null,
              );
              return {
                rows: result.issues.map((issue) => forgeIssueRow(source.accountId, issue)),
                source: { ...source, page: next, hasMore: result.hasMore } as Source,
                error: null as string | null,
              };
            }
            const result = await api().listLinearIssues(
              source.projectId,
              filter,
              source.cursor,
              search || null,
            );
            return {
              rows: result.issues.map((issue) => linearIssueRow(source.accountId, issue)),
              source: {
                ...source,
                cursor: result.nextCursor,
                hasMore: result.nextCursor !== null,
              } as Source,
              error: null as string | null,
            };
          } catch (cause) {
            return {
              rows: [] as IssueRow[],
              // A failed page is temporary: the source keeps its continuation
              // so Load more can try the same page again.
              source,
              error: issueError(cause),
            };
          }
        }),
      );
      if (token !== queryToken.current) return;
      setGroups((current) =>
        current.map((item) => {
          if (item.key !== group.key) return item;
          const rows = additions.reduce(
            (all, addition) => mergeRows(all, addition.rows),
            item.rows,
          );
          const errors = additions
            .map((addition) => addition.error)
            .filter((message): message is string => Boolean(message));
          const updated = new Map(
            additions.map((addition) => [sourceKey(addition.source), addition.source]),
          );
          const sources = item.sources.map((source) => updated.get(sourceKey(source)) ?? source);
          return {
            ...item,
            rows,
            hasMore: sources.some((source) => source.hasMore),
            loading: false,
            error: errors[0] ?? null,
            sources,
          };
        }),
      );
    },
    [filter, search],
  );

  const toggleProject = (projectId: string) => {
    setHidden((current) => {
      const next = new Set(current);
      if (next.has(projectId)) next.delete(projectId);
      else next.add(projectId);
      writeHidden(next);
      return next;
    });
  };

  const filters = ISSUE_FILTERS.filter((key) => filterSupported(groups, key));
  const threadId = selectedThreadId;
  const chooseAccount = (projectId: string, accountId: string) => {
    setAccountChoices((current) => {
      const next = { ...current, [projectId]: accountId };
      writeProjectAccounts(next);
      return next;
    });
  };
  // Bitbucket Cloud's tracker was removed by Atlassian; each such project
  // says so instead of sending a request that cannot work.
  const unavailableProjects = projects
    .filter((project) => !hidden.has(project.id))
    .filter((project) => repos[project.id]?.forge === "bitbucket");
  // A project whose repository no connected account serves is named like the
  // pull request page names it, instead of silently missing from the list.
  // Wait for the account list before saying an account is missing, and say
  // nothing at all when that list failed to load: an unread list is not an
  // empty one. Bitbucket is already explained by its own note.
  const unmatchedProjects =
    !accountsReady || !forgeAccountsKnown
      ? []
      : projects
          .filter((project) => !hidden.has(project.id))
          .filter((project) => {
            const repo = repos[project.id];
            return (
              repo !== undefined &&
              repo.forge !== "bitbucket" &&
              accountsForRepo(accounts, repo).length === 0
            );
          });

  return (
    <main className="q-main" aria-label={t("title")}>
      <header className="q-page-head q-issues-head">
        <h1>{t("title")}</h1>
        <form
          className="q-issues-search"
          aria-label={t("search.label")}
          onSubmit={(event) => {
            event.preventDefault();
            runSearch(searchInput.trim());
          }}
        >
          <SearchIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <input
            aria-label={t("search.label")}
            placeholder={t("search.placeholder")}
            value={searchInput}
            onChange={(event) => setSearchInput(event.target.value)}
          />
          {search ? (
            <button
              type="button"
              className="q-link"
              onClick={() => {
                setSearchInput("");
                runSearch("");
              }}
            >
              {t("search.clear")}
            </button>
          ) : null}
        </form>
        <div className="q-seg" role="tablist" aria-label={t("filters.label")}>
          {filters.map((key) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={filter === key}
              onClick={() => changeFilter(key)}
            >
              {t(`filters.${key}`)}
            </button>
          ))}
        </div>
        <span className="grow" />
        <Menu>
          <MenuTrigger className="q-btn ghost" aria-label={t("projects.label")}>
            {t("projects.label")}
          </MenuTrigger>
          <MenuContent align="end" className="w-64">
            {projects.length === 0 ? (
              <MenuFooter>{t("projects.none")}</MenuFooter>
            ) : (
              projects.map((project) => {
                const repo = repos[project.id];
                const candidates = repo ? accountsForRepo(accounts, repo) : [];
                const chosen =
                  candidates.find(
                    (candidate) => candidate.account_id === accountChoices[project.id],
                  ) ?? candidates[0];
                return (
                  <MenuGroup
                    key={project.id}
                    aria-label={project.name}
                    className="not-first:-mx-1 not-first:mt-1 not-first:border-t not-first:border-border not-first:px-1 not-first:pt-1"
                  >
                    <MenuCheckboxItem
                      value={project.id}
                      checked={!hidden.has(project.id)}
                      onCheckedChange={() => toggleProject(project.id)}
                    >
                      {project.name}
                    </MenuCheckboxItem>
                    {candidates.length > 1 ? (
                      <MenuRadioGroup
                        value={chosen?.account_id ?? ""}
                        onValueChange={(accountId: string) => chooseAccount(project.id, accountId)}
                        aria-label={t("projects.accountFor", { name: project.name })}
                      >
                        {candidates.map((candidate) => (
                          <MenuRadioItem key={candidate.account_id} value={candidate.account_id}>
                            {candidate.username ?? candidate.display_name ?? candidate.account_id}
                          </MenuRadioItem>
                        ))}
                      </MenuRadioGroup>
                    ) : null}
                  </MenuGroup>
                );
              })
            )}
          </MenuContent>
        </Menu>
      </header>

      <div className="q-issues-body">
        <div className="q-issues-list">
          {loading ? (
            <p className="q-faint q-issues-note" role="status">
              {t("reading")}
            </p>
          ) : null}
          {accountsError ? (
            <div className="q-issues-note q-danger" role="alert">
              {accountsError}
            </div>
          ) : null}
          {error ? (
            <div className="q-issues-note q-danger" role="alert">
              {error}
            </div>
          ) : null}
          {unavailableProjects.map((project) => (
            <div key={project.id} className="q-issues-note" role="note">
              <Trans
                t={t}
                i18nKey="bitbucket"
                values={{ name: project.name }}
                components={{ strong: <strong /> }}
              />
            </div>
          ))}
          {unmatchedProjects.map((project) => {
            const repo = repos[project.id];
            if (!repo) return null;
            return (
              <div key={project.id} className="q-issues-note" role="note">
                <Trans
                  t={t}
                  i18nKey="noAccount"
                  values={{ name: project.name, forge: forgeLabel(repo.forge) }}
                  components={{ strong: <strong /> }}
                />
              </div>
            );
          })}
          {!loading &&
          !error &&
          !accountsError &&
          groups.length === 0 &&
          unavailableProjects.length === 0 &&
          unmatchedProjects.length === 0 ? (
            <div className="q-empty q-issues-empty">
              <CircleDotIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
              <h3>{t("empty.title")}</h3>
              <p>{t("empty.body")}</p>
            </div>
          ) : null}
          {groups.map((group) => (
            <section key={group.key} className="q-issues-group" aria-label={group.label}>
              <header className="q-issues-group-head">
                <span>{group.label}</span>
                <span className="q-faint">{formatNumber(group.rows.length)}</span>
              </header>
              {group.error ? <p className="q-issues-note q-danger">{group.error}</p> : null}
              {group.rows.map((row) => (
                <button
                  key={row.key}
                  type="button"
                  className="q-issue-row"
                  aria-current={selected?.row.key === row.key ? "true" : undefined}
                  onClick={() => setSelected({ row, capabilities: group.capabilities })}
                >
                  <CircleDotIcon
                    className={row.state === "closed" ? "q-icon s14 q-faint" : "q-icon s14 q-ok"}
                    strokeWidth={1.5}
                    aria-hidden
                  />
                  <span className="q-issue-num">{row.number}</span>
                  <span className="q-issue-title">{row.title}</span>
                  {row.repository ? <span className="q-issue-repo">{row.repository}</span> : null}
                  {row.assignees.length > 0 ? (
                    <span className="q-issue-meta">{row.assignees[0]}</span>
                  ) : null}
                  <span className="q-issue-meta">{relativeIssueAge(row.updatedAt)}</span>
                </button>
              ))}
              {group.hasMore ? (
                <button
                  type="button"
                  className="q-btn ghost q-issues-more"
                  disabled={group.loading || group.generation !== generation}
                  onClick={() => void loadMore(group)}
                >
                  {group.loading ? t("common:loading") : t("loadMore")}
                </button>
              ) : null}
            </section>
          ))}
        </div>
        <div className="q-issues-detail">
          {selected ? (
            <IssueDetail
              // The pane stays mounted for the same issue and account, so a
              // comment draft survives saving an edit anywhere in the pane.
              // The list is refreshed separately.
              key={`${selected.row.key}:${selected.row.accountId}`}
              row={selected.row}
              capabilities={selected.capabilities}
              threadId={threadId}
              linked={linked.has(selected.row.key)}
              onChanged={() => {
                setRefresh((value) => value + 1);
              }}
              onLinkChange={() => {
                setRefresh((value) => value + 1);
                if (threadId) notifyThreadIssueLinks(threadId);
              }}
            />
          ) : (
            <div className="q-empty q-issues-empty">
              <CircleDotIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
              <h3>{t("select.title")}</h3>
              <p>{t("select.body")}</p>
            </div>
          )}
        </div>
      </div>
    </main>
  );
}

function sourceKey(source: Source): string {
  return source.kind === "forge"
    ? `forge:${source.repo.base_url}:${source.repo.owner}/${source.repo.repo}`
    : `linear:${source.projectId}`;
}

/**
 * The identity of a listing. It names the account and, for Linear, the team,
 * so switching either one starts a new generation even though the source
 * itself (`sourceKey`) is matched as before.
 */
function sourceSignatureKey(source: Source): string {
  return source.kind === "forge"
    ? `forge:${source.accountId}:${source.repo.base_url}:${source.repo.owner}/${source.repo.repo}`
    : `linear:${source.accountId}:${source.projectId}:${source.teamId}`;
}

function stateTone(state: ForgeIssueState): string {
  return state === "closed" ? "q-faint" : "q-ok";
}

type CloseReason = "completed" | "not_planned" | "duplicate";

function IssueDetail({
  row,
  capabilities,
  threadId,
  linked,
  onChanged,
  onLinkChange,
}: {
  row: IssueRow;
  capabilities: ForgeIssueCapabilities;
  threadId: string | null;
  linked: boolean;
  onChanged: () => void;
  onLinkChange: () => void;
}) {
  const { t } = useTranslation(["issues", "common"]);
  const [forge, setForge] = useState<ForgeIssue | null>(null);
  const [linear, setLinear] = useState<LinearIssue | null>(null);
  const [comments, setComments] = useState<ForgeIssueComment[]>([]);
  const [commentsCursor, setCommentsCursor] = useState<string | null>(null);
  const [linearCommentsCursor, setLinearCommentsCursor] = useState<string | null>(null);
  const [readOnly, setReadOnly] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [title, setTitle] = useState(row.title);
  const [body, setBody] = useState("");
  // i18n-ignore-next-line: the editable Labels field's value, split on commas when saved
  const [labels, setLabels] = useState(row.labels.map((label) => label.name).join(", "));
  // i18n-ignore-next-line: the editable Assignees field's value, split on commas when saved
  const [assignees, setAssignees] = useState(row.assignees.join(", "));
  const [comment, setComment] = useState("");
  const [closeReason, setCloseReason] = useState<CloseReason>("completed");
  const [duplicateNumber, setDuplicateNumber] = useState("");

  const repo = row.scope.tracker === "forge" ? row.scope.repo : null;
  const number = Number(row.id);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    if (row.scope.tracker === "forge" && repo) {
      Promise.all([
        api().getForgeIssue(row.accountId, repo, number),
        api().forgeIssueComments(row.accountId, repo, number, null),
      ])
        .then(([detail, page]) => {
          if (cancelled) return;
          setForge(detail.issue);
          setComments(page.comments);
          setCommentsCursor(page.next_cursor);
          setReadOnly(detail.readOnly);
          setTitle(detail.issue.summary.title);
          setBody(detail.issue.body);
          // i18n-ignore-next-line: the editable Labels field's value, split on commas when saved
          setLabels(detail.issue.summary.labels.map((label) => label.name).join(", "));
          setAssignees(
            // i18n-ignore-next-line: the editable Assignees field's value, split on commas when saved
            detail.issue.summary.assignees.map((person) => person.login).join(", "),
          );
          setLoading(false);
        })
        .catch((cause: unknown) => {
          if (cancelled) return;
          setError(issueError(cause));
          setLoading(false);
        });
    } else {
      api()
        .getLinearIssue(row.accountId, row.id, null)
        .then(({ issue, readOnly: readOnlyIssue }) => {
          if (cancelled) return;
          setLinear(issue);
          setLinearCommentsCursor(issue.comments_next_cursor);
          setReadOnly(readOnlyIssue);
          setTitle(issue.summary.title);
          setBody(issue.description);
          // i18n-ignore-next-line: the editable Labels field's value, split on commas when saved
          setLabels(issue.summary.labels.join(", "));
          // i18n-ignore-next-line: the editable Assignees field's value, split on commas when saved
          setAssignees(issue.summary.assignees.join(", "));
          setLoading(false);
        })
        .catch((cause: unknown) => {
          if (cancelled) return;
          setError(issueError(cause));
          setLoading(false);
        });
    }
    return () => {
      cancelled = true;
    };
  }, [row.id, row.accountId, row.scope, repo, number]);

  const summary = forge?.summary ?? linear?.summary ?? null;
  const state: ForgeIssueState = summary
    ? "state" in summary
      ? summary.state
      : linearStateIsClosed(summary.state_type)
        ? "closed"
        : "open"
    : row.state;
  const webUrl = summary && "url" in summary ? summary.url : (forge?.summary.web_url ?? row.webUrl);
  // The header reads what the detail returned, so labels and assignees a
  // stale list row carries never replace the tracker's current answer.
  const detailAssignees = forge
    ? forge.summary.assignees.map((person) => person.login)
    : linear
      ? linear.summary.assignees
      : row.assignees;
  const detailLabels: Array<{ name: string; color: string | null }> = forge
    ? forge.summary.labels
    : linear
      ? linear.summary.labels.map((name) => ({ name, color: null }))
      : row.labels;

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    try {
      await action();
    } catch (cause) {
      pushToast({
        title: t("detail.rejected"),
        description: issueError(cause),
        variant: "error",
      });
    } finally {
      setBusy(false);
    }
  };

  const update = (change: Partial<ForgeIssueUpdate>) =>
    run(async () => {
      if (!repo) return;
      const result = await api().updateForgeIssue(row.accountId, repo, number, {
        title: null,
        body: null,
        state: null,
        close_reason: null,
        duplicate_issue_id: null,
        labels: null,
        assignees: null,
        ...change,
      });
      setForge(result.issue);
      setReadOnly(result.readOnly);
      setTitle(result.issue.summary.title);
      setBody(result.issue.body);
      // i18n-ignore-next-line: the editable Labels field's value, split on commas when saved
      setLabels(result.issue.summary.labels.map((label) => label.name).join(", "));
      // i18n-ignore-next-line: the editable Assignees field's value, split on commas when saved
      setAssignees(result.issue.summary.assignees.map((person) => person.login).join(", "));
      onChanged();
    });

  const loadMoreComments = () =>
    run(async () => {
      if (!repo || !commentsCursor) return;
      const page = await api().forgeIssueComments(row.accountId, repo, number, commentsCursor);
      setComments((current) => [...current, ...page.comments]);
      setCommentsCursor(page.next_cursor);
    });

  const loadMoreLinearComments = () =>
    run(async () => {
      if (!linearCommentsCursor) return;
      const { issue } = await api().getLinearIssue(row.accountId, row.id, linearCommentsCursor);
      const seen = new Set((linear?.comments ?? []).map((item) => item.id));
      setLinear((current) =>
        current
          ? {
              ...current,
              comments: [
                ...current.comments,
                ...issue.comments.filter((item) => !seen.has(item.id)),
              ],
            }
          : issue,
      );
      setLinearCommentsCursor(issue.comments_next_cursor);
    });

  const postComment = () =>
    run(async () => {
      if (!comment.trim()) return;
      if (repo) {
        const posted = await api().commentForgeIssue(row.accountId, repo, number, comment);
        setComments((current) => [...current, posted]);
      } else {
        const posted = await api().commentLinearIssue(row.accountId, row.id, comment);
        setLinear((current) =>
          current ? { ...current, comments: [...current.comments, posted] } : current,
        );
      }
      setComment("");
    });

  const toggleLink = () =>
    run(async () => {
      if (!threadId) return;
      if (linked) {
        await api().unlinkIssue(threadId, row.scope, row.id);
      } else {
        await api().linkIssue(threadId, row.accountId, row.scope, row.id);
      }
      onLinkChange();
    });

  if (loading) {
    return (
      <div className="q-issues-note" role="status">
        <LoaderCircleIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />{" "}
        {t("detail.reading")}
      </div>
    );
  }
  if (error) {
    return (
      <div className="q-issues-detail-inner">
        <h2>{row.title}</h2>
        <p className="q-danger" role="alert">
          {error}
        </p>
      </div>
    );
  }

  const commentsFor = forge ? comments : (linear?.comments ?? []);
  const linkedPrs = forge?.linked_pull_requests ?? [];
  const linearPrs = linear?.linked_pull_requests ?? [];
  const canWrite = !readOnly && row.scope.tracker === "forge" && capabilities.write;
  const canComment = !readOnly && capabilities.comments;
  const duplicateInvalid =
    closeReason === "duplicate" && !/^[1-9][0-9]*$/.test(duplicateNumber.trim());
  const closeChange = (): Partial<ForgeIssueUpdate> => {
    if (!capabilities.close_reason) return { state: "closed" };
    return {
      state: "closed",
      close_reason: closeReason,
      duplicate_issue_id: closeReason === "duplicate" ? Number(duplicateNumber.trim()) : null,
    };
  };

  return (
    <article className="q-issues-detail-inner" aria-label={t("detail.label")}>
      <header className="q-issues-detail-head">
        <span className={`q-issue-state ${stateTone(state)}`}>{issueStateLabel(state)}</span>
        <span className="q-faint">{row.number}</span>
        <span className="q-faint">{row.tracker}</span>
        <span style={{ flex: 1 }} />
        <a
          className="q-btn ghost"
          href={webUrl}
          target="_blank"
          rel="noopener noreferrer"
          aria-label={t("detail.openOnTracker")}
        >
          <ExternalLinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </a>
        <button
          type="button"
          className="q-btn ghost"
          disabled={!threadId || busy}
          title={threadId ? undefined : t("detail.linkNeedsThread")}
          onClick={() => void toggleLink()}
        >
          {linked ? (
            <UnlinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          ) : (
            <LinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          )}
          {linked ? t("detail.unlink") : t("detail.link")}
        </button>
      </header>
      {readOnly ? (
        <p className="q-issues-note">
          {row.scope.tracker === "linear" ? t("detail.readOnlyLinear") : t("detail.readOnlyForge")}
        </p>
      ) : null}
      {canWrite ? (
        <input
          className="q-issue-title-input"
          aria-label={t("detail.titleLabel")}
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          onBlur={() => {
            if (title.trim() && title !== (forge?.summary.title ?? "")) {
              void update({ title: title.trim() });
            }
          }}
        />
      ) : (
        <h2>{title}</h2>
      )}
      <div className="q-issues-detail-meta">
        {summary && "author" in summary && summary.author ? <span>{summary.author}</span> : null}
        {detailAssignees.length > 0 ? (
          <span>{formatList(detailAssignees, "conjunction", "narrow")}</span>
        ) : null}
        {detailLabels.length > 0 ? (
          <span className="q-issue-labels">
            {detailLabels.map((label) => (
              <span key={label.name} className="q-issue-label">
                {label.name}
              </span>
            ))}
          </span>
        ) : null}
      </div>
      {canWrite ? (
        <textarea
          className="q-issue-body-input"
          aria-label={t("detail.descriptionLabel")}
          rows={8}
          value={body}
          onChange={(event) => setBody(event.target.value)}
          onBlur={() => {
            if (body !== (forge?.body ?? "")) void update({ body });
          }}
        />
      ) : (
        <div className="q-issue-description">
          <Markdown text={body || t("detail.noDescription")} />
        </div>
      )}
      {canWrite && (capabilities.labels || capabilities.assignees) ? (
        <div className="q-issues-fields">
          {capabilities.labels ? (
            <label>
              <span>{t("detail.labels")}</span>
              <input
                value={labels}
                onChange={(event) => setLabels(event.target.value)}
                onBlur={() => {
                  const next = labels
                    .split(",")
                    .map((value) => value.trim())
                    .filter(Boolean);
                  const before = forge?.summary.labels.map((label) => label.name) ?? [];
                  if (next.join("\u0000") !== before.join("\u0000")) {
                    void update({ labels: next });
                  }
                }}
                placeholder={t("detail.labelsPlaceholder")}
              />
            </label>
          ) : null}
          {capabilities.assignees ? (
            <label>
              <span>{t("detail.assignees")}</span>
              <input
                value={assignees}
                onChange={(event) => setAssignees(event.target.value)}
                onBlur={() => {
                  const next = assignees
                    .split(",")
                    .map((value) => value.trim())
                    .filter(Boolean);
                  const before = forge?.summary.assignees.map((person) => person.login) ?? [];
                  if (next.join("\u0000") !== before.join("\u0000")) {
                    void update({ assignees: next });
                  }
                }}
                placeholder={t("detail.assigneesPlaceholder")}
              />
            </label>
          ) : null}
        </div>
      ) : null}
      {canWrite ? (
        <div className="q-issues-actions">
          {state === "open" ? (
            <>
              {capabilities.close_reason ? (
                <ChipSelect
                  label={t("detail.closeReason")}
                  value={closeReason}
                  options={[
                    { value: "completed", label: t("detail.reasons.completed") },
                    { value: "not_planned", label: t("detail.reasons.not_planned") },
                    { value: "duplicate", label: t("detail.reasons.duplicate") },
                  ]}
                  onChange={(value) => setCloseReason(value as CloseReason)}
                />
              ) : null}
              {capabilities.close_reason && closeReason === "duplicate" ? (
                <input
                  className="q-issue-duplicate"
                  aria-label={t("detail.duplicateLabel")}
                  inputMode="numeric"
                  placeholder={t("detail.duplicatePlaceholder")}
                  value={duplicateNumber}
                  onChange={(event) => setDuplicateNumber(event.target.value)}
                />
              ) : null}
              <button
                type="button"
                className="q-btn secondary"
                disabled={busy || duplicateInvalid}
                title={duplicateInvalid ? t("detail.duplicateMissing") : undefined}
                onClick={() => void update(closeChange())}
              >
                {t("detail.close")}
              </button>
            </>
          ) : (
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy}
              onClick={() => void update({ state: "open" })}
            >
              {t("detail.reopen")}
            </button>
          )}
        </div>
      ) : null}
      {linkedPrs.length > 0 || linearPrs.length > 0 ? (
        <section className="q-issues-section">
          <h3>{t("detail.linkedPrs")}</h3>
          {linkedPrs.map((pr) => (
            <a
              key={pr.web_url}
              className="q-issue-pr"
              href={pr.web_url}
              target="_blank"
              rel="noopener noreferrer"
            >
              <span>#{pr.number}</span>
              <span>{pr.title}</span>
              {pr.state ? <span className="q-faint">{pr.state}</span> : null}
            </a>
          ))}
          {linearPrs.map((pr) => (
            <a
              key={pr.url}
              className="q-issue-pr"
              href={pr.url}
              target="_blank"
              rel="noopener noreferrer"
            >
              <span>{pr.title}</span>
              {pr.state ? <span className="q-faint">{pr.state}</span> : null}
            </a>
          ))}
        </section>
      ) : null}
      <section className="q-issues-section">
        <h3>
          <MessageSquareIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />{" "}
          {t("detail.comments")}
        </h3>
        {commentsFor.length === 0 ? <p className="q-faint">{t("detail.noComments")}</p> : null}
        {commentsFor.map((item) =>
          "comment_id" in item ? (
            <div key={item.comment_id} className="q-issue-comment">
              <header>
                <b>{item.author || t("detail.someone")}</b>
                <span className="q-faint">{relativeIssueAge(item.created_at)}</span>
              </header>
              <Markdown text={item.body} />
            </div>
          ) : (
            <div key={item.id} className="q-issue-comment">
              <header>
                <b>{item.author || t("detail.someone")}</b>
                <span className="q-faint">{relativeIssueAge(item.created_at)}</span>
              </header>
              <Markdown text={item.body} />
            </div>
          ),
        )}
        {forge && commentsCursor ? (
          <button
            type="button"
            className="q-btn ghost"
            disabled={busy}
            onClick={() => void loadMoreComments()}
          >
            {busy ? t("common:loading") : t("detail.loadMoreComments")}
          </button>
        ) : null}
        {!forge && linearCommentsCursor ? (
          <button
            type="button"
            className="q-btn ghost"
            disabled={busy}
            onClick={() => void loadMoreLinearComments()}
          >
            {busy ? t("common:loading") : t("detail.loadMoreComments")}
          </button>
        ) : null}
        {canComment ? (
          <div className="q-issues-comment-form">
            <textarea
              className="q-issue-notes"
              aria-label={t("detail.newComment")}
              rows={3}
              placeholder={t("detail.writeComment")}
              value={comment}
              onChange={(event) => setComment(event.target.value)}
            />
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy || !comment.trim()}
              onClick={() => void postComment()}
            >
              {busy ? t("detail.sending") : t("detail.comment")}
            </button>
          </div>
        ) : null}
      </section>
    </article>
  );
}
