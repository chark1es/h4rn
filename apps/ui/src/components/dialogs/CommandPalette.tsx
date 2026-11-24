import {
  ArchiveIcon,
  ChartNoAxesColumnIcon,
  CheckIcon,
  CircleDotIcon,
  Code2Icon,
  FileDiffIcon,
  GitForkIcon,
  GitPullRequestIcon,
  InboxIcon,
  KeyboardIcon,
  LibraryIcon,
  PanelLeftIcon,
  PanelRightIcon,
  PinIcon,
  PlusIcon,
  RefreshCwIcon,
  SearchIcon,
  ServerIcon,
  SettingsIcon,
  Share2Icon,
  ShieldCheckIcon,
  SmartphoneIcon,
  SquareIcon,
  SunMoonIcon,
  TerminalIcon,
  WorkflowIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { KeyGlyphs } from "@/components/common/Kbd";
import {
  CommandDialog,
  CommandEmpty,
  CommandFooter,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandShortcut,
} from "@/components/ui/command";
import { openGuestShare } from "@/features/guest/guestStore";
import { glyphForThread } from "@/lib/agent";
import { harnessLabel } from "@/lib/entities";
import { latestForkableTurn } from "@/lib/fork";
import { paletteScore } from "@/lib/palette";
import {
  acknowledgeThread,
  closePalette,
  forkThread,
  getClient,
  interruptThread,
  navigate,
  openLinkPullRequest,
  openNewThread,
  openShortcuts,
  openThread,
  pendingApprovalIn,
  requestSearchFocus,
  respondApproval,
  setPanelTab,
  setTab,
  setThreadTab,
  togglePanel,
  togglePin,
  toggleSidebar,
  toggleTerminal,
  toggleTheme,
  toggleView,
  useApp,
} from "@/lib/store/appStore";
import type { StreamTurn } from "@/lib/store/projection";

const NO_TURNS: StreamTurn[] = [];

function projectName(path: string): string {
  return path.split("/").filter(Boolean).pop() ?? path;
}

export function CommandPalette() {
  const open = useApp((state) => state.ui.paletteOpen);
  const route = useApp((state) => state.ui.route);
  const threads = useApp((state) => state.threads);
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection.status);
  const sidebarOpen = useApp((state) => state.ui.layout.sidebarOpen);
  const panelOpen = useApp((state) => state.ui.layout.panelOpen);
  const terminalOpen = useApp((state) => state.ui.layout.terminalOpen);
  const approvalId = useApp((state) =>
    state.ui.route.kind === "thread"
      ? pendingApprovalIn(state.logs[state.ui.route.threadId])
      : null,
  );
  const [query, setQuery] = useState("");
  const lastFocus = useRef<HTMLElement | null>(null);

  const currentId = route.kind === "thread" ? route.threadId : null;
  const current = currentId ? threads[currentId] : undefined;
  const currentTurns = useApp((state) =>
    currentId ? (state.logs[currentId]?.turns ?? NO_TURNS) : NO_TURNS,
  );
  const forkTurn = latestForkableTurn(currentTurns);

  // The palette opens from a shortcut, not a trigger. Track the last element
  // focused outside the dialog so Escape can put focus back where it was.
  useEffect(() => {
    const record = (event: FocusEvent) => {
      const target = event.target;
      if (!(target instanceof HTMLElement)) return;
      if (target.closest('[data-slot="dialog-content"]')) return;
      lastFocus.current = target;
    };
    document.addEventListener("focusin", record);
    return () => document.removeEventListener("focusin", record);
  }, []);

  // Reset the filter each time the palette opens so it always starts at the
  // prototype's short list rather than a stale search.
  useEffect(() => {
    if (open) setQuery("");
  }, [open]);

  const searching = query.trim().length > 0;

  const recent = useMemo(
    () =>
      Object.values(threads)
        .filter((thread) => thread.thread_id !== currentId)
        .sort((a, b) => Date.parse(b.last_activity) - Date.parse(a.last_activity))
        .slice(0, searching ? 5 : 2),
    [threads, currentId, searching],
  );

  const close = (action: () => void) => {
    closePalette();
    action();
  };

  const currentGlyph = current
    ? glyphForThread(current, hosts[current.host]?.online ?? false)
    : null;

  // The resting palette mirrors the prototype: three groups, ten rows, which
  // fit the 454 px list exactly. The long tail appears once the person starts
  // typing.

  return (
    <CommandDialog
      open={open}
      onOpenChange={(value) => (value ? undefined : closePalette())}
      title="Command palette"
      description="Search threads, run a command, or move around h4rn."
      filter={paletteScore}
      onCloseAutoFocus={(event) => {
        event.preventDefault();
        const target = lastFocus.current;
        if (target && document.contains(target)) target.focus();
      }}
    >
      <CommandInput
        placeholder="Search threads, hosts, files, or commands"
        autoFocus
        onValueChange={setQuery}
        trailing={
          current ? (
            <span className="q-pal-ctx">
              {currentGlyph ? <AgentGlyph glyph={currentGlyph} size={12} /> : null}
              <span
                style={{
                  maxWidth: 220,
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
              >
                {current.title}
              </span>
            </span>
          ) : null
        }
      />
      <CommandList>
        <CommandEmpty>
          No commands match that. Try a thread title, a host name, or a command.
        </CommandEmpty>

        {current ? (
          <CommandGroup heading="This thread">
            {approvalId ? (
              <>
                <CommandItem
                  value="approve pending action go"
                  onSelect={() =>
                    close(() => void respondApproval(current.thread_id, approvalId, "approve"))
                  }
                >
                  <ShieldCheckIcon aria-hidden />
                  Approve pending action
                  <CommandShortcut command="approval.approve" />
                </CommandItem>
                <CommandItem
                  value="deny pending action refuse"
                  onSelect={() =>
                    close(() => void respondApproval(current.thread_id, approvalId, "deny"))
                  }
                >
                  <XIcon aria-hidden />
                  Deny pending action
                  <CommandShortcut command="approval.deny" />
                </CommandItem>
              </>
            ) : null}
            <CommandItem
              value="changes diff files"
              onSelect={() => close(() => setPanelTab("changes"))}
            >
              <FileDiffIcon aria-hidden />
              Open changes
              <CommandShortcut command="panel.changes" />
            </CommandItem>
            <CommandItem
              value="subagents agents tree"
              onSelect={() => close(() => setPanelTab("subagents"))}
            >
              <WorkflowIcon aria-hidden />
              Open subagents
              <CommandShortcut command="panel.subagents" />
            </CommandItem>
            <CommandItem value="link pull request pr url" onSelect={() => openLinkPullRequest()}>
              <GitPullRequestIcon aria-hidden />
              Link pull request…
            </CommandItem>
            <CommandItem
              value="fork branch copy latest turn"
              disabled={!forkTurn}
              onSelect={() => {
                if (forkTurn) close(() => void forkThread(current.thread_id, forkTurn));
              }}
            >
              <GitForkIcon aria-hidden />
              Fork from latest turn
            </CommandItem>
            {current.tab !== "settled" ? (
              <CommandItem
                value="share guest teammate link"
                onSelect={() => close(() => openGuestShare(current.thread_id))}
              >
                <Share2Icon aria-hidden />
                Share with guest…
              </CommandItem>
            ) : null}
            <CommandItem
              value="pin keep working"
              onSelect={() => close(() => void togglePin(current.thread_id))}
            >
              <PinIcon aria-hidden />
              {current.pinned ? "Unpin from Working" : "Keep in Working"}
              <CommandShortcut command="thread.pin" />
            </CommandItem>
            {current.tab === "done" ? (
              <CommandItem
                value="acknowledge done"
                onSelect={() => close(() => void acknowledgeThread(current.thread_id))}
              >
                <CheckIcon aria-hidden />
                Acknowledge
                <CommandShortcut command="thread.acknowledge" />
              </CommandItem>
            ) : null}
            {current.tab !== "settled" ? (
              <CommandItem
                value="settle now archive"
                onSelect={() => close(() => setThreadTab(current.thread_id, "settled"))}
              >
                <InboxIcon aria-hidden />
                Settle now
                <CommandShortcut command="thread.settle" />
              </CommandItem>
            ) : null}
            {current.tab === "working" ? (
              <CommandItem
                value="interrupt turn stop"
                onSelect={() => close(() => void interruptThread(current.thread_id))}
              >
                <SquareIcon aria-hidden />
                Interrupt turn
                <CommandShortcut command="turn.interrupt" />
              </CommandItem>
            ) : null}
          </CommandGroup>
        ) : null}

        <CommandGroup heading="Threads">
          {recent.map((thread) => (
            <CommandItem
              key={thread.thread_id}
              value={`${thread.title} ${thread.host} ${harnessLabel(thread.harness)}`}
              onSelect={() => close(() => openThread(thread.thread_id))}
            >
              <span className="ic inline-flex w-4 justify-center">
                <AgentGlyph
                  glyph={glyphForThread(thread, hosts[thread.host]?.online ?? false)}
                  size={14}
                />
              </span>
              <span className="min-w-0 truncate">{thread.title}</span>
              <span className="ml-auto shrink-0 text-[12px] text-fg-3">
                {hosts[thread.host]?.name ?? thread.host} · {harnessLabel(thread.harness)}
              </span>
            </CommandItem>
          ))}
          <CommandItem value="new thread start" onSelect={() => close(openNewThread)}>
            <PlusIcon aria-hidden />
            New thread…
            <CommandShortcut command="thread.new" />
          </CommandItem>
          {searching ? (
            <CommandItem
              value="search threads find"
              onSelect={() =>
                close(() => {
                  navigate({ kind: "inbox" });
                  requestSearchFocus();
                })
              }
            >
              <SearchIcon aria-hidden />
              Search threads…
            </CommandItem>
          ) : null}
        </CommandGroup>

        <CommandGroup heading="Go to">
          <CommandItem value="working tab" onSelect={() => close(() => setTab("working"))}>
            <InboxIcon aria-hidden />
            Go to Working
            <CommandShortcut command="tab.working" />
          </CommandItem>
          <CommandItem value="done tab" onSelect={() => close(() => setTab("done"))}>
            <CheckIcon aria-hidden />
            Go to Done
            <CommandShortcut command="tab.done" />
          </CommandItem>
          <CommandItem value="settled tab" onSelect={() => close(() => setTab("settled"))}>
            <ArchiveIcon aria-hidden />
            Go to Settled
            <CommandShortcut command="tab.settled" />
          </CommandItem>
          <CommandItem
            value="pair a device phone qr code"
            onSelect={() =>
              close(() =>
                navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" }),
              )
            }
          >
            <SmartphoneIcon aria-hidden />
            Pair a device…
          </CommandItem>
          {searching ? (
            <>
              <CommandItem
                value="hosts machines"
                onSelect={() => close(() => navigate({ kind: "hosts" }))}
              >
                <ServerIcon aria-hidden />
                Hosts
              </CommandItem>
              <CommandItem
                value="library mcp skills"
                onSelect={() =>
                  close(() =>
                    navigate({ kind: "placeholder", surface: "library", path: "/library" }),
                  )
                }
              >
                <LibraryIcon aria-hidden />
                MCP servers and skills
                <CommandShortcut command="nav.tools" />
              </CommandItem>
              <CommandItem
                value="usage limits"
                onSelect={() =>
                  close(() => navigate({ kind: "placeholder", surface: "usage", path: "/usage" }))
                }
              >
                <ChartNoAxesColumnIcon aria-hidden />
                Usage &amp; limits
                <CommandShortcut command="nav.usage" />
              </CommandItem>
              <CommandItem
                value="ide editor"
                onSelect={() => close(() => navigate({ kind: "ide" }))}
              >
                <Code2Icon aria-hidden />
                IDE
              </CommandItem>
              <CommandItem
                value="issues trackers"
                onSelect={() =>
                  close(() => navigate({ kind: "placeholder", surface: "issues", path: "/issues" }))
                }
              >
                <CircleDotIcon aria-hidden />
                Open issues
                <CommandShortcut command="nav.issues" />
              </CommandItem>
              <CommandItem
                value="pull requests prs"
                onSelect={() =>
                  close(() => navigate({ kind: "placeholder", surface: "pulls", path: "/pulls" }))
                }
              >
                <GitPullRequestIcon aria-hidden />
                Pull requests
                <CommandShortcut command="nav.pulls" />
              </CommandItem>
              <CommandItem
                value="settings"
                onSelect={() =>
                  close(() =>
                    navigate({ kind: "placeholder", surface: "settings", path: "/settings" }),
                  )
                }
              >
                <SettingsIcon aria-hidden />
                Settings
                <CommandShortcut command="settings.open" />
              </CommandItem>
            </>
          ) : null}
        </CommandGroup>

        <CommandGroup heading="View">
          {current ? (
            <CommandItem value="open ide editor project" onSelect={() => close(toggleView)}>
              <Code2Icon aria-hidden />
              Open {projectName(current.workspace_path)} in the IDE
              <CommandShortcut command="view.toggle" />
            </CommandItem>
          ) : null}
          <CommandItem value="toggle right panel" onSelect={() => close(togglePanel)}>
            <PanelRightIcon aria-hidden />
            {panelOpen ? "Hide right panel" : "Show right panel"}
            <CommandShortcut command="panel.toggle" />
          </CommandItem>
          <CommandItem value="toggle terminal shell" onSelect={() => close(toggleTerminal)}>
            <TerminalIcon aria-hidden />
            {terminalOpen ? "Hide terminal" : "Show terminal"}
            <CommandShortcut command="terminal.toggle" />
          </CommandItem>
          <CommandItem value="toggle sidebar" onSelect={() => close(toggleSidebar)}>
            <PanelLeftIcon aria-hidden />
            {sidebarOpen ? "Hide sidebar" : "Show sidebar"}
            <CommandShortcut command="sidebar.toggle" />
          </CommandItem>
          {searching ? (
            <>
              <CommandItem value="toggle theme dark light" onSelect={() => close(toggleTheme)}>
                <SunMoonIcon aria-hidden />
                Toggle theme
                <CommandShortcut command="theme.toggle" />
              </CommandItem>
              <CommandItem value="keyboard shortcuts help" onSelect={() => close(openShortcuts)}>
                <KeyboardIcon aria-hidden />
                Keyboard shortcuts
                <CommandShortcut command="shortcuts.open" />
              </CommandItem>
              <CommandItem
                value="copy thread link"
                disabled={!current}
                onSelect={() =>
                  close(() => {
                    if (!current) return;
                    void navigator.clipboard?.writeText(
                      `${window.location.origin}/t/${current.thread_id}`,
                    );
                  })
                }
              >
                Copy thread link
              </CommandItem>
              {connection !== "online" ? (
                <CommandItem
                  value="reconnect connection"
                  onSelect={() => close(() => getClient().reconnectNow())}
                >
                  <RefreshCwIcon aria-hidden />
                  Reconnect to the daemon
                </CommandItem>
              ) : null}
            </>
          ) : null}
        </CommandGroup>
      </CommandList>
      <CommandFooter>
        <span>
          <span className="q-kbd">
            <KeyGlyphs value="↑" />
          </span>{" "}
          <span className="q-kbd">
            <KeyGlyphs value="↓" />
          </span>{" "}
          Move
        </span>
        <span>
          <span className="q-kbd">
            <KeyGlyphs value="↵" />
          </span>{" "}
          Run
        </span>
        <span>
          <span className="q-kbd">Esc</span> Close
        </span>
        <span style={{ flex: 1 }} />
        <span>
          Searches{" "}
          {[...new Set(Object.values(hosts).map((host) => host.name))].slice(0, 3).join(", ") ||
            "this machine"}
        </span>
      </CommandFooter>
    </CommandDialog>
  );
}
