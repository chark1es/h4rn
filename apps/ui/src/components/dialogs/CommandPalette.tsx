import {
  ArchiveIcon,
  ArrowLeftRightIcon,
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
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { ESC, KeyGlyphs } from "@/components/common/Kbd";
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
import { type englishResources, formatList, i18n } from "@/lib/i18n";
import { type CommandId, commandName } from "@/lib/keybindings";
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

type PaletteItemKey = keyof (typeof englishResources)["commands"]["palette"]["items"];

/** English text for a palette row, searched alongside the shown name. */
const english = i18n.getFixedT("en", "commands");

function projectName(path: string): string {
  return path.split("/").filter(Boolean).pop() ?? path;
}

type RowName = { name: string; english: string };

/** A command's one name, shown in the active language and searched in English too. */
function named(id: CommandId): RowName {
  return { name: commandName(id), english: commandName(id, "en") };
}

/**
 * One palette row. cmdk matches what was typed against the shown name, the
 * English name and the English synonyms, so someone who learned the palette
 * in English still finds "settle" or "diff" after switching language.
 *
 * A row that opens a dialog ends in "…", the menu convention for "asks for
 * more first". The command's shared name stays without it, because Settings →
 * Shortcuts and the Keyboard shortcuts dialog list the same name.
 */
function PaletteItem({
  row,
  synonyms,
  command,
  icon,
  disabled,
  opensDialog,
  onSelect,
}: {
  row: RowName;
  synonyms: string;
  command?: CommandId;
  icon?: ReactNode;
  disabled?: boolean;
  opensDialog?: boolean;
  onSelect: () => void;
}) {
  return (
    <CommandItem
      value={row.name}
      keywords={[row.english, synonyms]}
      disabled={disabled ?? false}
      onSelect={onSelect}
    >
      {icon}
      {opensDialog ? `${row.name}…` : row.name}
      {command ? <CommandShortcut command={command} /> : null}
    </CommandItem>
  );
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
  const { t } = useTranslation("commands");
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

  const item = (key: PaletteItemKey): RowName => ({
    name: t(`palette.items.${key}`),
    english: english(`palette.items.${key}`),
  });

  const hostNames = [...new Set(Object.values(hosts).map((host) => host.name))].slice(0, 3);

  // The resting palette mirrors the prototype: three groups, ten rows, which
  // fit the 454 px list exactly. The long tail appears once the person starts
  // typing.

  return (
    <CommandDialog
      open={open}
      onOpenChange={(value) => (value ? undefined : closePalette())}
      title={t("palette.title")}
      description={t("palette.description")}
      filter={paletteScore}
      onCloseAutoFocus={(event) => {
        event.preventDefault();
        const target = lastFocus.current;
        if (target && document.contains(target)) target.focus();
      }}
    >
      <CommandInput
        placeholder={t("palette.placeholder")}
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
        <CommandEmpty>{t("palette.empty")}</CommandEmpty>

        {current ? (
          <CommandGroup heading={t("palette.groups.thisThread")}>
            {approvalId ? (
              <>
                <PaletteItem
                  row={named("approval.approve")}
                  synonyms="approve pending action go"
                  icon={<ShieldCheckIcon aria-hidden />}
                  command="approval.approve"
                  onSelect={() =>
                    close(() => void respondApproval(current.thread_id, approvalId, "approve"))
                  }
                />
                <PaletteItem
                  row={named("approval.deny")}
                  synonyms="deny pending action refuse"
                  icon={<XIcon aria-hidden />}
                  command="approval.deny"
                  onSelect={() =>
                    close(() => void respondApproval(current.thread_id, approvalId, "deny"))
                  }
                />
              </>
            ) : null}
            <PaletteItem
              row={named("panel.changes")}
              synonyms="open changes diff files"
              icon={<FileDiffIcon aria-hidden />}
              command="panel.changes"
              onSelect={() => close(() => setPanelTab("changes"))}
            />
            <PaletteItem
              row={named("panel.subagents")}
              synonyms="open subagents agents tree"
              icon={<WorkflowIcon aria-hidden />}
              command="panel.subagents"
              onSelect={() => close(() => setPanelTab("subagents"))}
            />
            <PaletteItem
              row={item("switchHarness")}
              synonyms="switch harness provider model"
              icon={<ArrowLeftRightIcon aria-hidden />}
              onSelect={() =>
                close(() => window.dispatchEvent(new CustomEvent("h4rn:switch-harness")))
              }
            />
            <PaletteItem
              row={item("linkPullRequest")}
              synonyms="link pull request pr url"
              icon={<GitPullRequestIcon aria-hidden />}
              onSelect={() => openLinkPullRequest()}
            />
            <PaletteItem
              row={item("fork")}
              synonyms="fork branch copy latest turn"
              icon={<GitForkIcon aria-hidden />}
              disabled={!forkTurn}
              onSelect={() => {
                if (forkTurn) close(() => void forkThread(current.thread_id, forkTurn));
              }}
            />
            {current.tab !== "settled" ? (
              <PaletteItem
                row={named("guest.share")}
                synonyms="share guest teammate link"
                opensDialog
                icon={<Share2Icon aria-hidden />}
                onSelect={() => close(() => openGuestShare(current.thread_id))}
              />
            ) : null}
            <PaletteItem
              row={current.pinned ? item("unpin") : named("thread.pin")}
              synonyms="pin keep working unpin"
              icon={<PinIcon aria-hidden />}
              command="thread.pin"
              onSelect={() => close(() => void togglePin(current.thread_id))}
            />
            {current.tab === "done" ? (
              <PaletteItem
                row={named("thread.acknowledge")}
                synonyms="acknowledge done"
                icon={<CheckIcon aria-hidden />}
                command="thread.acknowledge"
                onSelect={() => close(() => void acknowledgeThread(current.thread_id))}
              />
            ) : null}
            {current.tab !== "settled" ? (
              <PaletteItem
                row={named("thread.settle")}
                synonyms="settle now archive"
                icon={<InboxIcon aria-hidden />}
                command="thread.settle"
                onSelect={() => close(() => setThreadTab(current.thread_id, "settled"))}
              />
            ) : null}
            {current.tab === "working" ? (
              <PaletteItem
                row={named("turn.interrupt")}
                synonyms="interrupt turn stop"
                icon={<SquareIcon aria-hidden />}
                command="turn.interrupt"
                onSelect={() => close(() => void interruptThread(current.thread_id))}
              />
            ) : null}
          </CommandGroup>
        ) : null}

        <CommandGroup heading={t("palette.groups.threads")}>
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
          <PaletteItem
            row={named("thread.new")}
            synonyms="new thread start"
            opensDialog
            icon={<PlusIcon aria-hidden />}
            command="thread.new"
            onSelect={() => close(openNewThread)}
          />
          {searching ? (
            <PaletteItem
              row={item("searchThreads")}
              synonyms="search threads find"
              icon={<SearchIcon aria-hidden />}
              onSelect={() =>
                close(() => {
                  navigate({ kind: "inbox" });
                  requestSearchFocus();
                })
              }
            />
          ) : null}
        </CommandGroup>

        <CommandGroup heading={t("palette.groups.goTo")}>
          <PaletteItem
            row={named("tab.working")}
            synonyms="go to working tab"
            icon={<InboxIcon aria-hidden />}
            command="tab.working"
            onSelect={() => close(() => setTab("working"))}
          />
          <PaletteItem
            row={named("tab.done")}
            synonyms="go to done tab"
            icon={<CheckIcon aria-hidden />}
            command="tab.done"
            onSelect={() => close(() => setTab("done"))}
          />
          <PaletteItem
            row={named("tab.settled")}
            synonyms="go to settled tab"
            icon={<ArchiveIcon aria-hidden />}
            command="tab.settled"
            onSelect={() => close(() => setTab("settled"))}
          />
          <PaletteItem
            row={item("pairDevice")}
            synonyms="pair a device phone qr code"
            icon={<SmartphoneIcon aria-hidden />}
            onSelect={() =>
              close(() =>
                navigate({ kind: "placeholder", surface: "settings", path: "/settings/devices" }),
              )
            }
          />
          {searching ? (
            <>
              <PaletteItem
                row={item("hosts")}
                synonyms="hosts machines"
                icon={<ServerIcon aria-hidden />}
                onSelect={() => close(() => navigate({ kind: "hosts" }))}
              />
              <PaletteItem
                row={named("nav.tools")}
                synonyms="library mcp servers skills"
                icon={<LibraryIcon aria-hidden />}
                command="nav.tools"
                onSelect={() =>
                  close(() =>
                    navigate({ kind: "placeholder", surface: "library", path: "/library" }),
                  )
                }
              />
              <PaletteItem
                row={named("nav.usage")}
                synonyms="usage limits"
                icon={<ChartNoAxesColumnIcon aria-hidden />}
                command="nav.usage"
                onSelect={() =>
                  close(() => navigate({ kind: "placeholder", surface: "usage", path: "/usage" }))
                }
              />
              <PaletteItem
                row={item("ide")}
                synonyms="ide editor"
                icon={<Code2Icon aria-hidden />}
                onSelect={() => close(() => navigate({ kind: "ide" }))}
              />
              <PaletteItem
                row={named("nav.issues")}
                synonyms="open issues trackers"
                icon={<CircleDotIcon aria-hidden />}
                command="nav.issues"
                onSelect={() =>
                  close(() => navigate({ kind: "placeholder", surface: "issues", path: "/issues" }))
                }
              />
              <PaletteItem
                row={named("nav.pulls")}
                synonyms="pull requests prs"
                icon={<GitPullRequestIcon aria-hidden />}
                command="nav.pulls"
                onSelect={() =>
                  close(() => navigate({ kind: "placeholder", surface: "pulls", path: "/pulls" }))
                }
              />
              <PaletteItem
                row={named("settings.open")}
                synonyms="settings preferences"
                icon={<SettingsIcon aria-hidden />}
                command="settings.open"
                onSelect={() =>
                  close(() =>
                    navigate({ kind: "placeholder", surface: "settings", path: "/settings" }),
                  )
                }
              />
            </>
          ) : null}
        </CommandGroup>

        <CommandGroup heading={t("palette.groups.view")}>
          {current ? (
            <PaletteItem
              row={{
                name: t("palette.items.openInIde", {
                  project: projectName(current.workspace_path),
                }),
                english: english("palette.items.openInIde", {
                  project: projectName(current.workspace_path),
                }),
              }}
              synonyms={`open ide editor project ${commandName("view.toggle", "en")}`}
              icon={<Code2Icon aria-hidden />}
              command="view.toggle"
              onSelect={() => close(toggleView)}
            />
          ) : null}
          <PaletteItem
            row={panelOpen ? item("hideRightPanel") : item("showRightPanel")}
            synonyms={`toggle right panel ${commandName("panel.toggle", "en")}`}
            icon={<PanelRightIcon aria-hidden />}
            command="panel.toggle"
            onSelect={() => close(togglePanel)}
          />
          <PaletteItem
            row={terminalOpen ? item("hideTerminal") : item("showTerminal")}
            synonyms="toggle terminal shell"
            icon={<TerminalIcon aria-hidden />}
            command="terminal.toggle"
            onSelect={() => close(toggleTerminal)}
          />
          <PaletteItem
            row={sidebarOpen ? item("hideSidebar") : item("showSidebar")}
            synonyms="toggle sidebar"
            icon={<PanelLeftIcon aria-hidden />}
            command="sidebar.toggle"
            onSelect={() => close(toggleSidebar)}
          />
          {searching ? (
            <>
              <PaletteItem
                row={named("theme.toggle")}
                synonyms="toggle theme dark light"
                icon={<SunMoonIcon aria-hidden />}
                command="theme.toggle"
                onSelect={() => close(toggleTheme)}
              />
              <PaletteItem
                row={named("shortcuts.open")}
                synonyms="keyboard shortcuts help"
                opensDialog
                icon={<KeyboardIcon aria-hidden />}
                command="shortcuts.open"
                onSelect={() => close(openShortcuts)}
              />
              <PaletteItem
                row={item("copyLink")}
                synonyms="copy thread link"
                disabled={!current}
                onSelect={() =>
                  close(() => {
                    if (!current) return;
                    void navigator.clipboard?.writeText(
                      `${window.location.origin}/t/${current.thread_id}`,
                    );
                  })
                }
              />
              {connection !== "online" ? (
                <PaletteItem
                  row={item("reconnect")}
                  synonyms="reconnect connection"
                  icon={<RefreshCwIcon aria-hidden />}
                  onSelect={() => close(() => getClient().reconnectNow())}
                />
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
          {t("palette.footer.move")}
        </span>
        <span>
          <span className="q-kbd">
            <KeyGlyphs value="↵" />
          </span>{" "}
          {t("palette.footer.run")}
        </span>
        <span>
          <span className="q-kbd">{ESC}</span> {t("palette.footer.close")}
        </span>
        <span style={{ flex: 1 }} />
        <span>
          {hostNames.length > 0
            ? t("palette.footer.searchesHosts", {
                hosts: formatList(hostNames, "conjunction", "narrow"),
              })
            : t("palette.footer.searchesThisMachine")}
        </span>
      </CommandFooter>
    </CommandDialog>
  );
}
