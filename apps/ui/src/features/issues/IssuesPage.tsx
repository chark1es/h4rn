import {
  CircleDotIcon,
  ExternalLinkIcon,
  LinkIcon,
  LoaderCircleIcon,
  MessageSquareIcon,
  SearchIcon,
  UnlinkIcon,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";

import { ChipSelect } from "@/components/common/ChipSelect";
import { Markdown } from "@/components/thread/Markdown";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { forgeLabel } from "@/features/forge/store";
import { useProjects } from "@/features/projects/store";
import type {
  ForgeAccount,
  ForgeIssue,
  ForgeIssueComment,
  ForgeIssueFilter,
  ForgeIssueState,
  ForgeIssueSummary,
  ForgeIssueUpdate,
  ForgeRepoRef,
  IssueScope,
  LinearAccount,
  LinearIssue,
  LinearProjectTeam,
  ThreadIssue,
} from "@/gen";
import { getClient, pushToast, useApp } from "@/lib/store/appStore";

import {
  forgeIssueRow,
  groupLabel,
  ISSUE_FILTERS,
  IssueApi,
  type IssueRow,
  issueError,
  issueKey,
  issueStateLabel,
  linearIssueRow,
  relativeIssueAge,
  type TrackerGroup,
  trackerKey,
} from "./store";
import { notifyThreadIssueLinks } from "./threadLinks";

const api = () => new IssueApi((request) => getClient().request(request));

const HIDDEN_PROJECTS = "h4rn:issues:hidden-projects";

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

type Source =
  | { kind: "forge"; accountId: string; repo: ForgeRepoRef; page: number }
  | { kind: "linear"; accountId: string; projectId: string; cursor: string | null };

type Group = TrackerGroup & { sources: Source[] };

type Selection = {
  row: IssueRow;
  readOnly: boolean;
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

/**
 * Issues from every project's tracker, grouped by account.
 *
 * Every row comes from the daemon's tracker read. A failed read is shown in
 * its group and never replaced by remembered rows.
 */
export function IssuesPage() {
  const projects = useProjects((state) => state.projects);
  const selectedThreadId = useApp((state) => state.ui.selectedThreadId);
  const connection = useApp((state) => state.connection.status);

  const [accounts, setAccounts] = useState<ForgeAccount[]>([]);
  const [linearAccounts, setLinearAccounts] = useState<LinearAccount[]>([]);
  const [mappings, setMappings] = useState<LinearProjectTeam[]>([]);
  const [repos, setRepos] = useState<Record<string, ForgeRepoRef>>({});
  const [hidden, setHidden] = useState<Set<string>>(() => readHidden());
  const [filter, setFilter] = useState<ForgeIssueFilter>("open");
  const [searchInput, setSearchInput] = useState("");
  const [search, setSearch] = useState("");
  const [groups, setGroups] = useState<Group[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Selection | null>(null);
  const [linked, setLinked] = useState<Set<string>>(new Set());
  const [refresh, setRefresh] = useState(0);

  // A cold deep link mounts before the socket handshake finishes.
  useEffect(() => {
    if (connection === "connecting") return;
    let cancelled = false;
    Promise.all([api().forgeAccounts(), api().linearAccounts(), api().projectTeams()])
      .then(([forgeAccounts, linear, teams]) => {
        if (cancelled) return;
        setAccounts(forgeAccounts);
        setLinearAccounts(linear);
        setMappings(teams);
      })
      .catch((cause: unknown) => {
        if (!cancelled) setError(issueError(cause));
      });
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
      const account = accounts.find(
        (candidate) =>
          candidate.forge === repo.forge &&
          candidate.base_url.replace(/\/$/, "") === repo.base_url.replace(/\/$/, ""),
      );
      if (account) {
        forgeSources.push({ kind: "forge", accountId: account.account_id, repo, page: 1 });
      }
    }
    const linearSources: Source[] = [];
    for (const project of visible) {
      const mapping = mappings.find((candidate) => candidate.project_id === project.id);
      if (mapping) {
        linearSources.push({
          kind: "linear",
          accountId: mapping.account_id,
          projectId: project.id,
          cursor: null,
        });
      }
    }
    const sources = [...forgeSources, ...linearSources];
    if (sources.length === 0) {
      setGroups([]);
      setLoading(false);
      return;
    }
    void Promise.all(
      sources.map(async (source) => {
        try {
          if (source.kind === "forge") {
            const result = await api().listForgeIssues(source.repo, filter, 1, search || null);
            return {
              source,
              rows: result.issues.map((issue) => forgeIssueRow(source.accountId, issue)),
              hasMore: result.hasMore,
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
            source,
            rows: result.issues.map((issue) => linearIssueRow(source.accountId, issue)),
            hasMore: result.nextCursor !== null,
            readOnly: result.readOnly,
            error: null as string | null,
          };
        } catch (cause) {
          return {
            source,
            rows: [] as IssueRow[],
            hasMore: false,
            readOnly: true,
            error: issueError(cause),
          };
        }
      }),
    ).then((results) => {
      if (cancelled) return;
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
          } satisfies Group);
        group.rows = mergeRows(group.rows, result.rows);
        group.hasMore = group.hasMore || result.hasMore;
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
    linearAccounts,
    mappings,
    projects,
    repos,
    hidden,
    filter,
    search,
    refresh,
    connection,
  ]);

  const loadMore = useCallback(
    async (group: Group) => {
      setGroups((current) =>
        current.map((item) => (item.key === group.key ? { ...item, loading: true } : item)),
      );
      const additions = await Promise.all(
        group.sources.map(async (source) => {
          try {
            if (source.kind === "forge") {
              const next = source.page + 1;
              const result = await api().listForgeIssues(source.repo, filter, next, search || null);
              return {
                rows: result.issues.map((issue) => forgeIssueRow(source.accountId, issue)),
                hasMore: result.hasMore,
                source: { ...source, page: next } as Source,
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
              hasMore: result.nextCursor !== null,
              source: { ...source, cursor: result.nextCursor } as Source,
              error: null as string | null,
            };
          } catch (cause) {
            return {
              rows: [] as IssueRow[],
              hasMore: false,
              source,
              error: issueError(cause),
            };
          }
        }),
      );
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
          return {
            ...item,
            rows,
            hasMore: additions.some((addition) => addition.hasMore),
            loading: false,
            error: errors[0] ?? null,
            sources: additions.map((addition) => addition.source),
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

  const threadId = selectedThreadId;

  return (
    <main className="q-main" aria-label="Issues">
      <header className="q-page-head q-issues-head">
        <h1>Issues</h1>
        <form
          className="q-issues-search"
          aria-label="Search issues"
          onSubmit={(event) => {
            event.preventDefault();
            setSearch(searchInput.trim());
          }}
        >
          <SearchIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
          <input
            aria-label="Search issues"
            placeholder="Search the tracker"
            value={searchInput}
            onChange={(event) => setSearchInput(event.target.value)}
          />
          {search ? (
            <button
              type="button"
              className="q-link"
              onClick={() => {
                setSearchInput("");
                setSearch("");
              }}
            >
              Clear
            </button>
          ) : null}
        </form>
        <div className="q-seg" role="tablist" aria-label="Issue filters">
          {ISSUE_FILTERS.map((item) => (
            <button
              key={item.key}
              type="button"
              role="tab"
              aria-selected={filter === item.key}
              onClick={() => setFilter(item.key)}
            >
              {item.label}
            </button>
          ))}
        </div>
        <span className="grow" />
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button type="button" className="q-btn ghost" aria-label="Projects">
              Projects
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-64 max-h-80 overflow-y-auto">
            <DropdownMenuLabel>Projects</DropdownMenuLabel>
            <DropdownMenuSeparator />
            {projects.length === 0 ? (
              <p className="q-faint" style={{ padding: "6px 8px", fontSize: 12 }}>
                No projects yet.
              </p>
            ) : (
              projects.map((project) => (
                <DropdownMenuCheckboxItem
                  key={project.id}
                  checked={!hidden.has(project.id)}
                  onCheckedChange={() => toggleProject(project.id)}
                  onSelect={(event) => event.preventDefault()}
                >
                  {project.name}
                </DropdownMenuCheckboxItem>
              ))
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </header>

      <div className="q-issues-body">
        <div className="q-issues-list">
          {loading ? (
            <p className="q-faint q-issues-note" role="status">
              Reading trackers…
            </p>
          ) : null}
          {error ? (
            <div className="q-issues-note q-danger" role="alert">
              {error}
            </div>
          ) : null}
          {!loading && !error && groups.length === 0 ? (
            <div className="q-empty q-issues-empty">
              <CircleDotIcon className="q-icon s22 q-faint" strokeWidth={1.5} aria-hidden />
              <h3>No issues to show</h3>
              <p>
                Add a project with a forge remote, or choose a Linear team in Settings → Source
                control &amp; issues.
              </p>
            </div>
          ) : null}
          {groups.map((group) => (
            <section key={group.key} className="q-issues-group" aria-label={group.label}>
              <header className="q-issues-group-head">
                <span>{group.label}</span>
                <span className="q-faint">{group.rows.length}</span>
              </header>
              {group.error ? <p className="q-issues-note q-danger">{group.error}</p> : null}
              {group.rows.map((row) => (
                <button
                  key={row.key}
                  type="button"
                  className="q-issue-row"
                  aria-current={selected?.row.key === row.key ? "true" : undefined}
                  onClick={() => setSelected({ row, readOnly: false })}
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
                  disabled={group.loading}
                  onClick={() => void loadMore(group)}
                >
                  {group.loading ? "Loading…" : "Load more"}
                </button>
              ) : null}
            </section>
          ))}
        </div>
        <div className="q-issues-detail">
          {selected ? (
            <IssueDetail
              key={`${selected.row.key}:${refresh}`}
              row={selected.row}
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
              <h3>Select an issue</h3>
              <p>Its description, comments and actions open here.</p>
            </div>
          )}
        </div>
      </div>
    </main>
  );
}

function stateTone(state: ForgeIssueState): string {
  return state === "closed" ? "q-faint" : "q-ok";
}

function IssueDetail({
  row,
  threadId,
  linked,
  onChanged,
  onLinkChange,
}: {
  row: IssueRow;
  threadId: string | null;
  linked: boolean;
  onChanged: () => void;
  onLinkChange: () => void;
}) {
  const [forge, setForge] = useState<ForgeIssue | null>(null);
  const [linear, setLinear] = useState<LinearIssue | null>(null);
  const [comments, setComments] = useState<ForgeIssueComment[]>([]);
  const [readOnly, setReadOnly] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [title, setTitle] = useState(row.title);
  const [body, setBody] = useState("");
  const [labels, setLabels] = useState(row.labels.map((label) => label.name).join(", "));
  const [assignees, setAssignees] = useState(row.assignees.join(", "));
  const [comment, setComment] = useState("");
  const [closeReason, setCloseReason] = useState<"completed" | "not_planned" | "duplicate">(
    "completed",
  );

  const repo = row.scope.tracker === "forge" ? row.scope.repo : null;
  const number = Number(row.id);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    if (row.scope.tracker === "forge" && repo) {
      Promise.all([api().getForgeIssue(repo, number), api().forgeIssueComments(repo, number)])
        .then(([detail, page]) => {
          if (cancelled) return;
          setForge(detail.issue);
          setComments(page.comments);
          setReadOnly(detail.readOnly);
          setTitle(detail.issue.summary.title);
          setBody(detail.issue.body);
          setLabels(detail.issue.summary.labels.map((label) => label.name).join(", "));
          setAssignees(detail.issue.summary.assignees.map((person) => person.login).join(", "));
          setLoading(false);
        })
        .catch((cause: unknown) => {
          if (cancelled) return;
          setError(issueError(cause));
          setLoading(false);
        });
    } else {
      api()
        .getLinearIssue(row.accountId, row.id)
        .then(({ issue, readOnly: readOnlyIssue }) => {
          if (cancelled) return;
          setLinear(issue);
          setReadOnly(readOnlyIssue);
          setTitle(issue.summary.title);
          setBody(issue.description);
          setLabels(issue.summary.labels.join(", "));
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
      : summary.state_type === "completed" || summary.state_type === "canceled"
        ? "closed"
        : "open"
    : row.state;
  const webUrl = summary && "url" in summary ? summary.url : (forge?.summary.web_url ?? row.webUrl);

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    try {
      await action();
    } catch (cause) {
      pushToast({
        title: "The tracker did not accept that.",
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
      const result = await api().updateForgeIssue(repo, number, {
        title: null,
        body: null,
        state: null,
        close_reason: null,
        labels: null,
        assignees: null,
        ...change,
      });
      setForge(result.issue);
      setReadOnly(result.readOnly);
      setTitle(result.issue.summary.title);
      setBody(result.issue.body);
      setLabels(result.issue.summary.labels.map((label) => label.name).join(", "));
      setAssignees(result.issue.summary.assignees.map((person) => person.login).join(", "));
      onChanged();
    });

  const postComment = () =>
    run(async () => {
      if (!comment.trim()) return;
      if (repo) {
        const posted = await api().commentForgeIssue(repo, number, comment);
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
        <LoaderCircleIcon className="q-icon s14" strokeWidth={1.5} aria-hidden /> Reading the issue…
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
  const canWrite = !readOnly && row.scope.tracker === "forge";
  const canComment = !readOnly;

  return (
    <article className="q-issues-detail-inner" aria-label="Issue">
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
          aria-label="Open on the tracker"
        >
          <ExternalLinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </a>
        <button
          type="button"
          className="q-btn ghost"
          disabled={!threadId || busy}
          title={threadId ? undefined : "Open a thread to link this issue"}
          onClick={() => void toggleLink()}
        >
          {linked ? (
            <UnlinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          ) : (
            <LinkIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          )}
          {linked ? "Unlink" : "Link to this thread"}
        </button>
      </header>
      {readOnly ? (
        <p className="q-issues-note">
          This project does not own the repository, so the issue is read-only.
        </p>
      ) : null}
      {canWrite ? (
        <input
          className="q-issue-title-input"
          aria-label="Issue title"
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
        {row.assignees.length > 0 ? <span>{row.assignees.join(", ")}</span> : null}
        {row.labels.length > 0 ? (
          <span className="q-issue-labels">
            {row.labels.map((label) => (
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
          aria-label="Issue description"
          rows={8}
          value={body}
          onChange={(event) => setBody(event.target.value)}
          onBlur={() => {
            if (body !== (forge?.body ?? "")) void update({ body });
          }}
        />
      ) : (
        <div className="q-issue-description">
          <Markdown text={body || "_No description._"} />
        </div>
      )}
      {canWrite ? (
        <div className="q-issues-fields">
          <label>
            <span>Labels</span>
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
              placeholder="bug, frontend"
            />
          </label>
          <label>
            <span>Assignees</span>
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
              placeholder="alice, bob"
            />
          </label>
        </div>
      ) : null}
      {canWrite ? (
        <div className="q-issues-actions">
          {state === "open" ? (
            <>
              <ChipSelect
                label="Close reason"
                value={closeReason}
                options={[
                  { value: "completed", label: "Completed" },
                  { value: "not_planned", label: "Not planned" },
                  { value: "duplicate", label: "Duplicate" },
                ]}
                onChange={setCloseReason}
              />
              <button
                type="button"
                className="q-btn secondary"
                disabled={busy}
                onClick={() => void update({ state: "closed", close_reason: closeReason })}
              >
                Close issue
              </button>
            </>
          ) : (
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy}
              onClick={() => void update({ state: "open" })}
            >
              Reopen issue
            </button>
          )}
        </div>
      ) : null}
      {linkedPrs.length > 0 || linearPrs.length > 0 ? (
        <section className="q-issues-section">
          <h3>Linked pull requests</h3>
          {linkedPrs.map((pr) => (
            <a
              key={pr.number}
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
          <MessageSquareIcon className="q-icon s14" strokeWidth={1.5} aria-hidden /> Comments
        </h3>
        {commentsFor.length === 0 ? <p className="q-faint">No comments yet.</p> : null}
        {commentsFor.map((item) =>
          "comment_id" in item ? (
            <div key={item.comment_id} className="q-issue-comment">
              <header>
                <b>{item.author || "Someone"}</b>
                <span className="q-faint">{relativeIssueAge(item.created_at)}</span>
              </header>
              <Markdown text={item.body} />
            </div>
          ) : (
            <div key={item.id} className="q-issue-comment">
              <header>
                <b>{item.author || "Someone"}</b>
                <span className="q-faint">{relativeIssueAge(item.created_at)}</span>
              </header>
              <Markdown text={item.body} />
            </div>
          ),
        )}
        {canComment ? (
          <div className="q-issues-comment-form">
            <textarea
              className="q-issue-notes"
              aria-label="New comment"
              rows={3}
              placeholder="Write a comment"
              value={comment}
              onChange={(event) => setComment(event.target.value)}
            />
            <button
              type="button"
              className="q-btn secondary"
              disabled={busy || !comment.trim()}
              onClick={() => void postComment()}
            >
              {busy ? "Sending…" : "Comment"}
            </button>
          </div>
        ) : null}
      </section>
    </article>
  );
}

/** Display a forge account label, shared by tests. */
export function accountLabel(account: ForgeAccount): string {
  return `${forgeLabel(account.forge)} · ${account.username ?? account.base_url}`;
}

/** The scope a row links with; exported so tests can assert it. */
export function rowScope(row: IssueRow): IssueScope {
  return row.scope;
}

/** Rows are grouped by tracker, not by repository. */
export function groupRows(rows: IssueRow[]): Map<string, IssueRow[]> {
  const groups = new Map<string, IssueRow[]>();
  for (const row of rows) {
    const key = `${row.scope.tracker}:${row.accountId}`;
    groups.set(key, [...(groups.get(key) ?? []), row]);
  }
  return groups;
}

/** Filter labels for the compact header, exported for tests. */
export function filterLabel(filter: ForgeIssueFilter): string {
  return ISSUE_FILTERS.find((item) => item.key === filter)?.label ?? "Open";
}

/** The linked key for a stored thread issue, exported for tests. */
export function linkedIssueKey(issue: ThreadIssue): string {
  return issueKey(issue.scope, issue.issue_id);
}

/** A testable summary of what the detail header says. */
export function issueHeading(row: IssueRow): string {
  return `${row.number} ${row.title}`;
}

export function forgeIssueSummaryRows(accountId: string, issues: ForgeIssueSummary[]): IssueRow[] {
  return issues.map((issue) => forgeIssueRow(accountId, issue));
}

export function linearIssueSummaryRows(
  accountId: string,
  issues: LinearIssue["summary"][],
): IssueRow[] {
  return issues.map((issue) => linearIssueRow(accountId, issue));
}

export const issueRowHelpers = { sortRows, mergeRows };
