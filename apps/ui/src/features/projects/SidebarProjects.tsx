import {
  ArrowUpDownIcon,
  CheckIcon,
  ChevronDownIcon,
  CopyIcon,
  EllipsisIcon,
  FolderPlusIcon,
  PencilIcon,
  PinIcon,
  SquarePenIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { ThreadRow } from "@/components/inbox/ThreadRow";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { HostSummary } from "@/gen";
import { monogram } from "@/lib/grouping";
import { needsAttention } from "@/lib/inbox";
import {
  acknowledgeThread,
  openAddProject,
  openNewThread,
  openThread,
  pushToast,
  setThreadTab,
  snoozeThread,
  togglePin,
  toggleProject,
  useApp,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import {
  addProject,
  moveProject,
  PROJECT_SORT_LABELS,
  type ProjectSort,
  removeProject,
  renameProject,
  type SidebarProject,
  setProjectSort,
  setThreadSort,
  THREAD_SORT_LABELS,
  type ThreadSort,
  useProjects,
} from "./store";

function folderOf(group: SidebarProject): string {
  return group.project?.path ?? group.threads[0]?.workspace_path ?? "";
}

/** "Projects" heading with the sort menu and Add project. */
export function ProjectsHeader() {
  const projectSort = useProjects((state) => state.prefs.projectSort);
  const threadSort = useProjects((state) => state.prefs.threadSort);
  return (
    <div className="q-side-h q-proj-h">
      <span>Projects</span>
      <span className="q-proj-acts">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              className="q-ibtn s22"
              aria-label={`Sort projects: ${PROJECT_SORT_LABELS[projectSort]}`}
              title="Sort projects and threads"
            >
              <ArrowUpDownIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-56">
            <DropdownMenuLabel>Sort projects</DropdownMenuLabel>
            {(Object.keys(PROJECT_SORT_LABELS) as ProjectSort[]).map((value) => (
              <DropdownMenuItem key={value} onSelect={() => setProjectSort(value)}>
                <span className="inline-flex w-4 justify-center">
                  {projectSort === value ? (
                    <CheckIcon className="q-icon s14" strokeWidth={2} aria-hidden />
                  ) : null}
                </span>
                {PROJECT_SORT_LABELS[value]}
              </DropdownMenuItem>
            ))}
            <DropdownMenuSeparator />
            <DropdownMenuLabel>Sort threads</DropdownMenuLabel>
            {(Object.keys(THREAD_SORT_LABELS) as ThreadSort[]).map((value) => (
              <DropdownMenuItem key={value} onSelect={() => setThreadSort(value)}>
                <span className="inline-flex w-4 justify-center">
                  {threadSort === value ? (
                    <CheckIcon className="q-icon s14" strokeWidth={2} aria-hidden />
                  ) : null}
                </span>
                {THREAD_SORT_LABELS[value]}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
        <button
          type="button"
          className="q-ibtn s22"
          aria-label="Add project"
          title="Add project"
          onClick={openAddProject}
        >
          <FolderPlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        </button>
      </span>
    </div>
  );
}

/** One project in the sidebar: header, menu, drag target and its threads. */
export function ProjectGroupSection({
  group,
  order,
  dragging,
  onDragging,
  hosts,
  selectedId,
  closed,
  now,
}: {
  group: SidebarProject;
  /** Every group key in display order, for reordering. */
  order: string[];
  dragging: string | null;
  onDragging: (key: string | null) => void;
  hosts: Record<string, HostSummary>;
  selectedId: string | null;
  closed: boolean;
  now: number;
}) {
  const [renaming, setRenaming] = useState(false);
  const [dropBefore, setDropBefore] = useState(false);
  const you = group.threads.some((thread) => needsAttention(thread, now));
  const active = group.threads.some((thread) => thread.thread_id === selectedId);
  const hostName = hosts[group.host]?.name ?? group.host;
  const folder = folderOf(group);
  const newThreadHere = () => openNewThread({ host: group.host, workspace: folder });
  // Projects are registered with this daemon, so only its own folders can be kept.
  const localHost = useApp((state) => state.localHostId === group.host);

  return (
    <section
      className={cn("q-pg", dropBefore && dragging && dragging !== group.key && "q-pg-drop")}
      aria-label={`${group.name} on ${hostName}`}
      onDragOver={(event) => {
        if (!dragging || dragging === group.key) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "move";
        setDropBefore(true);
      }}
      onDragLeave={(event) => {
        if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
        setDropBefore(false);
      }}
      onDrop={(event) => {
        event.preventDefault();
        setDropBefore(false);
        if (dragging && dragging !== group.key) moveProject(order, dragging, group.key);
        onDragging(null);
      }}
    >
      {/* biome-ignore lint/a11y/noStaticElementInteractions: drag-to-reorder is a pointer shortcut; Sort projects in the header menu is the keyboard path */}
      <div
        className={cn("q-pg-h", active && "act", dragging === group.key && "dragging")}
        draggable={!renaming}
        onDragStart={(event) => {
          event.dataTransfer.effectAllowed = "move";
          event.dataTransfer.setData("text/plain", group.name);
          onDragging(group.key);
        }}
        onDragEnd={() => onDragging(null)}
      >
        {renaming && group.project ? (
          <RenameField
            initial={group.name}
            onDone={async (name) => {
              setRenaming(false);
              if (group.project && name && name !== group.name) {
                await renameProject(group.project.id, name);
              }
            }}
          />
        ) : (
          <button
            type="button"
            className="q-pg-tog"
            aria-expanded={!closed}
            title={folder}
            onClick={() => toggleProject(group.key)}
          >
            <span className="q-pmono" aria-hidden="true">
              {monogram(group.name)}
            </span>
            <span className="nm">{group.name}</span>
            <span className="hn">{hostName}</span>
            <span className="grow" />
            {closed ? (
              <>
                {you ? <span className="q-dot you" role="img" aria-label="needs you" /> : null}
                <span className="ct q-num">{group.threads.length}</span>
              </>
            ) : null}
            <ChevronDownIcon className="q-icon s12 chev" strokeWidth={1.5} aria-hidden />
          </button>
        )}
        {renaming ? null : (
          <>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button
                  type="button"
                  className="q-ibtn q-pg-more"
                  aria-label={`${group.name} actions`}
                  title="Project actions"
                >
                  <EllipsisIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                </button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-56">
                <DropdownMenuItem onSelect={newThreadHere}>
                  <SquarePenIcon aria-hidden />
                  New thread here
                </DropdownMenuItem>
                {group.project ? (
                  <DropdownMenuItem onSelect={() => setRenaming(true)}>
                    <PencilIcon aria-hidden />
                    Rename…
                  </DropdownMenuItem>
                ) : !localHost ? null : (
                  <DropdownMenuItem
                    onSelect={() =>
                      void addProject({ type: "existing", path: folder }).then((result) => {
                        if ("error" in result) {
                          pushToast({
                            title: "Couldn't keep the project.",
                            description: result.error,
                            variant: "error",
                          });
                        }
                      })
                    }
                  >
                    <PinIcon aria-hidden />
                    Keep in the sidebar
                  </DropdownMenuItem>
                )}
                <DropdownMenuItem
                  onSelect={() => {
                    void navigator.clipboard?.writeText(folder);
                    pushToast({ title: "Path copied.", description: folder, variant: "info" });
                  }}
                >
                  <CopyIcon aria-hidden />
                  Copy path
                </DropdownMenuItem>
                {group.project ? (
                  <>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem
                      onSelect={() => {
                        const project = group.project;
                        if (!project) return;
                        void removeProject(project.id).then((ok) => {
                          if (ok) {
                            pushToast({
                              title: `Removed ${project.name} from the sidebar.`,
                              description:
                                group.threads.length > 0
                                  ? "The folder is untouched, and its threads still list here."
                                  : "The folder on the host is untouched.",
                              variant: "info",
                            });
                          }
                        });
                      }}
                    >
                      <XIcon aria-hidden />
                      Remove from sidebar
                    </DropdownMenuItem>
                  </>
                ) : null}
              </DropdownMenuContent>
            </DropdownMenu>
            <button
              type="button"
              className="q-ibtn q-pg-add"
              aria-label={`New thread in ${group.name} on ${hostName}`}
              title={`New thread in ${group.name}`}
              onClick={newThreadHere}
            >
              <SquarePenIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          </>
        )}
      </div>
      {closed ? null : group.threads.length === 0 ? (
        <div className="q-rows">
          <button type="button" className="q-pg-empty" onClick={newThreadHere}>
            No threads yet. Start one
          </button>
        </div>
      ) : (
        <div className="q-rows">
          {group.threads.map((thread) => (
            <ThreadRow
              key={thread.thread_id}
              thread={thread}
              host={hosts[thread.host]}
              index={0}
              compact
              selected={selectedId === thread.thread_id}
              tabStop={selectedId === thread.thread_id}
              resumeAt={thread.resume_at ?? thread.limit?.reset_at ?? null}
              now={now}
              onOpen={() => openThread(thread.thread_id)}
              onAcknowledge={() => void acknowledgeThread(thread.thread_id)}
              onSettle={() => void setThreadTab(thread.thread_id, "settled")}
              onReopen={() => void setThreadTab(thread.thread_id, "done")}
              onPin={() => void togglePin(thread.thread_id)}
              onSnooze={(until) => void snoozeThread(thread.thread_id, until)}
            />
          ))}
        </div>
      )}
    </section>
  );
}

/** Drop zone after the last project, so a project can move to the end. */
export function ProjectsTail({
  order,
  dragging,
  onDragging,
}: {
  order: string[];
  dragging: string | null;
  onDragging: (key: string | null) => void;
}) {
  const [over, setOver] = useState(false);
  if (!dragging) return null;
  return (
    <div
      className={cn("q-pg-tail", over && "on")}
      aria-hidden
      onDragOver={(event) => {
        event.preventDefault();
        setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(event) => {
        event.preventDefault();
        setOver(false);
        moveProject(order, dragging, null);
        onDragging(null);
      }}
    />
  );
}

function RenameField({
  initial,
  onDone,
}: {
  initial: string;
  onDone: (name: string | null) => void | Promise<void>;
}) {
  const [value, setValue] = useState(initial);
  const ref = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);
  return (
    <input
      ref={ref}
      className="q-input q-pg-rename"
      aria-label="Project name"
      value={value}
      onChange={(event) => setValue(event.target.value)}
      onBlur={() => void onDone(value.trim() || null)}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          void onDone(value.trim() || null);
        }
        if (event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          void onDone(null);
        }
      }}
    />
  );
}

export function useProjectDrag(): [string | null, (key: string | null) => void] {
  return useState<string | null>(null);
}
