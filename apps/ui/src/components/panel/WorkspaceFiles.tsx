import {
  ChevronRightIcon,
  EyeIcon,
  FileCodeIcon,
  FileIcon,
  FolderIcon,
  FolderOpenIcon,
  RefreshCwIcon,
  SearchIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ShortcutCaps } from "@/components/ui/kbd";

import { gitBadge } from "@/features/ide/useIdeSession";
import { isHtmlPath } from "@/features/preview/htmlPreview";
import type { FileEntry, FsListing, FsTurnChanges, HostId, ProtoError } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { formatNumber } from "@/lib/i18n";
import { getClient, openInIde, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

type Row = { entry: FileEntry; depth: number };

function flatten(listings: Record<string, FsListing>, expanded: Record<string, true>): Row[] {
  const rows: Row[] = [];
  const visit = (directory: string, depth: number) => {
    const listing = listings[directory];
    if (!listing) return;
    for (const entry of listing.entries) {
      rows.push({ entry, depth });
      if (entry.kind === "directory" && expanded[entry.path]) visit(entry.path, depth + 1);
    }
  };
  visit("", 0);
  return rows;
}

function entryIcon(entry: FileEntry, open: boolean) {
  if (entry.kind === "directory") {
    return open ? (
      <FolderOpenIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
    ) : (
      <FolderIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
    );
  }
  const extension = entry.name.split(".").at(-1)?.toLowerCase();
  if (
    extension &&
    ["ts", "tsx", "js", "jsx", "rs", "py", "go", "css", "html", "json", "toml"].includes(extension)
  ) {
    return <FileCodeIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />;
  }
  return <FileIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />;
}

/**
 * One row of the workspace tree.
 *
 * A file row opens the IDE when clicked. An HTML file also carries an
 * "Open preview" action beside it, so the panel's Preview tab is one click
 * away without changing what a click on the name does.
 */
export function WorkspaceFileRow({
  entry,
  depth,
  open,
  active,
  onToggle,
  onOpenFile,
  onOpenPreview,
}: {
  entry: FileEntry;
  depth: number;
  open: boolean;
  active: boolean;
  onToggle: (path: string) => void;
  onOpenFile: (path: string) => void;
  onOpenPreview: (path: string) => void;
}) {
  const { t } = useTranslation("ide");
  const badge = gitBadge(entry.git_status);
  const directory = entry.kind === "directory";
  const row = (
    <button
      type="button"
      role="treeitem"
      aria-expanded={directory ? open : undefined}
      aria-selected={active || undefined}
      aria-label={entry.name}
      title={entry.path}
      className={cn("q-fnode", active && "sel")}
      style={{ ["--d" as string]: depth }}
      onClick={() => (directory ? onToggle(entry.path) : onOpenFile(entry.path))}
    >
      {directory ? (
        <ChevronRightIcon
          className={cn("q-icon s12 q-fchev", open && "open")}
          strokeWidth={1.5}
          aria-hidden
        />
      ) : (
        <span style={{ width: 12 }} />
      )}
      {entryIcon(entry, open)}
      <span className="grow q-mono" style={{ fontSize: 12 }}>
        {entry.name}
      </span>
      {badge ? <span className={cn("gs", badge.tone)}>{badge.letter}</span> : null}
    </button>
  );

  if (directory || !isHtmlPath(entry.path)) return row;
  return (
    <div className="q-frow" role="presentation">
      {row}
      <button
        type="button"
        className="q-fopen"
        aria-label={t("workspace.openPreviewOf", { name: entry.name })}
        title={t("workspace.openPreview")}
        onClick={() => onOpenPreview(entry.path)}
      >
        <EyeIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </button>
    </div>
  );
}

/**
 * The panel's Files tab (and the single file tree in Layout v2). It fetches
 * directories lazily and opens files in the IDE's project tabs.
 */
export function WorkspaceFiles({
  host,
  workspace,
  workspaceName,
  agentChanges,
  onOpenAgentChanges,
  activePath = null,
  agentChangesLabel = null,
  onOpenPreview,
}: {
  host: HostId;
  workspace: string;
  workspaceName: string;
  agentChanges: FsTurnChanges | null;
  onOpenAgentChanges: () => void;
  /** Opens one HTML file in the panel's Preview tab. */
  onOpenPreview: (path: string) => void;
  /** The file the editor has open, highlighted in the tree. */
  activePath?: string | null;
  /** The IDE footer names the thread and agent instead of a bare count. */
  agentChangesLabel?: string | null;
}) {
  const { t } = useTranslation(["ide", "common"]);
  const focusNonce = useApp((state) => state.ui.focusFilesNonce);
  const [listings, setListings] = useState<Record<string, FsListing>>({});
  const [expanded, setExpanded] = useState<Record<string, true>>({});
  const [rootOpen, setRootOpen] = useState(true);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ProtoError | null>(null);
  const [query, setQuery] = useState("");
  const [input, setInput] = useState<HTMLInputElement | null>(null);

  useEffect(() => {
    if (focusNonce > 0) input?.focus();
  }, [focusNonce, input]);

  const load = useCallback(
    async (path: string) => {
      try {
        const response = await getClient().request({
          type: "fs_list",
          host,
          workspace,
          path,
        });
        if (response.type !== "fs_listing") return;
        setListings((current) => ({ ...current, [path]: response.listing }));
        setError(null);
      } catch (cause) {
        setError(toProtoError(cause));
      } finally {
        setLoading(false);
      }
    },
    [host, workspace],
  );

  useEffect(() => {
    setListings({});
    setExpanded({});
    setLoading(true);
    setError(null);
    void load("");
  }, [load]);

  const toggle = (path: string) => {
    const open = expanded[path] === true;
    setExpanded((current) => {
      const next = { ...current };
      if (open) delete next[path];
      else next[path] = true;
      return next;
    });
    if (!open && !listings[path]) void load(path);
  };

  const rows = useMemo(() => {
    const all = flatten(listings, expanded);
    const needle = query.trim().toLowerCase();
    if (!needle) return all;
    return all.filter((row) => row.entry.path.toLowerCase().includes(needle));
  }, [listings, expanded, query]);

  return (
    <>
      <div className="q-rp-bar">
        <label className="q-filter">
          <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <input
            ref={setInput}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={t("workspace.filter", { name: workspaceName })}
            aria-label={t("workspace.filterLabel")}
            spellCheck={false}
          />
          <ShortcutCaps command="panel.files" />
        </label>
      </div>
      <div className="q-rp-body q-scroll" style={{ padding: "6px 0 8px" }}>
        {loading ? (
          <div
            className="q-tree"
            role="status"
            aria-busy="true"
            aria-label={t("workspace.loading")}
          >
            {[64, 48, 72, 56, 80].map((width, index) => (
              <div key={width} style={{ paddingLeft: index > 2 ? 16 : 0, marginBottom: 8 }}>
                <div className="q-skel" style={{ height: 10, width: `${width}%` }} />
              </div>
            ))}
          </div>
        ) : error ? (
          <div className="q-sub-empty">
            <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>
              {t("workspace.failed")}
            </p>
            <p style={{ margin: "4px 0 10px" }}>{error.message}</p>
            <button
              type="button"
              className="q-btn secondary"
              style={{ height: 26 }}
              onClick={() => void load("")}
            >
              <RefreshCwIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              {t("common:retry")}
            </button>
          </div>
        ) : rows.length === 0 ? (
          <div className="q-sub-empty">
            {query ? t("workspace.noMatch", { query }) : t("workspace.empty")}
          </div>
        ) : (
          <div role="tree" aria-label={t("workspace.tree")}>
            <button
              type="button"
              role="treeitem"
              aria-expanded={rootOpen}
              aria-label={workspaceName}
              title={workspaceName}
              className="q-fnode"
              style={{ ["--d" as string]: 0 }}
              onClick={() => setRootOpen((value) => !value)}
            >
              <ChevronRightIcon
                className={cn("q-icon s12 q-fchev", rootOpen && "open")}
                strokeWidth={1.5}
                aria-hidden
              />
              {rootOpen ? (
                <FolderOpenIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              ) : (
                <FolderIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              )}
              <span className="grow q-mono" style={{ fontSize: 12 }}>
                {workspaceName}
              </span>
            </button>
            {rootOpen
              ? rows.map((row) => (
                  <WorkspaceFileRow
                    key={row.entry.path}
                    entry={row.entry}
                    depth={row.depth + 1}
                    open={row.entry.kind === "directory" && expanded[row.entry.path] === true}
                    active={activePath === row.entry.path}
                    onToggle={toggle}
                    onOpenFile={openInIde}
                    onOpenPreview={onOpenPreview}
                  />
                ))
              : null}
          </div>
        )}
      </div>
      {agentChanges && agentChanges.files.length > 0 ? (
        <button type="button" className="q-agentchg" onClick={onOpenAgentChanges}>
          <span className="grow">
            <span className="t">{t("workspace.agentChanges")}</span>
            <span className="s">
              {agentChangesLabel ?? t("workspace.inThisTurn", { count: agentChanges.files.length })}
            </span>
          </span>
          <span className="q-num q-pm">
            <span className="q-add">
              +{formatNumber(agentChanges.files.reduce((sum, file) => sum + file.added, 0))}
            </span>{" "}
            <span className="q-del">
              −{formatNumber(agentChanges.files.reduce((sum, file) => sum + file.removed, 0))}
            </span>
          </span>
          <ChevronRightIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        </button>
      ) : null}
    </>
  );
}
