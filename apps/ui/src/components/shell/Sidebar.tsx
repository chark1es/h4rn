import type { LucideIcon } from "lucide-react";
import {
  BlocksIcon,
  ChartNoAxesColumnIcon,
  Code2Icon,
  GitPullRequestIcon,
  InboxIcon,
  SearchIcon,
  SettingsIcon,
  SquarePenIcon,
} from "lucide-react";
import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, machineSeed } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { isSnoozed } from "@/lib/inbox";
import type { Route } from "@/lib/router";
import { navigate, openNewThread, setUi, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const NAV_ITEMS: Array<{
  key: string;
  label: string;
  icon: LucideIcon;
  route: Route;
  count?: string;
}> = [
  { key: "inbox", label: "Inbox", icon: InboxIcon, route: { kind: "inbox" } },
  {
    key: "pulls",
    label: "Pull requests",
    icon: GitPullRequestIcon,
    route: { kind: "placeholder", surface: "pulls", path: "/pulls" },
    count: "3",
  },
  { key: "ide", label: "Editor", icon: Code2Icon, route: { kind: "ide" } },
  {
    key: "library",
    label: "MCP & skills",
    icon: BlocksIcon,
    route: { kind: "placeholder", surface: "library", path: "/library" },
  },
  {
    key: "usage",
    label: "Usage",
    icon: ChartNoAxesColumnIcon,
    route: { kind: "placeholder", surface: "usage", path: "/usage" },
  },
];

function isActive(route: Route, key: string): boolean {
  if (key === "inbox") return route.kind === "inbox" || route.kind === "thread";
  if (key === "ide") return route.kind === "ide";
  if (route.kind === "placeholder") return route.surface === key;
  return false;
}

/** The prototype lists WSL distros under their Windows host. */
function parentHostId(hostId: string, wsl: boolean): string | null {
  if (!wsl) return null;
  const prefix = hostId.split("-")[0];
  return prefix && prefix !== hostId ? prefix : null;
}

export function Sidebar() {
  const route = useApp((state) => state.ui.route);
  const hosts = useApp((state) => state.hosts);
  const threads = useApp((state) => state.threads);
  const connection = useApp((state) => state.connection);
  const now = useNow();
  const [showAllHosts, setShowAllHosts] = useState(false);

  const needsYou = Object.values(threads).filter(
    (thread) =>
      !isSnoozed(thread, now) &&
      (thread.needs_attention ||
        thread.agent_state === "awaiting_approval" ||
        thread.agent_state === "error"),
  ).length;

  const hostList = Object.values(hosts);
  const online = hostList.filter((host) => host.online).length;
  const roots = hostList
    .filter((host) => !host.platform.startsWith("wsl:"))
    .sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name));
  const children = hostList.filter((host) => host.platform.startsWith("wsl:"));

  const rows: Array<{
    id: string;
    name: string;
    online: boolean;
    lastSeen: string | null;
    sub: boolean;
    harnesses: string;
  }> = [];
  for (const host of roots) {
    rows.push({
      id: host.host_id,
      name: host.name,
      online: host.online,
      lastSeen: host.online ? null : relativeAge(host.last_seen ?? "", now),
      sub: false,
      harnesses: host.harnesses.map(harnessLabel).join(", "),
    });
    for (const child of children) {
      if (parentHostId(child.host_id, true) !== host.host_id) continue;
      rows.push({
        id: child.host_id,
        name: child.name,
        online: child.online,
        lastSeen: child.online ? null : relativeAge(child.last_seen ?? "", now),
        sub: true,
        harnesses: child.harnesses.map(harnessLabel).join(", "),
      });
    }
  }
  for (const child of children) {
    if (rows.some((row) => row.id === child.host_id)) continue;
    rows.push({
      id: child.host_id,
      name: child.name,
      online: child.online,
      lastSeen: child.online ? null : relativeAge(child.last_seen ?? "", now),
      sub: true,
      harnesses: child.harnesses.map(harnessLabel).join(", "),
    });
  }

  const collapsed = rows.length > 6 && !showAllHosts;
  const visibleHosts = collapsed ? rows.slice(0, 6) : rows;
  const hidden = rows.length - visibleHosts.length;

  const firstOnline = rows.find((row) => row.online);
  const transport =
    connection.status === "online"
      ? `${connection.transport === "mock" ? "Direct" : connection.transport} · ${
          firstOnline?.name ?? "local"
        }${connection.latencyMs !== null ? ` ${Math.round(connection.latencyMs)} ms` : ""}`
      : connection.status === "connecting"
        ? "Connecting…"
        : connection.status === "degraded"
          ? "Reconnecting…"
          : "Offline";

  return (
    <nav className="q-side" aria-label="Main">
      <div className="q-side-top">
        <span className="q-word">h4rn</span>
        <button
          type="button"
          className="q-ibtn"
          title="New thread ⌘⇧N"
          aria-label="New thread"
          onClick={openNewThread}
        >
          <SquarePenIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        </button>
      </div>

      <button type="button" className="q-side-search" onClick={() => setUi({ paletteOpen: true })}>
        <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
        <span>Search or run</span>
        <span className="q-kbd">⌘K</span>
      </button>

      {NAV_ITEMS.map((item) => {
        const count = item.key === "inbox" ? (needsYou > 0 ? String(needsYou) : null) : item.count;
        const active = isActive(route, item.key);
        return (
          <button
            key={item.key}
            type="button"
            className="q-nav"
            aria-current={active ? "page" : undefined}
            onClick={() => navigate(item.route)}
          >
            <item.icon className="q-icon" strokeWidth={1.5} aria-hidden />
            <span className="lbl">{item.label}</span>
            {count ? (
              <span className={cn("ct num", item.key === "inbox" && "you")}>{count}</span>
            ) : null}
          </button>
        );
      })}

      <div className="q-side-h">
        <span>Hosts</span>
        <span className="num">
          {online} of {hostList.length}
        </span>
      </div>

      {visibleHosts.map((host) => (
        <button
          key={host.id}
          type="button"
          className={cn("q-host", host.sub && "sub")}
          title={host.harnesses}
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/hosts" })
          }
        >
          <EntityAvatar
            seed={machineSeed(host.id)}
            kind="machine"
            label={host.name}
            size={16}
            decorative={false}
          />
          <span className="lbl" style={host.online ? undefined : { color: "var(--fg-3)" }}>
            {host.name}
          </span>
          {host.online ? (
            <span className="q-dot on" role="img" aria-label="online" />
          ) : (
            <span className="meta num">{host.lastSeen}</span>
          )}
        </button>
      ))}

      {rows.length > 6 ? (
        <button
          type="button"
          className="q-host"
          onClick={() => setShowAllHosts((value) => !value)}
          style={{ color: "var(--fg-3)" }}
        >
          <span className="lbl">{collapsed ? `Show ${hidden} more` : "Show fewer"}</span>
        </button>
      ) : null}

      <div className="q-side-foot">
        <button
          type="button"
          className="q-nav"
          aria-current={
            route.kind === "placeholder" && route.surface === "settings" ? "page" : undefined
          }
          onClick={() =>
            navigate({ kind: "placeholder", surface: "settings", path: "/settings/hosts" })
          }
        >
          <SettingsIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          <span className="lbl">Settings</span>
        </button>
        <div className="q-conn">
          <span
            className={cn(
              "q-dot",
              connection.status === "online"
                ? "on"
                : connection.status === "degraded"
                  ? "warn"
                  : "off",
            )}
          />
          <span>{transport}</span>
        </div>
      </div>
    </nav>
  );
}
