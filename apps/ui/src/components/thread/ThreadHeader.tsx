import {
  AlarmClockIcon,
  ArchiveIcon,
  CheckCheckIcon,
  FileDiffIcon,
  GitBranchIcon,
  GitForkIcon,
  LinkIcon,
  MoreHorizontalIcon,
  PanelRightIcon,
  PencilIcon,
  PinIcon,
  ServerIcon,
  Share2Icon,
  SquareCodeIcon,
  SquareIcon,
  SunMoonIcon,
  TerminalIcon,
} from "lucide-react";

import { useState } from "react";
import { useTranslation } from "react-i18next";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { SidebarReveal } from "@/components/shell/Sidebar";
import { GitActionsDialog } from "@/components/thread/GitActionsDialog";
import { RenameThreadDialog } from "@/components/thread/RenameThreadDialog";
import { ThreadForkLine } from "@/components/thread/ThreadForkLine";
import { ShortcutCaps } from "@/components/ui/kbd";
import {
  Menu,
  MenuContent,
  MenuFooter,
  MenuGroup,
  MenuGroupLabel,
  MenuItem,
  MenuSeparator,
  MenuTrigger,
} from "@/components/ui/menu";
import { ThreadPullRequestBadge } from "@/features/forge/ThreadPullRequestBadge";
import { openGuestShare } from "@/features/guest/guestStore";
import { useThreadSettings } from "@/features/threads/settings";
import type { HostSummary, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useViewportWidth } from "@/hooks/use-viewport-width";
import { glyphForThread } from "@/lib/agent";
import { harnessSeed, machineSeed, threadHarnessLabel } from "@/lib/entities";
import { latestForkableTurn } from "@/lib/fork";
import { absoluteDateTime } from "@/lib/format";
import { formatRelativeTime } from "@/lib/i18n";
import { titleWithShortcut } from "@/lib/keybindings";
import {
  acknowledgeThread,
  forkThread,
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
import type { StreamTurn } from "@/lib/store/projection";
import { nonDefaultSettings } from "@/lib/threadControls";
import { cn } from "@/lib/utils";
import { HarnessSwitch } from "./HarnessSwitch";

const NO_TURNS: StreamTurn[] = [];

export function ViewCluster({
  terminalOpen = false,
  onToggleTerminal,
}: {
  terminalOpen?: boolean;
  onToggleTerminal?: (() => void) | undefined;
}) {
  const { t } = useTranslation("thread");
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
        aria-label={t("header.view")}
        title={titleWithShortcut(t("header.switchView"), "view.toggle")}
      >
        <button
          type="button"
          role="tab"
          aria-selected={view === "thread"}
          onClick={() => setView("thread")}
        >
          {t("header.thread")}
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
          aria-label={t("header.toggleTerminal")}
          aria-pressed={terminalOpen}
          title={titleWithShortcut(t("header.terminal"), "terminal.toggle")}
          onClick={onToggleTerminal}
        >
          <TerminalIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        </button>
      ) : null}
      <button
        type="button"
        className={cn("q-ibtn", panelOpen && "on")}
        aria-label={t("header.rightPanel")}
        aria-pressed={panelOpen}
        title={titleWithShortcut(t("header.rightPanel"), "panel.toggle")}
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
  const { t } = useTranslation("thread");
  const [renaming, setRenaming] = useState(false);
  const [gitOpen, setGitOpen] = useState(false);
  const hostOnline = host?.online ?? false;
  const working = summary.tab === "working";
  const turns = useApp((state) => state.logs[summary.thread_id]?.turns ?? NO_TURNS);
  const forkTurn = latestForkableTurn(turns);
  const threadSettings = useThreadSettings();
  const changedSettings = nonDefaultSettings(summary, threadSettings.defaultPermissionMode);
  const unreachable =
    summary.agent_state === "unreachable" ||
    summary.agent_state === "error" ||
    (working && !hostOnline);
  // A dropped connection outranks the last agent state: do not animate work
  // the daemon can no longer confirm.
  const glyph =
    stale && working && !unreachable
      ? { kind: "offline" as const, label: t("glyph.lastSeenRunning") }
      : glyphForThread(summary, hostOnline);
  const isPhone = useIsPhone();
  const hostName = host?.name ?? summary.host;
  const stateLabel =
    summary.tab === "done" && summary.agent_state === "done"
      ? t("header.doneAge", {
          when: formatRelativeTime(summary.last_activity, Date.now(), {
            numeric: "auto",
            style: "narrow",
          }),
        })
      : glyph.label;

  const threadMenu = (
    <Menu>
      <MenuTrigger className={isPhone ? "navbtn" : "q-ibtn"} aria-label={t("header.actions")}>
        <MoreHorizontalIcon
          className={isPhone ? "q-icon s22" : "q-icon"}
          strokeWidth={1.5}
          aria-hidden
        />
      </MenuTrigger>
      <MenuContent align="end" className="min-w-56">
        <MenuGroup>
          <MenuGroupLabel>{t("header.thread")}</MenuGroupLabel>
          <MenuItem
            icon={<PinIcon />}
            command="thread.pin"
            onSelect={() => void togglePin(summary.thread_id)}
          >
            {summary.pinned ? t("header.menu.unpin") : t("header.menu.keep")}
          </MenuItem>
          <MenuItem icon={<PencilIcon />} onSelect={() => setRenaming(true)}>
            {t("header.menu.rename")}
          </MenuItem>
          <MenuItem
            icon={<GitForkIcon />}
            disabled={!forkTurn}
            onSelect={() => {
              if (forkTurn) void forkThread(summary.thread_id, forkTurn);
            }}
          >
            {t("header.menu.fork")}
          </MenuItem>
          <MenuItem icon={<GitBranchIcon />} onSelect={() => setGitOpen(true)}>
            {t("header.menu.git")}
          </MenuItem>
          <MenuItem
            icon={<CheckCheckIcon />}
            onSelect={() => void setThreadTab(summary.thread_id, "done")}
          >
            {t("header.menu.moveToDone")}
          </MenuItem>
          <MenuItem
            icon={<ArchiveIcon />}
            command="thread.settle"
            onSelect={() => void setThreadTab(summary.thread_id, "settled")}
          >
            {t("header.menu.settleNow")}
          </MenuItem>
          <MenuItem
            icon={<AlarmClockIcon />}
            command="thread.snooze"
            onSelect={() =>
              void snoozeThread(summary.thread_id, new Date(Date.now() + 3_600_000).toISOString())
            }
          >
            {t("header.menu.snoozeHour")}
          </MenuItem>
        </MenuGroup>
        <MenuSeparator />
        {summary.tab !== "settled" ? (
          <MenuItem
            icon={<Share2Icon />}
            command="guest.share"
            onSelect={() => openGuestShare(summary.thread_id)}
          >
            {t("header.menu.share")}
          </MenuItem>
        ) : null}
        <MenuItem
          icon={<LinkIcon />}
          onSelect={() => {
            void navigator.clipboard
              ?.writeText(`${window.location.origin}/t/${summary.thread_id}`)
              .then(() => pushToast({ title: t("header.menu.linkCopied"), variant: "success" }));
          }}
        >
          {t("header.menu.copyLink")}
        </MenuItem>
        <MenuItem icon={<ServerIcon />} onSelect={() => navigate({ kind: "hosts" })}>
          {t("header.menu.openHosts")}
        </MenuItem>
        {isPhone ? null : (
          <MenuItem icon={<SquareCodeIcon />} onSelect={() => navigate({ kind: "ide" })}>
            {t("header.menu.openInIde")}
          </MenuItem>
        )}
        {isPhone ? null : (
          <>
            <MenuSeparator />
            <MenuItem icon={<SunMoonIcon />} command="theme.toggle" onSelect={toggleTheme}>
              {t("header.menu.toggleTheme")}
            </MenuItem>
          </>
        )}
        <MenuSeparator />
        <MenuFooter>
          {t("header.menu.started", { time: absoluteDateTime(summary.last_activity) })}
        </MenuFooter>
      </MenuContent>
    </Menu>
  );

  const renameDialog = (
    <RenameThreadDialog
      threadId={summary.thread_id}
      initial={summary.title}
      open={renaming}
      onOpenChange={setRenaming}
    />
  );

  const gitDialog = <GitActionsDialog summary={summary} open={gitOpen} onOpenChange={setGitOpen} />;

  if (isPhone) {
    return (
      <>
        {renameDialog}
        {gitDialog}
        <div className="q-ph-nav">
          <button type="button" className="navbtn" aria-label={t("header.back")} onClick={onBack}>
            <span aria-hidden className="q-ph-back-glyph">
              ‹
            </span>
            {t("header.inbox")}
          </button>
          <span className="ttl" />
          <div className="q-ph-nav-actions">
            {onToggleTerminal ? (
              <button
                type="button"
                className={cn("navbtn", terminalOpen && "on")}
                aria-label={t("header.toggleTerminal")}
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
                aria-label={t("header.panels")}
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
          <ThreadForkLine summary={summary} />
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
              seed={harnessSeed(summary.host, summary.harness, summary.agent_id)}
              kind="harness"
              label={threadHarnessLabel(summary.harness, summary.agent_id)}
              size={14}
            />
            <HarnessSwitch summary={summary} />
            <span className="grow" />
            <ThreadPullRequestBadge threadId={summary.thread_id} />
            <span className={cn("q-th-state", summary.needs_attention && "q-accent")}>
              <AgentGlyph glyph={glyph} size={14} />
              <span title={stateLabel}>{stateLabel}</span>
            </span>
          </div>
        </div>
      </>
    );
  }

  return (
    <header className="q-th-head">
      {renameDialog}
      {gitDialog}
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
            seed={harnessSeed(summary.host, summary.harness, summary.agent_id)}
            kind="harness"
            label={threadHarnessLabel(summary.harness, summary.agent_id)}
            size={14}
          />
          <HarnessSwitch summary={summary} />
          <span className="opt">
            <span className="q-sep">/</span> {summary.model}
          </span>
          {changedSettings.length > 0 ? (
            <span className="q-faint" style={{ fontSize: 11.5 }}>
              {changedSettings.join(" · ")}
            </span>
          ) : null}
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
        <ThreadForkLine summary={summary} />
      </div>

      <div className={cn("q-th-state", summary.needs_attention && "q-accent")}>
        <AgentGlyph glyph={glyph} size={14} />
        <span title={stateLabel}>{stateLabel}</span>
      </div>

      <div className="q-th-actions">
        <ThreadPullRequestBadge threadId={summary.thread_id} />
        {working && !stale && !unreachable ? (
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void interruptThread(summary.thread_id)}
          >
            <SquareIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            {t("header.stop")}
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
              {t("header.snooze")}
            </button>
            <button
              type="button"
              className="q-btn primary"
              onClick={() => void acknowledgeThread(summary.thread_id)}
            >
              {t("header.acknowledge")} <ShortcutCaps command="thread.acknowledge" />
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
