import {
  ChartNoAxesColumnIcon,
  GitPullRequestIcon,
  InboxIcon,
  ServerIcon,
  SettingsIcon,
} from "lucide-react";
import type * as React from "react";

import { useNow } from "@/hooks/use-now";
import { isSnoozed } from "@/lib/inbox";
import type { Route } from "@/lib/router";
import { navigate, useApp } from "@/lib/store/appStore";

const TABS = [
  { key: "inbox", label: "Inbox", icon: InboxIcon, route: { kind: "inbox" } as Route },
  {
    key: "pulls",
    label: "PRs",
    icon: GitPullRequestIcon,
    route: { kind: "placeholder", surface: "pulls", path: "/pulls" } as Route,
  },
  {
    key: "usage",
    label: "Usage",
    icon: ChartNoAxesColumnIcon,
    route: { kind: "placeholder", surface: "usage", path: "/usage" } as Route,
  },
  {
    key: "pairing",
    label: "Hosts",
    icon: ServerIcon,
    route: { kind: "placeholder", surface: "settings", path: "/settings/hosts" } as Route,
  },
  {
    key: "settings",
    label: "Settings",
    icon: SettingsIcon,
    route: { kind: "placeholder", surface: "settings", path: "/settings/hosts" } as Route,
  },
];

function active(route: Route, key: string): boolean {
  if (key === "inbox") return route.kind === "inbox" || route.kind === "thread";
  if (key === "pairing") {
    return route.kind === "placeholder" && route.path === "/settings/hosts";
  }
  if (key === "settings") {
    return (
      route.kind === "placeholder" &&
      (route.surface === "library" ||
        (route.surface === "settings" && route.path !== "/settings/hosts"))
    );
  }
  return route.kind === "placeholder" && route.surface === key;
}

export function PhoneNav() {
  const route = useApp((state) => state.ui.route);
  const threads = useApp((state) => state.threads);
  const now = useNow();
  const needsYou = Object.values(threads).filter(
    (thread) =>
      !isSnoozed(thread, now) &&
      (thread.needs_attention ||
        thread.agent_state === "awaiting_approval" ||
        thread.agent_state === "error"),
  ).length;

  return (
    <nav className="q-ph-tabbar" aria-label="Main">
      {TABS.map((tab) => (
        <button
          key={tab.key}
          type="button"
          className="q-ph-tab"
          aria-current={active(route, tab.key) ? "page" : undefined}
          onClick={() => navigate(tab.route)}
        >
          <tab.icon className="q-icon s24" size={24} strokeWidth={1.5} aria-hidden />
          {tab.key === "inbox" && needsYou > 0 ? <span className="bdg">{needsYou}</span> : null}
          <span>{tab.label}</span>
        </button>
      ))}
    </nav>
  );
}

export function PhoneLargeTitle({
  title,
  children,
  actions,
}: {
  title: string;
  children?: React.ReactNode;
  actions?: React.ReactNode;
}) {
  return (
    <>
      <div className="q-ph-nav">
        <span style={{ width: 44 }} />
        <span className="ttl" />
        {actions}
      </div>
      <h1 className="q-ph-lt">{title}</h1>
      {children}
    </>
  );
}

export function phoneActive(route: Route, key: string): boolean {
  return active(route, key);
}
