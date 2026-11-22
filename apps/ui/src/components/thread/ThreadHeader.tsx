import {
  CircleSlash2Icon,
  ExternalLinkIcon,
  FileDiffIcon,
  GitBranchIcon,
  MoreHorizontalIcon,
  PanelRightIcon,
  PencilIcon,
  PinIcon,
  Share2Icon,
  SquareIcon,
  SunMoonIcon,
  TerminalIcon,
} from "lucide-react";

import { useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { SidebarReveal } from "@/components/shell/Sidebar";
import { RenameThreadDialog } from "@/components/thread/RenameThreadDialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { HostSummary, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useViewportWidth } from "@/hooks/use-viewport-width";
import { glyphForThread } from "@/lib/agent";
import { harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { absoluteDateTime, relativeAge } from "@/lib/format";
import { shortcutText, titleWithShortcut } from "@/lib/keybindings";
import {
  acknowledgeThread,
  interruptThread,
  navigate,
  panelFits,
  pushToast,
  setThreadTab,
  setView,
  snoozeThread,
  togglePanel,
  togglePin,
  toggleTheme,
  useApp,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

export function ViewCluster({
  terminalOpen = false,
  onToggleTerminal,
}: {
  terminalOpen?: boolean;
  onToggleTerminal?: (() => void) | undefined;
}) {
  const view = useApp((state) => state.ui.layout.view);
  const layout = useApp((state) => state.ui.layout);
  const viewportWidth = useViewportWidth();
  // Pressed means the panel is on screen, not that it was asked for.
  const panelOpen = layout.panelOpen && panelFits(layout, viewportWidth);
  return (
    <>
      <div
        className="q-seg inline q-viewseg"
        role="tablist"
        aria-label="View"
        title={titleWithShortcut("Switch view", "view.toggle")}
      >
        <button
          type="button"
          role="tab"
          aria-selected={view === "thread"}
          onClick={() => setView("thread")}
        >
          Thread
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={view === "ide"}
          onClick={() => setView("ide")}
        >
          IDE
        </button>
      </div>
      {onToggleTerminal ? (
        <button
          type="button"
          className={cn("q-ibtn", terminalOpen && "on")}
          aria-label="Toggle terminal"
          aria-pressed={terminalOpen}
          title={titleWithShortcut("Terminal", "terminal.toggle")}
          onClick={onToggleTerminal}
        >
          <TerminalIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        </button>
      ) : null}
      <button
        type="button"
        className={cn("q-ibtn", panelOpen && "on")}
        aria-label="Right panel"
        aria-pressed={panelOpen}
        title={titleWithShortcut("Right panel", "panel.toggle")}
        onClick={togglePanel}
      >
        <PanelRightIcon className="q-icon" strokeWidth={1.5} aria-hidden />
      </button>
    </>
  );
}

export function ThreadHeader({
  summary,
  host,
  onBack,
  terminalOpen = false,
  onToggleTerminal,
  onOpenPanel,
  stale = false,
}: {
  summary: ThreadSummary;
  host: HostSummary | undefined;
  onBack: () => void;
  terminalOpen?: boolean;
  onToggleTerminal?: () => void;
  onOpenPanel?: () => void;
  stale?: boolean;
}) {
  const [renaming, setRenaming] = useState(false);
  const hostOnline = host?.online ?? false;
  const working = summary.tab === "working";
  const unreachable =
    summary.agent_state === "unreachable" ||
    summary.agent_state === "error" ||
    (working && !hostOnline);
  // A dropped connection outranks the last agent state: do not animate work
  // the daemon can no longer confirm.
  const glyph =
    stale && working && !unreachable
      ? { kind: "offline" as const, label: "Last seen running" }
      : glyphForThread(summary, hostOnline);
  const isPhone = useIsPhone();
  const hostName = host?.name ?? summary.host;
  const stateLabel =
    summary.tab === "done" && summary.agent_state === "done"
      ? `Done · ${relativeAge(summary.last_activity)}`
      : glyph.label;

  const threadMenu = (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button type="button" className={isPhone ? "navbtn" : "q-ibtn"} aria-label="Thread actions">
          <MoreHorizontalIcon className={isPhone ? "q-icon s22" : "q-icon"} strokeWidth={1.5} />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuLabel>Thread</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => void togglePin(summary.thread_id)}>
          <PinIcon aria-hidden />
          {summary.pinned ? "Unpin from Working" : "Keep in Working"}
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => setRenaming(true)}>
          <PencilIcon aria-hidden />
          Rename…
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => void setThreadTab(summary.thread_id, "done")}>
          Move to Done
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => void setThreadTab(summary.thread_id, "settled")}>
          Settle now
        </DropdownMenuItem>
        <DropdownMenuItem
          onSelect={() =>
            void snoozeThread(summary.thread_id, new Date(Date.now() + 3_600_000).toISOString())
          }
        >
          Snooze for 1 h
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          onSelect={() => {
            void navigator.clipboard
              ?.writeText(`${window.location.origin}/t/${summary.thread_id}`)
              .then(() => pushToast({ title: "Thread link copied.", variant: "success" }));
          }}
        >
          <Share2Icon aria-hidden />
          Copy thread link
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => navigate({ kind: "hosts" })}>
          <ExternalLinkIcon aria-hidden />
          Open hosts
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => navigate({ kind: "ide" })}>
          <CircleSlash2Icon aria-hidden />
          Open in IDE
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={toggleTheme}>
          <SunMoonIcon aria-hidden />
          Toggle theme
          <span className="ml-auto text-small text-fg-3">{shortcutText("theme.toggle")}</span>
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem disabled>
          <span className="text-small">Started {absoluteDateTime(summary.last_activity)}</span>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );

  const renameDialog = (
    <RenameThreadDialog
      threadId={summary.thread_id}
      initial={summary.title}
      open={renaming}
      onOpenChange={setRenaming}
    />
  );

  if (isPhone) {
    return (
      <>
        {renameDialog}
        <div className="q-ph-nav">
          <button type="button" className="navbtn" aria-label="Back to inbox" onClick={onBack}>
            <span aria-hidden className="q-ph-back-glyph">
              ‹
            </span>
            Inbox
          </button>
          <span className="ttl" />
          <div className="q-ph-nav-actions">
            {onToggleTerminal ? (
              <button
                type="button"
                className={cn("navbtn", terminalOpen && "on")}
                aria-label="Toggle terminal"
                aria-pressed={terminalOpen}
                onClick={onToggleTerminal}
              >
                <TerminalIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
              </button>
            ) : null}
            {onOpenPanel ? (
              <button
                type="button"
                className="navbtn"
                aria-label="Changes and subagents"
                onClick={onOpenPanel}
              >
                <FileDiffIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
              </button>
            ) : null}
            {threadMenu}
          </div>
        </div>
        <div className="q-ph-th-title">
          <div className="q-ph-th-name">{summary.title}</div>
          <div className="q-ph-th-meta">
            <EntityAvatar
              seed={machineSeed(summary.host)}
              kind="machine"
              label={hostName}
              size={16}
            />
            <span>{hostName}</span>
            <span className="q-sep">·</span>
            <EntityAvatar
              seed={harnessSeed(summary.host, summary.harness)}
              kind="harness"
              label={harnessLabel(summary.harness)}
              size={14}
            />
            <span>{harnessLabel(summary.harness)}</span>
            <span className="grow" />
            <span className={cn("q-th-state", summary.needs_attention && "q-accent")}>
              <AgentGlyph glyph={glyph} size={14} />
              <span>{stateLabel}</span>
            </span>
          </div>
        </div>
      </>
    );
  }

  return (
    <header className="q-th-head">
      {renameDialog}
      <SidebarReveal />
      <div className="q-th-titles">
        <h1>{summary.title}</h1>
        <div className="q-prov">
          <EntityAvatar
            seed={machineSeed(summary.host)}
            kind="machine"
            label={hostName}
            size={14}
          />
          <span>{hostName}</span>
          <span className="q-sep">/</span>
          <EntityAvatar
            seed={harnessSeed(summary.host, summary.harness)}
            kind="harness"
            label={harnessLabel(summary.harness)}
            size={14}
          />
          <span>{harnessLabel(summary.harness)}</span>
          <span className="opt">
            <span className="q-sep">/</span> {summary.model}
          </span>
          {summary.branch ? (
            <span className="br">
              <span className="q-sep">/</span>
              <GitBranchIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              <span className="q-mono" style={{ fontSize: 11.5 }}>
                {summary.branch}
              </span>
            </span>
          ) : null}
        </div>
      </div>

      <div className={cn("q-th-state", summary.needs_attention && "q-accent")}>
        <AgentGlyph glyph={glyph} size={14} />
        <span>{stateLabel}</span>
      </div>

      <div className="q-th-actions">
        {working && !stale && !unreachable ? (
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void interruptThread(summary.thread_id)}
          >
            <SquareIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Stop
          </button>
        ) : null}

        {summary.tab === "done" ? (
          <>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() =>
                void snoozeThread(summary.thread_id, new Date(Date.now() + 3_600_000).toISOString())
              }
            >
              Snooze
            </button>
            <button
              type="button"
              className="q-btn primary"
              onClick={() => void acknowledgeThread(summary.thread_id)}
            >
              Acknowledge <span className="q-kbd">A</span>
            </button>
          </>
        ) : null}

        {threadMenu}
        <span className="q-vsep" />
        <ViewCluster terminalOpen={terminalOpen} onToggleTerminal={onToggleTerminal} />
      </div>
    </header>
  );
}
