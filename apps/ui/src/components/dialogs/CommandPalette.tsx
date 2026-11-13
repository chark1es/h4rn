import {
  ChartNoAxesColumnIcon,
  CheckIcon,
  Code2Icon,
  GitPullRequestIcon,
  InboxIcon,
  KeyboardIcon,
  LibraryIcon,
  PinIcon,
  PlusIcon,
  RefreshCwIcon,
  SearchIcon,
  ServerIcon,
  SettingsIcon,
  ShieldCheckIcon,
  SquareIcon,
  SunMoonIcon,
} from "lucide-react";
import { useMemo } from "react";
import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ALT, ENTER, KeyGlyphs, MOD, SHIFT } from "@/components/common/Kbd";
import {
  CommandDialog,
  CommandEmpty,
  CommandFooter,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
  CommandShortcut,
} from "@/components/ui/command";
import { glyphForThread } from "@/lib/agent";
import { harnessLabel, machineSeed } from "@/lib/entities";
import {
  acknowledgeThread,
  closePalette,
  getClient,
  interruptThread,
  navigate,
  openNewThread,
  openShortcuts,
  openThread,
  requestSearchFocus,
  setTab,
  setThreadTab,
  togglePin,
  toggleTheme,
  useApp,
} from "@/lib/store/appStore";

export function CommandPalette() {
  const open = useApp((state) => state.ui.paletteOpen);
  const route = useApp((state) => state.ui.route);
  const threads = useApp((state) => state.threads);
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection.status);

  const currentId = route.kind === "thread" ? route.threadId : null;
  const current = currentId ? threads[currentId] : undefined;

  const recent = useMemo(
    () =>
      Object.values(threads)
        .filter((thread) => thread.thread_id !== currentId)
        .sort((a, b) => Date.parse(b.last_activity) - Date.parse(a.last_activity))
        .slice(0, 3),
    [threads, currentId],
  );

  const close = (action: () => void) => {
    closePalette();
    action();
  };

  const currentGlyph = current
    ? glyphForThread(current, hosts[current.host]?.online ?? false)
    : null;

  return (
    <CommandDialog
      open={open}
      onOpenChange={(value) => (value ? undefined : closePalette())}
      title="Command palette"
      description="Search threads, run a command, or move around h4rn."
    >
      <CommandInput
        placeholder="Search threads, hosts, files, or commands"
        autoFocus
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
          <CommandGroup heading={`This thread · ${current.title}`}>
            {current.needs_attention || current.agent_state === "awaiting_approval" ? (
              <CommandItem
                value="approve pending action go"
                onSelect={() =>
                  close(() =>
                    document
                      .querySelector<HTMLButtonElement>('[data-approval-action="approve"]')
                      ?.click(),
                  )
                }
              >
                <ShieldCheckIcon aria-hidden />
                Approve pending action
                <CommandShortcut>
                  {MOD} {ENTER}
                </CommandShortcut>
              </CommandItem>
            ) : null}
            <CommandItem
              value="pin keep working"
              onSelect={() => close(() => void togglePin(current.thread_id))}
            >
              <PinIcon aria-hidden />
              {current.pinned ? "Unpin from Working" : "Keep in Working"}
              <CommandShortcut>P</CommandShortcut>
            </CommandItem>
            <CommandItem
              value="acknowledge done"
              onSelect={() => close(() => void acknowledgeThread(current.thread_id))}
            >
              <CheckIcon aria-hidden />
              Acknowledge
              <CommandShortcut>A</CommandShortcut>
            </CommandItem>
            <CommandItem
              value="settle now archive"
              onSelect={() => close(() => void setThreadTab(current.thread_id, "settled"))}
            >
              <InboxIcon aria-hidden />
              Settle now
              <CommandShortcut>{SHIFT} S</CommandShortcut>
            </CommandItem>
            {current.tab === "working" ? (
              <CommandItem
                value="interrupt turn stop"
                onSelect={() => close(() => void interruptThread(current.thread_id))}
              >
                <SquareIcon aria-hidden />
                Interrupt turn
                <CommandShortcut>{MOD} .</CommandShortcut>
              </CommandItem>
            ) : null}
          </CommandGroup>
        ) : null}

        <CommandSeparator />
        <CommandGroup heading="Threads">
          {recent.map((thread) => (
            <CommandItem
              key={thread.thread_id}
              value={`${thread.title} ${thread.host} ${harnessLabel(thread.harness)}`}
              onSelect={() => close(() => openThread(thread.thread_id))}
            >
              <EntityAvatar
                seed={machineSeed(thread.host)}
                kind="machine"
                label={hosts[thread.host]?.name ?? thread.host}
                size={14}
              />
              <span className="min-w-0 truncate">{thread.title}</span>
              <span className="ml-auto shrink-0 text-small text-fg-3">
                {hosts[thread.host]?.name ?? thread.host} · {harnessLabel(thread.harness)}
              </span>
            </CommandItem>
          ))}
          <CommandItem value="new thread start" onSelect={() => close(openNewThread)}>
            <PlusIcon aria-hidden />
            New thread…
            <CommandShortcut>
              {MOD} {SHIFT} N
            </CommandShortcut>
          </CommandItem>
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
            <CommandShortcut>/</CommandShortcut>
          </CommandItem>
          <CommandItem value="working tab" onSelect={() => close(() => setTab("working"))}>
            Go to Working
            <CommandShortcut>{ALT} 1</CommandShortcut>
          </CommandItem>
          <CommandItem value="done tab" onSelect={() => close(() => setTab("done"))}>
            Go to Done
            <CommandShortcut>{ALT} 2</CommandShortcut>
          </CommandItem>
          <CommandItem value="settled tab" onSelect={() => close(() => setTab("settled"))}>
            Go to Settled
            <CommandShortcut>{ALT} 3</CommandShortcut>
          </CommandItem>
        </CommandGroup>

        <CommandSeparator />
        <CommandGroup heading="Go to">
          <CommandItem value="inbox" onSelect={() => close(() => navigate({ kind: "inbox" }))}>
            <InboxIcon aria-hidden />
            Inbox
            <CommandShortcut>G I</CommandShortcut>
          </CommandItem>
          <CommandItem
            value="hosts machines"
            onSelect={() => close(() => navigate({ kind: "hosts" }))}
          >
            <ServerIcon aria-hidden />
            Hosts
            <CommandShortcut>G H</CommandShortcut>
          </CommandItem>
          <CommandItem
            value="library mcp skills"
            onSelect={() =>
              close(() => navigate({ kind: "placeholder", surface: "library", path: "/library" }))
            }
          >
            <LibraryIcon aria-hidden />
            Library
            <CommandShortcut>G L</CommandShortcut>
          </CommandItem>
          <CommandItem
            value="usage limits"
            onSelect={() =>
              close(() => navigate({ kind: "placeholder", surface: "usage", path: "/usage" }))
            }
          >
            <ChartNoAxesColumnIcon aria-hidden />
            Usage &amp; limits
            <CommandShortcut>G U</CommandShortcut>
          </CommandItem>
          <CommandItem value="ide editor" onSelect={() => close(() => navigate({ kind: "ide" }))}>
            <Code2Icon aria-hidden />
            IDE
            <CommandShortcut>G E</CommandShortcut>
          </CommandItem>
          <CommandItem
            value="pull requests prs"
            onSelect={() =>
              close(() => navigate({ kind: "placeholder", surface: "pulls", path: "/pulls" }))
            }
          >
            <GitPullRequestIcon aria-hidden />
            Pull requests
            <CommandShortcut>G P</CommandShortcut>
          </CommandItem>
          <CommandItem
            value="settings"
            onSelect={() =>
              close(() => navigate({ kind: "placeholder", surface: "settings", path: "/settings" }))
            }
          >
            <SettingsIcon aria-hidden />
            Settings
            <CommandShortcut>{MOD} ,</CommandShortcut>
          </CommandItem>
        </CommandGroup>

        <CommandSeparator />
        <CommandGroup heading="View">
          <CommandItem value="toggle theme dark light" onSelect={() => close(toggleTheme)}>
            <SunMoonIcon aria-hidden />
            Toggle theme
            <CommandShortcut>
              {MOD} {SHIFT} L
            </CommandShortcut>
          </CommandItem>
          <CommandItem value="keyboard shortcuts help" onSelect={() => close(openShortcuts)}>
            <KeyboardIcon aria-hidden />
            Keyboard shortcuts
            <CommandShortcut>?</CommandShortcut>
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
