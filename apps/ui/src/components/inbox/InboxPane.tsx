import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronDownIcon,
  ChevronRightIcon,
  CircleAlertIcon,
  ClockIcon,
  SearchIcon,
  SlidersHorizontalIcon,
  SquarePenIcon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { MOD, SHIFT } from "@/components/common/Kbd";
import { OfflineBanner } from "@/components/shell/OfflineBanner";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
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
import { ThreadRow } from "./ThreadRow";

const EMPTY_COPY: Record<ThreadTab, { title: string; description: string }> = {
  working: {
    title: "Working is clear",
    description:
      "Threads with a running turn, a pending approval or a failed step appear here. Start one to give the agent a task.",
  },
  done: {
    title: "No unacknowledged results",
    description:
      "A finished turn waits in Done until you acknowledge it. Reading a thread does not clear it.",
  },
  settled: {
    title: "Settled is empty",
    description: "Acknowledged threads and runs idle for 24 hours settle here, out of the way.",
  },
};

function ThreadRowSkeleton() {
  return (
    <div className="q-row" style={{ pointerEvents: "none" }}>
      <span className="q-row-st">
        <span className="q-skel" style={{ width: 10, height: 10, borderRadius: "50%" }} />
      </span>
      <span className="q-row-main" style={{ gap: 8, paddingTop: 2 }}>
        <span className="q-skel" style={{ height: 11, width: "72%" }} />
        <span className="q-skel" style={{ height: 9, width: "44%" }} />
      </span>
    </div>
  );
}

function FilterPopover({
  hosts,
  onSearch,
  phone = false,
}: {
  hosts: Array<{ id: HostId; name: string }>;
  onSearch: () => void;
  phone?: boolean;
}) {
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
        <button
          type="button"
          className={phone ? "navbtn" : "q-ibtn"}
          aria-label="Filter threads"
          title="Filter threads"
        >
          <SlidersHorizontalIcon
            className={phone ? "q-icon s22" : "q-icon"}
            strokeWidth={1.5}
            aria-hidden
          />
          {active ? (
            <span
              className="q-dot you"
              style={{ position: "absolute", top: 4, right: 4 }}
              aria-hidden
            />
          ) : null}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuLabel>Search</DropdownMenuLabel>
        <DropdownMenuItem onSelect={onSearch}>Search threads…</DropdownMenuItem>
        <DropdownMenuSeparator />
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
    <>
      <button
        type="button"
        className="q-snoozed"
        aria-expanded={open}
        onClick={() => setUi({ snoozedOpen: !open })}
      >
        <ClockIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        <span>Snoozed</span>
        <span className="q-faint q-num">{threads.length}</span>
        {open ? (
          <ChevronDownIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        ) : (
          <ChevronRightIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        )}
      </button>
      {open ? (
        <ul className="q-scroll" style={{ maxHeight: 224, borderTop: "1px solid var(--line)" }}>
          {threads.map((thread) => (
            <li key={thread.thread_id}>
              <button
                type="button"
                className="q-row"
                style={{ borderRadius: 0 }}
                onClick={() => openThread(thread.thread_id)}
              >
                <span className="q-row-st">
                  <EntityAvatar
                    seed={machineSeed(thread.host)}
                    kind="machine"
                    label={hosts[thread.host]?.name ?? thread.host}
                    size={14}
                  />
                </span>
                <span className="q-row-main">
                  <span className="q-row-l1">
                    <span className="q-row-title">{thread.title}</span>
                    <span className="q-row-time q-num">
                      {thread.snoozed_until
                        ? new Date(thread.snoozed_until).toLocaleTimeString([], {
                            hour: "2-digit",
                            minute: "2-digit",
                          })
                        : ""}
                    </span>
                  </span>
                  <span className="q-row-l2">
                    <span className="q-row-meta">
                      wakes in {relativeAge(thread.snoozed_until ?? "", now)} ·{" "}
                      {harnessLabel(thread.harness)}
                    </span>
                  </span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </>
  );
}

function FilterChip({ label, onRemove }: { label: string; onRemove: () => void }) {
  return (
    <span
      className="q-badge you"
      style={{ display: "inline-flex", alignItems: "center", gap: 4, height: 20 }}
    >
      {label}
      <button type="button" aria-label={`Remove ${label} filter`} onClick={onRemove}>
        <XIcon className="q-icon s12" strokeWidth={1.5} />
      </button>
    </span>
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
  const [searchOpen, setSearchOpen] = useState(false);

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
    if (focusNonce <= 0) return;
    setSearchOpen(true);
    window.setTimeout(() => searchRef.current?.focus(), 0);
  }, [focusNonce]);

  // Keyboard selection scrolls the virtual list.
  const virtualizer = useVirtualizer({
    count: visible.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => (isPhone ? 66 : 52),
    overscan: 8,
    getItemKey: (index) => visible[index]?.thread_id ?? index,
  });

  useEffect(() => {
    const index = visible.findIndex((thread) => thread.thread_id === selectedId);
    if (index >= 0) virtualizer.scrollToIndex(index, { align: "auto" });
  }, [selectedId, visible, virtualizer]);

  const hasFilter =
    filter.host !== null || filter.harness !== null || filter.needsAttention || filter.unread;
  const showSearch = searchOpen || query.length > 0;

  const segment = (
    <div className="q-seg" role="tablist" aria-label="Thread state">
      {TAB_ORDER.map((value) => (
        <button
          key={value}
          type="button"
          role="tab"
          aria-selected={tab === value}
          onClick={() => setTab(value)}
        >
          {TAB_LABELS[value]} <span className="c q-num">{loading ? "—" : counts[value]}</span>
        </button>
      ))}
    </div>
  );

  return (
    <section className="q-pane" aria-label="Inbox">
      {isPhone ? (
        <>
          <div className="q-ph-nav">
            <FilterPopover hosts={hostList} onSearch={() => setSearchOpen(true)} phone />
            <span className="ttl" />
            <button
              type="button"
              className="navbtn"
              aria-label="New thread"
              onClick={openNewThread}
            >
              <SquarePenIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
            </button>
          </div>
          <div className="q-ph-lt">Inbox</div>
        </>
      ) : (
        <div className="q-pane-head">
          <h2>Inbox</h2>
          <FilterPopover hosts={hostList} onSearch={() => setSearchOpen(true)} />
          <button
            type="button"
            className="q-ibtn"
            aria-label="New thread"
            title={`New thread (${MOD} ${SHIFT} N)`}
            onClick={openNewThread}
          >
            <SquarePenIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
      )}

      {showSearch ? (
        <div className="q-pane-sub" style={{ paddingTop: 4 }}>
          <div style={{ position: "relative" }}>
            <SearchIcon
              className="q-icon s14 q-faint"
              strokeWidth={1.5}
              aria-hidden
              style={{ position: "absolute", left: 9, top: 7 }}
            />
            <input
              ref={searchRef}
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onBlur={() => {
                if (!query) setSearchOpen(false);
              }}
              placeholder="Search threads"
              aria-label="Search threads"
              className="q-input"
              style={{ paddingLeft: 28, paddingRight: 28 }}
            />
            <button
              type="button"
              aria-label="Close search"
              style={{ position: "absolute", right: 6, top: 6, color: "var(--fg-3)" }}
              onClick={() => {
                setQuery("");
                setSearchOpen(false);
              }}
            >
              <XIcon className="q-icon s14" strokeWidth={1.5} />
            </button>
          </div>
        </div>
      ) : null}

      {isPhone ? (
        <div className="q-ph-seg">{segment}</div>
      ) : (
        <div className="q-pane-sub">{segment}</div>
      )}

      {hasFilter ? (
        <div
          className="q-pane-sub"
          style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap", paddingTop: 0 }}
        >
          <span className="q-faint" style={{ fontSize: 12 }}>
            Filtered:
          </span>
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

      <OfflineBanner inline />

      {error ? (
        <div className="q-banner err" style={{ margin: "0 12px 8px" }}>
          <span className="ic">
            <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </span>
          <span className="grow">
            <b>{error.message}</b>
            {error.hint ? ` ${error.hint}` : " Showing cached threads."}
          </span>
          <button
            type="button"
            className="q-btn ghost"
            style={{ height: 24 }}
            onClick={() => void loadThreads()}
          >
            Retry
          </button>
        </div>
      ) : null}

      <div ref={scrollRef} className="q-scroll">
        {loading && Object.keys(threads).length === 0 ? (
          <div className="q-rows" role="status" aria-busy="true" aria-label="Loading threads">
            {["a", "b", "c", "d", "e", "f", "g"].map((key) => (
              <ThreadRowSkeleton key={key} />
            ))}
          </div>
        ) : allEmpty ? (
          Object.keys(hosts).length === 0 ? (
            <div className="q-empty">
              <h3>Pair a host to start your first thread</h3>
              <p>
                h4rn needs a machine with a harness installed. Pairing arrives with the device flow;
                nothing runs until you approve it.
              </p>
              <button type="button" className="q-btn secondary" disabled>
                Pair a device
              </button>
            </div>
          ) : (
            <div className="q-empty">
              <h3>No threads yet</h3>
              <p>
                Threads you start on a paired host show up here. Anything running, queued, or
                waiting on you stays in Working until it finishes.
              </p>
              <button type="button" className="q-btn secondary" onClick={openNewThread}>
                <SquarePenIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                New thread{" "}
                <span className="q-kbd">
                  {MOD} {SHIFT} N
                </span>
              </button>
            </div>
          )
        ) : visible.length === 0 ? (
          <div className="q-empty">
            <h3>{EMPTY_COPY[tab].title}</h3>
            <p>{EMPTY_COPY[tab].description}</p>
            {query || hasFilter ? (
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => {
                  setQuery("");
                  setFilter({ host: null, harness: null, needsAttention: false, unread: false });
                }}
              >
                Clear search and filters
              </button>
            ) : tab === "working" ? (
              <button type="button" className="q-btn secondary" onClick={openNewThread}>
                New thread
              </button>
            ) : null}
          </div>
        ) : (
          <div
            className="q-rows"
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

      {loading || allEmpty ? null : <SnoozedSection threads={snoozed} now={now} />}
    </section>
  );
}
