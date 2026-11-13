import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronDownIcon,
  ChevronRightIcon,
  InboxIcon,
  PlusIcon,
  SearchIcon,
  SlidersHorizontalIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef } from "react";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { EmptyState } from "@/components/common/EmptyState";
import { Kbd, MOD, SHIFT } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import type { HarnessKind, HostId, ThreadSummary, ThreadTab } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, machineSeed } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { snoozedThreads, TAB_LABELS, TAB_ORDER, tabCounts, threadsForTab } from "@/lib/inbox";
import {
  acknowledgeThread,
  loadThreads,
  openNewThread,
  openThread,
  setFilter,
  setQuery,
  setTab,
  setThreadTab,
  setUi,
  snoozeThread,
  togglePin,
  useApp,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { ThreadRow } from "./ThreadRow";

const EMPTY_COPY: Record<ThreadTab, { title: string; description: string }> = {
  working: {
    title: "Working is clear.",
    description:
      "Threads with a running turn, a pending approval or a failed step appear here. Start one to give the agent a task.",
  },
  done: {
    title: "No unacknowledged results.",
    description:
      "A finished turn waits in Done until you acknowledge it. Reading a thread does not clear it.",
  },
  settled: {
    title: "Settled is empty.",
    description: "Acknowledged threads and runs idle for 24 hours settle here, out of the way.",
  },
};

function ThreadRowSkeleton() {
  return (
    <div className="flex h-16 items-center gap-2 border-b border-line/70 px-3 md:h-12">
      <Skeleton className="h-3 w-5" />
      <div className="flex-1 space-y-1.5">
        <Skeleton className="h-3 w-2/3" />
        <Skeleton className="h-2.5 w-1/2" />
      </div>
    </div>
  );
}

function FilterPopover({ hosts }: { hosts: Array<{ id: HostId; name: string }> }) {
  const filter = useApp((state) => state.ui.filter);
  const hostMap = useApp((state) => state.hosts);
  const harnesses = useMemo(() => {
    const set = new Set<HarnessKind>();
    for (const host of Object.values(hostMap)) {
      for (const harness of host.harnesses) set.add(harness);
    }
    return [...set];
  }, [hostMap]);
  const active =
    filter.host !== null || filter.harness !== null || filter.needsAttention || filter.unread;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          size="sm"
          variant={active ? "secondary" : "ghost"}
          aria-label="Filter threads"
          className="h-8 gap-1.5 px-2"
        >
          <SlidersHorizontalIcon className="size-3.5" />
          <span className="hidden sm:inline">Filter</span>
          {active ? <span className="size-1.5 rounded-full bg-cue" aria-hidden /> : null}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuLabel>Placement</DropdownMenuLabel>
        <DropdownMenuCheckboxItem
          checked={filter.needsAttention}
          onCheckedChange={(checked) => setFilter({ needsAttention: Boolean(checked) })}
        >
          Needs me
        </DropdownMenuCheckboxItem>
        <DropdownMenuCheckboxItem
          checked={filter.unread}
          onCheckedChange={(checked) => setFilter({ unread: Boolean(checked) })}
        >
          Unread
        </DropdownMenuCheckboxItem>
        <DropdownMenuSeparator />
        <DropdownMenuLabel>Host</DropdownMenuLabel>
        {hosts.map((host) => (
          <DropdownMenuCheckboxItem
            key={host.id}
            checked={filter.host === host.id}
            onCheckedChange={(checked) => setFilter({ host: checked ? host.id : null })}
          >
            {host.name}
          </DropdownMenuCheckboxItem>
        ))}
        <DropdownMenuSeparator />
        <DropdownMenuLabel>Harness</DropdownMenuLabel>
        {harnesses.map((harness) => (
          <DropdownMenuCheckboxItem
            key={harness}
            checked={filter.harness === harness}
            onCheckedChange={(checked) => setFilter({ harness: checked ? harness : null })}
          >
            {harnessLabel(harness)}
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function SnoozedSection({ threads, now }: { threads: ThreadSummary[]; now: number }) {
  const open = useApp((state) => state.ui.snoozedOpen);
  const hosts = useApp((state) => state.hosts);
  if (threads.length === 0) return null;

  return (
    <div className="border-t border-line bg-panel">
      <button
        type="button"
        className="flex h-9 w-full items-center gap-1.5 px-3 text-left text-micro font-medium text-ink-muted transition-colors duration-[var(--dur-instant)] hover:bg-paper"
        aria-expanded={open}
        onClick={() => setUi({ snoozedOpen: !open })}
      >
        {open ? (
          <ChevronDownIcon className="size-3.5" aria-hidden />
        ) : (
          <ChevronRightIcon className="size-3.5" aria-hidden />
        )}
        Snoozed ({threads.length})
      </button>
      {open ? (
        <ul className="max-h-56 overflow-y-auto border-t border-line/70">
          {threads.map((thread) => (
            <li key={thread.thread_id}>
              <button
                type="button"
                className="flex h-12 w-full items-center gap-2 border-b border-line/60 px-3 text-left transition-colors duration-[var(--dur-instant)] hover:bg-paper"
                onClick={() => openThread(thread.thread_id)}
              >
                <EntityAvatar
                  seed={machineSeed(thread.host)}
                  kind="machine"
                  label={hosts[thread.host]?.name ?? thread.host}
                  size={14}
                />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-compact text-ink">{thread.title}</span>
                  <span className="block truncate text-micro text-ink-muted">
                    wakes in {relativeAge(thread.snoozed_until ?? "", now)} ·{" "}
                    {harnessLabel(thread.harness)}
                  </span>
                </span>
                <span className="font-mono text-micro tabular text-ink-faint">
                  {thread.snoozed_until
                    ? new Date(thread.snoozed_until).toLocaleTimeString([], {
                        hour: "2-digit",
                        minute: "2-digit",
                      })
                    : ""}
                </span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

export function InboxPane() {
  const threads = useApp((state) => state.threads);
  const hosts = useApp((state) => state.hosts);
  const tab = useApp((state) => state.ui.tab);
  const query = useApp((state) => state.ui.query);
  const filter = useApp((state) => state.ui.filter);
  const loading = useApp((state) => state.loadingThreads);
  const error = useApp((state) => state.threadsError);
  const focusNonce = useApp((state) => state.ui.focusSearchNonce);
  const selectedId = useApp((state) => state.ui.selectedThreadId);
  const now = useNow();
  const isPhone = useIsPhone();
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);

  const state = useApp((s) => s);
  const counts = useMemo(() => tabCounts(state, now), [state, now]);
  const visible = useMemo(() => threadsForTab(state, tab, now), [state, tab, now]);
  const snoozed = useMemo(() => snoozedThreads(state, now), [state, now]);
  const hostList = useMemo(
    () =>
      Object.values(hosts)
        .map((host) => ({ id: host.host_id, name: host.name }))
        .sort((a, b) => a.name.localeCompare(b.name)),
    [hosts],
  );

  const allEmpty = Object.keys(threads).length === 0 && !loading;

  useEffect(() => {
    if (focusNonce > 0) searchRef.current?.focus();
  }, [focusNonce]);

  // Keyboard selection scrolls the virtual list.
  const virtualizer = useVirtualizer({
    count: visible.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => (isPhone ? 64 : 48),
    overscan: 8,
    getItemKey: (index) => visible[index]?.thread_id ?? index,
  });

  useEffect(() => {
    const index = visible.findIndex((thread) => thread.thread_id === selectedId);
    if (index >= 0) virtualizer.scrollToIndex(index, { align: "auto" });
  }, [selectedId, visible, virtualizer]);

  const hasFilter =
    filter.host !== null || filter.harness !== null || filter.needsAttention || filter.unread;

  return (
    <section
      aria-label="Inbox"
      className="flex h-full min-h-0 w-full flex-col bg-panel md:w-[304px] md:shrink-0 md:border-r md:border-line"
    >
      <header className="flex h-12 shrink-0 items-center justify-between px-3">
        <h1 className="text-headline">Inbox</h1>
        <div className="flex items-center gap-1">
          <FilterPopover hosts={hostList} />
          <Button
            size="icon"
            variant="ghost"
            aria-label="New thread"
            title={`New thread (${MOD} ${SHIFT} N)`}
            onClick={openNewThread}
          >
            <PlusIcon />
          </Button>
        </div>
      </header>

      <div className="shrink-0 px-3 pb-1">
        <div className="flex items-center gap-1.5">
          <div className="relative flex-1">
            <SearchIcon
              className="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-ink-faint"
              aria-hidden
            />
            <input
              ref={searchRef}
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search threads"
              aria-label="Search threads"
              className="h-8 w-full rounded-sm border border-line bg-raised pr-7 pl-7 text-micro text-ink outline-none placeholder:text-ink-faint focus-visible:border-cue focus-visible:ring-2 focus-visible:ring-cue/30"
            />
            {query ? (
              <button
                type="button"
                aria-label="Clear search"
                className="absolute top-1/2 right-1 flex size-6 -translate-y-1/2 items-center justify-center rounded-sm text-ink-faint hover:text-ink"
                onClick={() => setQuery("")}
              >
                <XIcon className="size-3.5" />
              </button>
            ) : (
              <Kbd className="pointer-events-none absolute top-1/2 right-1.5 -translate-y-1/2 border-line/60 bg-transparent">
                /
              </Kbd>
            )}
          </div>
        </div>
      </div>

      <div className="shrink-0 px-3">
        <div role="tablist" aria-label="Thread state" className="flex gap-0.5 border-b border-line">
          {TAB_ORDER.map((value) => (
            <button
              key={value}
              type="button"
              role="tab"
              aria-selected={tab === value}
              onClick={() => setTab(value)}
              className={cn(
                "relative h-8 flex-1 px-1 text-micro font-medium whitespace-nowrap transition-colors duration-[var(--dur-instant)]",
                tab === value ? "text-ink" : "text-ink-muted hover:text-ink",
              )}
            >
              {TAB_LABELS[value]}{" "}
              <span className="font-mono tabular text-ink-faint">
                {loading ? "—" : counts[value]}
              </span>
              {tab === value ? (
                <span aria-hidden className="absolute inset-x-1 -bottom-px h-0.5 bg-cue" />
              ) : null}
            </button>
          ))}
        </div>
      </div>

      {hasFilter ? (
        <div className="flex shrink-0 items-center gap-1.5 px-3 py-1.5">
          <span className="text-micro text-ink-faint">Filtered:</span>
          {filter.host ? (
            <FilterChip
              label={hosts[filter.host]?.name ?? filter.host}
              onRemove={() => setFilter({ host: null })}
            />
          ) : null}
          {filter.harness ? (
            <FilterChip
              label={harnessLabel(filter.harness)}
              onRemove={() => setFilter({ harness: null })}
            />
          ) : null}
          {filter.needsAttention ? (
            <FilterChip label="Needs me" onRemove={() => setFilter({ needsAttention: false })} />
          ) : null}
          {filter.unread ? (
            <FilterChip label="Unread" onRemove={() => setFilter({ unread: false })} />
          ) : null}
        </div>
      ) : null}

      {error ? (
        <div className="mx-3 mt-2 shrink-0 rounded-sm border border-strike/40 bg-strike-tint px-2.5 py-2 text-micro text-strike-ink">
          <p className="font-medium">Couldn’t load threads from the daemon.</p>
          <p className="mt-0.5 text-strike-ink/80">{error.message} Showing cached threads.</p>
          <button
            type="button"
            className="mt-1 font-medium underline underline-offset-2"
            onClick={() => void loadThreads()}
          >
            Retry
          </button>
        </div>
      ) : null}

      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
        {loading && Object.keys(threads).length === 0 ? (
          <div role="status" aria-busy="true" aria-label="Loading threads">
            {["a", "b", "c", "d", "e", "f"].map((key) => (
              <ThreadRowSkeleton key={key} />
            ))}
          </div>
        ) : allEmpty ? (
          Object.keys(hosts).length === 0 ? (
            <EmptyState
              icon={InboxIcon}
              title="Pair a host to start your first thread."
              description="h4rn needs a machine with a harness installed. Pairing arrives with the device flow; nothing runs until you approve it."
              action={
                <Button variant="secondary" disabled title="Pairing arrives in Phase 2">
                  Pair a device
                </Button>
              }
            />
          ) : (
            <EmptyState
              icon={InboxIcon}
              title="No threads yet."
              description="Start one on a paired host. The turn appears in Working the moment the daemon accepts it."
              action={<Button onClick={openNewThread}>New thread</Button>}
              shortcut={`${MOD} ${SHIFT} N`}
            />
          )
        ) : visible.length === 0 ? (
          <EmptyState
            icon={InboxIcon}
            title={EMPTY_COPY[tab].title}
            description={EMPTY_COPY[tab].description}
            action={
              query || hasFilter ? (
                <Button
                  variant="secondary"
                  onClick={() => {
                    setQuery("");
                    setFilter({ host: null, harness: null, needsAttention: false, unread: false });
                  }}
                >
                  Clear search and filters
                </Button>
              ) : tab === "working" ? (
                <Button onClick={openNewThread}>New thread</Button>
              ) : undefined
            }
            compact
          />
        ) : (
          <div
            role="listbox"
            aria-label={`${TAB_LABELS[tab]} threads`}
            style={{ height: virtualizer.getTotalSize(), position: "relative" }}
          >
            {virtualizer.getVirtualItems().map((item) => {
              const thread = visible[item.index];
              if (!thread) return null;
              return (
                <div
                  key={item.key}
                  ref={virtualizer.measureElement}
                  data-index={item.index}
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    transform: `translateY(${item.start}px)`,
                  }}
                >
                  <ThreadRow
                    thread={thread}
                    host={hosts[thread.host]}
                    index={item.index}
                    selected={selectedId === thread.thread_id}
                    now={now}
                    onOpen={() => openThread(thread.thread_id)}
                    onAcknowledge={() => void acknowledgeThread(thread.thread_id)}
                    onSettle={() => void setThreadTab(thread.thread_id, "settled")}
                    onReopen={() => void setThreadTab(thread.thread_id, "done")}
                    onPin={() => void togglePin(thread.thread_id)}
                    onSnooze={(until) => void snoozeThread(thread.thread_id, until)}
                  />
                </div>
              );
            })}
          </div>
        )}
      </div>

      <SnoozedSection threads={snoozed} now={now} />
    </section>
  );
}

function FilterChip({ label, onRemove }: { label: string; onRemove: () => void }) {
  return (
    <span className="inline-flex h-5 items-center gap-1 rounded-sm bg-cue-tint px-1.5 text-micro text-cue-ink">
      {label}
      <button type="button" aria-label={`Remove ${label} filter`} onClick={onRemove}>
        <XIcon className="size-3" />
      </button>
    </span>
  );
}
