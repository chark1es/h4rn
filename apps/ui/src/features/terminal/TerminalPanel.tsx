import "@xterm/xterm/css/xterm.css";
import "./terminal.css";

import { PlusIcon, SearchIcon, SquareTerminalIcon, XIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { getClient, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { TerminalView } from "./TerminalView";
import type { TerminalController } from "./terminal-controller";

type Tab = { key: string; title: string };

function labelFor(cwd: string): string {
  const parts = cwd.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? "terminal";
}

function shortTitle(title: string): string {
  if (!title.startsWith("/")) return title;
  const parts = title.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? title;
}

export function TerminalPanel({
  threadId,
  host,
  cwd,
  hidden = false,
  onClose,
}: {
  threadId: string;
  host: string;
  cwd: string;
  /** Keeps the mounted sessions alive while the panel is tucked away. */
  hidden?: boolean;
  onClose: () => void;
}) {
  const client = getClient();
  const online = useApp((state) => state.connection.status === "online");
  const [tabs, setTabs] = useState<Tab[]>(() => [{ key: "tab-1", title: labelFor(cwd) }]);
  const [activeKey, setActiveKey] = useState<string>("tab-1");
  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const nextKey = useRef(1);
  const controllers = useRef(new Map<string, TerminalController>());
  const searchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (searchOpen && !hidden) searchRef.current?.focus();
  }, [searchOpen, hidden]);

  const addTab = () => {
    nextKey.current += 1;
    const key = `tab-${nextKey.current}`;
    setTabs((current) => [...current, { key, title: labelFor(cwd) }]);
    setActiveKey(key);
  };

  const closeTab = (key: string) => {
    setTabs((current) => {
      const index = current.findIndex((tab) => tab.key === key);
      const next = current.filter((tab) => tab.key !== key);
      if (activeKey === key) {
        const fallback = next[Math.max(0, index - 1)] ?? next[0];
        setActiveKey(fallback?.key ?? "");
      }
      return next;
    });
    controllers.current.delete(key);
  };

  const activeController = () => controllers.current.get(activeKey);

  const runSearch = (backwards: boolean) => {
    const controller = activeController();
    if (!controller || !query) return;
    if (backwards) controller.findPrevious(query);
    else controller.findNext(query);
  };

  return (
    <section
      aria-label="Terminal panel"
      className={cn(
        "flex h-[44dvh] max-h-[460px] min-h-[200px] w-full min-w-0 shrink-0 flex-col overflow-hidden border-t border-line bg-stage text-stage-ink sm:h-[38dvh]",
        hidden && "hidden",
      )}
    >
      <div className="flex h-9 shrink-0 items-center gap-1.5 border-b border-white/10 px-2">
        <SquareTerminalIcon className="size-3.5 shrink-0 text-stage-ink-muted" aria-hidden />
        <span className="shrink-0 text-micro font-medium tracking-wide text-stage-ink-muted uppercase">
          Terminal
        </span>

        <div className="ml-1 flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto">
          {tabs.map((tab) => {
            const active = tab.key === activeKey;
            return (
              <div
                key={tab.key}
                className={cn(
                  "flex h-6 shrink-0 items-center rounded-sm border border-transparent",
                  active
                    ? "border-white/15 bg-white/10 text-stage-ink"
                    : "text-stage-ink-muted hover:bg-white/5",
                )}
              >
                <button
                  type="button"
                  role="tab"
                  aria-selected={active}
                  className="max-w-[150px] truncate px-2 text-micro"
                  onClick={() => setActiveKey(tab.key)}
                  title={tab.title}
                >
                  {tab.title}
                </button>
                <button
                  type="button"
                  aria-label={`Close ${tab.title}`}
                  className="mr-1 inline-flex size-4 items-center justify-center rounded-sm text-stage-ink-muted transition-colors hover:bg-white/10 hover:text-stage-ink"
                  onClick={() => closeTab(tab.key)}
                >
                  <XIcon className="size-3" aria-hidden />
                </button>
              </div>
            );
          })}
        </div>

        {searchOpen ? (
          <input
            ref={searchRef}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                runSearch(event.shiftKey);
              } else if (event.key === "Escape") {
                setSearchOpen(false);
              }
            }}
            placeholder="Find in terminal"
            aria-label="Find in terminal"
            className="h-6 w-36 shrink-0 rounded-sm border border-white/15 bg-black/30 px-2 text-micro text-stage-ink placeholder:text-stage-ink-muted focus:border-cue/60 focus:outline-none"
          />
        ) : null}

        <span
          className={cn(
            "hidden shrink-0 items-center gap-1.5 px-1 text-micro sm:flex",
            online ? "text-standby" : "text-strike",
          )}
          title={online ? "Daemon connected" : "Daemon offline"}
        >
          <span
            className={cn("size-1.5 rounded-full", online ? "bg-standby" : "bg-strike")}
            aria-hidden
          />
          {online ? "live" : "offline"}
        </span>

        <button
          type="button"
          aria-label="Find in terminal"
          aria-pressed={searchOpen}
          className={cn(
            "inline-flex size-6 shrink-0 items-center justify-center rounded-sm text-stage-ink-muted transition-colors hover:bg-white/10 hover:text-stage-ink",
            searchOpen && "bg-white/10 text-stage-ink",
          )}
          onClick={() => setSearchOpen((value) => !value)}
        >
          <SearchIcon className="size-3.5" aria-hidden />
        </button>
        <button
          type="button"
          aria-label="New terminal"
          className="inline-flex size-6 shrink-0 items-center justify-center rounded-sm text-stage-ink-muted transition-colors hover:bg-white/10 hover:text-stage-ink"
          onClick={addTab}
        >
          <PlusIcon className="size-3.5" aria-hidden />
        </button>
        <button
          type="button"
          aria-label="Close terminal panel"
          className="inline-flex size-6 shrink-0 items-center justify-center rounded-sm text-stage-ink-muted transition-colors hover:bg-white/10 hover:text-stage-ink"
          onClick={onClose}
        >
          <XIcon className="size-3.5" aria-hidden />
        </button>
      </div>

      {tabs.length === 0 ? (
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 bg-stage px-6 text-center">
          <SquareTerminalIcon className="size-5 text-stage-ink-muted" aria-hidden />
          <p className="text-compact font-medium text-stage-ink">No terminal is open.</p>
          <p className="max-w-[48ch] text-micro text-stage-ink-muted">
            Start a shell in <span className="font-mono">{cwd}</span>. It runs on {host} and keeps
            running when this panel closes.
          </p>
          <button
            type="button"
            className="mt-1 inline-flex h-7 items-center gap-1.5 rounded-sm border border-white/20 px-2.5 text-micro font-medium text-stage-ink transition-colors hover:bg-white/10"
            onClick={addTab}
          >
            <PlusIcon className="size-3" aria-hidden />
            New terminal
          </button>
        </div>
      ) : (
        <div className="relative flex min-h-0 flex-1 flex-col">
          {tabs.map((tab) => {
            const active = tab.key === activeKey;
            return (
              <div
                key={tab.key}
                className={cn("min-h-0 flex-1 flex-col", active ? "flex" : "hidden")}
              >
                <TerminalView
                  client={client}
                  threadId={threadId}
                  host={host}
                  cwd={cwd}
                  active={active && !hidden}
                  onTitle={(title) =>
                    setTabs((current) =>
                      current.map((entry) =>
                        entry.key === tab.key ? { ...entry, title: shortTitle(title) } : entry,
                      ),
                    )
                  }
                  onController={(controller) => {
                    if (controller) controllers.current.set(tab.key, controller);
                    else controllers.current.delete(tab.key);
                  }}
                />
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}
