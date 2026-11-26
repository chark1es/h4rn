import {
  ChartNoAxesColumnIcon,
  GitPullRequestIcon,
  InboxIcon,
  ServerIcon,
  SettingsIcon,
} from "lucide-react";
import type * as React from "react";
import { useTranslation } from "react-i18next";

import { pendingMcpApprovals, useMcp } from "@/features/mcp/store";
import { useNow } from "@/hooks/use-now";
import { formatNumber } from "@/lib/i18n";
import { isSnoozed } from "@/lib/inbox";
import type { Route } from "@/lib/router";
import { navigate, useApp } from "@/lib/store/appStore";

/**
 * The phone's tab bar. Each tab uses the name its destination has in the
 * sidebar, so a place is called the same everywhere; only pull requests get a
 * shorter name here, to fit five tabs on a phone.
 */
const TABS = [
  { key: "inbox", labelKey: "inbox", icon: InboxIcon, route: { kind: "inbox" } as Route },
  {
    key: "pulls",
    labelKey: "phoneNav.pulls",
    icon: GitPullRequestIcon,
    route: { kind: "placeholder", surface: "pulls", path: "/pulls" } as Route,
  },
  {
    key: "usage",
    labelKey: "footer.usage",
    icon: ChartNoAxesColumnIcon,
    route: { kind: "placeholder", surface: "usage", path: "/usage" } as Route,
  },
  {
    key: "pairing",
    labelKey: "hosts.title",
    icon: ServerIcon,
    route: { kind: "placeholder", surface: "settings", path: "/settings/hosts" } as Route,
  },
  {
    key: "settings",
    labelKey: "footer.settings",
    icon: SettingsIcon,
    route: { kind: "placeholder", surface: "settings", path: "/settings/hosts" } as Route,
  },
] as const;

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
  const { t } = useTranslation("sidebar");
  const route = useApp((state) => state.ui.route);
  const threads = useApp((state) => state.threads);
  const approvals = useMcp((state) => state.approvals);
  const now = useNow();
  const needsYou =
    Object.values(threads).filter(
      (thread) =>
        !isSnoozed(thread, now) &&
        (thread.needs_attention ||
          thread.agent_state === "awaiting_approval" ||
          thread.agent_state === "error"),
    ).length + pendingMcpApprovals(approvals).length;

  return (
    <nav className="q-ph-tabbar" aria-label={t("phoneNav.label")}>
      {TABS.map((tab) => (
        <button
          key={tab.key}
          type="button"
          className="q-ph-tab"
          aria-current={active(route, tab.key) ? "page" : undefined}
          onClick={() => navigate(tab.route)}
        >
          <tab.icon className="q-icon s24" size={24} strokeWidth={1.5} aria-hidden />
          {tab.key === "inbox" && needsYou > 0 ? (
            <span className="bdg">{formatNumber(needsYou)}</span>
          ) : null}
          <span>{t(tab.labelKey)}</span>
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
