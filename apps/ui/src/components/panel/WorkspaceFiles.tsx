import { Tree, TreeGroup, TreeItem, TreeItemIndicator, TreeItemLabel } from "@ui-sdk/react/tree";
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
import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ShortcutCaps } from "@/components/ui/kbd";
import { Tip } from "@/components/ui/tooltip";

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

const CODE_EXTENSIONS = ["ts", "tsx", "js", "jsx", "rs", "py", "go", "css", "html", "json", "toml"];
const ROOT = ".";

function FileGlyph({ entry }: { entry: FileEntry }) {
  if (entry.kind === "directory") return <FolderGlyph />;
  const extension = entry.name.split(".").at(-1)?.toLowerCase();
  const Icon = extension && CODE_EXTENSIONS.includes(extension) ? FileCodeIcon : FileIcon;
  return <Icon className="size-3.5 shrink-0 text-fg-3" strokeWidth={1.5} aria-hidden />;
}

function FolderGlyph() {
  return (
    <>
      <FolderIcon
        className="size-3.5 shrink-0 text-fg-3 group-data-expanded/row:hidden"
        strokeWidth={1.5}
        aria-hidden
      />
      <FolderOpenIcon
        className="hidden size-3.5 shrink-0 text-fg-3 group-data-expanded/row:block"
        strokeWidth={1.5}
        aria-hidden
      />
    </>
  );
}

const rowClass = cn(
  "group/row relative mx-1.5 flex h-6.5 cursor-default items-center gap-1.5 rounded-md pr-2.5 text-sm text-muted-foreground select-none",
  "ps-[calc(var(--spacing)*2.5+var(--d,0)*var(--spacing)*3.5)]",
  "transition-colors duration-micro ease-enter hover:bg-hover",
  "data-selected:bg-sel data-selected:text-foreground",
  "group-focus-visible/item:outline-2 group-focus-visible/item:-outline-offset-2 group-focus-visible/item:outline-ring",
);

function RowChevron() {
  return (
    <TreeItemIndicator className="grid size-3.5 shrink-0 place-items-center">
      <ChevronRightIcon
        className="size-3.5 text-fg-3 transition-transform duration-micro ease-enter group-data-expanded/row:rotate-90 motion-reduce:transition-none rtl:-scale-x-100"
        strokeWidth={1.5}
        aria-hidden
      />
    </TreeItemIndicator>
  );
}

/**
 * One row of the workspace tree, with its children when it is an open folder.
 *
 * A file row opens the IDE when pressed. An HTML file also carries an
 * "Open preview" action, so the panel's Preview tab is one click away without
 * changing what a press on the name does.
 */
export function WorkspaceFileRow({
  entry,
  depth,
  onOpenPreview,
  children,
}: {
  entry: FileEntry;
  depth: number;
  onOpenPreview: (path: string) => void;
  children?: ReactNode;
}) {
  const { t } = useTranslation("ide");
  const badge = gitBadge(entry.git_status);
  const directory = entry.kind === "directory";
  const preview = !directory && isHtmlPath(entry.path);
  return (
    <TreeItem
      value={entry.path}
      branch={directory}
      label={entry.name}
      aria-label={entry.name}
      className="group/item outline-none"
    >
      <TreeItemLabel
        title={entry.path}
        className={cn(rowClass, preview && "pr-8")}
        style={{ ["--d" as string]: depth }}
      >
        {directory ? <RowChevron /> : <span className="size-3.5 shrink-0" />}
        <FileGlyph entry={entry} />
        <span className="min-w-0 flex-1 truncate font-mono text-xs">{entry.name}</span>
        {badge ? (
          <span className={cn("shrink-0 font-mono text-xs font-medium", badge.tone)}>
            {badge.letter}
          </span>
        ) : null}
        {preview ? (
          <Tip label={t("workspace.openPreview")}>
            <button
              type="button"
              className="hit-area absolute end-1 top-0.5 grid size-5.5 place-items-center rounded-sm text-fg-3 opacity-55 transition-opacity duration-micro ease-enter outline-none group-hover/row:opacity-100 hover:bg-fill hover:text-foreground focus-visible:opacity-100 focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-ring"
              aria-label={t("workspace.openPreviewOf", { name: entry.name })}
              onClick={(event) => {
                event.stopPropagation();
                onOpenPreview(entry.path);
              }}
              onKeyDown={(event) => event.stopPropagation()}
            >
              <EyeIcon className="size-3.5" strokeWidth={1.5} aria-hidden />
            </button>
          </Tip>
        ) : null}
      </TreeItemLabel>
      {children}
    </TreeItem>
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

  const listingsRef = useRef(listings);
  listingsRef.current = listings;
  const kinds = useMemo(() => {
    const map = new Map<string, FileEntry["kind"]>();
    for (const listing of Object.values(listings)) {
      for (const entry of listing.entries) map.set(entry.path, entry.kind);
    }
    return map;
  }, [listings]);
  const expandedValues = useMemo(
    () => [...(rootOpen ? [ROOT] : []), ...Object.keys(expanded)],
    [rootOpen, expanded],
  );

  const needle = query.trim().toLowerCase();
  const matches = useMemo(() => {
    if (!needle) return null;
    return flatten(listings, expanded).filter((row) =>
      row.entry.path.toLowerCase().includes(needle),
    );
  }, [listings, expanded, needle]);
  const empty = matches ? matches.length === 0 : (listings[""]?.entries.length ?? 0) === 0;

  const level = (directory: string, depth: number): ReactNode =>
    listings[directory]?.entries.map((entry) => (
      <WorkspaceFileRow key={entry.path} entry={entry} depth={depth} onOpenPreview={onOpenPreview}>
        {entry.kind === "directory" && expanded[entry.path] && listings[entry.path] ? (
          <TreeGroup>{level(entry.path, depth + 1)}</TreeGroup>
        ) : null}
      </WorkspaceFileRow>
    ));

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
        ) : empty ? (
          <div className="q-sub-empty">
            {query ? t("workspace.noMatch", { query }) : t("workspace.empty")}
          </div>
        ) : (
          <Tree
            aria-label={t("workspace.tree")}
            value={activePath}
            onValueChange={() => undefined}
            expanded={expandedValues}
            onExpandedChange={(next) => {
              setRootOpen(next.includes(ROOT));
              setExpanded(
                Object.fromEntries(
                  next.filter((value) => value !== ROOT).map((value) => [value, true as const]),
                ),
              );
            }}
            loadChildren={async (value) => {
              if (value !== ROOT && !listingsRef.current[value]) await load(value);
            }}
            onAction={(value) => {
              if (kinds.get(value) === "file") openInIde(value);
            }}
          >
            <TreeItem
              value={ROOT}
              branch
              aria-label={workspaceName}
              label={workspaceName}
              className="group/item outline-none"
            >
              <TreeItemLabel
                title={workspaceName}
                className={rowClass}
                style={{ ["--d" as string]: 0 }}
              >
                <RowChevron />
                <FolderGlyph />
                <span className="min-w-0 flex-1 truncate font-mono text-xs">{workspaceName}</span>
              </TreeItemLabel>
              <TreeGroup>
                {matches
                  ? matches.map((row) => (
                      <WorkspaceFileRow
                        key={row.entry.path}
                        entry={row.entry}
                        depth={row.depth + 1}
                        onOpenPreview={onOpenPreview}
                      />
                    ))
                  : level("", 1)}
              </TreeGroup>
            </TreeItem>
          </Tree>
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
