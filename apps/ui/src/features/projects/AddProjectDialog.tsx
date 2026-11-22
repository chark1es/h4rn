import {
  ArrowUpIcon,
  CheckIcon,
  FolderGit2Icon,
  FolderIcon,
  FolderOpenIcon,
  FolderPlusIcon,
  GitBranchIcon,
  HomeIcon,
  LoaderCircleIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { DirectoryListing, ProjectSource } from "@/gen";
import { closeAddProject, pushToast, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { addProject, browseDirectories, shortPath } from "./store";

type Mode = "open" | "new" | "clone";

const MODES: Array<{ value: Mode; label: string; hint: string; icon: typeof FolderIcon }> = [
  {
    value: "open",
    label: "Open folder",
    hint: "A folder already on the host",
    icon: FolderOpenIcon,
  },
  {
    value: "new",
    label: "New folder",
    hint: "Create an empty project",
    icon: FolderPlusIcon,
  },
  {
    value: "clone",
    label: "Clone repository",
    hint: "From a git URL",
    icon: GitBranchIcon,
  },
];

/** The folder `git clone` would create for a URL or `owner/repo`. */
export function cloneFolderName(url: string): string {
  const tail = url
    .trim()
    .replace(/\/+$/, "")
    .split(/[/:]/)
    .pop()
    ?.replace(/\.git$/, "");
  return tail && tail !== "." && tail !== ".." ? tail : "";
}

function joinPath(parent: string, name: string): string {
  return `${parent.replace(/\/+$/, "")}/${name}`;
}

/** Lists folders on the host; clicking one opens it. */
function FolderBrowser({
  listing,
  loading,
  error,
  onOpen,
}: {
  listing: DirectoryListing | null;
  loading: boolean;
  error: string | null;
  onOpen: (path: string | null) => void;
}) {
  const [draft, setDraft] = useState("");
  const home = listing?.home ?? null;
  useEffect(() => {
    if (listing) setDraft(shortPath(listing.path, home));
  }, [listing, home]);

  return (
    <div className="q-fb">
      <div className="q-fb-bar">
        <button
          type="button"
          className="q-ibtn"
          aria-label="Up one folder"
          title="Up one folder"
          disabled={!listing?.parent}
          onClick={() => onOpen(listing?.parent ?? null)}
        >
          <ArrowUpIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </button>
        <button
          type="button"
          className="q-ibtn"
          aria-label="Home folder"
          title="Home folder"
          onClick={() => onOpen("~")}
        >
          <HomeIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </button>
        <input
          className="q-input q-mono"
          aria-label="Folder path"
          value={draft}
          spellCheck={false}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              onOpen(draft.trim() || "~");
            }
          }}
          onBlur={() => {
            const current = listing ? shortPath(listing.path, home) : "";
            if (draft.trim() && draft.trim() !== current) onOpen(draft.trim());
          }}
        />
      </div>
      <div className="q-fb-list" role="listbox" aria-label="Folders" aria-busy={loading}>
        {error ? (
          <div className="q-fb-msg err">{error}</div>
        ) : loading && !listing ? (
          <div className="q-fb-msg">Reading folders…</div>
        ) : listing && listing.entries.length === 0 ? (
          <div className="q-fb-msg">No folders inside.</div>
        ) : (
          listing?.entries.map((entry) => (
            <button
              key={entry.path}
              type="button"
              role="option"
              aria-selected={false}
              className="q-fb-row"
              onClick={() => onOpen(entry.path)}
            >
              {entry.git ? (
                <FolderGit2Icon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              ) : (
                <FolderIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              )}
              <span className="q-fb-nm">{entry.name}</span>
              {entry.git ? <span className="q-faint tag">git</span> : null}
            </button>
          ))
        )}
      </div>
    </div>
  );
}

function useFolderBrowser(open: boolean) {
  const [listing, setListing] = useState<DirectoryListing | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const go = useCallback(async (path: string | null) => {
    setLoading(true);
    const result = await browseDirectories(path);
    setLoading(false);
    if ("error" in result) {
      setError(result.error);
      return false;
    }
    setError(null);
    setListing(result.listing);
    return true;
  }, []);

  useEffect(() => {
    if (!open) return;
    setListing(null);
    setError(null);
    // Most people keep projects in ~/code; fall back to home.
    void go("~/code").then((ok) => {
      if (!ok) void go(null);
    });
  }, [open, go]);

  return { listing, loading, error, go };
}

/**
 * Adds a project the way T3 Code does: open an existing folder, create a new
 * one (optionally with `git init`), or clone a repository, all on the host.
 */
export function AddProjectDialog() {
  const open = useApp((state) => state.ui.addProjectOpen);
  const localHostId = useApp((state) => state.localHostId);
  const hostName = useApp((state) =>
    state.localHostId ? (state.hosts[state.localHostId]?.name ?? state.localHostId) : null,
  );
  const connection = useApp((state) => state.connection.status);
  const [mode, setMode] = useState<Mode>("open");
  const [name, setName] = useState("");
  const [gitInit, setGitInit] = useState(true);
  const [url, setUrl] = useState("");
  const [folderName, setFolderName] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const browser = useFolderBrowser(open);

  useEffect(() => {
    if (!open) return;
    setMode("open");
    setName("");
    setGitInit(true);
    setUrl("");
    setFolderName("");
    setBusy(false);
    setProblem(null);
  }, [open]);

  const listing = browser.listing;
  const home = listing?.home ?? null;
  const current = listing ? shortPath(listing.path, home) : "";
  const cloneName = folderName.trim() || cloneFolderName(url);

  const summary = useMemo(() => {
    if (!listing) return null;
    if (mode === "open") return `Adds ${current}${listing.git ? " (a git repository)" : ""}.`;
    if (mode === "new") {
      return name.trim()
        ? `Creates ${joinPath(current, name.trim())}${gitInit ? " and runs git init" : ""}.`
        : `Goes in ${current}.`;
    }
    return cloneName
      ? `Clones into ${joinPath(current, cloneName)}.`
      : `Clones into a folder in ${current}.`;
  }, [listing, mode, current, name, gitInit, cloneName]);

  const offline = connection !== "online" || !localHostId;
  const ready =
    !offline &&
    !busy &&
    Boolean(listing) &&
    (mode === "open" ||
      (mode === "new" && name.trim().length > 0) ||
      (mode === "clone" && url.trim().length > 0 && cloneName.length > 0));

  const submit = async () => {
    if (!ready || !listing) return;
    const source: ProjectSource =
      mode === "open"
        ? { type: "existing", path: listing.path }
        : mode === "new"
          ? { type: "new_folder", parent: listing.path, name: name.trim(), git_init: gitInit }
          : {
              type: "clone",
              url: url.trim(),
              parent: listing.path,
              name: folderName.trim() || null,
            };
    setBusy(true);
    setProblem(null);
    const result = await addProject(source);
    setBusy(false);
    if ("error" in result) {
      setProblem(result.error);
      return;
    }
    const project = result.project;
    closeAddProject();
    pushToast({
      title: `Added ${project.name}.`,
      description: `${shortPath(project.path, home)} is in the sidebar, ready for its first thread.`,
      variant: "success",
    });
  };

  const submitLabel = busy
    ? mode === "clone"
      ? "Cloning…"
      : mode === "new"
        ? "Creating…"
        : "Adding…"
    : mode === "open"
      ? `Add ${listing ? (listing.path.split("/").filter(Boolean).pop() ?? "folder") : "folder"}`
      : mode === "new"
        ? "Create project"
        : "Clone";

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeAddProject())}>
      <DialogContent className="gap-4 sm:max-w-[600px]">
        <DialogHeader>
          <DialogTitle>Add project</DialogTitle>
          <DialogDescription>
            A project is a folder on {hostName ?? "a host"}. Threads you start there are listed
            under it.
          </DialogDescription>
        </DialogHeader>

        <div className="q-addp-modes" role="radiogroup" aria-label="How to add the project">
          {MODES.map((option) => (
            // biome-ignore lint/a11y/useSemanticElements: a card-style radio needs a button
            <button
              key={option.value}
              type="button"
              role="radio"
              aria-checked={mode === option.value}
              className="q-addp-mode"
              onClick={() => {
                setMode(option.value);
                setProblem(null);
              }}
            >
              <option.icon className="q-icon" strokeWidth={1.5} aria-hidden />
              <span className="q-addp-mode-t">{option.label}</span>
              <span className="q-addp-mode-s">{option.hint}</span>
              {mode === option.value ? (
                <CheckIcon className="q-icon s12 tick" strokeWidth={2} aria-hidden />
              ) : null}
            </button>
          ))}
        </div>

        {mode === "clone" ? (
          <label className="q-addp-field">
            <span className="lab">Repository</span>
            <input
              className="q-input q-mono"
              value={url}
              placeholder="https://github.com/owner/repo, git@…, or owner/repo"
              spellCheck={false}
              autoComplete="off"
              onChange={(event) => setUrl(event.target.value)}
            />
          </label>
        ) : null}

        {mode === "new" ? (
          <label className="q-addp-field">
            <span className="lab">Name</span>
            <input
              className="q-input"
              value={name}
              placeholder="my-project"
              spellCheck={false}
              autoComplete="off"
              onChange={(event) => setName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void submit();
                }
              }}
            />
          </label>
        ) : null}

        <div className="q-addp-field">
          <span className="lab">
            {mode === "open" ? "Folder" : mode === "new" ? "Create in" : "Clone into"}
          </span>
          <FolderBrowser
            listing={listing}
            loading={browser.loading}
            error={browser.error}
            onOpen={(path) => void browser.go(path)}
          />
        </div>

        {mode === "clone" ? (
          <label className="q-addp-field">
            <span className="lab">Folder name</span>
            <input
              className="q-input q-mono"
              value={folderName}
              placeholder={cloneFolderName(url) || "repo"}
              spellCheck={false}
              autoComplete="off"
              onChange={(event) => setFolderName(event.target.value)}
            />
          </label>
        ) : null}

        {mode === "new" ? (
          <label className="q-addp-check">
            <input
              type="checkbox"
              className="sr-only"
              checked={gitInit}
              onChange={(event) => setGitInit(event.target.checked)}
            />
            <span className={cn("q-check", gitInit && "on")} aria-hidden>
              {gitInit ? <CheckIcon className="q-icon s12" strokeWidth={2} /> : null}
            </span>
            Initialize a git repository
          </label>
        ) : null}

        {summary ? <p className="q-addp-sum q-mono">{summary}</p> : null}
        {problem ? (
          <div className="q-banner err" role="alert">
            {problem}
          </div>
        ) : null}
        {busy && mode === "clone" ? (
          <p className="q-faint" style={{ margin: 0, fontSize: 12 }}>
            Cloning on {hostName ?? "the host"}. A large repository can take a few minutes.
          </p>
        ) : null}

        <DialogFooter>
          <Button variant="ghost" onClick={closeAddProject}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={!ready}>
            {busy ? <LoaderCircleIcon className="q-icon s14 animate-spin" aria-hidden /> : null}
            {submitLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
