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
  LinkIcon,
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
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandFooter,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { ESC, Kbd, KbdGroup } from "@/components/ui/kbd";
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

type PaletteRow = {
  key: string;
  /** What the search matches, with `keywords`. */
  value: string;
  keywords?: string[];
  label: ReactNode;
  icon?: ReactNode;
  trailing?: ReactNode;
  command?: CommandId | undefined;
  disabled?: boolean | undefined;
  onSelect: () => void;
};

type PaletteGroup = { key: string; heading: string; rows: PaletteRow[] };

/**
 * One palette row. The search matches the shown name, the English name and
 * the English synonyms, so someone who learned the palette in English still
 * finds "settle" or "diff" after switching language.
 *
 * A row that opens a dialog ends in "…", the menu convention for "asks for
 * more first". The command's shared name stays without it, because Settings →
 * Shortcuts and the Keyboard shortcuts dialog list the same name.
 */
function paletteRow({
  key,
  row,
  synonyms,
  command,
  icon,
  disabled,
  opensDialog,
  onSelect,
}: {
  key: string;
  row: RowName;
  synonyms: string;
  command?: CommandId;
  icon?: ReactNode;
  disabled?: boolean;
  opensDialog?: boolean;
  onSelect: () => void;
}): PaletteRow {
  return {
    key,
    value: row.name,
    keywords: [row.english, synonyms],
    label: opensDialog ? `${row.name}…` : row.name,
    icon,
    command,
    disabled,
    onSelect,
  };
}

/**
 * Hides rows that score 0 and puts each group's best matches first. Groups
 * keep their order, so the current thread's actions stay on top.
 */
function rankGroups(groups: PaletteGroup[], query: string): PaletteGroup[] {
  if (!query.trim()) return groups;
  return groups
    .map((group) => ({
      ...group,
      rows: group.rows
        .map((row) => ({ row, score: paletteScore(row.value, query, row.keywords) }))
        .filter((entry) => entry.score > 0)
        .sort((a, b) => b.score - a.score)
        .map((entry) => entry.row),
    }))
    .filter((group) => group.rows.length > 0);
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

  const currentId = route.kind === "thread" ? route.threadId : null;
  const current = currentId ? threads[currentId] : undefined;
  const currentTurns = useApp((state) =>
    currentId ? (state.logs[currentId]?.turns ?? NO_TURNS) : NO_TURNS,
  );
  const forkTurn = latestForkableTurn(currentTurns);

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

  const go = (path: string, surface: "settings" | "library" | "usage" | "issues" | "pulls") =>
    close(() => navigate({ kind: "placeholder", surface, path }));

  // The resting palette mirrors the prototype: three groups, ten rows, which
  // fit the resting list exactly. The long tail appears once the person starts
  // typing.
  const groups: PaletteGroup[] = [];
  if (current) {
    groups.push({
      key: "thread",
      heading: t("palette.groups.thisThread"),
      rows: [
        ...(approvalId
          ? [
              paletteRow({
                key: "approve",
                row: named("approval.approve"),
                synonyms: "approve pending action go",
                icon: <ShieldCheckIcon aria-hidden />,
                command: "approval.approve",
                onSelect: () =>
                  close(() => void respondApproval(current.thread_id, approvalId, "approve")),
              }),
              paletteRow({
                key: "deny",
                row: named("approval.deny"),
                synonyms: "deny pending action refuse",
                icon: <XIcon aria-hidden />,
                command: "approval.deny",
                onSelect: () =>
                  close(() => void respondApproval(current.thread_id, approvalId, "deny")),
              }),
            ]
          : []),
        paletteRow({
          key: "changes",
          row: named("panel.changes"),
          synonyms: "open changes diff files",
          icon: <FileDiffIcon aria-hidden />,
          command: "panel.changes",
          onSelect: () => close(() => setPanelTab("changes")),
        }),
        paletteRow({
          key: "subagents",
          row: named("panel.subagents"),
          synonyms: "open subagents agents tree",
          icon: <WorkflowIcon aria-hidden />,
          command: "panel.subagents",
          onSelect: () => close(() => setPanelTab("subagents")),
        }),
        paletteRow({
          key: "switchHarness",
          row: item("switchHarness"),
          synonyms: "switch harness provider model",
          icon: <ArrowLeftRightIcon aria-hidden />,
          onSelect: () => close(() => window.dispatchEvent(new CustomEvent("h4rn:switch-harness"))),
        }),
        paletteRow({
          key: "linkPullRequest",
          row: item("linkPullRequest"),
          synonyms: "link pull request pr url",
          icon: <GitPullRequestIcon aria-hidden />,
          onSelect: () => openLinkPullRequest(),
        }),
        paletteRow({
          key: "fork",
          row: item("fork"),
          synonyms: "fork branch copy latest turn",
          icon: <GitForkIcon aria-hidden />,
          disabled: !forkTurn,
          onSelect: () => {
            if (forkTurn) close(() => void forkThread(current.thread_id, forkTurn));
          },
        }),
        ...(current.tab !== "settled"
          ? [
              paletteRow({
                key: "share",
                row: named("guest.share"),
                synonyms: "share guest teammate link",
                opensDialog: true,
                icon: <Share2Icon aria-hidden />,
                onSelect: () => close(() => openGuestShare(current.thread_id)),
              }),
            ]
          : []),
        paletteRow({
          key: "pin",
          row: current.pinned ? item("unpin") : named("thread.pin"),
          synonyms: "pin keep working unpin",
          icon: <PinIcon aria-hidden />,
          command: "thread.pin",
          onSelect: () => close(() => void togglePin(current.thread_id)),
        }),
        ...(current.tab === "done"
          ? [
              paletteRow({
                key: "acknowledge",
                row: named("thread.acknowledge"),
                synonyms: "acknowledge done",
                icon: <CheckIcon aria-hidden />,
                command: "thread.acknowledge",
                onSelect: () => close(() => void acknowledgeThread(current.thread_id)),
              }),
            ]
          : []),
        ...(current.tab !== "settled"
          ? [
              paletteRow({
                key: "settle",
                row: named("thread.settle"),
                synonyms: "settle now archive",
                icon: <ArchiveIcon aria-hidden />,
                command: "thread.settle",
                onSelect: () => close(() => setThreadTab(current.thread_id, "settled")),
              }),
            ]
          : []),
        ...(current.tab === "working"
          ? [
              paletteRow({
                key: "interrupt",
                row: named("turn.interrupt"),
                synonyms: "interrupt turn stop",
                icon: <SquareIcon aria-hidden />,
                command: "turn.interrupt",
                onSelect: () => close(() => void interruptThread(current.thread_id)),
              }),
            ]
          : []),
      ],
    });
  }
  groups.push({
    key: "threads",
    heading: t("palette.groups.threads"),
    rows: [
      ...recent.map(
        (thread): PaletteRow => ({
          key: `thread:${thread.thread_id}`,
          value: `${thread.title} ${thread.host} ${harnessLabel(thread.harness)}`,
          label: thread.title,
          icon: (
            <AgentGlyph
              glyph={glyphForThread(thread, hosts[thread.host]?.online ?? false)}
              size={14}
            />
          ),
          trailing: `${hosts[thread.host]?.name ?? thread.host} · ${harnessLabel(thread.harness)}`,
          onSelect: () => close(() => openThread(thread.thread_id)),
        }),
      ),
      paletteRow({
        key: "new",
        row: named("thread.new"),
        synonyms: "new thread start",
        opensDialog: true,
        icon: <PlusIcon aria-hidden />,
        command: "thread.new",
        onSelect: () => close(openNewThread),
      }),
      ...(searching
        ? [
            paletteRow({
              key: "search",
              row: item("searchThreads"),
              synonyms: "search threads find",
              icon: <SearchIcon aria-hidden />,
              onSelect: () =>
                close(() => {
                  navigate({ kind: "inbox" });
                  requestSearchFocus();
                }),
            }),
          ]
        : []),
    ],
  });
  groups.push({
    key: "goTo",
    heading: t("palette.groups.goTo"),
    rows: [
      paletteRow({
        key: "working",
        row: named("tab.working"),
        synonyms: "go to working tab",
        icon: <InboxIcon aria-hidden />,
        command: "tab.working",
        onSelect: () => close(() => setTab("working")),
      }),
      paletteRow({
        key: "done",
        row: named("tab.done"),
        synonyms: "go to done tab",
        icon: <CheckIcon aria-hidden />,
        command: "tab.done",
        onSelect: () => close(() => setTab("done")),
      }),
      paletteRow({
        key: "settled",
        row: named("tab.settled"),
        synonyms: "go to settled tab",
        icon: <ArchiveIcon aria-hidden />,
        command: "tab.settled",
        onSelect: () => close(() => setTab("settled")),
      }),
      paletteRow({
        key: "pair",
        row: item("pairDevice"),
        synonyms: "pair a device phone qr code",
        icon: <SmartphoneIcon aria-hidden />,
        onSelect: () => go("/settings/devices", "settings"),
      }),
      ...(searching
        ? [
            paletteRow({
              key: "hosts",
              row: item("hosts"),
              synonyms: "hosts machines",
              icon: <ServerIcon aria-hidden />,
              onSelect: () => close(() => navigate({ kind: "hosts" })),
            }),
            paletteRow({
              key: "library",
              row: named("nav.tools"),
              synonyms: "library mcp servers skills",
              icon: <LibraryIcon aria-hidden />,
              command: "nav.tools",
              onSelect: () => go("/library", "library"),
            }),
            paletteRow({
              key: "usage",
              row: named("nav.usage"),
              synonyms: "usage limits",
              icon: <ChartNoAxesColumnIcon aria-hidden />,
              command: "nav.usage",
              onSelect: () => go("/usage", "usage"),
            }),
            paletteRow({
              key: "ide",
              row: item("ide"),
              synonyms: "ide editor",
              icon: <Code2Icon aria-hidden />,
              onSelect: () => close(() => navigate({ kind: "ide" })),
            }),
            paletteRow({
              key: "issues",
              row: named("nav.issues"),
              synonyms: "open issues trackers",
              icon: <CircleDotIcon aria-hidden />,
              command: "nav.issues",
              onSelect: () => go("/issues", "issues"),
            }),
            paletteRow({
              key: "pulls",
              row: named("nav.pulls"),
              synonyms: "pull requests prs",
              icon: <GitPullRequestIcon aria-hidden />,
              command: "nav.pulls",
              onSelect: () => go("/pulls", "pulls"),
            }),
            paletteRow({
              key: "settings",
              row: named("settings.open"),
              synonyms: "settings preferences",
              icon: <SettingsIcon aria-hidden />,
              command: "settings.open",
              onSelect: () => go("/settings", "settings"),
            }),
          ]
        : []),
    ],
  });
  groups.push({
    key: "view",
    heading: t("palette.groups.view"),
    rows: [
      ...(current
        ? [
            paletteRow({
              key: "openInIde",
              row: {
                name: t("palette.items.openInIde", {
                  project: projectName(current.workspace_path),
                }),
                english: english("palette.items.openInIde", {
                  project: projectName(current.workspace_path),
                }),
              },
              synonyms: `open ide editor project ${commandName("view.toggle", "en")}`,
              icon: <Code2Icon aria-hidden />,
              command: "view.toggle",
              onSelect: () => close(toggleView),
            }),
          ]
        : []),
      paletteRow({
        key: "panel",
        row: panelOpen ? item("hideRightPanel") : item("showRightPanel"),
        synonyms: `toggle right panel ${commandName("panel.toggle", "en")}`,
        icon: <PanelRightIcon aria-hidden />,
        command: "panel.toggle",
        onSelect: () => close(togglePanel),
      }),
      paletteRow({
        key: "terminal",
        row: terminalOpen ? item("hideTerminal") : item("showTerminal"),
        synonyms: "toggle terminal shell",
        icon: <TerminalIcon aria-hidden />,
        command: "terminal.toggle",
        onSelect: () => close(toggleTerminal),
      }),
      paletteRow({
        key: "sidebar",
        row: sidebarOpen ? item("hideSidebar") : item("showSidebar"),
        synonyms: "toggle sidebar",
        icon: <PanelLeftIcon aria-hidden />,
        command: "sidebar.toggle",
        onSelect: () => close(toggleSidebar),
      }),
      ...(searching
        ? [
            paletteRow({
              key: "theme",
              row: named("theme.toggle"),
              synonyms: "toggle theme dark light",
              icon: <SunMoonIcon aria-hidden />,
              command: "theme.toggle",
              onSelect: () => close(toggleTheme),
            }),
            paletteRow({
              key: "shortcuts",
              row: named("shortcuts.open"),
              synonyms: "keyboard shortcuts help",
              opensDialog: true,
              icon: <KeyboardIcon aria-hidden />,
              command: "shortcuts.open",
              onSelect: () => close(openShortcuts),
            }),
            paletteRow({
              key: "copyLink",
              row: item("copyLink"),
              synonyms: "copy thread link",
              icon: <LinkIcon aria-hidden />,
              disabled: !current,
              onSelect: () =>
                close(() => {
                  if (!current) return;
                  void navigator.clipboard?.writeText(
                    `${window.location.origin}/t/${current.thread_id}`,
                  );
                }),
            }),
            ...(connection !== "online"
              ? [
                  paletteRow({
                    key: "reconnect",
                    row: item("reconnect"),
                    synonyms: "reconnect connection",
                    icon: <RefreshCwIcon aria-hidden />,
                    onSelect: () => close(() => getClient().reconnectNow()),
                  }),
                ]
              : []),
          ]
        : []),
    ],
  });

  return (
    <CommandDialog
      open={open}
      onOpenChange={(value) => (value ? undefined : closePalette())}
      title={t("palette.title")}
      description={t("palette.description")}
    >
      <Command filter={false} autoHighlight loop query={query} onQueryChange={setQuery}>
        <CommandInput
          aria-label={t("palette.title")}
          placeholder={t("palette.placeholder")}
          trailing={
            current ? (
              <span className="inline-flex h-5.5 max-w-60 shrink-0 items-center gap-1.5 rounded-md bg-fill px-2 text-xs text-fg-2">
                {currentGlyph ? <AgentGlyph glyph={currentGlyph} size={12} /> : null}
                <span className="truncate">{current.title}</span>
              </span>
            ) : null
          }
        />
        <CommandEmpty>{t("palette.empty")}</CommandEmpty>
        <CommandList>
          {rankGroups(groups, query).map((group) => (
            <CommandGroup key={group.key} heading={group.heading}>
              {group.rows.map((row) => (
                <CommandItem
                  key={row.key}
                  value={row.key}
                  icon={row.icon}
                  trailing={row.trailing}
                  command={row.command}
                  disabled={row.disabled}
                  onSelect={row.onSelect}
                >
                  {row.label}
                </CommandItem>
              ))}
            </CommandGroup>
          ))}
        </CommandList>
      </Command>
      <CommandFooter>
        <span className="inline-flex items-center gap-1.5">
          <KbdGroup>
            <Kbd>↑</Kbd>
            <Kbd>↓</Kbd>
          </KbdGroup>
          {t("palette.footer.move")}
        </span>
        <span className="inline-flex items-center gap-1.5">
          <Kbd>↵</Kbd> {t("palette.footer.run")}
        </span>
        <span className="inline-flex items-center gap-1.5">
          <Kbd>{ESC}</Kbd> {t("palette.footer.close")}
        </span>
        <span className="flex-1" />
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
