import "@xterm/xterm/css/xterm.css";
import "./terminal.css";

import { ChevronLeftIcon, PlusIcon, SearchIcon, TerminalIcon, XIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Tabs, TabsList, TabsTab } from "@/components/ui/tabs";
import { useIsPhone } from "@/hooks/use-media-query";
import { machineSeed } from "@/lib/entities";
import { getClient, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

import { TerminalView } from "./TerminalView";
import type { TerminalController } from "./terminal-controller";

type Tab = { key: string; title: string };

function shortTitle(title: string): string {
  if (!title.startsWith("/")) return title;
  const parts = title.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? title;
}

function shortPath(path: string): string {
  return path.replace(/^\/(?:home|Users)\/[^/]+/, "~");
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
  const { t } = useTranslation("thread");
  const client = getClient();
  const isPhone = useIsPhone();
  const online = useApp((state) => state.connection.status === "online");
  const hostName = useApp((state) => state.hosts[host]?.name ?? host);
  const [tabs, setTabs] = useState<Tab[]>(() => [{ key: "tab-1", title: "zsh" }]);
  const [activeKey, setActiveKey] = useState<string>("tab-1");
  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [dimensions, setDimensions] = useState({ cols: 0, rows: 0 });
  const nextKey = useRef(1);
  const controllers = useRef(new Map<string, TerminalController>());
  const searchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (searchOpen && !hidden) searchRef.current?.focus();
  }, [searchOpen, hidden]);

  const addTab = () => {
    nextKey.current += 1;
    const key = `tab-${nextKey.current}`;
    setTabs((current) => [...current, { key, title: "zsh" }]);
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
    <section aria-label={t("terminal.panel")} className={cn("q-term", hidden && "hidden")}>
      {isPhone ? (
        <>
          <div className="q-ph-nav">
            <button
              type="button"
              className="navbtn"
              aria-label={t("terminal.back")}
              onClick={onClose}
            >
              <ChevronLeftIcon className="q-icon s22 -ms-1" strokeWidth={1.5} aria-hidden />
              {t("terminal.thread")}
            </button>
            <span className="ttl">{t("terminal.title")}</span>
            <button
              type="button"
              className="navbtn"
              aria-label={t("terminal.new")}
              onClick={addTab}
            >
              <PlusIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
            </button>
          </div>
          <div className="q-ph-term-meta">
            <EntityAvatar seed={machineSeed(host)} kind="machine" label={hostName} size={14} />
            <span>{hostName}</span>
            <span className="q-sep">·</span>
            <span className="q-mono">{shortPath(cwd)}</span>
            <span className="grow" />
            <span className="q-num">
              {dimensions.cols || "—"} × {dimensions.rows || "—"}
            </span>
          </div>
        </>
      ) : (
        <div className="q-term-head">
          <Tabs
            className="min-w-0 flex-1 flex-row"
            value={activeKey}
            onValueChange={(value) => {
              if (value) setActiveKey(value);
            }}
          >
            <TabsList variant="bare" aria-label={t("terminal.tabs")} className="q-ttabs">
              {tabs.map((tab, index) => {
                return (
                  <div key={tab.key} className="q-ttab-wrap">
                    <TabsTab value={tab.key} className="q-ttab" title={tab.title}>
                      {index === 0 ? (
                        <TerminalIcon
                          className="q-icon s14 q-faint"
                          strokeWidth={1.5}
                          aria-hidden
                        />
                      ) : null}
                      {tab.title}
                      <span className="q-faint">{hostName}</span>
                    </TabsTab>
                    <button
                      type="button"
                      aria-label={t("terminal.close", { name: tab.title })}
                      className="q-ttab-x"
                      onClick={() => closeTab(tab.key)}
                    >
                      <XIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                    </button>
                  </div>
                );
              })}
              <button
                type="button"
                aria-label={t("terminal.new")}
                className="q-ibtn"
                onClick={addTab}
              >
                <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </button>
            </TabsList>
          </Tabs>

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
              placeholder={t("terminal.find")}
              aria-label={t("terminal.find")}
              className="q-input"
              style={{ width: 144, height: 24, flex: "none" }}
            />
          ) : null}

          <span className="q-term-meta q-faint q-mono">
            {shortPath(cwd)} ·{" "}
            <span className="q-num">
              {dimensions.cols || "—"} × {dimensions.rows || "—"}
            </span>
          </span>
          <span
            className={cn("q-health", online ? "q-ok" : "q-danger")}
            title={online ? t("terminal.connected") : t("terminal.disconnected")}
            style={{ flex: "none", marginRight: 4, fontSize: 12 }}
          >
            <span className={cn("q-dot", online ? "on" : "off")} aria-hidden />
            {online ? t("terminal.live") : t("terminal.offline")}
          </span>

          <button
            type="button"
            aria-label={t("terminal.find")}
            aria-pressed={searchOpen}
            className={cn("q-ibtn", searchOpen && "on")}
            onClick={() => setSearchOpen((value) => !value)}
          >
            <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </button>
          <button
            type="button"
            aria-label={t("terminal.closePanel")}
            className="q-ibtn"
            onClick={onClose}
          >
            <XIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
      )}

      {tabs.length === 0 ? (
        <div
          style={{
            display: "flex",
            flex: 1,
            minHeight: 0,
            flexDirection: "column",
            alignItems: "center",
            justifyContent: "center",
            gap: 8,
            textAlign: "center",
            padding: "0 24px",
          }}
        >
          <TerminalIcon className="q-icon s20 q-faint" strokeWidth={1.5} aria-hidden />
          <p style={{ margin: 0, fontWeight: 500 }}>{t("terminal.noneOpen")}</p>
          <p className="q-faint" style={{ margin: 0, maxWidth: "48ch", fontSize: 12 }}>
            <Trans
              t={t}
              i18nKey="terminal.startIn"
              values={{ cwd, host }}
              components={{ path: <span className="q-mono" /> }}
            />
          </p>
          <button type="button" className="q-btn secondary" onClick={addTab}>
            <PlusIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
            {t("terminal.new")}
          </button>
        </div>
      ) : (
        <div
          style={{
            position: "relative",
            display: "flex",
            flex: 1,
            minHeight: 0,
            flexDirection: "column",
          }}
        >
          {tabs.map((tab) => {
            const active = tab.key === activeKey;
            return (
              <div
                key={tab.key}
                style={{
                  minHeight: 0,
                  flex: 1,
                  display: active ? "flex" : "none",
                  flexDirection: "column",
                }}
              >
                <TerminalView
                  client={client}
                  threadId={threadId}
                  host={host}
                  cwd={cwd}
                  active={active && !hidden}
                  onDimensions={(next) => {
                    if (active) setDimensions(next);
                  }}
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
